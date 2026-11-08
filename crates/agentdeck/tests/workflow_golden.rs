//! Golden-file test: the Rust `route workflow` state machine must match the
//! committed snapshot in `tests/golden/workflow/` (captured once from the
//! legacy Node `workflow.mjs`; `UPDATE_GOLDEN=1` rewrites it from the Rust
//! output) on the same scripted ops. CLI storage (private dir, permissions,
//! thread binding) is tested against the built binary on temp dirs.
//! No Node needed.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/workflow/state-machine.json")
}

fn t_target() -> serde_json::Value {
    serde_json::json!({"providerInstanceId": "codex", "model": "gpt-6.1-sol", "options": {"reasoningEffort": "high"}})
}

fn diamond() -> serde_json::Value {
    serde_json::json!({"title": "Diamond", "concurrency": 2, "steps": [
        {"id": "a", "task": "root"},
        {"id": "b", "task": "left", "needs": ["a"], "role": "research"},
        {"id": "c", "task": "right", "needs": ["a"]},
        {"id": "d", "task": "join", "needs": ["b", "c"], "role": "review"},
        {"id": "indep", "task": "solo"},
    ]})
}

/// Scripted ops; both sides execute the same list with the same route stubs.
fn ops() -> Vec<serde_json::Value> {
    let t = t_target();
    let launched = |id: &str| {
        serde_json::json!({"taskId": id, "status": "running", "workState": "working", "hasPendingChildRuns": false,
            "providerInstanceId": "codex", "model": "gpt-6.1-sol"})
    };
    let done = |id: &str, summary: &str| {
        serde_json::json!({"taskId": id, "status": "completed", "workState": "result_available", "hasPendingChildRuns": false,
            "providerInstanceId": "codex", "model": "gpt-6.1-sol", "latestTerminalSummary": summary})
    };
    vec![
        serde_json::json!({"op": "validate", "plan": diamond()}),
        serde_json::json!({"op": "validate", "plan": {"title": "t", "steps": [{"id": "a", "task": "x", "needs": ["b"]}, {"id": "b", "task": "y", "needs": ["a"]}]}}),
        serde_json::json!({"op": "validate", "plan": {"title": "t", "steps": [{"id": "a", "task": "x"}, {"id": "a", "task": "y"}]}}),
        serde_json::json!({"op": "validate", "plan": {"title": "t", "steps": [{"id": "a", "task": "x", "needs": ["zz"]}]}}),
        serde_json::json!({"op": "validate", "plan": {"title": "t", "concurrency": 0, "steps": [{"id": "a", "task": "x"}]}}),
        serde_json::json!({"op": "validate", "plan": {"title": "t", "steps": [{"id": "__proto__", "task": "x"}]}}),
        serde_json::json!({"op": "validate", "plan": {"title": "t", "steps": [{"id": "a", "task": "x", "role": "boss"}]}}),
        serde_json::json!({"op": "validate", "plan": {"title": "t", "script": "rm -rf", "steps": [{"id": "a", "task": "x"}]}}),
        serde_json::json!({"op": "new", "plan": diamond(), "thread": "thread-1"}),
        serde_json::json!({"op": "next"}),
        serde_json::json!({"op": "prepare", "step": "a", "source": "claudeAgent", "route": {"target": t, "jevConfidence": 0.3, "reason": "r", "ranking": ["dropped"]}}),
        serde_json::json!({"op": "prepare", "step": "a", "source": "claudeAgent", "route": {"target": t}}),
        serde_json::json!({"op": "prepare", "step": "b", "source": "claudeAgent", "route": {"target": t}}),
        serde_json::json!({"op": "release", "step": "a", "notDispatched": false}),
        serde_json::json!({"op": "record", "step": "b", "response": launched("t-b")}),
        serde_json::json!({"op": "record", "step": "a", "response": {"taskId": "t-a", "status": "running", "workState": "working", "hasPendingChildRuns": false}}),
        serde_json::json!({"op": "record", "step": "a", "response": launched("t-a")}),
        serde_json::json!({"op": "next"}),
        serde_json::json!({"op": "record", "step": "a", "response": done("t-a", "ROOT RESULT")}),
        serde_json::json!({"op": "record", "step": "a", "response": done("t-a", "ROOT RESULT")}),
        serde_json::json!({"op": "next"}),
        serde_json::json!({"op": "prepare", "step": "b", "source": "claudeAgent", "route": {"target": t}}),
        serde_json::json!({"op": "prepare", "step": "c", "source": "claudeAgent", "route": {"target": t}}),
        serde_json::json!({"op": "next"}),
        serde_json::json!({"op": "prepare", "step": "d", "source": "claudeAgent", "route": {"target": t}}),
        serde_json::json!({"op": "record", "step": "b", "response": launched("t-b")}),
        serde_json::json!({"op": "record", "step": "c", "response": launched("t-c")}),
        serde_json::json!({"op": "record", "step": "b", "response": done("t-b", "B")}),
        serde_json::json!({"op": "prepare", "step": "d", "source": "claudeAgent", "route": {"target": t}}),
        serde_json::json!({"op": "record", "step": "c", "response": done("t-c", "C")}),
        serde_json::json!({"op": "record", "step": "d", "response": launched("t-d")}),
        serde_json::json!({"op": "record", "step": "d", "response": done("t-d", "\n  BLOCKED: missing key.")}),
        serde_json::json!({"op": "next"}),
        serde_json::json!({"op": "record", "step": "d", "response": done("t-d", "later result")}),
        serde_json::json!({"op": "stop"}),
        serde_json::json!({"op": "next"}),
        serde_json::json!({"op": "prepare", "step": "indep", "source": "s", "route": {"target": t}}),
        serde_json::json!({"op": "resume"}),
        serde_json::json!({"op": "next"}),
        serde_json::json!({"op": "release", "step": "indep", "notDispatched": true}),
        serde_json::json!({"op": "prepare", "step": "indep", "source": "s", "route": {"target": t}}),
        serde_json::json!({"op": "new", "plan": {"title": "h", "steps": [{"id": "u", "task": "build ui"}]}, "thread": "th"}),
        serde_json::json!({"op": "prepare", "step": "u", "source": "s", "route": {"target": t, "briefHint": "Delegated-agent rule: never ask questions.", "reason": "r"}}),
        serde_json::json!({"op": "new", "plan": {"title": "n", "steps": [{"id": "a", "task": "t"}]}, "thread": "th"}),
        serde_json::json!({"op": "prepare", "step": "a", "source": "s", "route": {"target": null, "reason": "r"}}),
        serde_json::json!({"op": "new", "plan": {"title": "w", "steps": [{"id": "x", "task": "x"}, {"id": "y", "task": "y"}, {"id": "z", "task": "z"}]}, "thread": "th"}),
        serde_json::json!({"op": "prepare", "step": "x", "source": "s", "route": {"target": t}}),
        serde_json::json!({"op": "prepare", "step": "y", "source": "s", "route": {"target": t}}),
        serde_json::json!({"op": "prepare", "step": "z", "source": "s", "route": {"target": t}}),
        serde_json::json!({"op": "new", "plan": {"title": "PR handoff", "steps": [{"id": "pr", "task": "Push the fix", "role": "implementation"}]}, "thread": "th"}),
        serde_json::json!({"op": "prepare", "step": "pr", "source": "s", "route": {"target": t}}),
        serde_json::json!({"op": "record", "step": "pr", "response": done("t-pr", "PR https://github.com/example/repo/pull/1, head abc123, checks and reviews pending")}),
        serde_json::json!({"op": "next"}),
        // Title/task length limits (ported from the legacy workflow.test.mjs).
        serde_json::json!({"op": "validate", "plan": {"title": "x".repeat(509), "steps": [{"id": "a", "task": "x"}]}}),
        serde_json::json!({"op": "validate", "plan": {"title": "x".repeat(509), "steps": [{"id": "ab", "task": "x"}]}}),
        serde_json::json!({"op": "validate", "plan": {"title": "x".repeat(510), "steps": [{"id": "a", "task": "x"}]}}),
        serde_json::json!({"op": "validate", "plan": {"title": "t", "steps": [{"id": "a", "task": "x".repeat(120_000)}]}}),
        serde_json::json!({"op": "validate", "plan": {"title": "t", "steps": [{"id": "a", "task": "x".repeat(120_001)}]}}),
        // A confirmed non-dispatch releases a prepared step back to pending.
        serde_json::json!({"op": "new", "plan": {"title": "rel", "steps": [{"id": "x", "task": "x"}]}, "thread": "th"}),
        serde_json::json!({"op": "prepare", "step": "x", "source": "s", "route": {"target": t}}),
        serde_json::json!({"op": "release", "step": "x", "notDispatched": true}),
        serde_json::json!({"op": "next"}),
    ]
}

