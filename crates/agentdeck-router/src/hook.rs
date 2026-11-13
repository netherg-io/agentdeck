//! Port of `hook.mjs` (with `repairReasoning`): the pre-tool delegation
//! gate as `agentdeck route hook --harness <name>`.
//!
//! Same stdin/stdout JSON protocol and fail-closed behavior: any error
//! denies delegation (exit 0, the verdict rides on stdout). Routing runs in
//! the same binary (`route` with `--validate-target`), like the .mjs spawns
//! `route.mjs`.

use serde_json::{Map, Value};

use crate::load::is_delegated_child;
use crate::reasoning::{read_catalog, repair_reasoning, validate_reasoning};

const ROUTE_TIMEOUT_SECS: u64 = 55;
const ROUTE_MAX_BYTES: usize = 2 * 1024 * 1024;
const USER_CHOICE: &str = "[route:user-choice]";
const USER_CHOICE_HINT: &str = " If the user explicitly asked for a specific model, call delegate_task with that exact target and start the task with [route:user-choice]; routing then only warns.";

const NOISE: &[&str] = &[
    "openrouter",
    "anthropic",
    "google",
    "openai",
    "meta",
    "moonshotai",
    "opencode",
    "contributor",
    "latest",
    "preview",
    "low",
    "medium",
    "high",
    "xhigh",
    "max",
    "ultra",
];

/// `="..."`: answers picked in an AskUserQuestion dialog.
static ASKED_ANSWER: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r#"="[^"]*""#).unwrap());

const CYRILLIC: &[(&str, &[&str])] = &[
    ("haiku", &["хайку"]),
    ("sonnet", &["сонет", "соннет"]),
    ("opus", &["опус"]),
    ("fable", &["фейбл", "фабл"]),
    ("argon", &["аргон"]),
    ("muse", &["мьюз", "муз"]),
    ("gemini", &["гемини", "джемини"]),
    ("kimi", &["кими"]),
    ("gpt", &["гпт"]),
    ("claude", &["клод"]),
    ("codex", &["кодекс"]),
    ("astra", &["астра"]),
];

fn state_dir() -> anyhow::Result<std::path::PathBuf> {
    if let Some(d) = std::env::var_os("XDG_STATE_HOME") {
        return Ok(std::path::PathBuf::from(d));
    }
    Ok(crate::home_dir()?.join(".local/state"))
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// stdout verdict: agy/opencode/shell always print `{decision, reason}`;
/// other harnesses print the PreToolUse envelope, or nothing on clean allow.
fn emit(harness: &str, allowed: bool, reason: &str, out: &mut Vec<u8>) {
    if matches!(harness, "agy" | "opencode" | "shell") {
        out.extend_from_slice(
            serde_json::json!({"decision": if allowed { "allow" } else { "deny" }, "reason": reason})
                .to_string()
                .as_bytes(),
        );
        out.push(b'\n');
    } else if !reason.is_empty() || !allowed {
        let mut specific = Map::new();
        specific.insert("hookEventName".into(), Value::from("PreToolUse"));
        if !allowed {
            specific.insert("permissionDecision".into(), Value::from("deny"));
            specific.insert("permissionDecisionReason".into(), Value::from(reason));
        } else {
            specific.insert("additionalContext".into(), Value::from(reason));
        }
        let mut root = Map::new();
        root.insert("hookSpecificOutput".into(), Value::Object(specific));
        out.extend_from_slice(Value::Object(root).to_string().as_bytes());
        out.push(b'\n');
    }
}

fn is_delegate_tool(name: &str) -> bool {
    name.ends_with("delegate_task")
}

fn is_native_tool(name: &str) -> bool {
    matches!(
        name,
        "Agent"
            | "spawn_agent"
            | "collaboration.spawn_agent"
            | "collaborationspawn_agent"
            | "invoke_subagent"
            | "start_subagent"
            | "subagent"
    )
}

/// Append a verdict line to `decisions.jsonl`; never fails the hook.
const SHOWN_ERROR: &str = "model-route: routing failed or timed out; delegation was blocked. The cause is in the latest deny:router-error entry of model-route/decisions.jsonl. Retry the router or do the work yourself.";

fn record(
    harness: &str,
    instance: Option<&str>,
    tool_name: Option<&str>,
    args: &Value,
    route: Option<&Value>,
    decision: &str,
    reason: &str,
) {
    let result = (|| -> anyhow::Result<()> {
        let dir = state_dir()?.join("model-route");
        std::fs::create_dir_all(&dir)?;
        let t = iso_now();
        let mut e = Map::new();
        e.insert("t".into(), Value::from(t));
        e.insert("harness".into(), Value::from(harness));
        if let Some(i) = instance {
            e.insert("instance".into(), Value::from(i));
        }
        if let Some(n) = tool_name {
            e.insert("toolName".into(), Value::from(n));
        }
        if let Some(n) = args.get("task_name") {
            e.insert("task_name".into(), n.clone());
        }
        e.insert("decision".into(), Value::from(decision));
        e.insert("reason".into(), Value::from(reason));
        // Node drops `undefined` (missing keys) but keeps `null`: the route
        // target is `null` — not missing — when nothing is adequate.
        if let Some(r) = route {
            for k in ["target", "size", "confidence"] {
                let v = match k {
                    "confidence" => r.get("jevConfidence"),
                    _ => r.get(k),
                };
                if let Some(v) = v {
                    e.insert(k.into(), v.clone());
                }
            }
        }
        if tool_name.is_some_and(is_delegate_tool)
            && let Some(v) = args.get("target")
        {
            e.insert("requestedTarget".into(), v.clone());
        }
        if let Some(r) = route {
            for k in ["blocked", "ranking", "activeLoad"] {
                if let Some(v) = r.get(k) {
                    e.insert(k.into(), v.clone());
                }
            }
        }
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("decisions.jsonl"))?;
        writeln!(f, "{}", Value::Object(e))?;
        Ok(())
    })();
    if result.is_err() {
        eprintln!("model-route: decision log unavailable");
    }
}

