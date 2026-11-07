//! Port of `t3-mcp-call.mjs`: shell-side T3 MCP client. It reuses only the
//! inherited session connection (socket, ACP env or an ancestor Claude
//! `--mcp-config`), refreshes the model catalog and runs the same routing
//! hook before dispatching `delegate_task`.
//!
//! `agentdeck route t3-mcp-call TOOL JSON_FILE|-`; `orchestrator_capabilities`
//! needs no file. Exit 2 on usage errors, 1 on failures (like the .mjs).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

use serde_json::Value;

const TOOLS: &[&str] = &[
    "orchestrator_capabilities",
    "delegate_task",
    "task_status",
    "task_cancel",
];

pub const USAGE: &str =
    "usage: t3-mcp-call TOOL JSON_FILE|- (orchestrator_capabilities needs no file)";

#[derive(Clone, Debug, PartialEq)]
pub struct Connection {
    pub endpoint: String,
    pub authorization: String,
}

fn is_obj(v: &Value) -> bool {
    v.is_object()
}

fn check_connection(endpoint: &str, authorization: &str) -> anyhow::Result<Connection> {
    // Same loopback check as the shell launcher, with Node's exact split:
    // unparseable endpoints are invalid; parseable but non-loopback ones
    // fail the session-authorization check.
    if url::host_invalid(endpoint) {
        anyhow::bail!("Invalid scoped T3 MCP endpoint.");
    }
    let (scheme, rest) = endpoint
        .split_once("://")
        .ok_or_else(|| anyhow::anyhow!("Invalid scoped T3 MCP endpoint."))?;
    if scheme != "http" {
        anyhow::bail!("Expected a loopback T3 MCP endpoint with session authorization.");
    }
    let authority = rest.split('/').next().unwrap_or("");
    if authority.contains('@') || rest.contains(['?', '#']) || authorization.trim().is_empty() {
        anyhow::bail!("Expected a loopback T3 MCP endpoint with session authorization.");
    }
    let host = authority
        .rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(authority);
    if host != "127.0.0.1" && host != "localhost" && host != "[::1]" {
        anyhow::bail!("Expected a loopback T3 MCP endpoint with session authorization.");
    }
    Ok(Connection {
        endpoint: endpoint.to_string(),
        authorization: authorization.to_string(),
    })
}

/// `connectionFromConfig`: the `t3-code` server entry only. `None` when the
/// entry is absent; invalid entries fail (like the Node `checkConnection`
/// throw), they never silently clear.
pub fn connection_from_config(config: &Value) -> anyhow::Result<Option<Connection>> {
    let server = match config.get("mcpServers").and_then(|m| m.get("t3-code")) {
        None => return Ok(None),
        Some(s) => s,
    };
    let url = server.get("url").and_then(Value::as_str).unwrap_or("");
    let authorization = server
        .get("headers")
        .and_then(|h| h.as_object())
        .into_iter()
        .flat_map(|o| o.iter())
        .find(|(k, _)| k.to_lowercase() == "authorization")
        .and_then(|(_, v)| v.as_str())
        .unwrap_or("");
    check_connection(url, authorization).map(Some)
}

