//! Port of `claude-shell-mcp.mjs`: launcher proxy that forwards the
//! original Claude CLI's args and byte streams unchanged, snoops stdin for
//! SDK `mcp_set_servers` control frames, and serves the captured session
//! connection on a private per-launch Unix socket named by
//! `T3_SHELL_MCP_SOCKET`.
//!
//! `agentdeck route claude-shell` is the same entry point; a symlink named
//! `claude-t3-shell` pointing at the binary dispatches to it by argv[0],
//! like `agent-process`.

use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value;

pub const SOCKET_ENV: &str = "T3_SHELL_MCP_SOCKET";

pub fn original_bin() -> String {
    std::env::var("CLAUDE_SHELL_MCP_ORIGINAL").unwrap_or_else(|_| {
        format!(
            "{}/.local/bin/claude",
            std::env::var("HOME").unwrap_or_default()
        )
    })
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Connection {
    pub endpoint: String,
    pub authorization: String,
}

/// `--input-format stream-json` (either spelling) means a streaming session.
pub fn is_streaming_args(argv: &[String]) -> bool {
    let mut i = 0;
    while i < argv.len() {
        if argv[i] == "--input-format" {
            return argv.get(i + 1).is_some_and(|v| v == "stream-json");
        }
        if let Some(v) = argv[i].strip_prefix("--input-format=") {
            return v == "stream-json";
        }
        i += 1;
    }
    false
}

/// Same scope rule as t3-mcp-call: loopback http, no userinfo/query/hash,
/// bearer authorization set.
pub fn extract_connection(servers: &Value) -> Option<Connection> {
    let entry = servers.get("t3-code")?;
    if !entry.is_object() {
        return None;
    }
    let url = entry.get("url")?.as_str()?;
    let headers = entry.get("headers");
    let authorization = headers
        .and_then(|h| h.as_object())
        .into_iter()
        .flat_map(|o| o.iter())
        .find(|(k, _)| k.to_lowercase() == "authorization")
        .and_then(|(_, v)| v.as_str())
        .unwrap_or("");
    if authorization.trim().is_empty() {
        return None;
    }
    // Hand-rolled loopback check (no url crate): scheme http, host-only
    // authority from the loopback set, path without ? or #.
    let rest = url.strip_prefix("http://")?;
    let authority = rest.split('/').next()?;
    if authority.contains('@') || rest.contains(['?', '#']) {
        return None;
    }
    let host = authority
        .rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(authority);
    if !["127.0.0.1", "localhost", "[::1]"].contains(&host) {
        return None;
    }
    if host.starts_with('[') && host != "[::1]" {
        return None;
    }
    Some(Connection {
        endpoint: url.to_string(),
        authorization: authorization.to_string(),
    })
}

/// Accept only a real `control_request` with
/// `request.subtype == "mcp_set_servers"`; anything else (including other
/// control requests carrying a `servers` field) leaves the stored connection
/// untouched. Never throws, never leaks.
pub fn update_connection(current: Option<Connection>, text: &str) -> Option<Connection> {
    let obj: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return current,
    };
    if obj.get("type").and_then(Value::as_str) != Some("control_request") {
        return current;
    }
    let request = match obj.get("request") {
        Some(r) => r,
        None => return current,
    };
    if request.get("subtype").and_then(Value::as_str) != Some("mcp_set_servers") {
        return current;
    }
    match request.get("servers") {
        Some(servers) if servers.is_object() => extract_connection(servers),
        _ => current,
    }
}

/// Legacy seed from this launch's own `--mcp-config` (inline JSON or file);
/// later `mcp_set_servers` frames are authoritative. Never throws.
pub fn seed_from_args(
    argv: &[String],
    read_file: impl Fn(&str) -> Option<String>,
) -> Option<Connection> {
    let mut value: Option<&str> = None;
    let mut i = 0;
    while i < argv.len() {
        if argv[i] == "--mcp-config" {
            value = argv.get(i + 1).map(String::as_str);
            break;
        }
        if let Some(v) = argv[i].strip_prefix("--mcp-config=") {
            value = Some(v);
            break;
        }
        i += 1;
    }
    let value = value?;
    let text = if value.trim_start().starts_with('{') {
        value.to_string()
    } else {
        read_file(value)?
    };
    let config: Value = serde_json::from_str(&text).ok()?;
    if !config.is_object() || config.get("mcpServers").is_none() {
        return None;
    }
    extract_connection(config.get("mcpServers")?)
}

pub fn format_response(connection: Option<&Connection>) -> String {
    match connection {
        Some(c) => {
            serde_json::json!({"endpoint": c.endpoint, "authorization": c.authorization})
                .to_string()
                + "\n"
        }
        None => "{\"error\":\"not_configured\"}\n".to_string(),
    }
}

/// Byte-forwarding snoop: the caller writes every chunk to the child
/// unchanged; `push` returns the complete `\n`-terminated lines (trailing
/// `\r` stripped) seen so far. Split multibyte sequences survive because
/// splitting only happens on `\n` bytes.
#[derive(Default)]
pub struct Snoop {
    buf: Vec<u8>,
}

impl Snoop {
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(chunk);
        let mut lines = Vec::new();
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=pos).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if let Ok(s) = String::from_utf8(line) {
                lines.push(s);
            }
        }
        lines
    }

    /// Trailing bytes without a newline still form a final line (Node's
    /// `flush` path).
    pub fn flush(&mut self) -> Option<String> {
        if self.buf.is_empty() {
            return None;
        }
        let line: Vec<u8> = std::mem::take(&mut self.buf);
        String::from_utf8(line)
            .ok()
            .map(|s| s.strip_suffix('\r').map(str::to_string).unwrap_or(s))
    }
}

