//! Port of `load.mjs`: live T3 agent metadata from the state SQLite database.

use rusqlite::OpenFlags;

use crate::policy::ActiveAgent;

const ACTIVE_AGENTS_SQL: &str = "
      WITH active_turns AS (
        SELECT DISTINCT p.provider_thread_id AS id, p.provider_instance_id AS provider,
          COALESCE(json_extract(n.payload_json, '$.model'), s.model) AS model
        FROM orchestration_v2_projection_provider_turns v
        JOIN orchestration_v2_projection_provider_threads p USING (provider_thread_id)
        LEFT JOIN orchestration_v2_projection_provider_sessions s USING (provider_session_id)
        LEFT JOIN orchestration_v2_projection_subagents n
          ON n.provider_thread_id = p.provider_thread_id AND n.origin = 'provider_native' AND n.status = 'running'
        WHERE v.status = 'running' AND p.status = 'active' AND (s.status IS NULL OR s.status = 'ready')
      )
      SELECT id, provider, model FROM active_turns
      UNION ALL
      -- Claude/agy native children have no provider thread or turn of their own. The model stays
      -- NULL unless the child payload names it (agent types may override the parent's model), so
      -- callers reserve the maximum burn; a stopped parent session marks a leftover 'running' row.
      SELECT n.subagent_id AS id,
        COALESCE(n.provider_instance_id, json_extract(n.payload_json, '$.providerInstanceId')) AS provider,
        COALESCE(json_extract(n.payload_json, '$.model'), json_extract(n.payload_json, '$.modelSelection.model')) AS model
      FROM orchestration_v2_projection_subagents n
      JOIN orchestration_v2_projection_runs r USING (run_id)
      LEFT JOIN orchestration_v2_projection_provider_threads p ON p.provider_thread_id = r.provider_thread_id
      LEFT JOIN orchestration_v2_projection_provider_sessions s ON s.provider_session_id = p.provider_session_id
      WHERE n.origin = 'provider_native' AND n.status = 'running' AND n.provider_thread_id IS NULL
        AND (s.status IS NULL OR s.status = 'ready')
      UNION ALL
      -- A held queued run (queueHeld) waits for a manual release and may never start: no slot.
      SELECT r.run_id AS id, r.provider_instance_id AS provider,
        CASE WHEN r.status = 'waiting'
          THEN COALESCE(s.model, json_extract(r.payload_json, '$.modelSelection.model'))
          ELSE COALESCE(json_extract(r.payload_json, '$.modelSelection.model'), s.model) END AS model
      FROM orchestration_v2_projection_runs r
      LEFT JOIN orchestration_v2_projection_provider_threads p USING (provider_thread_id)
      LEFT JOIN orchestration_v2_projection_provider_sessions s USING (provider_session_id)
      WHERE r.status IN ('queued', 'preparing', 'starting', 'waiting', 'running')
        AND NOT (r.status = 'queued' AND json_extract(r.payload_json, '$.queueHeld') IS 1)
        AND NOT (r.status = 'running' AND EXISTS (SELECT 1 FROM active_turns live WHERE live.id = r.provider_thread_id))
        AND NOT EXISTS (
          SELECT 1 FROM orchestration_v2_projection_provider_turns v
          JOIN orchestration_v2_projection_run_attempts a ON a.attempt_id = v.run_attempt_id
          JOIN active_turns live ON live.id = v.provider_thread_id
          WHERE a.run_id = r.run_id AND v.status = 'running'
        )
    ";

pub struct AgentLoad {
    pub known: bool,
    pub agents: Vec<ActiveAgent>,
    pub note: Option<String>,
}

pub fn read_active_agents(file: &std::path::Path) -> AgentLoad {
    let unknown = || AgentLoad {
        known: false,
        agents: vec![],
        note: Some("live T3 agent metadata unavailable".into()),
    };
    let Ok(db) = rusqlite::Connection::open_with_flags(file, OpenFlags::SQLITE_OPEN_READ_ONLY)
    else {
        return unknown();
    };
    if db.execute_batch("PRAGMA busy_timeout=1000").is_err() {
        return unknown();
    }
    let agents = (|| {
        let mut stmt = db.prepare(ACTIVE_AGENTS_SQL)?;
        let rows = stmt.query_map([], |row| {
            Ok(ActiveAgent {
                id: row.get::<_, String>(0)?,
                provider: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                model: row.get::<_, Option<String>>(2)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
    })();
    match agents {
        Ok(agents) => AgentLoad {
            known: true,
            agents,
            note: None,
        },
        Err(_) => unknown(),
    }
}

/// Hook helper (part 2 uses it): whether a native session/thread id belongs to
/// an app-owned delegated child.
pub fn is_delegated_child(
    native_id: Option<&str>,
    thread_id: Option<&str>,
    file: &std::path::Path,
) -> bool {
    if native_id.is_none() && thread_id.is_none() {
        return false;
    }
    let Ok(db) = rusqlite::Connection::open_with_flags(file, OpenFlags::SQLITE_OPEN_READ_ONLY)
    else {
        return false;
    };
    if db.execute_batch("PRAGMA busy_timeout=1000").is_err() {
        return false;
    }
    db.query_row(
        "SELECT 1 FROM orchestration_v2_projection_subagents
      WHERE child_thread_id = ?1 OR child_thread_id IN (
        SELECT thread_id FROM orchestration_v2_projection_provider_threads
        WHERE json_extract(payload_json, '$.nativeThreadRef.nativeId') = ?2)
      LIMIT 1",
        rusqlite::params![thread_id, native_id],
        |_| Ok(()),
    )
    .is_ok()
}

/// Ported from the legacy Node `load.test.mjs` (deleted with the router
/// `.mjs` sources): the active-agent matrix over a real projection schema.
#[cfg(test)]
mod legacy_suite_tests {
    use super::*;

    const SCHEMA: &str = "
      CREATE TABLE orchestration_v2_projection_subagents (subagent_id TEXT PRIMARY KEY, thread_id TEXT NOT NULL,
        run_id TEXT, parent_node_id TEXT NOT NULL, provider TEXT NOT NULL, provider_thread_id TEXT, child_thread_id TEXT,
        origin TEXT NOT NULL, status TEXT NOT NULL, started_at TEXT, completed_at TEXT, updated_at TEXT NOT NULL,
        payload_json TEXT NOT NULL, driver TEXT, provider_instance_id TEXT);
      CREATE TABLE orchestration_v2_projection_provider_threads (provider_thread_id TEXT PRIMARY KEY, thread_id TEXT,
        owner_node_id TEXT, provider TEXT NOT NULL, provider_session_id TEXT, status TEXT NOT NULL, first_run_ordinal INTEGER,
        last_run_ordinal INTEGER, updated_at TEXT NOT NULL, payload_json TEXT NOT NULL, driver TEXT, provider_instance_id TEXT);
      CREATE TABLE orchestration_v2_projection_provider_sessions (provider_session_id TEXT PRIMARY KEY, thread_id TEXT,
        provider TEXT NOT NULL, status TEXT NOT NULL, model TEXT, updated_at TEXT NOT NULL, payload_json TEXT NOT NULL,
        driver TEXT, provider_instance_id TEXT);
      CREATE TABLE orchestration_v2_projection_provider_turns (provider_turn_id TEXT PRIMARY KEY, thread_id TEXT NOT NULL,
        provider_thread_id TEXT NOT NULL, node_id TEXT NOT NULL, run_attempt_id TEXT, ordinal INTEGER NOT NULL,
        status TEXT NOT NULL, started_at TEXT, completed_at TEXT, payload_json TEXT NOT NULL);
      CREATE TABLE orchestration_v2_projection_runs (run_id TEXT PRIMARY KEY, thread_id TEXT NOT NULL, ordinal INTEGER NOT NULL,
        provider TEXT NOT NULL, provider_thread_id TEXT, status TEXT NOT NULL, requested_at TEXT NOT NULL, completed_at TEXT,
        payload_json TEXT NOT NULL, provider_instance_id TEXT);
      CREATE TABLE orchestration_v2_projection_run_attempts (attempt_id TEXT PRIMARY KEY, thread_id TEXT NOT NULL,
        run_id TEXT NOT NULL, attempt_ordinal INTEGER NOT NULL, root_node_id TEXT NOT NULL, provider TEXT NOT NULL,
        provider_thread_id TEXT NOT NULL, provider_turn_id TEXT, status TEXT NOT NULL, payload_json TEXT NOT NULL,
        provider_instance_id TEXT);";

    #[test]
    fn active_agent_matrix_matches_legacy_suite() {
        let dir = std::env::temp_dir().join(format!(
            "router-load-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("state.sqlite");
        let db = rusqlite::Connection::open(&file).unwrap();
        db.execute_batch(SCHEMA).unwrap();
        let t = "2026-10-09T00:00:00Z";
        let session = |id: &str, provider: &str, status: &str, model: &str| {
            db.execute(
                "INSERT INTO orchestration_v2_projection_provider_sessions VALUES (?1, 'th', ?2, ?3, ?4, ?5, '{}', ?6, ?7)",
                (id, provider, status, model, t, provider, provider),
            )
            .unwrap()
        };
        let pthread = |id: &str, provider: &str, session_id: &str, status: &str| {
            db.execute(
                "INSERT INTO orchestration_v2_projection_provider_threads VALUES (?1, 'th', 'n', ?2, ?3, ?4, 1, 1, ?5, '{}', ?6, ?7)",
                (id, provider, session_id, status, t, provider, provider),
            )
            .unwrap()
        };
        let run = |id: &str, provider: &str, thread: &str, status: &str, model: Option<&str>| {
            let payload = model
                .map(|m| format!(r#"{{"modelSelection": {{"model": "{m}"}}}}"#))
                .unwrap_or_else(|| "{}".into());
            db.execute(
                "INSERT INTO orchestration_v2_projection_runs VALUES (?1, 'th', 1, ?2, ?3, ?4, ?5, NULL, ?6, ?7)",
                (id, provider, thread, status, t, payload, provider),
            )
            .unwrap()
        };
        let turn = |id: &str, thread: &str, attempt: Option<&str>, status: &str| {
            db.execute(
                "INSERT INTO orchestration_v2_projection_provider_turns VALUES (?1, 'th', ?2, 'n', ?3, 1, ?4, ?5, NULL, '{}')",
                (id, thread, attempt, status, t),
            )
            .unwrap()
        };
        let attempt = |id: &str, run_id: &str, thread: &str| {
            db.execute(
                "INSERT INTO orchestration_v2_projection_run_attempts VALUES (?1, 'th', ?2, 1, 'n', 'x', ?3, NULL, 'running', '{}', 'x')",
                (id, run_id, thread),
            )
            .unwrap()
        };
        let child = |id: &str,
                     provider: &str,
                     run_id: &str,
                     thread: Option<&str>,
                     status: &str,
                     model: Option<&str>| {
            let payload = serde_json::json!({"providerInstanceId": provider,
                "providerThreadId": thread, "model": model, "status": status});
            db.execute(
                "INSERT INTO orchestration_v2_projection_subagents VALUES (?1, 'th', ?2, 'n', ?3, ?4, 'child', 'provider_native', ?5, NULL, NULL, ?6, ?7, ?8, ?9)",
                (
                    id,
                    run_id,
                    provider,
                    thread,
                    status,
                    t,
                    payload.to_string(),
                    provider,
                    provider,
                ),
            )
            .unwrap()
        };
        // Claude parent with NULL-thread native children; one queued next turn
        // and a waiting run already live.
        session("sc", "claudeAgent", "ready", "claude-opus-5-5");
        pthread("pt-claude", "claudeAgent", "sc", "active");
        run("run-c", "claudeAgent", "pt-claude", "running", Some("claude-opus-5-5"));
        run("run-c-wait", "claudeAgent", "pt-claude", "waiting", Some("claude-opus-5-5"));
        run("run-c-next", "claudeAgent", "pt-claude", "queued", Some("claude-sonnet-5-5"));
        attempt("att-c", "run-c-wait", "pt-claude");
        turn("turn-c", "pt-claude", Some("att-c"), "running");
        child("kid-sonnet", "claudeAgent", "run-c", None, "running", Some("claude-sonnet-5-5"));
        child("kid-unknown", "claudeAgent", "run-c", None, "running", None);
        child("kid-done", "claudeAgent", "run-c", None, "completed", Some("claude-opus-5-5"));
        child("kid-cancel", "claudeAgent", "run-c", None, "cancelled", Some("claude-opus-5-5"));
        // agy: an idle (finished) child and a running one with no model in payload.
        session("sa", "antigravity", "ready", "gemini-3.8-pro");
        pthread("pt-agy", "antigravity", "sa", "idle");
        run("run-a", "antigravity", "pt-agy", "completed", Some("gemini-3.8-pro"));
        child("agy-idle", "antigravity", "run-a", None, "idle", None);
        child("agy-running", "antigravity", "run-a", None, "running", None);
        // Leftover 'running' child whose parent session stopped.
        session("ss", "claudeAgent", "stopped", "claude-opus-5-5");
        pthread("pt-stale", "claudeAgent", "ss", "idle");
        run("run-s", "claudeAgent", "pt-stale", "completed", Some("claude-opus-5-5"));
        child("kid-stale", "claudeAgent", "run-s", None, "running", Some("claude-opus-5-5"));
        // Codex parent and native child with its own thread and turn: counted once via its turn.
        session("sx", "codex", "ready", "gpt-6.1-sol");
        pthread("pt-codex", "codex", "sx", "active");
        pthread("pt-codex-kid", "codex", "sx", "active");
        run("run-x", "codex", "pt-codex", "running", Some("gpt-6.1-sol"));
        turn("turn-x", "pt-codex", None, "running");
        turn("turn-x-kid", "pt-codex-kid", None, "running");
        child("codex-kid", "codex", "run-x", Some("pt-codex-kid"), "running", Some("gpt-6-astra"));
        // Running is projected before the provider's first turn: the branch still occupies a slot.
        pthread("pt-warm", "codex", "sx", "active");
        run("run-warm", "codex", "pt-warm", "running", Some("gpt-6.1-sol"));
        db.close().unwrap();

        let before = std::fs::read(&file).unwrap();
        let load = read_active_agents(&file);
        assert!(load.known);
        let mut got: Vec<(String, String, Option<String>)> = load
            .agents
            .iter()
            .map(|a| (a.id.clone(), a.provider.clone(), a.model.clone()))
            .collect();
        got.sort();
        let mut want = vec![
            ("pt-claude", "claudeAgent", Some("claude-opus-5-5")),
            ("kid-sonnet", "claudeAgent", Some("claude-sonnet-5-5")),
            ("kid-unknown", "claudeAgent", None),
            ("agy-running", "antigravity", None),
            ("run-c-next", "claudeAgent", Some("claude-sonnet-5-5")),
            ("pt-codex", "codex", Some("gpt-6.1-sol")),
            ("pt-codex-kid", "codex", Some("gpt-6-astra")),
            ("run-warm", "codex", Some("gpt-6.1-sol")),
        ];
        want.sort();
        assert_eq!(
            got,
            want.iter()
                .map(|(a, b, c)| (a.to_string(), b.to_string(), c.map(str::to_string)))
                .collect::<Vec<_>>()
        );
        assert_eq!(std::fs::read(&file).unwrap(), before);
        let missing = dir.join("missing.sqlite");
        assert!(!read_active_agents(&missing).known);
        assert!(!missing.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