/// Injectable ancestor-chain discovery (production passes the real process
/// table); mirrors the Node walk over at most 16 parents while `pid > 1`.
pub fn discover_with(
    env: &dyn Fn(&str) -> Option<String>,
    pid: u32,
    read: &dyn Fn(&str) -> Option<String>,
) -> anyhow::Result<Connection> {
    match (env("T3_ACP_MCP_ENDPOINT"), env("T3_ACP_MCP_AUTHORIZATION")) {
        (Some(endpoint), Some(authorization)) => {
            return check_connection(&endpoint, &authorization);
        }
        (Some(_), None) => {
            anyhow::bail!("Expected a loopback T3 MCP endpoint with session authorization.");
        }
        _ => {}
    }
    let mut pid = pid;
    for _ in 0..16 {
        if pid <= 1 {
            break;
        }
        let args = read(&format!("/proc/{pid}/cmdline"));
        let stat = read(&format!("/proc/{pid}/stat"));
        let (args, stat) = match (args, stat) {
            (Some(a), Some(s)) => (a, s),
            _ => break,
        };
        let parent: u32 = stat
            .rfind(") ")
            .and_then(|i| stat[i + 2..].split_whitespace().nth(1))
            .and_then(|s| s.parse().ok())
            .unwrap_or(1);
        let parts: Vec<&str> = args.split('\0').collect();
        let mut value: Option<&str> = None;
        if let Some(i) = parts.iter().position(|a| *a == "--mcp-config") {
            value = parts.get(i + 1).copied();
        } else if let Some(a) = parts.iter().find(|a| a.starts_with("--mcp-config=")) {
            value = Some(&a[14..]);
        }
        if let Some(value) = value {
            let text = if value.trim_start().starts_with('{') {
                value.to_string()
            } else {
                read(value).ok_or_else(|| {
                    anyhow::anyhow!("Cannot read this session's MCP configuration.")
                })?
            };
            let config: Value = serde_json::from_str(&text)
                .map_err(|_| anyhow::anyhow!("Cannot read this session's MCP configuration."))?;
            if let Some(connection) = connection_from_config(&config)? {
                return Ok(connection);
            }
            anyhow::bail!("Invalid scoped T3 MCP endpoint.");
        }
        pid = parent;
    }
    anyhow::bail!(
        "No inherited T3 MCP connection. Run inside a T3 Claude/ACP session; use native MCP in Codex."
    );
}

pub fn discover_connection() -> anyhow::Result<Connection> {
    discover_with(&|k| std::env::var(k).ok(), parent_pid(), &|p| {
        std::fs::read_to_string(p).ok()
    })
}

fn parent_pid() -> u32 {
    // ppid via /proc/self/stat (field 4 after the comm parens).
    std::fs::read_to_string("/proc/self/stat")
        .ok()
        .and_then(|s| {
            s.rfind(") ")
                .and_then(|i| s[i + 2..].split_whitespace().nth(2))
                .and_then(|v| v.parse().ok())
        })
        .unwrap_or(1)
}

/// Exact Node validation messages; `MAX_TASK` mirrors workflow's T3 limit.
pub fn validate_payload(tool: &str, payload: &Value) -> anyhow::Result<()> {
    if !TOOLS.contains(&tool) {
        anyhow::bail!("Supported tools: {}", TOOLS.join(", "));
    }
    if !is_obj(payload) {
        anyhow::bail!("Arguments must be a JSON object.");
    }
    if tool == "delegate_task" {
        let task_ok = payload
            .get("task")
            .and_then(Value::as_str)
            .is_some_and(|t| !t.trim().is_empty() && t.len() <= 120_000);
        let target = payload.get("target");
        let target_ok = target
            .and_then(|t| t.get("providerInstanceId"))
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty())
            && target
                .and_then(|t| t.get("model"))
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty());
        if !task_ok || !target_ok {
            anyhow::bail!(
                "delegate_task needs a complete task (at most 120000 characters) and exact target."
            );
        }
        if payload.get("mode") != Some(&Value::from("async"))
            || payload
                .get("clientRequestId")
                .and_then(Value::as_str)
                .is_none_or(|s| s.trim().is_empty())
        {
            anyhow::bail!("Shell delegation requires mode=async and a stable clientRequestId.");
        }
        if let Some(role) = payload.get("role")
            && !role
                .as_str()
                .is_some_and(|r| crate::workflow::ROLES.contains(&r))
        {
            anyhow::bail!(
                "delegate_task role must be one of {}.",
                crate::workflow::ROLES.join("|")
            );
        }
        if payload.get("runtimeMode").is_some() {
            anyhow::bail!("Omit runtimeMode to inherit the parent permissions.");
        }
    }
    Ok(())
}

