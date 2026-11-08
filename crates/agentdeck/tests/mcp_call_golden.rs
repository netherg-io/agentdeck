//! Golden-file test: the Rust `route t3-mcp-call` must match the committed
//! snapshots in `tests/golden/mcp_call/` (captured once from the legacy Node
//! `t3-mcp-call.mjs`; `UPDATE_GOLDEN=1` rewrites them from the Rust output)
//! — same connection discovery, validation messages and MCP envelope
//! handling (mock HTTP server, temp dirs; no real T3, no real credentials).
//! No Node needed.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::Command;

fn golden_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden/mcp_call")
        .join(format!("{name}.json"))
}

const GOOD_CONN: &str = "http://127.0.0.1:3773/mcp";
const GOOD_AUTH: &str = "Bearer fixture-secret";

fn good_servers() -> serde_json::Value {
    serde_json::json!({"t3-code": {"type": "http", "url": GOOD_CONN, "headers": {"Authorization": GOOD_AUTH}}})
}

fn payload() -> serde_json::Value {
    serde_json::json!({
        "task": "Do the thing",
        "target": {"providerInstanceId": "opencode", "model": "opencode-go/muse-spark-1.3-contributor", "options": {"variant": "high"}},
        "mode": "async",
        "clientRequestId": "routed-workflow-fixture-step",
    })
}

/// Cases for connection/validation discovery (JSON in/out).
fn cases() -> Vec<serde_json::Value> {
    let mut cases = vec![
        serde_json::json!({"kind": "config", "config": {"mcpServers": good_servers()}}),
        serde_json::json!({"kind": "config", "config": {"mcpServers": {"unrelated": good_servers()["t3-code"]}}}),
        serde_json::json!({"kind": "config", "config": {"mcpServers": {"t3-code": {"url": "https://evil.test/mcp", "headers": {"Authorization": GOOD_AUTH}}}}}),
        serde_json::json!({"kind": "config", "config": {"mcpServers": {"t3-code": {"url": "http://127.0.0.1:1/mcp?token=x", "headers": {"Authorization": GOOD_AUTH}}}}}),
        serde_json::json!({"kind": "config", "config": {"mcpServers": {"t3-code": {"url": GOOD_CONN, "headers": {"Authorization": ""}}}}}),
        serde_json::json!({"kind": "validate", "tool": "delegate_task", "payload": payload()}),
        serde_json::json!({"kind": "validate", "tool": "delegate_task", "payload": {}}),
        serde_json::json!({"kind": "validate", "tool": "delegate_task", "payload": {"task": "x", "target": {}, "mode": "async", "clientRequestId": "r"}}),
        serde_json::json!({"kind": "validate", "tool": "delegate_task", "payload": {"task": "x", "target": {"providerInstanceId": "p", "model": "m"}, "mode": "wait", "clientRequestId": "r"}}),
        serde_json::json!({"kind": "validate", "tool": "delegate_task", "payload": {"task": "x", "target": {"providerInstanceId": "p", "model": "m"}, "mode": "async", "clientRequestId": "r", "runtimeMode": "full-access"}}),
        serde_json::json!({"kind": "validate", "tool": "t3_thread_launch", "payload": {}}),
        serde_json::json!({"kind": "validate", "tool": "task_status", "payload": {"taskId": "t"}}),
        serde_json::json!({"kind": "discover-acp", "endpoint": GOOD_CONN, "auth": GOOD_AUTH}),
        serde_json::json!({"kind": "discover-acp", "endpoint": GOOD_CONN, "auth": null}),
        serde_json::json!({"kind": "discover-proc"}),
        serde_json::json!({"kind": "discover-none"}),
    ];
    for role in [
        serde_json::json!("implementation"),
        serde_json::json!("implement"),
        serde_json::json!("boss"),
        serde_json::Value::Null,
    ] {
        let mut p = payload();
        p["role"] = role;
        cases.push(serde_json::json!({"kind": "validate", "tool": "delegate_task", "payload": p}));
    }
    cases
}

