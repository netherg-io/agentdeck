//! Loopback T3 thread controller + read-only `statev2.sqlite` observer
//! (ADR-0002, Q16). The daemon never starts or stops T3 itself: it only
//! configures and wakes the Coordinator thread through T3's local MCP
//! endpoint, and only when that endpoint is provably loopback.

use std::collections::HashMap;
use std::path::Path;

use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};

/// Reject anything that is not an `http://` loopback URL without credentials,
/// query or fragment (same bar as `agentdeck-router::mcp_call`: a daemon that
/// can be pointed at a remote host is a credential leak).
pub fn check_loopback_url(endpoint: &str) -> anyhow::Result<()> {
    if endpoint.chars().any(|c| c.is_control() || c == ' ' || c == '\u{7f}') {
        anyhow::bail!("invalid T3 MCP endpoint: control characters or spaces");
    }
    let rest = endpoint
        .strip_prefix("http://")
        .ok_or_else(|| anyhow::anyhow!("T3 MCP endpoint must be loopback http:// (got {endpoint:?})"))?;
    let authority = rest.split('/').next().unwrap_or("");
    if authority.contains('@') || rest.contains(['?', '#']) {
        anyhow::bail!("T3 MCP endpoint must be loopback http:// without userinfo, query or fragment");
    }
    // Split host from port, honouring a bracketed `[::1]` literal.
    let host = if let Some(bracketed) = authority.strip_prefix('[') {
        bracketed.split(']').next().unwrap_or("")
    } else {
        authority.rsplit_once(':').map(|(h, _)| h).unwrap_or(authority)
    };
    if host != "127.0.0.1" && host != "localhost" && host != "::1" {
        anyhow::bail!("T3 MCP endpoint must be loopback (127.0.0.1, localhost or [::1])");
    }
    Ok(())
}

/// `Send` is deliberately unbounded: the daemon awaits these futures
/// in place and never spawns them.
#[allow(async_fn_in_trait)]
pub trait T3ThreadController {
    async fn configure_thread(
        &self,
        thread_id: &str,
        instance_id: &str,
        model: &str,
        options: &HashMap<String, serde_json::Value>,
    ) -> anyhow::Result<()>;
    async fn send_message(
        &self,
        thread_id: &str,
        message: &str,
        fresh_context: bool,
    ) -> anyhow::Result<()>;
}

/// JSON-RPC 2.0 Streamable HTTP client for the loopback T3 MCP endpoint.
/// Each call runs `initialize` then one `tools/call` (no session is kept:
/// the daemon ticks rarely, and a stale session id is worse than one extra
/// round trip).
pub struct HttpT3Controller {
    endpoint: String,
    authorization: Option<String>,
    client: reqwest::Client,
}

impl HttpT3Controller {
    pub fn new(endpoint: &str, token_file: Option<&Path>) -> anyhow::Result<Self> {
        check_loopback_url(endpoint)?;
        let authorization = token_file
            .map(std::fs::read_to_string)
            .transpose()
            .map_err(|e| anyhow::anyhow!("cannot read T3 token file: {e}"))?
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty());
        Ok(Self {
            endpoint: endpoint.to_string(),
            authorization,
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|_| anyhow::anyhow!("cannot build T3 MCP client"))?,
        })
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    async fn rpc(&self, method: &str, params: Option<serde_json::Value>, id: u64) -> anyhow::Result<serde_json::Value> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params.unwrap_or(serde_json::Value::Null),
        });
        let mut req = self
            .client
            .post(&self.endpoint)
            .timeout(std::time::Duration::from_secs(30))
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream");
        if let Some(auth) = &self.authorization {
            req = req.header("authorization", auth.clone());
        }
        let res = req
            .body(body.to_string())
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("T3 MCP tool call failed."))?;
        if !res.status().is_success() {
            anyhow::bail!("T3 MCP HTTP {}.", res.status().as_u16());
        }
        let is_sse = res
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("text/event-stream"));
        let text = res
            .text()
            .await
            .map_err(|_| anyhow::anyhow!("T3 MCP returned an unreadable body."))?;
        let envelope: serde_json::Value = if is_sse {
            parse_sse_envelope(&text, id)?
        } else {
            serde_json::from_str(&text)
                .map_err(|_| anyhow::anyhow!("Invalid T3 MCP JSON envelope."))?
        };
        if envelope.get("id") != Some(&serde_json::Value::from(id))
            || !envelope.get("result").is_some_and(|r| r.is_object())
        {
            anyhow::bail!("T3 MCP rejected the request or returned an invalid response.");
        }
        Ok(envelope["result"].clone())
    }

    async fn call_tool(&self, name: &str, arguments: serde_json::Value) -> anyhow::Result<()> {
        self.rpc(
            "initialize",
            Some(serde_json::json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "agentdeck-coordinator", "version": "1"},
            })),
            1,
        )
        .await?;
        let result = self
            .rpc(
                "tools/call",
                Some(serde_json::json!({"name": name, "arguments": arguments})),
                2,
            )
            .await?;
        if result.get("isError").and_then(serde_json::Value::as_bool) == Some(true) {
            anyhow::bail!("T3 MCP tool {name} returned isError");
        }
        Ok(())
    }
}