/// `t3-mcp-call` accepts the whole `prepare` output: send its `.payload`
/// exactly as saved.
pub fn unwrap_payload(parsed: Value) -> Value {
    if let Some(payload) = parsed.get("payload") {
        return payload.clone();
    }
    parsed
}

pub fn read_payload(file: Option<&str>) -> anyhow::Result<Value> {
    let text = match file {
        None => return Ok(Value::Object(Default::default())),
        Some("-") => {
            let mut s = String::new();
            std::io::stdin()
                .read_to_string(&mut s)
                .map_err(|_| anyhow::anyhow!("Cannot read valid JSON arguments."))?;
            s
        }
        Some(path) => std::fs::read_to_string(path)
            .map_err(|_| anyhow::anyhow!("Cannot read valid JSON arguments."))?,
    };
    let parsed: Value = serde_json::from_str(&text)
        .map_err(|_| anyhow::anyhow!("Cannot read valid JSON arguments."))?;
    Ok(unwrap_payload(parsed))
}

/// Read the socket's single pushed JSON line (up to the first `\n` or EOF,
/// like the Node client). Exact bridge error texts.
pub fn read_socket_response(path: &Path) -> anyhow::Result<Connection> {
    let mut stream = std::os::unix::net::UnixStream::connect(path)
        .map_err(|_| anyhow::anyhow!("Cannot connect to this Claude session MCP bridge."))?;
    stream
        .set_read_timeout(Some(Duration::from_millis(5000)))
        .ok();
    let mut data = Vec::new();
    let mut buf = [0u8; 1024];
    loop {
        let n = match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                anyhow::bail!("Session bridge timed out.")
            }
            Err(_) => {
                anyhow::bail!("Cannot connect to this Claude session MCP bridge.")
            }
        };
        data.extend_from_slice(&buf[..n]);
        if data.len() > 16384 {
            anyhow::bail!("Cannot connect to this Claude session MCP bridge.");
        }
        if data.contains(&b'\n') {
            break;
        }
    }
    let text = String::from_utf8_lossy(&data).into_owned();
    let value: Value = serde_json::from_str(&text)
        .map_err(|_| anyhow::anyhow!("Claude session MCP bridge returned no valid connection."))?;
    if !is_obj(&value) || value.get("error").is_some() {
        anyhow::bail!("Claude session MCP bridge is not configured yet.");
    }
    let endpoint = value.get("endpoint").and_then(Value::as_str).unwrap_or("");
    let authorization = value
        .get("authorization")
        .and_then(Value::as_str)
        .unwrap_or("");
    check_connection(endpoint, authorization)
        .map_err(|_| anyhow::anyhow!("Claude session MCP bridge returned no valid connection."))
}

pub fn scoped_connection() -> anyhow::Result<Connection> {
    if let Ok(path) = std::env::var("T3_SHELL_MCP_SOCKET") {
        return read_socket_response(Path::new(&path));
    }
    discover_connection()
}