fn iso_now() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let mut secs = now.as_secs() as i64;
    let ms = now.subsec_millis();
    let mut days = secs.div_euclid(86400);
    secs = secs.rem_euclid(86400);
    let (mut y, mut m, mut d) = (1970, 1, 1);
    while days >= 365 {
        let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
        let len = if leap { 366 } else { 365 };
        if days < len {
            break;
        }
        days -= len;
        y += 1;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    for (i, len) in [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ]
    .into_iter()
    .enumerate()
    {
        if days < len {
            m = i as i64 + 1;
            d = days + 1;
            break;
        }
        days -= len;
    }
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{ms:03}Z",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

/// Whether `<...>` opens a harness-injected block the hook must ignore.
fn strip_tag_name(name: &str) -> bool {
    if name == "system-reminder" || name == "task-notification" {
        return true;
    }
    for prefix in ["local-command-", "command-"] {
        if let Some(rest) = name.strip_prefix(prefix)
            && !rest.is_empty()
            && rest.bytes().all(|b| b.is_ascii_lowercase())
        {
            return true;
        }
    }
    false
}

/// Strip `<tag>...<\/tag>` harness blocks, mirroring
/// `/<(system-reminder|task-notification|local-command-[a-z]+|command-[a-z]+)>[\s\S]*?<\/\1>/g`.
/// (The `regex` crate has no backreferences, so this is a literal scan: the
/// first matching close tag ends the span, an unclosed `<` stays literal.)
fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(lt) = rest.find('<') {
        let after = &rest[lt + 1..];
        let mut len = 0;
        while len < after.len()
            && (after.as_bytes()[len].is_ascii_lowercase() || after.as_bytes()[len] == b'-')
        {
            len += 1;
        }
        let name = &after[..len];
        let open = !name.is_empty() && after[len..].starts_with('>') && strip_tag_name(name);
        if !open {
            out.push_str(&rest[..lt + 1]);
            rest = &rest[lt + 1..];
            continue;
        }
        let close = format!("</{name}>");
        if let Some(end) = after.find(&close) {
            out.push_str(&rest[..lt]);
            rest = &after[end + close.len()..];
        } else {
            out.push_str(&rest[..lt + 1]);
            rest = &rest[lt + 1..];
        }
    }
    out.push_str(rest);
    out
}
/// Last genuine user messages from the harness transcript; `None` when
/// unavailable or in an unknown format.
fn recent_user_texts(input: &Value, limit: usize) -> Option<Vec<String>> {
    let file =
        str_field(input, "transcript_path").or_else(|| str_field(input, "transcriptPath"))?;
    if file.is_empty() {
        return None;
    }
    let meta = std::fs::metadata(file).ok()?;
    let size = meta.len();
    let length = size.min(4 * 1024 * 1024) as usize;
    let bytes = (|| {
        use std::io::{Read, Seek};
        let mut f = std::fs::File::open(file).ok()?;
        f.seek(std::io::SeekFrom::End(-(length as i64))).ok()?;
        let mut buf = vec![0u8; length];
        f.read_exact(&mut buf).ok()?;
        Some(buf)
    })()?;
    let text = String::from_utf8_lossy(&bytes);
    let entries: Vec<Value> = text
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let mut asked = std::collections::HashSet::new();
    for e in &entries {
        let msg = e.get("message");
        if msg.and_then(|m| m.get("role")).and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        if let Some(content) = msg.and_then(|m| m.get("content")).and_then(Value::as_array) {
            for c in content {
                if c.get("type").and_then(Value::as_str) == Some("tool_use")
                    && c.get("name").and_then(Value::as_str) == Some("AskUserQuestion")
                    && let Some(id) = c.get("id").and_then(Value::as_str)
                {
                    asked.insert(id.to_string());
                }
            }
        }
    }
    let mut out = Vec::new();
    for entry in entries.iter().rev() {
        let message = entry
            .get("message")
            .or_else(|| entry.get("payload"))
            .or(Some(entry))?;
        if message.get("role").and_then(Value::as_str) != Some("user")
            || entry.get("isMeta").and_then(Value::as_bool) == Some(true)
        {
            continue;
        }
        let content = message.get("content");
        let mut parts: Vec<String> = Vec::new();
        match content {
            Some(Value::String(s)) => parts.push(s.clone()),
            Some(Value::Array(items)) => {
                for c in items {
                    match c.get("type").and_then(Value::as_str) {
                        Some("text") | Some("input_text") => {
                            parts.push(
                                c.get("text")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string(),
                            );
                        }
                        Some("tool_result")
                            if c.get("tool_use_id")
                                .and_then(Value::as_str)
                                .is_some_and(|id| asked.contains(id)) =>
                        {
                            let joined = match c.get("content") {
                                Some(Value::String(s)) => s.clone(),
                                Some(Value::Array(a)) => a
                                    .iter()
                                    .map(|x| {
                                        x.get("text").and_then(Value::as_str).unwrap_or_default()
                                    })
                                    .collect::<Vec<_>>()
                                    .join("\n"),
                                _ => String::new(),
                            };
                            // `="..."`: answers picked in an AskUserQuestion dialog.
                            parts.extend(ASKED_ANSWER.find_iter(&joined).map(|m| {
                                let s = m.as_str();
                                s[2..s.len() - 1].to_string()
                            }));
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        let said = strip_tags(&parts.join("\n")).trim().to_string();
        if said.is_empty() {
            continue;
        }
        out.push(said);
        if out.len() >= limit {
            break;
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

/// Whether the transcript shows a recent user message naming `model`;
/// `None` when the transcript cannot tell (the prefix is then trusted).
fn user_named_model(model: &str, input: &Value) -> Option<bool> {
    let texts = recent_user_texts(input, 6)?;
    let mut words: Vec<String> = model
        .to_lowercase()
        .split(|c: char| !c.is_ascii_alphabetic())
        .filter(|w| w.len() > 2 && !NOISE.contains(w))
        .map(str::to_string)
        .collect();
    let mut names = words.clone();
    for w in &words {
        if let Some((_, vars)) = CYRILLIC.iter().find(|(k, _)| k == w) {
            names.extend(vars.iter().map(|s| s.to_string()));
        }
    }
    words = names;
    if words.is_empty() {
        return None;
    }
    let patterns: Vec<regex::Regex> = words
        .iter()
        .map(|n| {
            regex::Regex::new(&format!(
                "(?i)(?:^|[^\\p{{L}}\\p{{N}}]){}",
                regex::escape(n)
            ))
        })
        .collect::<Result<_, _>>()
        .ok()?;
    Some(texts.iter().any(|t| patterns.iter().any(|p| p.is_match(t))))
}

fn proc_cmdline(pid: u32) -> anyhow::Result<Vec<String>> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline"))?;
    Ok(String::from_utf8_lossy(&raw)
        .split('\0')
        .map(str::to_string)
        .collect())
}

fn proc_ppid(pid: u32) -> anyhow::Result<u32> {
    let raw = std::fs::read(format!("/proc/{pid}/stat"))?;
    let text = String::from_utf8_lossy(&raw);
    let after = text
        .split_once(") ")
        .map(|(_, rest)| rest)
        .ok_or_else(|| anyhow::anyhow!("bad stat"))?;
    after
        .split(' ')
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("bad stat"))?
        .parse()
        .map_err(|_| anyhow::anyhow!("bad stat"))
}

fn ancestors() -> Vec<Vec<String>> {
    let mut out = Vec::new();
    let mut pid = std::os::unix::process::parent_id();
    for _ in 0..12 {
        if pid <= 1 {
            break;
        }
        let cmd = match proc_cmdline(pid) {
            Ok(c) => c,
            Err(_) => break,
        };
        out.push(cmd);
        pid = match proc_ppid(pid) {
            Ok(p) => p,
            Err(_) => break,
        };
    }
    out
}

/// Older T3 ran a Node server; current T3 is a native `t3 serve` binary.
fn t3_server(cmd: &[String]) -> bool {
    cmd.iter().any(|a| a.ends_with("/apps/server/dist/bin.mjs"))
        || (cmd.first().is_some_and(|a| a == "t3" || a.ends_with("/t3"))
            && cmd.get(1).is_some_and(|a| a == "serve"))
}

fn under_server(chain: &[Vec<String>], pred: impl Fn(&[String]) -> bool) -> bool {
    if let Some(i) = chain.iter().position(|c| pred(c)) {
        chain[i..].iter().any(|c| t3_server(c))
    } else {
        false
    }
}

fn in_t3(harness: &str, chain: &[Vec<String>], input: &Value) -> bool {
    match harness {
        "claude" => {
            chain.iter().any(|cmd| {
                cmd.iter().any(|a| a == "--mcp-config")
                    && cmd.iter().any(|a| a.contains("\"t3-code\""))
            }) || under_server(chain, |cmd| {
                cmd.iter().any(|a| a == "--input-format") && cmd.iter().any(|a| a == "stream-json")
            })
        }
        "codex" => under_server(chain, |cmd| cmd.iter().any(|a| a.contains("app-server"))),
        "agy" => under_server(chain, |cmd| {
            cmd.first()
                .is_some_and(|c| c.contains("/.t3/tools/antigravity-acp/"))
        }),
        "opencode" | "shell" => input.get("inT3").and_then(Value::as_bool) == Some(true),
        _ => false,
    }
}

fn route_args(source: &str, validate: Option<(&str, &str)>) -> Vec<String> {
    let mut args = vec!["route".into(), "--source".into(), source.into()];
    if let Some((provider, model)) = validate {
        args.extend(["--validate-target".into(), provider.into(), model.into()]);
    }
    args.push("-".into());
    args
}

fn preflight_hint(source: &str, provider: &str, model: &str) -> String {
    let args = route_args(source, Some((provider, model)));
    let command = args
        .iter()
        .enumerate()
        .map(|(i, arg)| {
            if matches!(i, 0 | 1 | 3 | 6) {
                arg.clone()
            } else {
                format!("'{}'", arg.replace('\'', "'\"'\"'"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        " To obtain this routing verdict before dispatch, save the exact delegate_task.task text in task.txt and run `agentdeck {command} < task.txt`. Use the full task, not a shortened goal; quota and load are rechecked at dispatch."
    )
}

/// Run the router in this binary (`route ...`) with `task_text` on stdin.
fn run_route(
    source: &str,
    validate: Option<(&str, &str)>,
    task_text: &str,
) -> anyhow::Result<Value> {
    let exe = std::env::current_exe()?;
    let mut child = std::process::Command::new(exe)
        .args(route_args(source, validate))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    use std::io::Write;
    child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("no stdin"))?
        .write_all(task_text.as_bytes())?;
    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(ROUTE_TIMEOUT_SECS);
    let status = loop {
        match child.try_wait()? {
            Some(s) => break s,
            None => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    anyhow::bail!("route timed out after {ROUTE_TIMEOUT_SECS}s");
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    };
    let output = child.wait_with_output()?;
    if output.stdout.len() + output.stderr.len() > ROUTE_MAX_BYTES {
        anyhow::bail!("route output too large");
    }
    if !status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim().chars().take(300).collect::<String>();
        anyhow::bail!("route exited with {status}: {detail}");
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}

struct Ctx {
    harness: String,
    out: Vec<u8>,
}

impl Ctx {
    fn emit(&mut self, allowed: bool, reason: &str) {
        emit(&self.harness, allowed, reason, &mut self.out);
    }

    fn deny(
        &mut self,
        instance: Option<&str>,
        tool_name: Option<&str>,
        args: &Value,
        route: Option<&Value>,
        decision: &str,
        reason: &str,
    ) {
        record(
            &self.harness,
            instance,
            tool_name,
            args,
            route,
            decision,
            reason,
        );
        self.emit(false, reason);
    }
}

fn default_instance(harness: &str) -> Option<&str> {
    match harness {
        "claude" => Some(
            if std::env::var_os("CLAUDE_CONFIG_DIR")
                .is_some_and(|d| d.to_string_lossy().contains(".claude-openrouter"))
            {
                "claudeAgent_openrouter"
            } else {
                "claudeAgent"
            },
        ),
        "codex" => Some("codex"),
        "agy" => Some("antigravity"),
        "opencode" => Some("opencode"),
        _ => None,
    }
}

/// Find the T3 provider instance id whose driver/config path matches.
fn settings_instance(settings: &Value, driver: &str, key: &str, path: &str) -> Option<String> {
    settings
        .get("providerInstances")?
        .as_object()?
        .iter()
        .find(|(_, v)| {
            v.get("driver").and_then(Value::as_str) == Some(driver)
                && v.get("config")
                    .and_then(|c| c.get(key))
                    .and_then(Value::as_str)
                    == Some(path)
        })
        .map(|(k, _)| k.clone())
}

/// Fail-closed entry point: always returns 0, the verdict rides on stdout.
pub fn run(argv: &[String]) -> i32 {
    let mut ctx = Ctx {
        harness: argv
            .iter()
            .position(|a| a == "--harness")
            .and_then(|i| argv.get(i + 1))
            .cloned()
            .unwrap_or_default(),
        out: Vec::new(),
    };
    if let Err(e) = run_inner(&mut ctx) {
        // Stderr and parser text stay out of the model-visible reason; the log keeps the real cause.
        let shown = SHOWN_ERROR;
        record(
            &ctx.harness,
            None,
            None,
            &Value::Null,
            None,
            "deny:router-error",
            &format!("{e:#} | {shown}"),
        );
        let reason = shown;
        ctx.emit(false, reason);
    }
    use std::io::Write;
    let _ = std::io::stdout().write_all(&ctx.out);
    0
}

/// A hook left in a harness file after the router plugin was disabled (an
/// unsynced profile, a copied settings.json) must not keep routing. An
/// unreadable registry keeps routing: hooks fail closed.
fn router_disabled() -> bool {
    let Ok(paths) = agentdeck_core::Paths::from_env() else {
        return false;
    };
    paths.registry_file().exists()
        && agentdeck_core::Registry::load(&paths).is_ok_and(|reg| {
            !agentdeck_core::plugins::plugin_enabled(&reg, agentdeck_core::plugins::PLUGIN_ROUTER)
        })
}

fn run_inner(ctx: &mut Ctx) -> anyhow::Result<()> {
    if router_disabled() {
        ctx.emit(true, "");
        return Ok(());
    }
    let home = crate::home_dir()?;
    let raw = {
        use std::io::Read;
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s)?;
        s
    };
    let input: Value = serde_json::from_str(if raw.is_empty() { "{}" } else { &raw })?;
    let harness = ctx.harness.clone();
    let (args, tool_name) = if harness == "agy" {
        let call = input.get("toolCall");
        let args = call
            .and_then(|c| c.get("args"))
            .and_then(|a| a.get("Arguments"))
            .or_else(|| call.and_then(|c| c.get("args")))
            .or_else(|| input.get("tool_input"))
            .cloned()
            .unwrap_or(Value::Object(Map::new()));
        let mut name = call
            .and_then(|c| c.get("name"))
            .and_then(Value::as_str)
            .or_else(|| str_field(&input, "tool_name"))
            .unwrap_or_default()
            .to_string();
        if name == "call_mcp_tool"
            && call
                .and_then(|c| c.get("args"))
                .and_then(|a| a.get("ServerName"))
                .and_then(Value::as_str)
                .is_some_and(|s| s == "t3-code" || s == "t3_code")
        {
            name = call
                .and_then(|c| c.get("args"))
                .and_then(|a| a.get("ToolName"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
        }
        (args, name)
    } else {
        (
            input
                .get("tool_input")
                .cloned()
                .unwrap_or(Value::Object(Map::new())),
            str_field(&input, "tool_name")
                .unwrap_or_default()
                .to_string(),
        )
    };
    let args_obj = if args.is_object() {
        args.clone()
    } else {
        Value::Object(Map::new())
    };
    let native = is_native_tool(&tool_name);
    let delegate = is_delegate_tool(&tool_name);
    let workflow = tool_name == "Workflow";
    if !native && !delegate && !workflow {
        ctx.emit(true, "");
        return Ok(());
    }
    let chain = ancestors();
    // delegate_task exists only in T3, so it is always routed: a missed T3
    // detection must not skip routing.
    if !delegate && !in_t3(&harness, &chain, &input) {
        ctx.emit(true, "");
        return Ok(());
    }
    let sqlite = home.join(".t3/userdata/statev2.sqlite");
    let native_id: Option<&str> = if harness == "agy" {
        str_field(&input, "conversationId")
    } else {
        str_field(&input, "session_id")
    };
    let thread_id: Option<&str> = str_field(&input, "threadId");
    if is_delegated_child(native_id, thread_id, &sqlite) {
        let reason = "model-route: you are a delegated child agent; nested delegation is disabled. Do this task yourself with your own tools. If it is too large, complete what fits and report the remainder in your final response.";
        ctx.deny(
            None,
            Some(&tool_name),
            &args_obj,
            None,
            "deny:nested",
            reason,
        );
        return Ok(());
    }
    let mut instance: Option<String> = default_instance(&harness).map(str::to_string);
    if harness == "claude"
        && let Some(dir) =
            std::env::var_os("CLAUDE_CONFIG_DIR").map(|d| d.to_string_lossy().into_owned())
        && !dir.is_empty()
    {
        let text = std::fs::read_to_string(home.join(".t3/userdata/settings.json"))?;
        let settings: Value = serde_json::from_str(&text)?;
        if let Some(id) = settings_instance(&settings, "claudeAgent", "homePath", &dir) {
            instance = Some(id);
        }
    }
    if harness == "shell" {
        let source = str_field(&input, "source").unwrap_or_default();
        instance = Some(source.to_string());
        let catalog = read_catalog(&state_dir()?)?;
        let known = catalog
            .get("providers")
            .and_then(Value::as_array)
            .is_some_and(|ps| {
                ps.iter().any(|p| {
                    p.get("providerInstanceId").and_then(Value::as_str) == Some(source)
                        && !source.is_empty()
                })
            });
        if !known {
            let reason = "model-route: shell source must be a provider from live scoped orchestrator_capabilities.";
            ctx.deny(
                instance.as_deref(),
                Some(&tool_name),
                &args_obj,
                None,
                "deny:shell-source",
                reason,
            );
            return Ok(());
        }
    }
    if harness == "codex"
        && let Some(dir) = std::env::var_os("CODEX_HOME").map(|d| d.to_string_lossy().into_owned())
        && !dir.is_empty()
    {
        let text = std::fs::read_to_string(home.join(".t3/userdata/settings.json"))?;
        let settings: Value = serde_json::from_str(&text)?;
        if let Some(id) = settings_instance(&settings, "codex", "shadowHomePath", &dir) {
            instance = Some(id);
        }
    }
    if std::env::var_os("MODEL_ROUTE_HOOK_DEBUG").is_some()
        || state_dir()?.join("model-route/debug").exists()
    {
        let dir = state_dir()?.join("model-route");
        std::fs::create_dir_all(&dir)?;
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("hook-input.jsonl"))?;
        writeln!(
            f,
            "{}",
            serde_json::json!({"harness": harness, "t": iso_now(), "input": input})
        )?;
    }
    if workflow {
        let reason = "model-route: Workflow can launch or resume agents with unchecked inherited models. Route each child task separately with route.mjs and use T3 delegate_task with its exact target and no runtimeMode. Same-model inheritance does not bypass routing. Do not retry Workflow to evade this check.";
        ctx.deny(
            instance.as_deref(),
            Some(&tool_name),
            &args_obj,
            None,
            "deny:workflow",
            reason,
        );
        return Ok(());
    }
    let get_str = |key: &str| args.get(key).and_then(Value::as_str).unwrap_or_default();
    let prompt = if !get_str("message").is_empty() {
        get_str("message")
    } else if !get_str("prompt").is_empty() {
        get_str("prompt")
    } else if !get_str("instructions").is_empty() {
        get_str("instructions")
    } else {
        get_str("task")
    }
    .to_string();
    if delegate {
        let task = str_field(&args, "task").unwrap_or_default();
        let target = args.get("target");
        let valid = !task.trim().is_empty()
            && target
                .and_then(|t| t.get("providerInstanceId"))
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty())
            && target
                .and_then(|t| t.get("model"))
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty());
        if !valid {
            let reason = "model-route: delegate_task requires a task and an explicit exact providerInstanceId/model target.";
            ctx.deny(
                instance.as_deref(),
                Some(&tool_name),
                &args_obj,
                None,
                "deny:invalid-delegate",
                reason,
            );
            return Ok(());
        }
    }
    let target = args.get("target");
    let brief = if delegate {
        str_field(&args, "task").unwrap_or_default().to_string()
    } else if !prompt.trim().is_empty() {
        prompt.clone()
    } else {
        format!(
            "Subagent call from {harness}: {}",
            serde_json::to_string(&args)?
        )
    };
    let route = run_route(
        instance.as_deref().unwrap_or_default(),
        delegate.then(|| {
            (
                target
                    .and_then(|t| t.get("providerInstanceId"))
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                target
                    .and_then(|t| t.get("model"))
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            )
        }),
        &brief,
    )?;
    let confidence = route
        .get("jevConfidence")
        .and_then(Value::as_f64)
        .unwrap_or(f64::NAN);
    let warning = if confidence < 0.4 {
        let top = route
            .get("confidenceWarning")
            .and_then(|w| w.get("topTwo"))
            .cloned()
            .unwrap_or(Value::Array(Vec::new()));
        format!(
            " Confidence is low ({confidence}); compare expected quality and quota penalties: {top}."
        )
    } else {
        String::new()
    };
    if delegate {
        let allowed = route
            .get("validation")
            .and_then(|v| v.get("allowed"))
            .and_then(Value::as_bool)
            .ok_or_else(|| anyhow::anyhow!("invalid validation"))?;
        let mut validation = route.get("validation").cloned().unwrap_or(Value::Null);
        if args.get("role").and_then(Value::as_str) == Some("review")
            && route.get("requestedNoReview").and_then(Value::as_bool) == Some(true)
        {
            validation = serde_json::json!({"allowed": false, "reason": "never for audits or reviews (user rule 2026-10-17)"});
        }
        let user_choice = !allowed
            && str_field(&args, "task")
                .unwrap_or_default()
                .starts_with(USER_CHOICE);
        let mut choice_warning = String::new();
        if user_choice {
            match user_named_model(
                target
                    .and_then(|t| t.get("model"))
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                &input,
            ) {
                Some(false) => {
                    let reason = format!(
                        "model-route: [route:user-choice] needs a recent user message that names {}. Do not use it for models the user did not ask for.",
                        target
                            .and_then(|t| t.get("model"))
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                    );
                    ctx.deny(
                        instance.as_deref(),
                        Some(&tool_name),
                        &args_obj,
                        Some(&route),
                        "deny:user-choice",
                        &reason,
                    );
                    return Ok(());
                }
                _ => {
                    let router_target = route.get("target").cloned().unwrap_or(Value::Null);
                    let suffix = if router_target.is_null() {
                        String::new()
                    } else {
                        format!(
                            "; the router would pick {}/{}",
                            router_target
                                .get("providerInstanceId")
                                .and_then(Value::as_str)
                                .unwrap_or_default(),
                            router_target
                                .get("model")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                        )
                    };
                    let reason_text = validation
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    choice_warning = format!(
                        " User-requested target despite routing ({reason_text}){suffix}. Tell the user if this model is a poor fit."
                    );
                    validation =
                        serde_json::json!({"allowed": true, "reason": "user-requested target"});
                }
            }
        }
        let client_request_id = str_field(&args, "clientRequestId").unwrap_or_default();
        let workflow_retry = client_request_id.starts_with("routed-workflow-");
        let retry_hint: String = if workflow_retry {
            "This workflow call was denied before dispatch. Only if this step has no accepted or uncertain earlier launch, release its prepared step with --not-dispatched, check success, then prepare again. Do not change the saved payload or dispatch a different target directly.".to_string()
        } else if let Some(t) = route.get("target")
            && !t.is_null()
        {
            format!(
                "Use target {} with the same task and no runtimeMode.",
                serde_json::to_string(t)?
            )
        } else {
            String::new()
        };
        let allowed_now = validation.get("allowed").and_then(Value::as_bool) == Some(true);
        if allowed_now {
            let catalog = read_catalog(&state_dir()?)?;
            if let Some(invalid) = validate_reasoning(&catalog, target.unwrap_or(&Value::Null))? {
                let recommended = route
                    .get("requestedTarget")
                    .or_else(|| route.get("target"))
                    .cloned()
                    .unwrap_or(Value::Null);
                let repaired =
                    repair_reasoning(&catalog, target.unwrap_or(&Value::Null), &recommended)?;
                let tail = if workflow_retry {
                    retry_hint.clone()
                } else {
                    format!(
                        "Use target {} with the same task and no runtimeMode.",
                        serde_json::to_string(&repaired)?
                    )
                };
                let reason = format!("model-route: {invalid}. {tail}");
                ctx.deny(
                    instance.as_deref(),
                    Some(&tool_name),
                    &args_obj,
                    Some(&route),
                    "deny:reasoning",
                    &reason,
                );
                return Ok(());
            }
            let hint = route
                .get("requestedBriefHint")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !hint.is_empty() && !str_field(&args, "task").unwrap_or_default().contains(hint) {
                let tail = if workflow_retry {
                    format!(" {retry_hint}")
                } else {
                    " Resend the same call with the hint at the end of the task.".to_string()
                };
                let reason = format!(
                    "model-route: {}/{} needs this hint appended to the task verbatim: {}{tail}",
                    target
                        .and_then(|t| t.get("providerInstanceId"))
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                    target
                        .and_then(|t| t.get("model"))
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                    serde_json::to_string(hint)?,
                );
                ctx.deny(
                    instance.as_deref(),
                    Some(&tool_name),
                    &args_obj,
                    Some(&route),
                    "deny:hint",
                    &reason,
                );
                return Ok(());
            }
        }
        if allowed_now {
            let provider = target
                .and_then(|t| t.get("providerInstanceId"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let cfg = crate::policy::router_config()?;
            if let Some(limit) = cfg.instance_limits.get(provider)
                && let Err(error) = crate::admission::reserve(
                    &home,
                    &state_dir()?,
                    provider,
                    *limit,
                    str_field(&args, "task").unwrap_or_default(),
                )
            {
                let reason = format!("model-route: {error}. Retry after a running task finishes.");
                ctx.deny(
                    instance.as_deref(),
                    Some(&tool_name),
                    &args_obj,
                    Some(&route),
                    "deny:capacity",
                    &reason,
                );
                return Ok(());
            }
        }
        let reason_text = validation
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let reason = format!(
            "model-route: {reason_text}.{warning}{choice_warning}{}",
            if !allowed_now {
                format!(
                    " {retry_hint}{}{USER_CHOICE_HINT}",
                    preflight_hint(
                        instance.as_deref().unwrap_or_default(),
                        target
                            .and_then(|t| t.get("providerInstanceId"))
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                        target
                            .and_then(|t| t.get("model"))
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                    )
                )
            } else {
                String::new()
            }
        );
        let decision = if user_choice {
            "override:user-choice"
        } else if allowed_now {
            "allow:delegate"
        } else {
            "deny:delegate"
        };
        record(
            &ctx.harness,
            instance.as_deref(),
            Some(&tool_name),
            &args_obj,
            Some(&route),
            decision,
            &reason,
        );
        ctx.emit(allowed_now, &reason);
        return Ok(());
    }
    let target = route.get("target").cloned().unwrap_or(Value::Null);
    if target.is_null() {
        let errors: Vec<String> = route
            .get("ranking")
            .and_then(Value::as_array)
            .map(|r| {
                r.iter()
                    .filter_map(|e| {
                        e.get("reasoningError")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let blocked: Vec<String> = route
            .get("blocked")
            .and_then(Value::as_array)
            .map(|b| {
                b.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let blocked_text = if blocked.is_empty() {
            "none".to_string()
        } else {
            blocked.join(", ")
        };
        let reason = format!(
            "model-route: no adequate model with enough quota and usable reasoning controls (blocked: {blocked_text}). {}Split the task or do it yourself.{warning}",
            if errors.is_empty() {
                String::new()
            } else {
                errors.join("; ") + ". "
            },
        );
        ctx.deny(
            instance.as_deref(),
            Some(&tool_name),
            &args_obj,
            Some(&route),
            "deny:no-target",
            &reason,
        );
        return Ok(());
    }
    let brief_hint = route.get("briefHint").and_then(Value::as_str);
    let reason = format!(
        "model-route picked {}/{} ({}; {}; {}).{warning} Call the T3 MCP tool delegate_task with target {}, the complete instructions as task{}, and no runtimeMode.{USER_CHOICE_HINT}",
        target
            .get("providerInstanceId")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        target
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        route
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        route
            .get("size")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        route
            .get("difficulty")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        serde_json::to_string(&target)?,
        brief_hint
            .map(|h| format!(
                " followed verbatim by {}",
                serde_json::to_string(h).unwrap_or_default()
            ))
            .unwrap_or_default(),
    );
    let decision = format!(
        "deny:delegate {}/{}",
        target
            .get("providerInstanceId")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        target
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
    );
    ctx.deny(
        instance.as_deref(),
        Some(&tool_name),
        &args_obj,
        Some(&route),
        &decision,
        &reason,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{strip_tags, t3_server};

    #[test]
    fn native_context_requires_matching_harness_ancestry() {
        let input = serde_json::json!({"inT3": true});
        let codex = vec!["codex".into(), "app-server".into()];
        let server = vec!["/test/t3".into(), "serve".into()];
        assert!(!super::in_t3("codex", &[], &input));
        assert!(!super::in_t3("codex", std::slice::from_ref(&codex), &input));
        assert!(!super::in_t3(
            "codex",
            std::slice::from_ref(&server),
            &input
        ));
        assert!(super::in_t3("codex", &[codex, server], &input));
    }

    #[test]
    fn recognizes_node_and_native_t3_servers() {
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(t3_server(&v(&["node", "/x/apps/server/dist/bin.mjs"])));
        assert!(t3_server(&v(&[
            "/h/.t3/runtime/versions/0.1.4/t3",
            "serve"
        ])));
        assert!(!t3_server(&v(&[
            "/h/.t3/runtime/versions/0.1.4/t3",
            "__service-launcher"
        ])));
        assert!(!t3_server(&v(&["/usr/bin/mt3", "serve"])));
    }

    #[test]
    fn strips_matching_pair_and_keeps_unclosed() {
        assert_eq!(
            strip_tags("a<system-reminder>drop</system-reminder>b"),
            "ab"
        );
        assert_eq!(
            strip_tags("a<system-reminder>keep"),
            "a<system-reminder>keep"
        );
        // Mismatched close tag: the opener stays literal, like the .mjs.
        assert_eq!(
            strip_tags("a<task-notification>x</system-reminder>"),
            "a<task-notification>x</system-reminder>"
        );
        assert_eq!(strip_tags("<command-x></command-x>"), "");
        // Non-greedy: the first matching close tag ends the span.
        assert_eq!(
            strip_tags("<command-x>a</command-x>b</command-x>"),
            "b</command-x>"
        );
        // Plain angle brackets are untouched.
        assert_eq!(strip_tags("a < b > c"), "a < b > c");
    }
}
