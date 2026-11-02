//! Opt-in automatic evaluation: `record` spawns a detached eval, budgets bind.
//! Drives the built binary with fake HOME dirs and a mock judge (or FIFO to
//! hold the judge open); never touches real providers or config.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_agentdeck"))
}

fn fresh(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "agentdeck-autoeval-{}-{tag}-{}",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn router(dir: &Path, workflow_eval: Option<serde_json::Value>) -> PathBuf {
    let mut v = serde_json::json!({"pool": [], "accounts": {}, "version": 1,
        "decisionModel": "t/judge"});
    if let Some(we) = workflow_eval {
        v["workflowEval"] = we;
    }
    let path = dir.join("router.json");
    std::fs::write(&path, v.to_string()).unwrap();
    path
}

fn mock_jev(dir: &Path) -> PathBuf {
    let path = dir.join("mock.json");
    std::fs::write(
        &path,
        serde_json::json!({"answers": {"outcome": {"score": 2, "confidence": 0.8}},
            "usage": {}})
        .to_string(),
    )
    .unwrap();
    path
}

/// A prepared single-step state, as `prepare` would checkpoint it.
fn prepared_state(home: &Path, thread: &str) -> PathBuf {
    let runs = home.join("model-route/workflows");
    std::fs::create_dir_all(&runs).unwrap();
    let file = runs.join("11111111-2222-3333-4444-555555555555.json");
    std::fs::write(
        &file,
        serde_json::json!({
            "version": 1, "runId": "11111111-2222-3333-4444-555555555555",
            "parentThreadId": thread, "stopped": false,
            "plan": {"title": "t", "steps": [{"id": "a", "task": "do thing",
                "needs": [], "role": "implementation"}]},
            "progress": [{"id": "a", "state": "prepared",
                "payload": {"task": "do thing",
                    "target": {"providerInstanceId": "codex", "model": "gpt-6.1-sol"},
                    "role": "implementation", "title": "t: a",
                    "mode": "async", "clientRequestId": "x"},
                "route": {"target": {"providerInstanceId": "codex",
                    "model": "gpt-6.1-sol"}}}]
        })
        .to_string(),
    )
    .unwrap();
    file
}

fn completed_response() -> String {
    serde_json::json!({"taskId": "auto-t1", "status": "completed",
        "workState": "result_available", "hasPendingChildRuns": false,
        "providerInstanceId": "codex", "model": "gpt-6.1-sol",
        "latestTerminalSummary": "did the thing"})
    .to_string()
}

fn run_record(home: &Path, router: &Path, jev: &Path, state: &Path, thread: &str) -> Child {
    let mut child = Command::new(bin())
        .args([
            "route",
            "workflow",
            "record",
            state.to_str().unwrap(),
            "a",
            "--thread",
            thread,
        ])
        .env("XDG_STATE_HOME", home)
        .env("MODEL_ROUTE_ROUTER_JSON", router)
        .env("AGENTDECK_ROUTE_MOCK_JEV", jev)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(completed_response().as_bytes())
        .unwrap();
    child
}

fn wait_exit(mut child: Child, secs: u64) -> std::process::Output {
    let until = Instant::now() + Duration::from_secs(secs);
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        assert!(Instant::now() < until, "record did not exit promptly");
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn wait_for(secs: u64, mut cond: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(secs);
    while !cond() {
        assert!(Instant::now() < until, "condition not met in time");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn ledger_count(home: &Path) -> Option<u64> {
    let dir = home.join("model-route/workflow-eval-budget");
    let entry = std::fs::read_dir(dir).ok()?.next()?.ok()?;
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(entry.path()).ok()?).ok()?;
    v.get("count")?.as_u64()
}

fn journal_mentions(home: &Path, task: &str) -> bool {
    let text = std::fs::read_to_string(home.join("model-route/outcomes.jsonl")).unwrap_or_default();
    text.contains(task)
}

fn state_eval_status(home: &Path) -> Option<String> {
    let text =
        std::fs::read(home.join("model-route/workflows/11111111-2222-3333-4444-555555555555.json"))
            .ok()?;
    serde_json::from_slice::<serde_json::Value>(&text)
        .ok()?
        .pointer("/progress/0/eval/status")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

#[test]
fn record_without_auto_is_byte_identical() {
    let dir = fresh("off");
    let home = dir.join("home");
    let router = router(&dir, None);
    let mock = mock_jev(&dir);
    let state = prepared_state(&home, "thread-1");
    let child = run_record(&home, &router, &mock, &state, "thread-1");
    let out = wait_exit(child, 20);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        v,
        serde_json::json!({"step": "a", "state": "completed", "taskId": "auto-t1"}),
        "default record output must stay byte-identical (no autoEval key)"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn record_with_auto_spawns_detached_eval() {
    let dir = fresh("on");
    let home = dir.join("home");
    let router = router(
        &dir,
        Some(serde_json::json!({"auto": true, "sample": 1, "maxPerDay": 20})),
    );
    let mock = mock_jev(&dir);
    let state = prepared_state(&home, "thread-1");
    let child = run_record(&home, &router, &mock, &state, "thread-1");
    let out = wait_exit(child, 20);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["autoEval"], "started");
    // The detached child completes the eval on its own: state marker,
    // journal line and budget ledger appear after record already exited.
    wait_for(20, || {
        state_eval_status(&home).as_deref() == Some("evaluated")
    });
    assert!(journal_mentions(&home, "auto-t1"));
    let log = home.join("model-route/workflows/11111111-2222-3333-4444-555555555555.eval-a.log");
    assert!(log.exists(), "detached child must log next to the state");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&log).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let out = Command::new(bin())
        .args(["route", "workflow", "eval-budget"])
        .env("XDG_STATE_HOME", &home)
        .env("MODEL_ROUTE_ROUTER_JSON", &router)
        .output()
        .unwrap();
    let b: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(b["count"], serde_json::json!(1));
    assert_eq!(b["maxPerDay"], serde_json::json!(20));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn record_returns_before_eval_child_finishes() {
    let dir = fresh("fifo");
    let home = dir.join("home");
    let router = router(&dir, Some(serde_json::json!({"auto": true})));
    let fifo = dir.join("mock-fifo");
    assert!(
        Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    // O_RDWR never blocks: the child can open its read side while we hold
    // the write side closed, and EOF arrives only when we drop it.
    let fifo_hold = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&fifo)
        .unwrap();
    let state = prepared_state(&home, "thread-1");
    let child = run_record(&home, &router, &fifo, &state, "thread-1");
    // record must exit while the judge is still blocked (nothing written yet).
    let out = wait_exit(child, 20);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()["autoEval"],
        "started"
    );
    // The child reserved its budget slot but produced no outcome yet.
    wait_for(20, || ledger_count(&home) == Some(1));
    assert!(
        !journal_mentions(&home, "auto-t1"),
        "record returned before the eval finished"
    );
    // Release the judge: the writer hold is the only other end, so EOF
    // follows this write once the handle drops.
    fifo_hold
        .try_clone()
        .unwrap()
        .write_all(
            serde_json::json!({"answers": {"outcome": {"score": 2, "confidence": 0.8}},
                "usage": {}})
            .to_string()
            .as_bytes(),
        )
        .unwrap();
    drop(fifo_hold);
    wait_for(20, || {
        state_eval_status(&home).as_deref() == Some("evaluated")
    });
    assert!(journal_mentions(&home, "auto-t1"));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn invalid_config_skips_with_reason_and_spawns_nothing() {
    let dir = fresh("invalid");
    let home = dir.join("home");
    let router = router(
        &dir,
        Some(serde_json::json!({"auto": true, "maxUsdPerDay": 1.0})),
    );
    let mock = mock_jev(&dir);
    let state = prepared_state(&home, "thread-1");
    let child = run_record(&home, &router, &mock, &state, "thread-1");
    let out = wait_exit(child, 20);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let auto = v["autoEval"].as_str().unwrap_or("");
    assert!(
        auto.starts_with("skipped:") && auto.contains("assumeUsdPerEval"),
        "got {v}"
    );
    assert!(
        !home
            .join("model-route/workflows/11111111-2222-3333-4444-555555555555.eval-a.log")
            .exists(),
        "invalid config must not spawn a child"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}