/// First `data:` JSON envelope whose `id` matches, like the
/// `agentdeck-router::mcp_call` SSE scan.
fn parse_sse_envelope(text: &str, id: u64) -> anyhow::Result<serde_json::Value> {
    let mut rest = text;
    while let Some(pos) = rest.find("\r\n\r\n").or_else(|| rest.find("\n\n")) {
        let sep = if rest[pos..].starts_with("\r\n") { 4 } else { 2 };
        let event = &rest[..pos];
        rest = &rest[pos + sep..];
        let mut data = String::new();
        for line in event.split(['\r', '\n']) {
            if let Some(d) = line.strip_prefix("data:") {
                let d = d.strip_prefix(' ').unwrap_or(d);
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(d);
            }
        }
        if data.is_empty() {
            continue;
        }
        let candidate: serde_json::Value = serde_json::from_str(&data)
            .map_err(|_| anyhow::anyhow!("Invalid T3 MCP SSE envelope."))?;
        if candidate.is_object() && candidate.get("id") == Some(&serde_json::Value::from(id)) {
            return Ok(candidate);
        }
    }
    anyhow::bail!("T3 MCP rejected the request or returned an invalid response.")
}

impl T3ThreadController for HttpT3Controller {
    async fn configure_thread(
        &self,
        thread_id: &str,
        instance_id: &str,
        model: &str,
        options: &HashMap<String, serde_json::Value>,
    ) -> anyhow::Result<()> {
        let mut selection = serde_json::json!({
            "instanceId": instance_id,
            "model": model,
        });
        if !options.is_empty() {
            selection["options"] = serde_json::json!(options);
        }
        self.call_tool(
            "t3_thread_configure",
            serde_json::json!({
                "threadId": thread_id,
                "modelSelection": selection,
            }),
        )
        .await
    }

    async fn send_message(
        &self,
        thread_id: &str,
        message: &str,
        fresh_context: bool,
    ) -> anyhow::Result<()> {
        self.call_tool(
            "t3_thread_send",
            serde_json::json!({
                "threadId": thread_id,
                "message": message,
                "freshContext": fresh_context,
            }),
        )
        .await
    }
}

/// Read-only view of one Coordinator thread, for the Watchdog tick.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ObservedThread {
    pub thread_id: String,
    pub title: Option<String>,
    pub is_running: bool,
    pub active_run_id: Option<String>,
    pub provider_instance_id: Option<String>,
    pub model: Option<String>,
    pub run_count: u64,
    pub item_count: u64,
    pub updated_at_epoch: i64,
    pub last_message_text: String,
    pub pending_background_tasks: u64,
    pub hit_quota_limit: bool,
}

/// Substrings (lowercased match) proving the last provider message died on a
/// quota limit: Claude 5h/weekly, Antigravity buckets, Codex limits, and the
/// generic rate-limit shapes around them.
pub fn is_quota_limit_message(text: &str) -> bool {
    const MARKERS: &[&str] = &[
        "5-hour limit",
        "5 hour limit",
        "5h limit",
        "weekly limit",
        "rate limit",
        "rate_limit",
        "quota exceeded",
        "quota limit",
        "usage limit",
        "limit reached",
        "plan limit",
        "too many requests",
        "you've hit your",
        "you have hit your",
        "try again at",
        "resets at",
        "come back after",
    ];
    let lower = text.to_lowercase();
    MARKERS.iter().any(|m| lower.contains(m))
}

