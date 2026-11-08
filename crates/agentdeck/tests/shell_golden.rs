//! Differential test: the Rust `route claude-shell` must behave like the
//! Node `claude-shell-mcp.mjs` — same pure-function results, same
//! passthrough/streaming CLI contract. Skips the Node half when Node is
//! absent; the no-Node `--check` case always runs.

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

fn node_available() -> bool {
    Command::new("node")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn asset_url() -> String {
    let p = workspace_root().join("crates/agentdeck-core/assets/router/claude-shell-mcp.mjs");
    format!("file://{}", p.to_string_lossy())
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

fn node_results(dir: &std::path::Path) -> Vec<serde_json::Value> {
    let driver = dir.join("driver.mjs");
    std::fs::write(
        &driver,
        format!(
            r#"import fs from 'node:fs';
import {{ isStreamingArgs, extractConnection, updateConnection, seedFromArgs, formatResponse, createSnoop }} from {url};
const cases = JSON.parse(fs.readFileSync(0, 'utf8'));
const out = [];
for (const c of cases) {{
  if (c.kind === 'streaming') out.push(isStreamingArgs(c.argv));
  else if (c.kind === 'extract') out.push(extractConnection(c.servers) ?? null);
  else if (c.kind === 'update') out.push(updateConnection(c.current ?? null, c.text));
  else if (c.kind === 'seed') out.push(seedFromArgs(c.argv, name => {{ if (name in c.files) return c.files[name]; throw new Error('private'); }}));
  else if (c.kind === 'format') out.push(formatResponse(c.conn ?? null));
  else if (c.kind === 'snoop') {{
    const lines = [], fwd = [];
    const s = createSnoop(line => lines.push(line));
    s.on('data', chunk => fwd.push(chunk));
    for (const h of c.chunks) s.write(Buffer.from(h, 'hex'));
    s.end();
    await new Promise(resolve => s.on('end', resolve));
    out.push({{ lines, forwarded: Buffer.concat(fwd).toString('hex') }});
  }}
}}
process.stdout.write(JSON.stringify(out));
"#,
            url = serde_json::to_string(&asset_url()).unwrap()
        ),
    )
    .unwrap();
    let mut child = Command::new("node")
        .arg(&driver)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn node");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(serde_json::to_string(&cases()).unwrap().as_bytes())
        .unwrap();
    let out = child.wait_with_output().expect("node output");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn pure_functions_match_node() {
    if !node_available() {
        eprintln!("SKIP: node absent");
        return;
    }
    let dir = std::env::temp_dir().join(format!(
        "agentdeck-shell-parity-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let node = node_results(&dir);
    let rust = rust_results();
    assert_eq!(node.len(), rust.len());
    for (i, (n, r)) in node.iter().zip(rust.iter()).enumerate() {
        assert_eq!(
            n,
            r,
            "case {i}: {}",
            serde_json::to_string(&cases()[i]).unwrap()
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Fake original shared by both launchers: after stdin ends, read the pushed
/// socket line, dump both, exit 0.
fn write_fake(dir: &std::path::Path) {
    std::fs::write(
        dir.join("fake.mjs"),
        r#"import fs from 'node:fs';
import net from 'node:net';
let data = '';
process.stdin.on('data', c => { data += c; });
process.stdin.on('end', () => {
  fs.writeFileSync(process.env.DUMP_STDIN, data);
  const sock = net.connect(process.env.T3_SHELL_MCP_SOCKET);
  let line = '';
  sock.on('data', c => { line += c; });
  sock.on('end', () => { fs.writeFileSync(process.env.DUMP_SOCK, line); process.exit(0); });
  sock.on('error', e => { fs.writeFileSync(process.env.DUMP_SOCK, 'ERR ' + e.message); process.exit(3); });
});
"#,
    )
    .unwrap();
}

#[test]
fn streaming_proxy_matches_node() {
    if !node_available() {
        eprintln!("SKIP: node absent");
        return;
    }
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
    // ORIGINAL=node with the fake as its first argv is fiddly; instead wrap
    // the fake in a shell script for the Node side too.
    std::fs::write(
        dir.join("fake.sh"),
        format!(
            "#!/bin/sh\nexec node {} \"$@\"\n",
            dir.join("fake.mjs").display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(dir.join("fake.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let frame_text = frame(GOOD);
    let input = format!("{frame_text}\nuser line\n");
    // Temporarily point the helper at the wrapper by overriding env inside run_launcher is overkill;
    // reuse run_launcher with CLAUDE_SHELL_MCP_ORIGINAL=fake.sh via env swap below.
    let run = |rust: bool| {
        let tag = if rust { "rust" } else { "node" };
        std::fs::create_dir_all(dir.join(format!("rt-{tag}"))).unwrap();
        let mut child = if rust {
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentdeck"));
            cmd.arg("route").arg("claude-shell")
                .arg("--input-format").arg("stream-json")
                .arg("--mcp-config").arg(serde_json::json!({"mcpServers": serde_json::from_str::<serde_json::Value>(GOOD).unwrap()}).to_string());
            child_env(&mut cmd, &dir, tag);
            cmd.stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            cmd.spawn().expect("spawn agentdeck")
        } else {
            let mut cmd = Command::new("node");
            cmd.arg(workspace_root().join("crates/agentdeck-core/assets/router/claude-shell-mcp.mjs"))
                .arg("--input-format").arg("stream-json")
                .arg("--mcp-config").arg(serde_json::json!({"mcpServers": serde_json::from_str::<serde_json::Value>(GOOD).unwrap()}).to_string());
            child_env(&mut cmd, &dir, tag);
            cmd.stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            cmd.spawn().expect("spawn node")
        };
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let out = child.wait_with_output().expect("launcher output");
        (
            out.status.code().unwrap_or(-1),
            std::fs::read_to_string(dir.join(format!("stdin-{tag}"))).unwrap_or_default(),
            std::fs::read_to_string(dir.join(format!("sock-{tag}"))).unwrap_or_default(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };
    let (nc, nstdin, nsock, nerr) = run(false);
    let (rc, rstdin, rsock, rerr) = run(true);
    let want_sock = String::from(
        "{\"endpoint\":\"http://127.0.0.1:3773/mcp\",\"authorization\":\"Bearer first-secret\"}\n",
    );
    assert_eq!(
        (nc, nstdin.clone(), nsock.clone()),
        (0, input.clone(), want_sock),
        "node: {nerr}"
    );
    assert_eq!(
        (rc, rstdin, rsock),
        (nc, nstdin, nsock),
        "rust stderr: {rerr}"
    );
    // The proxy removes its own session directory on exit.
    for tag in ["node", "rust"] {
        let leftovers: Vec<_> = std::fs::read_dir(dir.join(format!("rt-{tag}")))
            .unwrap()
            .collect();
        assert!(
            leftovers.is_empty(),
            "leftover runtime files: {leftovers:?}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

fn child_env(cmd: &mut Command, dir: &std::path::Path, tag: &str) {
    cmd.env("CLAUDE_SHELL_MCP_ORIGINAL", dir.join("fake.sh"));
    cmd.env("XDG_RUNTIME_DIR", dir.join(format!("rt-{tag}")));
    cmd.env("DUMP_STDIN", dir.join(format!("stdin-{tag}")));
    cmd.env("DUMP_SOCK", dir.join(format!("sock-{tag}")));
}

#[test]
fn passthrough_exit_code_matches_node() {
    if !node_available() {
        eprintln!("SKIP: node absent");
        return;
    }
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
    let run = |rust: bool| {
        let child = if rust {
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentdeck"));
            cmd.arg("route")
                .arg("claude-shell")
                .arg("--print")
                .arg("hi");
            cmd.env("CLAUDE_SHELL_MCP_ORIGINAL", dir.join("fake.sh"));
            cmd.stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            cmd.spawn().expect("spawn agentdeck")
        } else {
            let mut cmd = Command::new("node");
            cmd.arg(
                workspace_root().join("crates/agentdeck-core/assets/router/claude-shell-mcp.mjs"),
            )
            .arg("--print")
            .arg("hi");
            cmd.env("CLAUDE_SHELL_MCP_ORIGINAL", dir.join("fake.sh"));
            cmd.stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            cmd.spawn().expect("spawn node")
        };
        child.wait_with_output().expect("output")
    };
    let n = run(false);
    let r = run(true);
    assert_eq!(
        (n.status.code(), &n.stdout, &n.stderr),
        (r.status.code(), &r.stdout, &r.stderr)
    );
    assert_eq!(n.status.code(), Some(7));
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