fn is_uuid_at(b: &[u8], i: usize) -> bool {
    let hex = |j: usize| b[i + j].is_ascii_hexdigit();
    if b.len() < i + 36 {
        return false;
    }
    (0..8).all(hex)
        && b[i + 8] == b'-'
        && (9..13).all(hex)
        && b[i + 13] == b'-'
        && (14..18).all(hex)
        && b[i + 18] == b'-'
        && (19..23).all(hex)
        && b[i + 23] == b'-'
        && (24..36).all(hex)
}

/// Mask every runId so run-to-run randomness compares equal.
fn normalize(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < b.len() {
        if is_uuid_at(b, i) {
            out.push_str("RUN");
            i += 36;
        } else {
            out.push(b[i] as char);
            i += 1;
        }
    }
    out
}

fn rust_run() -> Vec<serde_json::Value> {
    use agentdeck_router::workflow::*;
    let mut state: Option<State> = None;
    ops()
        .iter()
        .map(|op| {
            let out: Result<serde_json::Value, _> = (|| match op["op"].as_str().unwrap() {
                "validate" => validate_plan(&op["plan"])
                    .map(|p| serde_json::to_value(p).unwrap())
                    .map_err(|e| (e.usage, e.message)),
                "new" => {
                    let s = new_state(&op["plan"], op["thread"].as_str().unwrap())
                        .map_err(|e| (e.usage, e.message))?;
                    state = Some(s.clone());
                    Ok(serde_json::to_value(s).unwrap())
                }
                "next" => Ok(next(state.as_ref().unwrap())),
                "prepare" => {
                    let s = state.as_mut().unwrap();
                    let route = op["route"].clone();
                    prepare(
                        s,
                        op["step"].as_str().unwrap(),
                        op["source"].as_str().unwrap_or(""),
                        move |_, _| {
                            if route.is_null() {
                                return Ok(serde_json::Value::Null);
                            }
                            Ok(route.clone())
                        },
                    )
                    .map_err(|e| (e.usage, e.message))
                }
                "release" => {
                    let s = state.as_mut().unwrap();
                    release(
                        s,
                        op["step"].as_str().unwrap(),
                        op["notDispatched"].as_bool().unwrap(),
                    )
                    .map_err(|e| (e.usage, e.message))
                }
                "record" => {
                    let s = state.as_mut().unwrap();
                    record(s, op["step"].as_str().unwrap(), &op["response"])
                        .map_err(|e| (e.usage, e.message))
                }
                "stop" => Ok(stop(state.as_mut().unwrap())),
                "resume" => {
                    state.as_mut().unwrap().stopped = false;
                    Ok(next(state.as_ref().unwrap()))
                }
                _ => unreachable!(),
            })();
            match out {
                Ok(v) => serde_json::json!({"ok": true, "value": v}),
                Err((usage, message)) => {
                    serde_json::json!({"ok": false, "usage": usage, "message": message})
                }
            }
        })
        .collect()
}

