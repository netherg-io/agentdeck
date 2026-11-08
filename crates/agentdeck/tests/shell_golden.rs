//! Golden-file test: the Rust `route claude-shell` must match the committed
//! snapshots in `tests/golden/shell/` (captured once from the legacy Node
//! `claude-shell-mcp.mjs`; `UPDATE_GOLDEN=1` rewrites them from the Rust
//! output) — same pure-function results, same passthrough/streaming CLI
//! contract. No Node needed.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn workspace_root() -> PathBuf {
    let mut d = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    d.pop();
    d.pop();
    d
}

fn golden_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden/shell")
        .join(format!("{name}.json"))
}

fn write_golden(name: &str, value: &serde_json::Value) {
    let path = golden_path(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, serde_json::to_string_pretty(value).unwrap() + "\n").unwrap();
}

fn read_golden(name: &str) -> serde_json::Value {
    let path = golden_path(name);
    if !path.exists() {
        panic!("{name}: missing golden {}; regenerate with UPDATE_GOLDEN=1", path.display());
    }
    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap()
}

const GOOD: &str = r#"{"t3-code": {"type": "http", "url": "http://127.0.0.1:3773/mcp", "headers": {"Authorization": "Bearer first-secret"}}}"#;
const ROTATED: &str = r#"{"t3-code": {"type": "http", "url": "http://127.0.0.1:3773/mcp", "headers": {"Authorization": "Bearer second-secret"}}}"#;

fn frame(servers: &str) -> String {
    format!(
        r#"{{"type": "control_request", "request": {{"subtype": "mcp_set_servers", "servers": {servers}}}}}"#
    )
}