struct ServerHandle {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    socket_path: PathBuf,
    dir: PathBuf,
}

impl ServerHandle {
    fn close(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop so join does not hang.
        let _ = UnixStream::connect(&self.socket_path);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        let _ = std::fs::remove_file(&self.socket_path);
        let _ = std::fs::remove_dir(&self.dir);
    }
}

/// Serve one JSON line per connection (`{endpoint, authorization}` or
/// `{error:"not_configured"}`), then end. Directory is `0700`, socket
/// `0600`, like the Node launcher.
fn serve_once(
    socket_path: PathBuf,
    dir: PathBuf,
    get: Arc<Mutex<Option<Connection>>>,
) -> std::io::Result<ServerHandle> {
    std::fs::create_dir_all(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path)?;
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    let stop = Arc::new(AtomicBool::new(false));
    let stop_loop = stop.clone();
    let thread = std::thread::spawn(move || {
        while !stop_loop.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let line = {
                        let guard = get.lock().unwrap_or_else(|e| e.into_inner());
                        format_response(guard.as_ref())
                    };
                    let _ = stream.write_all(line.as_bytes());
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(_) => break,
            }
        }
    });
    Ok(ServerHandle {
        stop,
        thread: Some(thread),
        socket_path,
        dir,
    })
}

static CHILD_PID: AtomicI32 = AtomicI32::new(0);

#[cfg(unix)]
fn forward_signal(signum: std::os::raw::c_int) {
    let pid = CHILD_PID.load(Ordering::SeqCst);
    if pid > 0 {
        unsafe {
            libc::kill(pid, signum);
        }
    }
}

#[cfg(unix)]
fn install_forwarding() {
    unsafe {
        libc::signal(
            libc::SIGTERM,
            forward_signal as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGINT,
            forward_signal as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGHUP,
            forward_signal as *const () as libc::sighandler_t,
        );
    }
}

/// Streaming launch: private runtime dir + socket, spawn the original with
/// the socket path in its env, snoop stdin through to the child byte-identical.
fn launch_streaming(argv: &[String]) -> i32 {
    let original = original_bin();
    let read_file = |p: &str| std::fs::read_to_string(p).ok();
    let connection = Arc::new(Mutex::new(seed_from_args(argv, read_file)));
    let mut runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    // Linux sun_path includes a NUL byte; long XDG_RUNTIME_DIR/TMPDIR values need a short fallback.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let suffix = format!(
        "claude-shell-mcp-{:x}-{:x}",
        std::process::id(),
        nanos & 0xffffff
    );
    let candidate = runtime.join(&suffix).join("t3-shell-mcp.sock");
    if candidate.as_os_str().len() >= 108 {
        runtime = PathBuf::from("/tmp");
    }
    let dir = runtime.join(suffix);
    if std::fs::create_dir_all(&runtime).is_err()
        || std::fs::DirBuilder::new().mode(0o700).create(&dir).is_err()
    {
        return 1;
    }
    let socket_path = dir.join("t3-shell-mcp.sock");
    let server = match serve_once(socket_path.clone(), dir.clone(), connection.clone()) {
        Ok(s) => s,
        Err(_) => {
            let _ = std::fs::remove_dir_all(&dir);
            return 1;
        }
    };
    let mut child = match std::process::Command::new(&original)
        .args(argv)
        .stdin(std::process::Stdio::piped())
        .env(SOCKET_ENV, &socket_path)
        .spawn()
    {
        Ok(c) => c,
        Err(_) => {
            server.close();
            return 127;
        }
    };
    CHILD_PID.store(child.id() as i32, Ordering::SeqCst);
    install_forwarding();
    // Pump stdin -> snoop -> child stdin, byte-identical.
    let mut child_stdin = child.stdin.take();
    let pump_conn = connection.clone();
    let pump = std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut locked = stdin.lock();
        let mut snoop = Snoop::default();
        let mut chunk = [0u8; 8192];
        loop {
            let n = match locked.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            for line in snoop.push(&chunk[..n]) {
                let next = update_connection(
                    pump_conn.lock().unwrap_or_else(|e| e.into_inner()).clone(),
                    &line,
                );
                *pump_conn.lock().unwrap_or_else(|e| e.into_inner()) = next;
            }
            if let Some(w) = child_stdin.as_mut()
                && w.write_all(&chunk[..n]).is_err()
            {
                break;
            }
        }
        if let Some(line) = snoop.flush() {
            let next = update_connection(
                pump_conn.lock().unwrap_or_else(|e| e.into_inner()).clone(),
                &line,
            );
            *pump_conn.lock().unwrap_or_else(|e| e.into_inner()) = next;
        }
        // Closing our handle lets a still-running child see EOF.
        drop(child_stdin);
    });
    let code = match child.wait() {
        Ok(s) => s.code().unwrap_or(127),
        Err(_) => 127,
    };
    let _ = pump.join();
    server.close();
    code
}

/// CLI entry: passthrough when not streaming, proxy when streaming.
/// Returns the process exit code.
pub fn run(argv: &[String]) -> i32 {
    if !is_streaming_args(argv) {
        let status = std::process::Command::new(original_bin())
            .args(argv)
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .status();
        return match status {
            Ok(s) => s.code().unwrap_or(127),
            Err(_) => 127,
        };
    }
    launch_streaming(argv)
}