fn spawn_with_input(
    program: &std::ffi::OsStr,
    args: &[&str],
    input: &str,
    timeout: Duration,
    max_bytes: usize,
) -> anyhow::Result<std::process::Output> {
    use std::process::Stdio;
    let mut child = std::process::Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("no stdin"))?
        .write_all(input.as_bytes())?;
    let start = std::time::Instant::now();
    loop {
        match child.try_wait()? {
            Some(_) => break,
            None => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    anyhow::bail!("timeout");
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
    let out = child.wait_with_output()?;
    if out.stdout.len() > max_bytes {
        anyhow::bail!("output too large");
    }
    Ok(out)
}

fn this_exe_route(args: &[&str], input: &str, timeout_secs: u64) -> anyhow::Result<String> {
    let exe = std::env::current_exe()?;
    let out = spawn_with_input(
        exe.as_os_str(),
        args,
        input,
        Duration::from_secs(timeout_secs),
        2 * 1024 * 1024,
    )
    .map_err(|_| anyhow::anyhow!("model-route failed; nothing dispatched."))?;
    if out.status.code() != Some(0) {
        anyhow::bail!("model-route failed; nothing dispatched.");
    }
    String::from_utf8(out.stdout)
        .map_err(|_| anyhow::anyhow!("model-route failed; nothing dispatched."))
}

/// Same pre-dispatch hook the Node bridge runs: deny blocks dispatch.
pub fn check_route(payload: &Value, capabilities: &Value) -> anyhow::Result<()> {
    let input = serde_json::json!({
        "tool_name": "delegate_task",
        "tool_input": payload,
        "inT3": true,
        "threadId": capabilities.get("parentThreadId"),
        "source": capabilities.get("inheritedProviderInstanceId"),
    });
    let stdout = this_exe_route(
        &["route", "hook", "--harness", "shell"],
        &input.to_string(),
        58,
    )?;
    let output: Value = serde_json::from_str(&stdout)
        .map_err(|_| anyhow::anyhow!("model-route failed; nothing dispatched."))?;
    if !["allow", "deny"].contains(&output.get("decision").and_then(Value::as_str).unwrap_or("")) {
        anyhow::bail!("model-route failed; nothing dispatched.");
    }
    if output.get("decision") == Some(&Value::from("deny")) {
        anyhow::bail!(
            "{}",
            output
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("model-route denied delegation before dispatch.")
        );
    }
    if let Some(reason) = output.get("reason").and_then(Value::as_str) {
        eprintln!("{reason}");
    }
    Ok(())
}

pub fn catalog_refresh(capabilities: &Value) -> anyhow::Result<()> {
    let exe_status = this_exe_route(&["route", "--catalog"], &capabilities.to_string(), 10);
    match exe_status {
        Ok(_) => Ok(()),
        Err(_) => anyhow::bail!("Cannot refresh the model catalog; nothing dispatched."),
    }
}

pub type CatalogFn = Box<dyn Fn(&Value) -> anyhow::Result<()> + Send + Sync>;
pub type RouteFn = Box<dyn Fn(&Value, &Value) -> anyhow::Result<()> + Send + Sync>;

pub struct Hooks {
    pub catalog: CatalogFn,
    pub route: RouteFn,
}

impl Hooks {
    pub fn live() -> Self {
        Self {
            catalog: Box::new(catalog_refresh),
            route: Box::new(check_route),
        }
    }
}

// Only protocol validation errors prove that delegation never reached the handler.
#[derive(Debug)]
struct RequestRejected(String);

impl std::fmt::Display for RequestRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RequestRejected {}

struct Rpc {
    client: reqwest::Client,
    endpoint: String,
    authorization: String,
    session: Option<String>,
    protocol: Option<String>,
    next_id: u64,
}

impl Rpc {
    async fn send(
        &mut self,
        method: &str,
        params: Option<Value>,
        notification: bool,
    ) -> anyhow::Result<Option<Value>> {
        let id = if notification {
            None
        } else {
            Some({
                self.next_id += 1;
                self.next_id
            })
        };
        let mut request = self
            .client
            .post(&self.endpoint)
            .timeout(Duration::from_secs(30));
        request = request.header("content-type", "application/json");
        request = request.header("accept", "application/json, text/event-stream");
        request = request.header("authorization", &self.authorization);
        if let Some(s) = &self.session {
            request = request.header("mcp-session-id", s);
        }
        if let Some(p) = &self.protocol {
            request = request.header("mcp-protocol-version", p);
        }
        let mut body = serde_json::Map::new();
        body.insert("jsonrpc".into(), Value::from("2.0"));
        if let Some(i) = id {
            body.insert("id".into(), Value::from(i));
        }
        body.insert("method".into(), Value::from(method));
        let want_unknown = method == "tools/call"
            && params
                .as_ref()
                .and_then(|p| p.get("name"))
                .and_then(Value::as_str)
                == Some("delegate_task");
        if let Some(p) = params {
            body.insert("params".into(), p);
        }
        let body = Value::Object(body);
        // redirect: "error" — never follow, report the status instead.
        let response = request
            .body(body.to_string())
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("T3 MCP tool call failed."))?;
        let status = response.status().as_u16();
        if !response.status().is_success() {
            if want_unknown {
                anyhow::bail!(
                    "T3 tool-call outcome unknown. Do not retry delegation: recover its taskId from parent activity first."
                );
            }
            anyhow::bail!("T3 MCP HTTP {status}.");
        }
        let session = response
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        if session.is_some() {
            self.session = session;
        }
        if notification {
            return Ok(None);
        }
        let is_sse = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("text/event-stream"));
        let envelope: Value = if is_sse {
            parse_sse_envelope(response, id).await?
        } else {
            response
                .json()
                .await
                .map_err(|_| anyhow::anyhow!("Invalid T3 MCP JSON envelope."))?
        };
        if envelope.get("id") == id.map(Value::from).as_ref()
            && let Some(error) = envelope.get("error")
            && let Some(code @ (-32700 | -32600 | -32601 | -32602)) =
                error.get("code").and_then(Value::as_i64)
            && let Some(message) = error.get("message").and_then(Value::as_str)
        {
            return Err(
                RequestRejected(format!("T3 MCP rejected request ({code}): {message}")).into(),
            );
        }
        if envelope.get("id") != id.map(Value::from).as_ref()
            || envelope.get("error").is_some()
            || !envelope.get("result").is_some_and(is_obj)
        {
            anyhow::bail!("T3 MCP rejected the request or returned an invalid response.");
        }
        if let Some(p) = envelope["result"].get("protocolVersion") {
            self.protocol = Some(p.to_string().trim_matches('"').to_string());
        }
        Ok(Some(envelope["result"].clone()))
    }
}

