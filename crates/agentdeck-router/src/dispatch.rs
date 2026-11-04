//! `agentdeck route dispatch`: the router, not the agent, delegates a task.
//!
//! `dispatch FILE` picks the model, calls T3 `delegate_task`, records the
//! attempt in a state file and returns at once; a detached watcher
//! (`dispatch --watch ID`) follows the child. When the child fails on a usage
//! limit the watcher cancels and archives it (T3's auto-resume skips archived
//! threads, so it never continues next to its replacement) and delegates the
//! same task to the next eligible model on another account.
//! `dispatch --status ID` prints the current state.
//!
//! The watcher is detached because harness background commands are killed
//! after 30 min to 2 h, while children often run longer.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::OpenFlags;
use serde_json::{Value, json};

use crate::mcp_call::{Hooks, call_tool, scoped_connection};

pub const USAGE: &str = "usage: agentdeck route dispatch TASK_FILE|- [--title TITLE] [--role implementation|research|review|design|test|general] [--isolation shared|worktree] [--id ID] [--max-hours H]\n       agentdeck route dispatch --status ID\n\nRoutes the task, delegates it through T3 and prints the launched attempt with its dispatch ID.\nA detached watcher follows the child: after a usage-limit failure it cancels and archives that\nchild and re-dispatches to another account (at most 4 attempts). T3 still wakes the parent when\neach child ends; then read `--status ID` (status running means a replacement is already working).";

const MAX_ATTEMPTS: usize = 4;
const POLL: Duration = Duration::from_secs(30);
const MAX_STATUS_ERRORS: u32 = 20;
const ACTIVE: &[&str] = &["queued", "running", "waiting"];

#[derive(Debug, Default, PartialEq)]
struct Args {
    file: String,
    title: Option<String>,
    role: Option<String>,
    isolation: Option<String>,
    id: Option<String>,
    max_hours: f64,
}