#[test]
fn state_machine_matches_golden() {
    let rust = rust_run();
    let ops_list = ops();
    let pr_record = ops_list.iter().position(|op| op["op"] == "record" && op["step"] == "pr").unwrap();
    assert_eq!(rust[pr_record]["value"]["state"], "completed");
    assert_eq!(rust[pr_record + 1]["value"]["complete"], true);
    let r = normalize(&serde_json::to_string(&rust).unwrap());
    if std::env::var("UPDATE_GOLDEN").is_ok() {
        let path = golden_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            serde_json::to_string_pretty(
                &serde_json::from_str::<serde_json::Value>(&r).unwrap(),
            )
            .unwrap()
                + "\n",
        )
        .unwrap();
    }
    // The golden was captured once from the legacy Node router; the golden
    // file is the contract and Node is no longer needed.
    if !golden_path().exists() {
        panic!("state-machine: missing golden; regenerate with UPDATE_GOLDEN=1");
    }
    let golden: serde_json::Value =
        serde_json::from_slice(&std::fs::read(golden_path()).unwrap()).unwrap();
    let n = serde_json::to_string(&golden).unwrap();
    if n != r {
        let a: serde_json::Value = serde_json::from_str(&n).unwrap();
        let b: serde_json::Value = serde_json::from_str(&r).unwrap();
        for (i, (x, y)) in a
            .as_array()
            .unwrap()
            .iter()
            .zip(b.as_array().unwrap())
            .enumerate()
        {
            assert_eq!(
                x,
                y,
                "op {i}: {}",
                serde_json::to_string(&ops()[i]).unwrap()
            );
        }
    }
}