/// Cases every implementation must answer identically (JSON in/out).
fn cases() -> Vec<serde_json::Value> {
    let good_cfg = format!(r#"{{"mcpServers": {GOOD}}}"#);
    vec![
        serde_json::json!({"kind": "streaming", "argv": ["--input-format", "stream-json"]}),
        serde_json::json!({"kind": "streaming", "argv": ["--input-format=stream-json"]}),
        serde_json::json!({"kind": "streaming", "argv": ["--print", "hi"]}),
        serde_json::json!({"kind": "streaming", "argv": ["--input-format", "text"]}),
        serde_json::json!({"kind": "extract", "servers": serde_json::from_str::<serde_json::Value>(GOOD).unwrap()}),
        serde_json::json!({"kind": "extract", "servers": serde_json::from_str::<serde_json::Value>(ROTATED).unwrap()}),
        serde_json::json!({"kind": "extract", "servers": null}),
        serde_json::json!({"kind": "extract", "servers": {}}),
        serde_json::json!({"kind": "extract", "servers": {"t3-code": null}}),
        serde_json::json!({"kind": "extract", "servers": {"t3-code": {"type": "http", "url": "https://127.0.0.1:1/mcp", "headers": {"Authorization": "Bearer x"}}}}),
        serde_json::json!({"kind": "extract", "servers": {"t3-code": {"type": "http", "url": "http://evil.test/mcp", "headers": {"Authorization": "Bearer x"}}}}),
        serde_json::json!({"kind": "extract", "servers": {"t3-code": {"type": "http", "url": "http://127.0.0.1:1/mcp?token=x", "headers": {"Authorization": "Bearer x"}}}}),
        serde_json::json!({"kind": "extract", "servers": {"t3-code": {"type": "http", "url": "http://127.0.0.1:1/mcp", "headers": {"Authorization": ""}}}}),
        serde_json::json!({"kind": "extract", "servers": {"t3-code": {"type": "http", "url": "http://127.0.0.1:1/mcp", "headers": {}}}}),
        serde_json::json!({"kind": "update", "current": null, "text": frame(GOOD)}),
        serde_json::json!({"kind": "update", "current": serde_json::from_str::<serde_json::Value>("{\"endpoint\": \"http://127.0.0.1:1/mcp\", \"authorization\": \"Bearer old\"}").unwrap(), "text": frame(ROTATED)}),
        serde_json::json!({"kind": "update", "current": serde_json::from_str::<serde_json::Value>("{\"endpoint\": \"http://127.0.0.1:1/mcp\", \"authorization\": \"Bearer old\"}").unwrap(), "text": frame("{}")}),
        serde_json::json!({"kind": "update", "current": serde_json::from_str::<serde_json::Value>("{\"endpoint\": \"http://127.0.0.1:1/mcp\", \"authorization\": \"Bearer old\"}").unwrap(), "text": r#"{"type": "control_request", "request": {"subtype": "other", "servers": {}}}"#}),
        serde_json::json!({"kind": "update", "current": serde_json::from_str::<serde_json::Value>("{\"endpoint\": \"http://127.0.0.1:1/mcp\", \"authorization\": \"Bearer old\"}").unwrap(), "text": "not json"}),
        serde_json::json!({"kind": "update", "current": null, "text": "null"}),
        serde_json::json!({"kind": "seed", "argv": ["--mcp-config", &good_cfg], "files": {}}),
        serde_json::json!({"kind": "seed", "argv": ["--print"], "files": {}}),
        serde_json::json!({"kind": "seed", "argv": ["--mcp-config", "{bad"], "files": {}}),
        serde_json::json!({"kind": "seed", "argv": ["--mcp-config", "/fixture/config.json"], "files": {"/fixture/config.json": &good_cfg}}),
        serde_json::json!({"kind": "seed", "argv": ["--mcp-config=/fixture/config.json"], "files": {"/fixture/config.json": &good_cfg}}),
        serde_json::json!({"kind": "format", "conn": null}),
        serde_json::json!({"kind": "format", "conn": {"endpoint": "http://127.0.0.1:1/mcp", "authorization": "Bearer s"}}),
        serde_json::json!({"kind": "snoop", "chunks": ["7b2274797065223a2275736572227d0a7b2261223a2022cf", "80227d0d", "0a"]}),
    ]
}

fn rust_results() -> Vec<serde_json::Value> {
    use agentdeck_router::shell::*;
    cases()
        .iter()
        .map(|c| match c["kind"].as_str().unwrap() {
            "streaming" => {
                let argv: Vec<String> = serde_json::from_value(c["argv"].clone()).unwrap();
                serde_json::Value::from(is_streaming_args(&argv))
            }
            "extract" => serde_json::to_value(extract_connection(&c["servers"])).unwrap(),
            "update" => {
                let current: Option<Connection> =
                    serde_json::from_value(c["current"].clone()).unwrap();
                serde_json::to_value(update_connection(current, c["text"].as_str().unwrap()))
                    .unwrap()
            }
            "seed" => {
                let argv: Vec<String> = serde_json::from_value(c["argv"].clone()).unwrap();
                let files = c["files"].clone();
                serde_json::to_value(seed_from_args(&argv, |name| {
                    files.get(name).and_then(|v| v.as_str()).map(str::to_string)
                }))
                .unwrap()
            }
            "format" => {
                let conn: Option<Connection> = serde_json::from_value(c["conn"].clone()).unwrap();
                serde_json::Value::from(format_response(conn.as_ref()))
            }
            "snoop" => {
                let mut snoop = Snoop::default();
                let mut lines = Vec::new();
                let mut fwd = Vec::new();
                for h in c["chunks"].as_array().unwrap() {
                    let bytes = hex_decode(h.as_str().unwrap());
                    fwd.extend_from_slice(&bytes);
                    lines.extend(snoop.push(&bytes));
                }
                if let Some(last) = snoop.flush() {
                    lines.push(last);
                }
                serde_json::json!({"lines": lines, "forwarded": hex_encode(&fwd)})
            }
            _ => unreachable!(),
        })
        .collect()
}

fn hex_decode(h: &str) -> Vec<u8> {
    (0..h.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&h[i..i + 2], 16).unwrap())
        .collect()
}

fn hex_encode(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[test]
fn pure_functions_match_golden() {
    let rust = rust_results();
    if std::env::var("UPDATE_GOLDEN").is_ok() {
        write_golden("pure", &serde_json::to_value(&rust).unwrap());
    }
    // The golden was captured once from the legacy Node router; the golden
    // file is the contract and Node is no longer needed.
    if !golden_path("pure").exists() {
        panic!("pure: missing golden; regenerate with UPDATE_GOLDEN=1");
    }
    let golden: Vec<serde_json::Value> = serde_json::from_value(read_golden("pure")).unwrap();
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

/// Fake original shared by the launcher under test: after stdin ends, read
/// the pushed socket line, dump both, exit 0. Plain Python (no Node): the
/// fake is just the spawned `ORIGINAL`, unrelated to the router.
fn write_fake(dir: &std::path::Path) {
    std::fs::write(
        dir.join("fake.py"),
        r#"import os, socket, sys
data = sys.stdin.buffer.read()
open(os.environ['DUMP_STDIN'], 'wb').write(data)
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(os.environ['T3_SHELL_MCP_SOCKET'])
line = b''
while not line.endswith(b'\n'):
    chunk = s.recv(4096)
    if not chunk:
        break
    line += chunk
open(os.environ['DUMP_SOCK'], 'wb').write(line)
"#,
    )
    .unwrap();
}

#[test]
fn streaming_proxy_matches_golden() {
    let dir = std::env::temp_dir().join(format!(
        "agentdeck-shell-cli-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    write_fake(&dir);
    // Wrap the fake in a shell script: the launcher spawns ORIGINAL with
    // the user argv, and exec of the wrapper keeps argv handling identical.
    std::fs::write(
        dir.join("fake.sh"),
        format!(
            "#!/bin/sh\nexec python3 {} \"$@\"\n",
            dir.join("fake.py").display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(dir.join("fake.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let frame_text = frame(GOOD);
    let input = format!("{frame_text}\nuser line\n");
    let run = || {
        std::fs::create_dir_all(dir.join("rt-rust")).unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentdeck"));
        cmd.arg("route").arg("claude-shell")
            .arg("--input-format").arg("stream-json")
            .arg("--mcp-config").arg(serde_json::json!({"mcpServers": serde_json::from_str::<serde_json::Value>(GOOD).unwrap()}).to_string());
        child_env(&mut cmd, &dir, "rust");
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().expect("spawn agentdeck");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let out = child.wait_with_output().expect("launcher output");
        (
            out.status.code().unwrap_or(-1),
            std::fs::read_to_string(dir.join("stdin-rust")).unwrap_or_default(),
            std::fs::read_to_string(dir.join("sock-rust")).unwrap_or_default(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };
    let golden = read_golden("streaming-proxy");
    if std::env::var("UPDATE_GOLDEN").is_ok() {
        let (rc, rstdin, rsock, _) = run();
        write_golden(
            "streaming-proxy",
            &serde_json::json!({"exit": rc, "stdin": rstdin, "sock": rsock}),
        );
    }
    let (rc, rstdin, rsock, rerr) = run();
    assert_eq!(
        (rc, rstdin, rsock),
        (
            golden["exit"].as_i64().unwrap() as i32,
            golden["stdin"].as_str().unwrap().to_string(),
            golden["sock"].as_str().unwrap().to_string(),
        ),
        "rust stderr: {rerr}"
    );
    // The proxy removes its own session directory on exit.
    let leftovers: Vec<_> = std::fs::read_dir(dir.join("rt-rust")).unwrap().collect();
    assert!(
        leftovers.is_empty(),
        "leftover runtime files: {leftovers:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

fn child_env(cmd: &mut Command, dir: &std::path::Path, tag: &str) {
    cmd.env("CLAUDE_SHELL_MCP_ORIGINAL", dir.join("fake.sh"));
    cmd.env("XDG_RUNTIME_DIR", dir.join(format!("rt-{tag}")));
    cmd.env("DUMP_STDIN", dir.join(format!("stdin-{tag}")));
    cmd.env("DUMP_SOCK", dir.join(format!("sock-{tag}")));
}

#[test]
fn passthrough_exit_code_matches_golden() {
    let dir = std::env::temp_dir().join(format!(
        "agentdeck-shell-pass-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("fake.sh"),
        "#!/bin/sh\necho out; echo err >&2; exit 7\n",
    )
    .unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(dir.join("fake.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let run = || {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentdeck"));
        cmd.arg("route")
            .arg("claude-shell")
            .arg("--print")
            .arg("hi");
        cmd.env("CLAUDE_SHELL_MCP_ORIGINAL", dir.join("fake.sh"));
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd.spawn().expect("spawn agentdeck").wait_with_output().expect("output")
    };
    let golden = read_golden("passthrough");
    if std::env::var("UPDATE_GOLDEN").is_ok() {
        let r = run();
        write_golden(
            "passthrough",
            &serde_json::json!({"exit": r.status.code(),
                "stdout": String::from_utf8_lossy(&r.stdout),
                "stderr": String::from_utf8_lossy(&r.stderr)}),
        );
    }
    let r = run();
    assert_eq!(
        (
            r.status.code(),
            String::from_utf8_lossy(&r.stdout).into_owned(),
            String::from_utf8_lossy(&r.stderr).into_owned(),
        ),
        (
            golden["exit"].as_i64().map(|c| c as i32),
            golden["stdout"].as_str().unwrap().to_string(),
            golden["stderr"].as_str().unwrap().to_string(),
        )
    );
    assert_eq!(r.status.code(), Some(7));
    let _ = std::fs::remove_dir_all(&dir);
}

/// argv[0] dispatch: a symlink named `claude-t3-shell` runs the launcher
/// proxy without the `route` prefix.
#[test]
fn argv0_dispatch_runs_claude_shell() {
    let dir = std::env::temp_dir().join(format!(
        "agentdeck-argv0-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let link = dir.join("claude-t3-shell");
    #[cfg(unix)]
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_agentdeck"), &link).unwrap();
    std::fs::write(dir.join("fake.sh"), "#!/bin/sh\necho out; exit 7\n").unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(dir.join("fake.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let out = Command::new(&link)
        .arg("--print")
        .arg("hi")
        .env("CLAUDE_SHELL_MCP_ORIGINAL", dir.join("fake.sh"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn via symlink");
    assert_eq!(out.status.code(), Some(7));
    assert_eq!(out.stdout, b"out\n");
    let _ = std::fs::remove_dir_all(&dir);
}
#[test]
fn check_runs_without_node_on_path() {
    let dir = std::env::temp_dir().join(format!(
        "agentdeck-nocheck-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    for rel in [
        ".t3/userdata",
        ".local/state/ai-usage",
        ".local/state/model-route",
        "rt",
        "bin",
    ] {
        std::fs::create_dir_all(dir.join(rel)).unwrap();
    }
    std::fs::copy(
        workspace_root().join("crates/agentdeck/tests/fixtures/catalog.fixture.json"),
        dir.join(".local/state/model-route/catalog.json"),
    )
    .unwrap();
    std::fs::write(dir.join(".t3/userdata/settings.json"), "{}").unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64() as i64;
    let usage = serde_json::json!({"accounts": [
        {"provider": "codex", "id": "codex-main", "plan": "Pro 5x", "status": "ok", "updated_at": now, "balances": [],
         "windows": [{"key": "weekly", "label": "Week", "remaining_percent": 80, "duration_mins": 10080, "resets_at": now + 3 * 86400}]},
    ]});
    std::fs::write(
        dir.join(".local/state/ai-usage/state.json"),
        serde_json::to_string(&usage).unwrap(),
    )
    .unwrap();
    let router = serde_json::json!({"version": 1,
        "pool": [{"providerInstanceId": "codex", "model": "gpt-6.1-sol", "enabled": true}],
        "accounts": {"codex": {"instances": ["codex"], "usageProvider": "codex", "usageId": "codex-main"}},
        "usageStatePath": dir.join(".local/state/ai-usage/state.json")});
    std::fs::write(
        dir.join("router.json"),
        serde_json::to_string(&router).unwrap(),
    )
    .unwrap();
    // Premise: node is really gone under this PATH.
    let empty_path = dir.join("bin");
    let probe = Command::new("node")
        .arg("--version")
        .env("PATH", &empty_path)
        .output();
    assert!(
        probe.is_err() || !probe.unwrap().status.success(),
        "node must be absent from PATH"
    );
    let out = Command::new(env!("CARGO_BIN_EXE_agentdeck"))
        .arg("route")
        .arg("--check")
        .env("HOME", &dir)
        .env("XDG_STATE_HOME", dir.join(".local/state"))
        .env("XDG_RUNTIME_DIR", dir.join("rt"))
        .env("MODEL_ROUTE_ROUTER_JSON", dir.join("router.json"))
        .env("PATH", &empty_path)
        .output()
        .expect("spawn agentdeck");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{text}");
    assert!(text.contains("allowed sizes"), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}