fn open_state_db(t3_root: &Path) -> Option<Connection> {
    let db = t3_root.join("userdata/statev2.sqlite");
    if !db.exists() {
        return None;
    }
    let conn = Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    let _ = conn.execute_batch("PRAGMA busy_timeout=1000");
    Some(conn)
}

fn table_exists(conn: &Connection, table: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
        [table],
        |_| Ok(()),
    )
    .is_ok()
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> bool {
    // Only called with static table names from this module.
    let sql = format!("PRAGMA table_info({table})");
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return false;
    };
    let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(1)) else {
        return false;
    };
    rows.flatten().any(|name| name == column)
}

struct ThreadRow {
    title: Option<String>,
    payload: serde_json::Value,
    updated_at: Option<String>,
    provider_instance_id: Option<String>,
}

fn thread_row(conn: &Connection, thread_id: &str) -> Option<ThreadRow> {
    if !table_exists(conn, "orchestration_v2_projection_threads") {
        return None;
    }
    let has_updated = column_exists(conn, "orchestration_v2_projection_threads", "updated_at");
    let has_inst = column_exists(
        conn,
        "orchestration_v2_projection_threads",
        "provider_instance_id",
    );
    let sql = match (has_updated, has_inst) {
        (true, true) => {
            "SELECT title, payload_json, updated_at, provider_instance_id \
             FROM orchestration_v2_projection_threads WHERE thread_id=?1"
        }
        (true, false) => {
            "SELECT title, payload_json, updated_at, NULL \
             FROM orchestration_v2_projection_threads WHERE thread_id=?1"
        }
        (false, true) => {
            "SELECT title, payload_json, NULL, provider_instance_id \
             FROM orchestration_v2_projection_threads WHERE thread_id=?1"
        }
        (false, false) => {
            "SELECT title, payload_json, NULL, NULL \
             FROM orchestration_v2_projection_threads WHERE thread_id=?1"
        }
    };
    let (title, payload, updated_at, provider_instance_id): (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = conn
        .query_row(sql, [thread_id], |r| {
            Ok((r.get(0).ok(), r.get(1).ok(), r.get(2).ok(), r.get(3).ok()))
        })
        .ok()?;
    let payload = payload
        .and_then(|p| serde_json::from_str(&p).ok())
        .unwrap_or(serde_json::Value::Null);
    Some(ThreadRow {
        title,
        payload,
        updated_at,
        provider_instance_id,
    })
}

fn payload_str(payload: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| {
        payload
            .get(*k)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    })
}

fn payload_u64(payload: &serde_json::Value, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|k| {
        payload.get(*k).and_then(|v| {
            v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        })
    })
}

fn parse_epoch_str(s: &str) -> Option<i64> {
    s.parse::<i64>()
        .ok()
        .or_else(|| crate::pool::parse_epoch_rfc3339(s))
}

fn payload_epoch(payload: &serde_json::Value, keys: &[&str]) -> Option<i64> {
    keys.iter().find_map(|k| {
        payload.get(*k).and_then(|v| {
            v.as_i64()
                .or_else(|| v.as_f64().map(|f| f as i64))
                .or_else(|| v.as_str().and_then(parse_epoch_str))
        })
    })
}