fn parse(argv: &[String]) -> Result<Args, String> {
    let mut a = Args {
        max_hours: 24.0,
        ..Default::default()
    };
    let mut it = argv.iter();
    while let Some(arg) = it.next() {
        let mut value = |name: &str| {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match arg.as_str() {
            "--title" => a.title = Some(value("--title")?),
            "--role" => {
                let role = value("--role")?;
                if !crate::workflow::ROLES.contains(&role.as_str()) {
                    return Err(format!(
                        "--role must be one of {}",
                        crate::workflow::ROLES.join("|")
                    ));
                }
                a.role = Some(role);
            }
            "--isolation" => {
                let v = value("--isolation")?;
                if v != "shared" && v != "worktree" {
                    return Err("--isolation must be shared or worktree".into());
                }
                a.isolation = Some(v);
            }
            "--id" => {
                let v = value("--id")?;
                if !valid_id(&v) {
                    return Err("--id: 1-80 of A-Z a-z 0-9 . _ -".into());
                }
                a.id = Some(v);
            }
            "--max-hours" => {
                a.max_hours = value("--max-hours")?
                    .parse()
                    .ok()
                    .filter(|h: &f64| *h > 0.0)
                    .ok_or("--max-hours must be a positive number")?
            }
            s if s.starts_with("--") => return Err(format!("unknown flag {s}")),
            s if a.file.is_empty() => a.file = s.to_string(),
            _ => return Err("only one TASK_FILE".into()),
        }
    }
    if a.file.is_empty() {
        return Err("TASK_FILE is required".into());
    }
    Ok(a)
}

fn valid_id(id: &str) -> bool {
    (1..=80).contains(&id.len())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && !id.starts_with('.')
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn dispatch_dir() -> PathBuf {
    crate::workflow::workflows_dir()
        .parent()
        .map(|p| p.join("dispatch"))
        .unwrap_or_else(|| PathBuf::from("dispatch"))
}

fn state_path(id: &str) -> anyhow::Result<PathBuf> {
    if !valid_id(id) {
        anyhow::bail!("invalid dispatch ID");
    }
    Ok(dispatch_dir().join(format!("{id}.json")))
}

fn load(id: &str) -> anyhow::Result<Value> {
    let path = state_path(id)?;
    let text = std::fs::read_to_string(&path)
        .map_err(|_| anyhow::anyhow!("no dispatch {id} ({})", path.display()))?;
    Ok(serde_json::from_str(&text)?)
}

/// Atomic replace, so `--status` never reads a half-written file.
fn save(state: &mut Value) -> anyhow::Result<()> {
    state["updatedAt"] = json!(now_secs());
    let path = state_path(state["id"].as_str().unwrap_or_default())?;
    std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")))?;
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec_pretty(state)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Reset time (or null) when the latest run of `thread_id` failed on a usage
/// limit. T3 records the class on the run's `error` item at once; its
/// `limitRecovery` metadata is only armed later by a sweep.
pub fn usage_limit_reset(db: &Path, thread_id: &str) -> Option<Value> {
    let conn = rusqlite::Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    conn.execute_batch("PRAGMA busy_timeout=2000").ok()?;
    conn.query_row(
        "SELECT json_extract(i.payload_json, '$.failure.resetAt')
           FROM orchestration_v2_projection_turn_items i
          WHERE i.thread_id = ?1 AND i.type = 'error'
            AND json_extract(i.payload_json, '$.failure.class') = 'usage_limit'
            AND i.run_id = (SELECT run_id FROM orchestration_v2_projection_runs
                             WHERE thread_id = ?1 ORDER BY ordinal DESC LIMIT 1)
          LIMIT 1",
        [thread_id],
        |row| row.get::<_, Option<String>>(0),
    )
    .ok()
    .map(|reset| reset.map(Value::from).unwrap_or(Value::Null))
}

fn settled(s: &Value) -> bool {
    let status = s.get("status").and_then(Value::as_str).unwrap_or("");
    let needs_input = s.get("hasPendingQuestion").and_then(Value::as_bool) == Some(true)
        || s.get("pendingApproval").is_some_and(|p| !p.is_null());
    needs_input
        || (!ACTIVE.contains(&status)
            && s.get("workState").and_then(Value::as_str) == Some("result_available")
            && s.get("hasPendingChildRuns").and_then(Value::as_bool) == Some(false))
}

async fn tool(name: &str, payload: Value) -> anyhow::Result<Value> {
    // Re-resolved per call: the session bridge may hand out a fresh token.
    let r = call_tool(name, payload, scoped_connection()?, &Hooks::live()).await?;
    if r.get("isError").and_then(Value::as_bool) == Some(true) {
        let text = r
            .pointer("/content/0/text")
            .and_then(Value::as_str)
            .unwrap_or("T3 tool error");
        anyhow::bail!("{name}: {text}");
    }
    Ok(r.get("structuredContent").cloned().unwrap_or(r))
}

fn str_of<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

/// Route (skipping excluded accounts) and delegate the next attempt. Returns
/// false when no model is eligible; the state then holds `no_model`.
async fn launch(state: &mut Value) -> anyhow::Result<bool> {
    let result = launch_inner(state).await;
    save_launch_result(state, result)
}

fn save_launch_result(state: &mut Value, result: anyhow::Result<bool>) -> anyhow::Result<bool> {
    if let Err(error) = &result {
        state["status"] = json!("failed");
        state["reason"] = json!(format!("dispatch failed: {error:#}"));
    }
    save(state)?;
    result
}

async fn launch_inner(state: &mut Value) -> anyhow::Result<bool> {
    let task = str_of(state, "task").to_string();
    let source = str_of(state, "source").to_string();
    let exclude: Vec<String> = state["excluded"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let route = crate::workflow::run_router_excluding(&task, &source, &exclude)?;
    let Some(target) = route.get("target").filter(|t| !t.is_null()).cloned() else {
        state["status"] = json!("no_model");
        state["reason"] = json!("no eligible model with enough quota");
        state["blocked"] = route.get("blocked").cloned().unwrap_or(Value::Null);
        return Ok(false);
    };
    let mut text = task;
    if let Some(hint) = route.get("briefHint").and_then(Value::as_str)
        && !text.contains(hint)
    {
        text = format!("{text}\n\n{hint}");
    }
    let n = state["attempts"].as_array().map(Vec::len).unwrap_or(0) + 1;
    let mut payload = json!({
        "task": text,
        "target": target,
        "mode": "async",
        "clientRequestId": format!("route-dispatch-{}-{n}", str_of(state, "id")),
    });
    for k in ["title", "role", "isolation"] {
        if let Some(v) = state.get(k).filter(|v| v.is_string()) {
            payload[k] = v.clone();
        }
    }
    let started = tool("delegate_task", payload).await?;
    if started
        .get("taskId")
        .and_then(Value::as_str)
        .is_none_or(|s| s.trim().is_empty())
    {
        anyhow::bail!(
            "T3 tool-call outcome unknown: response has no taskId. Do not retry delegation: recover its taskId from parent activity first."
        );
    }
    let attempt = json!({
        "taskId": started.get("taskId"),
        "childThreadId": started.get("childThreadId"),
        "link": started.get("link"),
        "target": target,
        "startedAt": now_secs(),
    });
    state["attempts"]
        .as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("corrupt state"))?
        .push(attempt);
    state["status"] = json!("running");
    Ok(true)
}

fn current(state: &Value) -> Value {
    state["attempts"]
        .as_array()
        .and_then(|a| a.last())
        .cloned()
        .unwrap_or(Value::Null)
}

/// What the parent sees: everything but the full task text.
fn summary(state: &Value) -> Value {
    let mut s = state.clone();
    if let Some(o) = s.as_object_mut() {
        o.remove("task");
        o.insert("current".into(), current(state));
    }
    s
}

fn spawn_watcher(id: &str) -> anyhow::Result<u32> {
    use std::os::unix::process::CommandExt;
    let log = std::fs::File::create(dispatch_dir().join(format!("{id}.log")))?;
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.args(["route", "dispatch", "--watch", id])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(log);
    // SAFETY: setsid is async-signal-safe; the watcher outlives this call
    // and must not die with the harness's process group.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    Ok(cmd.spawn()?.id())
}

pub async fn run(argv: &[String]) -> i32 {
    let result = match argv.first().map(String::as_str) {
        Some("--help") => {
            println!("{USAGE}");
            return 0;
        }
        Some("--status") if argv.len() == 2 => load(&argv[1]).map(|s| {
            println!(
                "{}",
                serde_json::to_string_pretty(&summary(&s)).unwrap_or_default()
            );
            0
        }),
        Some("--watch") if argv.len() == 2 => watch(&argv[1]).await.map(|_| 0),
        _ => match parse(argv) {
            Ok(a) => start(a).await,
            Err(e) => {
                eprintln!("{e}\n{USAGE}");
                return 2;
            }
        },
    };
    result.unwrap_or_else(|e| {
        eprintln!("dispatch: {e:#}");
        1
    })
}

async fn start(args: Args) -> anyhow::Result<i32> {
    let task = if args.file == "-" {
        std::io::read_to_string(std::io::stdin())?
    } else {
        std::fs::read_to_string(&args.file)?
    };
    if task.trim().is_empty() {
        anyhow::bail!("the task is empty");
    }
    let caps = tool("orchestrator_capabilities", json!({})).await?;
    let id = args
        .id
        .clone()
        .unwrap_or_else(|| format!("{}-{}", (now_secs() * 1000.0) as u64, std::process::id()));
    if state_path(&id)?.exists() {
        anyhow::bail!("dispatch {id} already exists; read it with --status {id}");
    }
    let mut state = json!({
        "id": id,
        "parentThreadId": caps.get("parentThreadId"),
        "source": str_of(&caps, "inheritedProviderInstanceId"),
        "task": task,
        "title": args.title,
        "role": args.role,
        "isolation": args.isolation,
        "deadline": now_secs() + args.max_hours * 3600.0,
        "status": "starting",
        "excluded": [],
        "attempts": [],
    });
    save(&mut state)?;
    if !launch(&mut state).await? {
        println!("{}", serde_json::to_string_pretty(&summary(&state))?);
        return Ok(1);
    }
    state["watcherPid"] = json!(spawn_watcher(&id)?);
    state["next"] = json!(format!(
        "T3 wakes you when the child ends; then run `agentdeck route dispatch --status {id}`. Status running means a replacement after a usage limit is already working."
    ));
    save(&mut state)?;
    println!("{}", serde_json::to_string_pretty(&summary(&state))?);
    Ok(0)
}

async fn watch(id: &str) -> anyhow::Result<()> {
    let db = crate::home_dir()?.join(".t3/userdata/statev2.sqlite");
    let mut errors = 0;
    loop {
        let mut state = load(id)?;
        if str_of(&state, "status") != "running" {
            return Ok(());
        }
        if now_secs() > state["deadline"].as_f64().unwrap_or(0.0) {
            state["status"] = json!("timeout");
            state["reason"] = json!("watch deadline passed; the child may still run");
            return save(&mut state);
        }
        tokio::time::sleep(POLL).await;
        let cur = current(&state);
        let task_id = str_of(&cur, "taskId").to_string();
        let status = match tool("task_status", json!({"taskId": task_id})).await {
            Ok(s) => {
                errors = 0;
                s
            }
            Err(e) => {
                errors += 1;
                eprintln!("dispatch: task_status {task_id}: {e:#}");
                if errors >= MAX_STATUS_ERRORS {
                    state["status"] = json!("lost");
                    state["reason"] = json!(format!("task_status kept failing: {e:#}"));
                    return save(&mut state);
                }
                continue;
            }
        };
        if !settled(&status) {
            continue;
        }
        let child = str_of(&cur, "childThreadId").to_string();
        let state_name = str_of(&status, "status").to_string();
        let last = state["attempts"]
            .as_array_mut()
            .and_then(|a| a.last_mut())
            .ok_or_else(|| anyhow::anyhow!("corrupt state"))?;
        last["status"] = json!(state_name);
        if state_name == "failed"
            && let Some(reset) = usage_limit_reset(&db, &child)
        {
            last["usageLimit"] = json!(true);
            last["resetAt"] = reset;
            last["cancelled"] = json!(
                tool(
                    "task_cancel",
                    json!({"taskId": task_id,
                    "reason": "usage limit; the router re-dispatched this task"})
                )
                .await
                .is_ok()
            );
            last["archived"] = json!(
                tool(
                    "t3_thread_organize",
                    json!({"action": "archive", "threadId": child})
                )
                .await
                .is_ok()
            );
            let provider = str_of(&cur["target"], "providerInstanceId").to_string();
            state["excluded"]
                .as_array_mut()
                .ok_or_else(|| anyhow::anyhow!("corrupt state"))?
                .push(json!(provider));
            let n = state["attempts"].as_array().map(Vec::len).unwrap_or(0);
            if n >= MAX_ATTEMPTS {
                state["status"] = json!("usage_limits");
                state["reason"] = json!(format!("{MAX_ATTEMPTS} attempts hit usage limits"));
                return save(&mut state);
            }
            eprintln!("dispatch: {provider} hit a usage limit; re-routing");
            match launch(&mut state).await {
                Ok(true) => {
                    save(&mut state)?;
                    continue;
                }
                Ok(false) => return save(&mut state),
                Err(e) => {
                    state["status"] = json!("failed");
                    state["reason"] = json!(format!("re-dispatch failed: {e:#}"));
                    return save(&mut state);
                }
            }
        }
        let summary_text = status
            .get("summary")
            .filter(|s| !s.is_null())
            .or(status.get("latestTerminalSummary"))
            .cloned()
            .unwrap_or(Value::Null);
        let blocked = state_name == "completed"
            && summary_text
                .as_str()
                .is_some_and(|s| s.trim_start().starts_with("BLOCKED:"));
        state["status"] = json!(if blocked {
            "blocked"
        } else {
            state_name.as_str()
        });
        state["summary"] = summary_text;
        state["needsInput"] = json!(
            status.get("hasPendingQuestion").and_then(Value::as_bool) == Some(true)
                || status.get("pendingApproval").is_some_and(|p| !p.is_null())
        );
        return save(&mut state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_flags_and_rejects_bad_input() {
        let a = parse(&v(&[
            "t.txt",
            "--title",
            "T",
            "--isolation",
            "worktree",
            "--id",
            "lr1",
        ]))
        .unwrap();
        assert_eq!(a.file, "t.txt");
        assert_eq!(a.title.as_deref(), Some("T"));
        assert_eq!(a.isolation.as_deref(), Some("worktree"));
        assert_eq!(a.id.as_deref(), Some("lr1"));
        assert_eq!(a.max_hours, 24.0);
        assert!(parse(&v(&[])).is_err());
        assert!(parse(&v(&["a", "b"])).is_err());
        assert!(parse(&v(&["a", "--isolation", "x"])).is_err());
        assert!(parse(&v(&["a", "--role", "implement"])).is_err());
        assert!(parse(&v(&["a", "--role", "boss"])).is_err());
        assert_eq!(
            parse(&v(&["a", "--role", "implementation"]))
                .unwrap()
                .role
                .as_deref(),
            Some("implementation")
        );
        assert!(parse(&v(&["a", "--max-hours", "0"])).is_err());
        for bad in ["../x", ".hidden", "a/b", ""] {
            assert!(parse(&v(&["a", "--id", bad])).is_err(), "{bad}");
        }
    }

    #[test]
    fn failed_initial_launch_is_saved_without_a_confirmed_attempt() {
        let id = format!("test-launch-failure-{}", std::process::id());
        let mut state = json!({"id": id, "status": "starting", "attempts": []});
        let result = save_launch_result(
            &mut state,
            Err(anyhow::anyhow!(
                "T3 MCP rejected request (-32602): invalid role"
            )),
        );
        assert!(result.is_err());
        let saved = load(&id).unwrap();
        std::fs::remove_file(state_path(&id).unwrap()).unwrap();
        assert_eq!(saved["status"], "failed");
        assert!(saved["reason"].as_str().unwrap().contains("-32602"));
        assert_eq!(saved["attempts"], json!([]));
    }

    #[test]
    fn settles_only_on_final_results_or_input_requests() {
        let running =
            json!({"status": "running", "workState": "working", "hasPendingChildRuns": false});
        let done = json!({"status": "completed", "workState": "result_available", "hasPendingChildRuns": false});
        let nested = json!({"status": "completed", "workState": "result_available", "hasPendingChildRuns": true});
        let asking =
            json!({"status": "running", "workState": "working", "hasPendingQuestion": true});
        assert!(!settled(&running));
        assert!(settled(&done));
        assert!(!settled(&nested));
        assert!(settled(&asking));
    }

    #[test]
    fn detects_usage_limit_on_latest_run_only() {
        let dir = std::env::temp_dir().join(format!("dispatch-limit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("state.sqlite");
        let _ = std::fs::remove_file(&db);
        let c = rusqlite::Connection::open(&db).unwrap();
        c.execute_batch(
            "CREATE TABLE orchestration_v2_projection_runs (run_id TEXT, thread_id TEXT, ordinal INTEGER);
             CREATE TABLE orchestration_v2_projection_turn_items (thread_id TEXT, run_id TEXT, type TEXT, payload_json TEXT);
             INSERT INTO orchestration_v2_projection_runs VALUES ('r1','t',1),('r1','u',1),('r2','u',2);
             INSERT INTO orchestration_v2_projection_turn_items VALUES
               ('t','r1','error','{\"failure\":{\"class\":\"usage_limit\",\"resetAt\":\"2026-11-05T16:00:00.000Z\"}}'),
               ('u','r1','error','{\"failure\":{\"class\":\"usage_limit\"}}'),
               ('u','r2','error','{\"failure\":{\"class\":\"provider_error\"}}');",
        )
        .unwrap();
        drop(c);
        assert_eq!(
            usage_limit_reset(&db, "t"),
            Some(Value::from("2026-11-05T16:00:00.000Z"))
        );
        // An older run's limit does not count once a later run failed otherwise.
        assert_eq!(usage_limit_reset(&db, "u"), None);
        assert_eq!(usage_limit_reset(&db, "missing"), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