fn rust_results() -> Vec<serde_json::Value> {
    use agentdeck_router::mcp_call::*;
    cases()
        .iter()
        .map(|c| match c["kind"].as_str().unwrap() {
            "config" => match connection_from_config(&c["config"]) {
                Ok(Some(conn)) => serde_json::json!({"endpoint": conn.endpoint, "authorization": conn.authorization}),
                Ok(None) => serde_json::Value::Null,
                Err(e) => serde_json::json!({"error": e.to_string()}),
            },
            "validate" => match validate_payload(c["tool"].as_str().unwrap(), &c["payload"]) {
                Ok(()) => serde_json::json!("ok"),
                Err(e) => serde_json::json!({"error": e.to_string()}),
            },
            "discover-acp" => {
                let ep = c["endpoint"].as_str().map(str::to_string);
                let auth = c["auth"].as_str().map(str::to_string);
                let env = move |k: &str| match k {
                    "T3_ACP_MCP_ENDPOINT" => ep.clone(),
                    "T3_ACP_MCP_AUTHORIZATION" => auth.clone(),
                    _ => None,
                };
                match discover_with(&env, 999, &|_| None) {
                    Ok(conn) => serde_json::json!({"endpoint": conn.endpoint}),
                    Err(e) => serde_json::json!({"error": e.to_string()}),
                }
            }
            "discover-proc" => {
                let config = serde_json::json!({"mcpServers": good_servers()}).to_string();
                let read = |name: &str| match name {
                    "/proc/20/cmdline" => Some("bash\0-c\0command\0".to_string()),
                    "/proc/20/stat" => Some("20 (bash child) S 10 rest".to_string()),
                    "/proc/10/cmdline" => Some(format!("claude\0--mcp-config\0{config}\0")),
                    "/proc/10/stat" => Some("10 (claude) S 1 rest".to_string()),
                    _ => None,
                };
                match discover_with(&|_| None, 20, &read) {
                    Ok(conn) => serde_json::json!({"endpoint": conn.endpoint}),
                    Err(e) => serde_json::json!({"error": e.to_string()}),
                }
            }
            "discover-none" => match discover_with(&|_| None, 20, &|_| None) {
                Ok(conn) => serde_json::json!({"endpoint": conn.endpoint}),
                Err(e) => serde_json::json!({"error": e.to_string()}),
            },
            _ => unreachable!(),
        })
        .collect()
}

#[test]
fn discovery_and_validation_match_golden() {
    let rust = rust_results();
    if std::env::var("UPDATE_GOLDEN").is_ok() {
        let path = golden_path("discovery");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_string_pretty(&rust).unwrap() + "\n").unwrap();
    }
    // The golden was captured once from the legacy Node router; the golden
    // file is the contract and Node is no longer needed.
    if !golden_path("discovery").exists() {
        panic!("discovery: missing golden; regenerate with UPDATE_GOLDEN=1");
    }
    let golden: Vec<serde_json::Value> =
        serde_json::from_slice(&std::fs::read(golden_path("discovery")).unwrap()).unwrap();
    assert_eq!(golden.len(), rust.len());
    for (i, (n, r)) in golden.iter().zip(rust.iter()).enumerate() {
        assert_eq!(
            n,
            r,
            "case {i}: {}",
            serde_json::to_string(&cases()[i]).unwrap()
        );
    }
}

/// Minimal in-test HTTP/1.1 mock speaking MCP streamable HTTP (JSON and SSE
/// variants, error injections). No spawned processes.
struct Mock {
    addr: std::net::SocketAddr,
    seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

fn start_mock(mode: &str) -> Mock {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen_loop = seen.clone();
    let mode = mode.to_string();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = match stream {
                Ok(s) => s,
                Err(_) => break,
            };
            let seen = seen_loop.clone();
            let mode = mode.clone();
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                // One connection serves every request on it (keep-alive).
                loop {
                    buf.clear();
                    // Read headers.
                    loop {
                        match stream.read(&mut tmp) {
                            Ok(0) => return,
                            Ok(n) => {
                                buf.extend_from_slice(&tmp[..n]);
                                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                                    break;
                                }
                            }
                            Err(_) => return,
                        }
                    }
                    let head_end = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
                    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
                    let len: usize = head
                        .lines()
                        .find(|l| l.to_lowercase().starts_with("content-length:"))
                        .and_then(|l| l.split(':').nth(1))
                        .and_then(|v| v.trim().parse().ok())
                        .unwrap_or(0);
                    while buf.len() < head_end + len {
                        match stream.read(&mut tmp) {
                            Ok(0) => return,
                            Ok(n) => buf.extend_from_slice(&tmp[..n]),
                            Err(_) => return,
                        }
                    }
                    let body: serde_json::Value =
                        serde_json::from_slice(&buf[head_end..head_end + len]).unwrap();
                    let id = body.get("id").cloned().unwrap_or(serde_json::Value::Null);
                    let name = body
                        .get("params")
                        .and_then(|p| p.get("name"))
                        .and_then(|n| n.as_str())
                        .unwrap_or(body.get("method").and_then(|m| m.as_str()).unwrap_or(""))
                        .to_string();
                    seen.lock().unwrap().push(name.clone());
                    let is_notification = body.get("id").is_none();
                    let payload = if is_notification {
                        (202, "application/json", String::new())
                    } else if mode == "http500" && name == "delegate_task" {
                        (500, "text/plain", "boom".to_string())
                    } else {
                        let result = if body.get("method").and_then(|m| m.as_str())
                            == Some("initialize")
                        {
                            serde_json::json!({"protocolVersion": "2025-06-18"})
                        } else if name == "orchestrator_capabilities" {
                            serde_json::json!({"content": [], "structuredContent": {
                                "parentThreadId": "parent-fixture", "inheritedProviderInstanceId": "claude_second",
                                "providers": [{"providerInstanceId": "claude_second", "driverKind": "claudeAgent", "models": []},
                                    {"providerInstanceId": "opencode", "driverKind": "opencode", "models": []}],
                                "features": {"appOwnedSubagents": true}}})
                        } else {
                            serde_json::json!({"content": [], "structuredContent": {"taskId": "actual-task-id", "status": "running"}})
                        };
                        let envelope = if mode.starts_with("invalid-params") && name == "delegate_task" {
                            serde_json::json!({"jsonrpc": "2.0", "id": id,
                                "error": {"code": -32602, "message": "Invalid parameters: role must be implementation"}})
                        } else {
                            serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result})
                        }.to_string();
                        if (mode == "sse" || mode == "invalid-params-sse")
                            && name == "delegate_task"
                        {
                            let sse = format!(
                                ": comment\r\ndata: null\r\n\r\nevent: message\r\ndata: {envelope}\r\n\r\n"
                            );
                            (200, "text/event-stream", sse)
                        } else {
                            (200, "application/json", envelope)
                        }
                    };
                    let response = format!(
                        "HTTP/1.1 {} OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{}",
                        payload.0,
                        payload.1,
                        payload.2.len(),
                        payload.2
                    );
                    if stream.write_all(response.as_bytes()).is_err() {
                        return;
                    }
                }
            });
        }
    });
    Mock { addr, seen }
}