/// Read-only inspection of `statev2.sqlite` for one thread. Returns
/// `Ok(None)` when the db, the table or the thread is absent; unknown
/// columns/keys degrade to defaults rather than failing the tick. T3's
/// projection schema drifts between releases, so every probe is best-effort.
pub fn observe_thread_sqlite(t3_root: &Path, thread_id: &str) -> anyhow::Result<Option<ObservedThread>> {
    let Some(conn) = open_state_db(t3_root) else {
        return Ok(None);
    };
    let Some(trow) = thread_row(&conn, thread_id) else {
        return Ok(None);
    };
    let thread_payload = trow.payload;
    let mut observed = ObservedThread {
        thread_id: thread_id.to_string(),
        title: trow.title,
        ..Default::default()
    };
    observed.run_count = payload_u64(&thread_payload, &["runCount", "run_count"]).unwrap_or(0);
    observed.item_count = payload_u64(&thread_payload, &["itemCount", "item_count"]).unwrap_or(0);
    observed.updated_at_epoch = trow
        .updated_at
        .as_deref()
        .and_then(parse_epoch_str)
        .or_else(|| payload_epoch(&thread_payload, &["updatedAt", "updated_at"]))
        .unwrap_or(0);

    if table_exists(&conn, "orchestration_v2_projection_thread_item_counts")
        && let Ok(count) = conn.query_row(
            "SELECT item_count FROM orchestration_v2_projection_thread_item_counts WHERE thread_id=?1",
            [thread_id],
            |r| r.get::<_, u64>(0),
        )
    {
        observed.item_count = observed.item_count.max(count);
    }

    let mut latest_run_id: Option<String> = None;
    let mut latest_run_status: Option<String> = None;

    if table_exists(&conn, "orchestration_v2_projection_runs") {
        let order = if column_exists(&conn, "orchestration_v2_projection_runs", "ordinal") {
            "ORDER BY ordinal DESC"
        } else {
            "ORDER BY rowid DESC"
        };
        let has_timestamps =
            column_exists(&conn, "orchestration_v2_projection_runs", "requested_at")
                && column_exists(&conn, "orchestration_v2_projection_runs", "completed_at");
        let sql = if has_timestamps {
            format!(
                "SELECT run_id, provider_instance_id, payload_json, status, requested_at, completed_at \
                 FROM orchestration_v2_projection_runs WHERE thread_id=?1 {order}"
            )
        } else {
            format!(
                "SELECT run_id, provider_instance_id, payload_json, status, NULL, NULL \
                 FROM orchestration_v2_projection_runs WHERE thread_id=?1 {order}"
            )
        };
        struct RunRow {
            run_id: String,
            instance: Option<String>,
            payload_json: Option<String>,
            status: String,
            requested_at: Option<String>,
            completed_at: Option<String>,
        }
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| anyhow::anyhow!("run projection query failed: {e}"))?;
        let rows: Vec<RunRow> = stmt
            .query_map([thread_id], |r| {
                Ok(RunRow {
                    run_id: r.get(0)?,
                    instance: r.get(1).ok(),
                    payload_json: r.get(2).ok(),
                    status: r.get(3)?,
                    requested_at: r.get(4).ok(),
                    completed_at: r.get(5).ok(),
                })
            })
            .map_err(|e| anyhow::anyhow!("run projection query failed: {e}"))?
            .filter_map(|r| r.ok())
            .collect();
        observed.run_count = observed.run_count.max(rows.len() as u64);
        for (idx, row) in rows.iter().enumerate() {
            let (run_id, instance, payload_json, status, requested_at, completed_at) = (
                &row.run_id,
                &row.instance,
                &row.payload_json,
                &row.status,
                &row.requested_at,
                &row.completed_at,
            );
            if idx == 0 {
                latest_run_id = Some(run_id.clone());
                latest_run_status = Some(status.clone());
            }
            let live = matches!(
                status.as_str(),
                "queued" | "preparing" | "starting" | "running" | "waiting"
            );
            if live {
                observed.is_running = true;
                if status == "running" || observed.active_run_id.is_none() {
                    observed.active_run_id = Some(run_id.clone());
                }
            }
            if observed.provider_instance_id.is_none() {
                observed.provider_instance_id = instance.clone();
            }
            let payload: serde_json::Value = payload_json
                .as_deref()
                .and_then(|p| serde_json::from_str(p).ok())
                .unwrap_or(serde_json::Value::Null);
            if observed.provider_instance_id.is_none() {
                observed.provider_instance_id = payload_str(&payload, &["providerInstanceId"])
                    .or_else(|| {
                        payload
                            .pointer("/modelSelection/instanceId")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string)
                    });
            }
            if observed.model.is_none() {
                observed.model = payload_str(&payload, &["model", "modelSelection.model"]).or_else(
                    || {
                        payload
                            .pointer("/modelSelection/model")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string)
                    },
                );
            }
            // Latest run first wins for tail text.
            if observed.last_message_text.is_empty() {
                observed.last_message_text = payload_str(
                    &payload,
                    &["lastMessage", "last_message", "lastAssistantMessage", "tail"],
                )
                .unwrap_or_default();
            }
            for ts in [
                completed_at.as_deref().and_then(parse_epoch_str),
                requested_at.as_deref().and_then(parse_epoch_str),
                payload_epoch(
                    &payload,
                    &[
                        "completedAt",
                        "updatedAt",
                        "updated_at",
                        "startedAt",
                        "requestedAt",
                    ],
                ),
            ]
            .into_iter()
            .flatten()
            {
                observed.updated_at_epoch = observed.updated_at_epoch.max(ts);
            }
        }
    }

    // Fallback provider_instance_id / model from the thread row itself when no run supplied them.
    if observed.provider_instance_id.is_none() {
        observed.provider_instance_id = trow
            .provider_instance_id
            .or_else(|| payload_str(&thread_payload, &["providerInstanceId"]))
            .or_else(|| {
                thread_payload
                    .pointer("/modelSelection/instanceId")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            });
    }
    if observed.model.is_none() {
        observed.model = thread_payload
            .pointer("/modelSelection/model")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
    }

    // Real T3 records structured quota failures as failed `error` turn_items on the latest run.
    let mut structured_usage_limit = false;
    if !observed.is_running
        && let Some(run_id) = latest_run_id.as_deref()
        && matches!(
            latest_run_status.as_deref(),
            Some("failed" | "interrupted" | "completed")
        )
        && table_exists(&conn, "orchestration_v2_projection_turn_items")
    {
        let order = if column_exists(&conn, "orchestration_v2_projection_turn_items", "ordinal") {
            "ORDER BY ordinal DESC"
        } else {
            "ORDER BY rowid DESC"
        };
        let sql = format!(
            "SELECT payload_json FROM orchestration_v2_projection_turn_items \
             WHERE thread_id=?1 AND run_id=?2 AND type='error' AND status='failed' \
             {order} LIMIT 1"
        );
        if let Ok(raw) = conn.query_row(&sql, [thread_id, run_id], |r| r.get::<_, String>(0))
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw)
        {
            if v.pointer("/failure/class").and_then(serde_json::Value::as_str) == Some("usage_limit")
            {
                structured_usage_limit = true;
            }
            if observed.last_message_text.is_empty() {
                observed.last_message_text = v
                    .pointer("/failure/message")
                    .and_then(serde_json::Value::as_str)
                    .or_else(|| v.get("title").and_then(serde_json::Value::as_str))
                    .unwrap_or("")
                    .to_string();
            }
        }
    }

    // Real T3 stores assistant messages in orchestration_v2_projection_messages.
    if observed.last_message_text.is_empty()
        && table_exists(&conn, "orchestration_v2_projection_messages")
        && let Ok(Some(text)) = conn.query_row(
            "SELECT json_extract(payload_json, '$.text') \
             FROM orchestration_v2_projection_messages \
             WHERE thread_id=?1 AND role='assistant' \
             ORDER BY updated_at DESC, message_id DESC LIMIT 1",
            [thread_id],
            |r| r.get::<_, Option<String>>(0),
        )
    {
        observed.last_message_text = text;
    }

    if observed.last_message_text.is_empty() {
        observed.last_message_text = payload_str(
            &thread_payload,
            &["lastMessage", "last_message", "lastAssistantMessage", "tail"],
        )
        .unwrap_or_default();
    }

    // Background work that will wake the thread on its own: live subagents
    // or pendingBackgroundTasks on provider threads.
    let mut pending: u64 = 0;
    if table_exists(&conn, "orchestration_v2_projection_subagents")
        && table_exists(&conn, "orchestration_v2_projection_runs")
    {
        pending += conn
            .query_row(
                "SELECT COUNT(*) FROM orchestration_v2_projection_subagents n \
                 JOIN orchestration_v2_projection_runs r USING (run_id) \
                 WHERE r.thread_id=?1 AND n.status IN ('running','queued','waiting','starting','pending')",
                [thread_id],
                |r| r.get::<_, u64>(0),
            )
            .unwrap_or(0);
    }
    if table_exists(&conn, "orchestration_v2_projection_provider_threads") {
        pending += conn
            .query_row(
                "SELECT COALESCE(SUM(json_array_length(payload_json, '$.pendingBackgroundTasks')), 0) \
                 FROM orchestration_v2_projection_provider_threads \
                 WHERE thread_id=?1 AND json_valid(payload_json) \
                 AND json_type(payload_json, '$.pendingBackgroundTasks') = 'array'",
                [thread_id],
                |r| r.get::<_, u64>(0),
            )
            .unwrap_or(0);
    }
    observed.pending_background_tasks = pending;
    observed.hit_quota_limit =
        structured_usage_limit || is_quota_limit_message(&observed.last_message_text);
    Ok(Some(observed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observer_matches_real_statev2_schema() {
        let dir = std::env::temp_dir().join(format!(
            "agentdeck-t3-real-schema-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("userdata")).unwrap();
        let db = Connection::open(dir.join("userdata/statev2.sqlite")).unwrap();
        db.execute_batch(
            "CREATE TABLE orchestration_v2_projection_threads (
               thread_id TEXT PRIMARY KEY,
               title TEXT NOT NULL,
               updated_at TEXT NOT NULL,
               payload_json TEXT NOT NULL,
               provider_instance_id TEXT
             );
             CREATE TABLE orchestration_v2_projection_runs (
               run_id TEXT PRIMARY KEY,
               thread_id TEXT NOT NULL,
               ordinal INTEGER NOT NULL,
               status TEXT NOT NULL,
               requested_at TEXT NOT NULL,
               completed_at TEXT,
               payload_json TEXT NOT NULL,
               provider_instance_id TEXT
             );
             CREATE TABLE orchestration_v2_projection_thread_item_counts (
               thread_id TEXT PRIMARY KEY,
               item_count INTEGER NOT NULL
             );
             CREATE TABLE orchestration_v2_projection_turn_items (
               turn_item_id TEXT PRIMARY KEY,
               thread_id TEXT NOT NULL,
               run_id TEXT,
               ordinal INTEGER NOT NULL,
               type TEXT NOT NULL,
               status TEXT NOT NULL,
               payload_json TEXT NOT NULL
             );
             CREATE TABLE orchestration_v2_projection_messages (
               message_id TEXT PRIMARY KEY,
               thread_id TEXT NOT NULL,
               role TEXT NOT NULL,
               updated_at TEXT NOT NULL,
               payload_json TEXT NOT NULL
             );
             CREATE TABLE orchestration_v2_projection_provider_threads (
               provider_thread_id TEXT PRIMARY KEY,
               thread_id TEXT,
               payload_json TEXT NOT NULL
             );",
        )
        .unwrap();
        db.execute(
            "INSERT INTO orchestration_v2_projection_threads VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                "t-real",
                "Real Thread",
                "2026-11-21T02:00:00Z",
                r#"{"modelSelection":{"instanceId":"codex_b","model":"gpt-6.2-sol"}}"#,
                "codex_b",
            ],
        )
        .unwrap();
        // Run 1 succeeded on claude_a; run 2 failed with structured usage_limit on codex_b.
        db.execute(
            "INSERT INTO orchestration_v2_projection_runs VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            rusqlite::params![
                "r1",
                "t-real",
                1,
                "completed",
                "2026-11-21T01:00:00Z",
                "2026-11-21T01:10:00Z",
                r#"{"modelSelection":{"instanceId":"claude_a","model":"claude-opus-4-6"}}"#,
                "claude_a",
            ],
        )
        .unwrap();
        db.execute(
            "INSERT INTO orchestration_v2_projection_runs VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            rusqlite::params![
                "r2",
                "t-real",
                2,
                "failed",
                "2026-11-21T02:00:00Z",
                "2026-11-21T02:05:00Z",
                r#"{"modelSelection":{"instanceId":"codex_b","model":"gpt-6.2-sol"}}"#,
                "codex_b",
            ],
        )
        .unwrap();
        db.execute(
            "INSERT INTO orchestration_v2_projection_thread_item_counts VALUES ('t-real', 142)",
            [],
        )
        .unwrap();
        db.execute(
            "INSERT INTO orchestration_v2_projection_turn_items VALUES ('ti1','t-real','r2',1,'error','failed',?1)",
            [r#"{"title":"Usage limit reached","failure":{"class":"usage_limit","message":"API rate limit exceeded"}}"#],
        )
        .unwrap();
        db.execute(
            "INSERT INTO orchestration_v2_projection_provider_threads VALUES ('pt1','t-real',?1)",
            [r#"{"pendingBackgroundTasks":["bg-1","bg-2"]}"#],
        )
        .unwrap();

        let obs = observe_thread_sqlite(&dir, "t-real").unwrap().unwrap();
        assert_eq!(obs.provider_instance_id.as_deref(), Some("codex_b"));
        assert_eq!(obs.model.as_deref(), Some("gpt-6.2-sol"));
        assert_eq!(obs.run_count, 2);
        assert_eq!(obs.item_count, 142);
        assert!(obs.hit_quota_limit);
        assert!(obs.last_message_text.contains("API rate limit"));
        assert_eq!(obs.pending_background_tasks, 2);
        assert_eq!(
            obs.updated_at_epoch,
            crate::pool::parse_epoch_rfc3339("2026-11-21T02:05:00Z").unwrap()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