/// Buffered SSE scan: return the first envelope whose `id` matches.
/// Malformed data lines fail exactly like the Node client.
async fn parse_sse_envelope(
    mut response: reqwest::Response,
    id: Option<u64>,
) -> anyhow::Result<Value> {
    let mut buffer = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(bytes)) => buffer.extend_from_slice(&bytes),
            Ok(None) => break,
            Err(_) => anyhow::bail!(
                "T3 tool-call outcome unknown. Do not retry delegation: recover its taskId from parent activity first."
            ),
        }
        let text = String::from_utf8_lossy(&buffer).into_owned();
        let mut rest = text.as_str();
        while let Some(pos) = rest.find("\r\n\r\n").or_else(|| rest.find("\n\n")) {
            let sep = if rest[pos..].starts_with("\r\n") {
                4
            } else {
                2
            };
            let event = &rest[..pos];
            rest = &rest[pos + sep..];
            let mut data_text = String::new();
            for line in event.split(['\r', '\n']) {
                if line.is_empty() {
                    continue;
                }
                if let Some(data) = line.strip_prefix("data:") {
                    let data = data.strip_prefix(' ').unwrap_or(data);
                    if !data_text.is_empty() {
                        data_text.push('\n');
                    }
                    data_text.push_str(data);
                }
            }
            if data_text.is_empty() {
                continue;
            }
            let candidate: Value = serde_json::from_str(&data_text)
                .map_err(|_| anyhow::anyhow!("Invalid T3 MCP SSE envelope."))?;
            if candidate.is_object() && candidate.get("id") == id.map(Value::from).as_ref() {
                return Ok(candidate);
            }
        }
    }
    anyhow::bail!("T3 MCP rejected the request or returned an invalid response.")
}