/// CLI storage contract on temp dirs: private dir/files, thread binding,
/// exit codes (0/1/2).
#[test]
fn cli_storage_contract() {
    let dir = std::env::temp_dir().join(format!(
        "agentdeck-wf-cli-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let state_home = dir.join("state");
    let run = |args: &[&str], stdin: Option<&str>| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentdeck"));
        cmd.arg("route").arg("workflow");
        for a in args {
            cmd.arg(a);
        }
        cmd.env("XDG_STATE_HOME", &state_home);
        cmd.stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd.spawn().expect("spawn agentdeck");
        if let Some(input) = stdin {
            child
                .stdin
                .take()
                .unwrap()
                .write_all(input.as_bytes())
                .unwrap();
        }
        child.wait_with_output().expect("output")
    };
    let plan = dir.join("plan.json");
    std::fs::write(&plan, serde_json::to_string(&diamond()).unwrap()).unwrap();
    let out = run(
        &["init", plan.to_str().unwrap(), "--thread", "parent"],
        None,
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let init: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let file = init["state"].as_str().unwrap().to_string();
    assert!(file.starts_with(state_home.join("model-route/workflows").to_str().unwrap()));
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    #[cfg(unix)]
    {
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(std::path::Path::new(&file).parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
    // Wrong thread and files outside the store are refused (exit 2).
    let out = run(&["next", &file, "--thread", "other"], None);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("different parent thread"));
    let out = run(
        &["next", plan.to_str().unwrap(), "--thread", "parent"],
        None,
    );
    assert_eq!(out.status.code(), Some(2));
    // Record needs a prepared step; garbage stdin is a failure.
    let out = run(&["record", &file, "a", "--thread", "parent"], Some("{bad"));
    assert!(out.status.code() == Some(1) || out.status.code() == Some(2));
    let out = run(&["stop", &file, "--thread", "parent"], None);
    assert_eq!(out.status.code(), Some(0));
    let stopped: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(stopped["stopped"], true);
    let out = run(&["resume", &file, "--thread", "parent"], None);
    assert_eq!(out.status.code(), Some(0));
    let out = run(&["release", &file, "a", "--thread", "parent"], None);
    assert_eq!(out.status.code(), Some(2));
    // Only a prepared step releases: `--not-dispatched` on a pending step
    // fails in both implementations (usage error, exit 2).
    let out = run(&["release", &file, "a", "--thread", "parent", "--not-dispatched"], None);
    assert_eq!(out.status.code(), Some(2));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Oversized dependency results fail `prepare` before routing (ported from
/// the legacy Node `workflow.test.mjs`): the route stub must stay untouched
/// and the step stays pending.
#[test]
fn oversized_dependency_results_fail_prepare_before_routing() {
    use agentdeck_router::workflow::*;
    use std::cell::Cell;
    let plan = serde_json::json!({"title": "large", "steps": [
        {"id": "a", "task": "a"},
        {"id": "b", "task": "b"},
        {"id": "join", "task": "join", "needs": ["a", "b"]},
    ]});
    let t = serde_json::json!({"providerInstanceId": "codex", "model": "gpt-6.1-sol",
        "options": {"reasoningEffort": "high"}});
    let mut state = new_state(&plan, "th").unwrap();
    let calls = Cell::new(0);
    let route = |_: &str, _: &str| {
        calls.set(calls.get() + 1);
        Ok(serde_json::json!({"target": t, "reason": "r"}))
    };
    for id in ["a", "b"] {
        prepare(&mut state, id, "src", &route).unwrap();
        record(
            &mut state,
            id,
            &serde_json::json!({"taskId": format!("large-{id}"), "status": "completed",
                "workState": "result_available", "hasPendingChildRuns": false,
                "providerInstanceId": "codex", "model": "gpt-6.1-sol",
                "latestTerminalSummary": "\"".repeat(63_000)}),
        )
        .unwrap();
    }
    let before = calls.get();
    let err = prepare(&mut state, "join", "src", &route).unwrap_err();
    assert!(err.message.contains("brief plus dependency results exceeds"), "{}", err.message);
    assert_eq!(calls.get(), before);
    assert_eq!(next(&state)["ready"], serde_json::json!(["join"]));
}
