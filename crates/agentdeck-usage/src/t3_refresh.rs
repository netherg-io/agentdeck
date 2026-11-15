//! Asks the running T3 server to re-probe providers so `~/.t3/caches` stay fresh.
//!
//! T3 (0.1.10) only runs its periodic provider-status refresh while a T3 client
//! holds a `provider-status` activity lease (`BackgroundPolicy.shouldRunScopeWork`),
//! so with the T3 window closed the caches agentdeck reads stop updating.
//! The explicit `server.refreshProviders` RPC is not gated by that policy.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio_tungstenite::tungstenite::Message;

use crate::util;

/// Fixed by T3's `/ws` route; a mismatch returns HTTP 426.
const ORCHESTRATION_PROTOCOL: &str = "2";
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServerRuntime {
    pid: u32,
    origin: String,
}

#[derive(Deserialize)]
struct Ticket {
    ticket: String,
}

/// Run one `server.refreshProviders` round trip. The bearer token is read from
/// `token_file`, issued through the running server's own `t3 auth session
/// issue` when missing or rejected, and stored 0600.
pub async fn refresh_providers(t3_root: &Path, token_file: &Path) -> Result<()> {
    let runtime_path = t3_root.join("userdata/server-runtime.json");
    let runtime: ServerRuntime = serde_json::from_slice(
        &std::fs::read(&runtime_path)
            .with_context(|| format!("T3 server is not running ({})", runtime_path.display()))?,
    )
    .with_context(|| format!("cannot parse {}", runtime_path.display()))?;

    let token = match std::fs::read_to_string(token_file) {
        Ok(token) if !token.trim().is_empty() => token.trim().to_owned(),
        _ => issue_token(t3_root, runtime.pid, token_file).await?,
    };
    let ticket = match ws_ticket(&runtime.origin, &token).await? {
        Some(ticket) => ticket,
        // Expired or revoked session: issue a new one once.
        None => {
            let token = issue_token(t3_root, runtime.pid, token_file).await?;
            ws_ticket(&runtime.origin, &token)
                .await?
                .context("T3 rejected a freshly issued session token")?
        }
    };

    let mut url = reqwest::Url::parse(&runtime.origin)
        .with_context(|| format!("bad T3 origin {:?}", runtime.origin))?;
    url.set_scheme(if url.scheme() == "https" { "wss" } else { "ws" })
        .map_err(|()| anyhow::anyhow!("bad T3 origin {:?}", runtime.origin))?;
    url.set_path("/ws");
    url.query_pairs_mut()
        .append_pair("orchestrationProtocol", ORCHESTRATION_PROTOCOL)
        .append_pair("wsTicket", &ticket);

    let (mut ws, _) = tokio_tungstenite::connect_async(url.as_str())
        .await
        .context("T3 WebSocket connect failed")?;
    // Effect RPC JSON framing, as the T3 web client sends it.
    let request = serde_json::json!({
        "_tag": "Request",
        "id": "1",
        "tag": "server.refreshProviders",
        "payload": {},
        "headers": [],
    });
    ws.send(Message::text(request.to_string())).await?;

    while let Some(message) = ws.next().await {
        let Message::Text(text) = message? else {
            continue;
        };
        if let Some(result) = parse_exit(&text) {
            let _ = ws.close(None).await;
            return result;
        }
    }
    bail!("T3 closed the WebSocket before answering server.refreshProviders")
}

/// `Some` for the RPC's `Exit` message (the reply may also arrive as an array
/// of messages), `None` for anything else the server pushes.
fn parse_exit(text: &str) -> Option<Result<()>> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let messages = match value {
        serde_json::Value::Array(items) => items,
        single => vec![single],
    };
    let exit = messages
        .iter()
        .find(|m| m["_tag"] == "Exit" && m["requestId"] == "1")?;
    if exit["exit"]["_tag"] == "Success" {
        return Some(Ok(()));
    }
    let mut cause = exit["exit"]["cause"].to_string();
    cause.truncate(300);
    Some(Err(anyhow::anyhow!(
        "T3 server.refreshProviders failed: {cause}"
    )))
}

/// `None` when T3 rejects the token (401/403), so the caller can re-issue it.
async fn ws_ticket(origin: &str, token: &str) -> Result<Option<String>> {
    let response = reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()?
        .post(format!(
            "{}/api/auth/websocket-ticket/rpc",
            origin.trim_end_matches('/')
        ))
        .bearer_auth(token)
        .send()
        .await
        .context("T3 websocket-ticket request failed")?;
    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Ok(None);
    }
    if !status.is_success() {
        bail!("T3 websocket-ticket returned HTTP {status}");
    }
    Ok(Some(response.json::<Ticket>().await?.ticket))
}

/// Issue a read-only bearer session with the binary of the running server, so
/// the CLI always matches the server's auth store and version.
async fn issue_token(t3_root: &Path, server_pid: u32, token_file: &Path) -> Result<String> {
    let exe = std::fs::read_link(format!("/proc/{server_pid}/exe"))
        .with_context(|| format!("cannot resolve the T3 server binary (pid {server_pid})"))?;
    let output = tokio::process::Command::new(&exe)
        .arg("auth")
        .arg("session")
        .arg("issue")
        .arg("--base-dir")
        .arg(t3_root)
        // `orchestration:read` is the scope T3 requires for server.refreshProviders.
        .args(["--scope", "orchestration:read"])
        .args(["--ttl", "30d", "--label", "agentdeck usage", "--token-only"])
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| format!("cannot run {}", exe.display()))?;
    if !output.status.success() {
        bail!(
            "`t3 auth session issue` failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let token = String::from_utf8(output.stdout)
        .context("t3 printed a non-UTF-8 token")?
        .trim()
        .to_owned();
    if token.is_empty() || token.contains(char::is_whitespace) {
        bail!("`t3 auth session issue --token-only` printed no single token");
    }
    util::atomic_write_mode(token_file, token.as_bytes(), Some(0o600))?;
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_exit_reads_success_failure_and_ignores_other_messages() {
        assert!(parse_exit(r#"{"_tag":"Pong"}"#).is_none());
        assert!(parse_exit("not json").is_none());
        assert!(
            parse_exit(r#"{"_tag":"Exit","requestId":"2","exit":{"_tag":"Success"}}"#).is_none()
        );
        assert!(
            parse_exit(r#"{"_tag":"Exit","requestId":"1","exit":{"_tag":"Success","value":{}}}"#)
                .unwrap()
                .is_ok()
        );
        assert!(
            parse_exit(
                r#"[{"_tag":"Pong"},{"_tag":"Exit","requestId":"1","exit":{"_tag":"Success"}}]"#
            )
            .unwrap()
            .is_ok()
        );
        let err = parse_exit(
            r#"{"_tag":"Exit","requestId":"1","exit":{"_tag":"Failure","cause":{"_tag":"Fail"}}}"#,
        )
        .unwrap()
        .unwrap_err();
        assert!(err.to_string().contains("Fail"), "{err}");
    }
}