fn test_hooks() -> agentdeck_router::mcp_call::Hooks {
    agentdeck_router::mcp_call::Hooks {
        catalog: Box::new(|_| Ok(())),
        route: Box::new(|_, _| Ok(())),
    }
}

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(f)
}

#[test]
fn delegate_flow_json_and_sse() {
    for mode in ["json", "sse"] {
        let mock = start_mock(mode);
        let conn = agentdeck_router::mcp_call::Connection {
            endpoint: format!("http://{}/mcp", mock.addr),
            authorization: GOOD_AUTH.into(),
        };
        let result = block_on(agentdeck_router::mcp_call::call_tool(
            "delegate_task",
            payload(),
            conn,
            &test_hooks(),
        ))
        .expect("delegate flow");
        assert_eq!(result["structuredContent"]["taskId"], "actual-task-id");
        assert_eq!(
            *mock.seen.lock().unwrap(),
            vec![
                "initialize",
                "notifications/initialized",
                "orchestrator_capabilities",
                "delegate_task"
            ]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>(),
            "mode {mode}"
        );
    }
}

#[test]
fn delegate_http_failure_reports_outcome_unknown() {
    let mock = start_mock("http500");
    let conn = agentdeck_router::mcp_call::Connection {
        endpoint: format!("http://{}/mcp", mock.addr),
        authorization: GOOD_AUTH.into(),
    };
    let err = block_on(agentdeck_router::mcp_call::call_tool(
        "delegate_task",
        payload(),
        conn,
        &test_hooks(),
    ))
    .expect_err("must fail");
    assert!(err.to_string().contains("outcome unknown"), "{err}");
}

#[test]
fn no_inherited_connection_cli_error_matches_golden() {
    // Deterministic failure: ACP endpoint set without authorization.
    let run_rust = Command::new(env!("CARGO_BIN_EXE_agentdeck"))
        .arg("route")
        .arg("t3-mcp-call")
        .arg("orchestrator_capabilities")
        .env("T3_ACP_MCP_ENDPOINT", GOOD_CONN)
        .env_remove("T3_ACP_MCP_AUTHORIZATION")
        .env_remove("T3_SHELL_MCP_SOCKET")
        .output()
        .expect("spawn agentdeck");
    let rust_err = String::from_utf8_lossy(&run_rust.stderr).into_owned();
    assert_eq!(run_rust.status.code(), Some(1));
    assert!(
        rust_err.contains("Expected a loopback T3 MCP endpoint with session authorization."),
        "{rust_err}"
    );
    let path = golden_path("no-inherited-connection");
    if std::env::var("UPDATE_GOLDEN").is_ok() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            serde_json::to_string_pretty(
                &serde_json::json!({"exit": run_rust.status.code(), "stderr": rust_err}),
            )
            .unwrap()
                + "\n",
        )
        .unwrap();
    }
    // The golden was captured once from the legacy Node CLI; the golden
    // file is the contract and Node is no longer needed.
    if !path.exists() {
        panic!("no-inherited-connection: missing golden; regenerate with UPDATE_GOLDEN=1");
    }
    let golden: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        golden["exit"].as_i64().unwrap() as i32,
        run_rust.status.code().unwrap()
    );
    assert_eq!(golden["stderr"].as_str().unwrap(), rust_err);
}

#[test]
fn delegate_invalid_params_preserves_rejection() {
    for mode in ["invalid-params", "invalid-params-sse"] {
        let mock = start_mock(mode);
        let conn = agentdeck_router::mcp_call::Connection {
            endpoint: format!("http://{}/mcp", mock.addr),
            authorization: GOOD_AUTH.into(),
        };
        let err = block_on(agentdeck_router::mcp_call::call_tool(
            "delegate_task",
            payload(),
            conn,
            &test_hooks(),
        ))
        .unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("-32602") && message.contains("role must be implementation"),
            "{message}"
        );
        assert!(!message.contains("outcome unknown"), "{message}");
        assert_eq!(
            mock.seen
                .lock()
                .unwrap()
                .iter()
                .filter(|name| *name == "delegate_task")
                .count(),
            1
        );
    }
}