/// Mirror of Node `callTool`: initialize, refresh catalog, run the routing
/// hook, then dispatch. Returns the raw `tools/call` result.
pub async fn call_tool(
    tool: &str,
    payload: Value,
    connection: Connection,
    hooks: &Hooks,
) -> anyhow::Result<Value> {
    validate_payload(tool, &payload)?;
    let mut rpc = Rpc {
        client: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| anyhow::anyhow!("T3 MCP tool call failed."))?,
        endpoint: connection.endpoint,
        authorization: connection.authorization,
        session: None,
        protocol: None,
        next_id: 0,
    };
    rpc.send(
        "initialize",
        Some(
            serde_json::json!({"protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": "t3-mcp-call", "version": "1"}}),
        ),
        false,
    )
    .await?;
    rpc.send("notifications/initialized", None, true).await?;
    if tool == "delegate_task" {
        let result = rpc
            .send(
                "tools/call",
                Some(serde_json::json!({"name": "orchestrator_capabilities", "arguments": {}})),
                false,
            )
            .await?
            .ok_or_else(|| anyhow::anyhow!("T3 delegation unavailable; nothing dispatched."))?;
        let capabilities = result.get("structuredContent");
        let usable = !result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            && capabilities.is_some_and(is_obj)
            && capabilities.and_then(|c| c.get("parentThreadId")).is_some()
            && capabilities
                .and_then(|c| c.get("inheritedProviderInstanceId"))
                .and_then(Value::as_str)
                .is_some_and(|s| {
                    capabilities
                        .and_then(|c| c.get("providers"))
                        .and_then(Value::as_array)
                        .is_some_and(|p| {
                            p.iter().any(|e| {
                                e.get("providerInstanceId").and_then(Value::as_str) == Some(s)
                            })
                        })
                })
            && capabilities
                .and_then(|c| c.get("features"))
                .and_then(|f| f.get("appOwnedSubagents"))
                .and_then(Value::as_bool)
                == Some(true);
        if !usable {
            anyhow::bail!("T3 delegation unavailable; nothing dispatched.");
        }
        let capabilities = capabilities.cloned().unwrap_or(Value::Null);
        (hooks.catalog)(&capabilities)?;
        (hooks.route)(&payload, &capabilities)?;
    }
    match rpc
        .send(
            "tools/call",
            Some(serde_json::json!({"name": tool, "arguments": payload})),
            false,
        )
        .await
    {
        Ok(Some(result)) => Ok(result),
        Ok(None) => anyhow::bail!("T3 MCP tool call failed."),
        Err(error) if error.is::<RequestRejected>() => Err(error),
        Err(_) if tool == "delegate_task" => {
            anyhow::bail!(
                "T3 tool-call outcome unknown. Do not retry delegation: recover its taskId from parent activity first."
            )
        }
        Err(_) => anyhow::bail!("T3 MCP tool call failed."),
    }
}

/// CLI entry. Returns the process exit code (0, 1, or 2 on usage errors).
pub async fn run(argv: &[String]) -> i32 {
    match run_inner(argv).await {
        Ok(()) => 0,
        Err((usage, e)) => {
            eprintln!("{e:#}");
            if usage { 2 } else { 1 }
        }
    }
}

async fn run_inner(argv: &[String]) -> Result<(), (bool, anyhow::Error)> {
    if argv.first().is_some_and(|a| a == "--help") {
        println!("{USAGE}");
        return Ok(());
    }
    let tool = argv.first().cloned().unwrap_or_default();
    let file = argv.get(1).map(String::as_str);
    if argv.len() > 2 || tool.is_empty() || (file.is_none() && tool != "orchestrator_capabilities")
    {
        return Err((true, anyhow::anyhow!("{USAGE}")));
    }
    let payload = read_payload(file).map_err(|e| (false, e))?;
    validate_payload(&tool, &payload).map_err(|e| (false, e))?;
    let connection = scoped_connection().map_err(|e| (false, e))?;
    let result = call_tool(&tool, payload, connection, &Hooks::live())
        .await
        .map_err(|e| (false, e))?;
    println!("{}", serde_json::to_string(&result).unwrap_or_default());
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        std::process::exit(1);
    }
    Ok(())
}

mod url {
    /// Reject anything `new URL` would reject for our purposes: control
    /// characters or spaces anywhere in the endpoint.
    pub fn host_invalid(url: &str) -> bool {
        url.chars()
            .any(|c| c.is_control() || c == ' ' || c == '\u{7f}')
    }
}

#[allow(dead_code)]
pub fn env_map() -> HashMap<String, String> {
    std::env::vars().collect()
}
