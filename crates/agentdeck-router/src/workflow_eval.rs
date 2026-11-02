//! Opt-in observational evaluation of settled routed-workflow tasks.
//!
//! The coordinator runs `agentdeck route workflow eval STATE STEP --thread ID`
//! explicitly after `record` saved a settled success. Nothing here runs
//! automatically: `record` never contacts the judge, so default workflow use
//! (and its Node parity) costs nothing and sends nothing anywhere.
//!
//! Privacy: invoking `eval` sends the dispatched brief and the delegated
//! agent's summary to the configured external decision provider (the same
//! provider/model/keys as routing judgments). Only validated metadata is
//! persisted: verdict, confidence, evidence ids/count, judge identity,
//! self-judgment flag and reported-or-unknown cost, attributed as a
//! child report (`basis: "child-report"`, joined by run/step/task id). The
//! verdict reflects the child's own report, not independent proof. Raw task
//! text, evidence text and summaries are never stored.
//!
//! Cost control: explicit invocation only, deterministic 1-in-N sampling
//! (`--sample N`), bounded inputs (over-long brief/summary are truncated and
//! the truncation is reported, never silent), and idempotency before any
//! network: a step whose T3 task already has a decider line is reported
//! `already-recorded` without calling the judge again.
//!
//! Crash safety: the judge call is a billed side effect, so a result that was
//! obtained but could not be recorded is reported `unknown` and never
//! silently retried; a judge call that returned no usable result is reported
//! `failed`. Both are terminal for that task id unless the coordinator passes
//! `--force`, which explicitly accepts a possible double bill. Interrupted,
//! blocked and failed steps are never sent to the judge and are never
//! reported as model failures.
//!
//! Coverage: settled `completed` steps of routed-workflow runs only. Direct
//! `delegate_task` calls outside a routed workflow are not covered by this
//! trigger. Outcomes never influence routing rankings.

use crate::outcomes;
use crate::workflow::{EvalStatus, State, WfError};

/// Upper bound for `--sample N` (1-in-N deterministic sampling).
pub const MAX_SAMPLE: u64 = 1024;

#[derive(Debug, Clone)]
pub struct EvalOptions {
    pub sample: u64,
    pub force: bool,
}

#[derive(Debug, Clone)]
pub struct EvalTarget {
    pub task_id: String,
    pub provider: String,
    pub model: String,
    pub category: String,
    pub brief: String,
    pub brief_truncated: bool,
    pub summary: String,
    pub summary_truncated: bool,
}

/// Truncate to `max` chars; the flag keeps truncation explicit, never silent.
pub fn truncate_chars(s: &str, max: usize) -> (String, bool) {
    if s.chars().count() <= max {
        (s.to_string(), false)
    } else {
        (s.chars().take(max).collect(), true)
    }
}

/// Deterministic 1-in-N gate over the T3 task id, so repeats agree without
/// state. `n <= 1` always passes.
pub fn sampled(task_id: &str, n: u64) -> bool {
    if n <= 1 {
        return true;
    }
    // FNV-1a 64: no new dependency for one hash.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in task_id.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h.is_multiple_of(n)
}

/// Only settled successful completions with enough data to judge. Anything
/// else is a reason string, never a model failure. Unknown steps are a usage
/// error from the caller, not from here.
pub fn eligible(state: &State, id: &str) -> Result<EvalTarget, String> {
    let step = state
        .plan
        .steps
        .iter()
        .find(|s| s.id == id)
        .ok_or_else(|| format!("unknown step {id}"))?;
    let p = state
        .progress
        .iter()
        .find(|p| p.id == id)
        .expect("progress mirrors plan");
    if p.state != "completed" {
        return Err(format!(
            "step is {}, not a settled success; only completed steps are evaluated",
            p.state
        ));
    }
    let task_id = p
        .task_id
        .clone()
        .filter(|t| !t.is_empty())
        .ok_or_else(|| "step has no bound task id".to_string())?;
    let target = p
        .payload
        .as_ref()
        .and_then(|pl| pl.get("target"))
        .ok_or_else(|| "step has no prepared target".to_string())?;
    let provider = target
        .get("providerInstanceId")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "prepared target lacks providerInstanceId".to_string())?
        .to_string();
    let model = target
        .get("model")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "prepared target lacks model".to_string())?
        .to_string();
    let brief_src = p
        .payload
        .as_ref()
        .and_then(|pl| pl.get("task"))
        .and_then(|v| v.as_str())
        .unwrap_or(&step.task);
    if brief_src.trim().is_empty() {
        return Err("no task brief to judge".to_string());
    }
    let summary = p
        .summary
        .as_ref()
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| "no usable summary; nothing to judge".to_string())?;
    let (brief, brief_truncated) = truncate_chars(brief_src, outcomes::MAX_EVAL_TASK);
    let (summary_text, summary_truncated) = truncate_chars(summary, outcomes::MAX_EVAL_TEXT);
    Ok(EvalTarget {
        task_id,
        provider,
        model,
        category: step.role.clone(),
        brief,
        brief_truncated,
        summary: summary_text,
        summary_truncated,
    })
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn marker(status: &str, task_id: &str, reason: Option<String>, truncated: bool) -> EvalStatus {
    EvalStatus {
        status: status.to_string(),
        task_id: task_id.to_string(),
        reason,
        truncated,
        verdict: None,
    }
}

/// Run one observational evaluation. Returns the JSON body; the caller
/// persists `state` when the marker changed. Never bills twice for one task:
/// the outcomes store is checked before any judge call, and `failed`/`unknown`
/// markers are terminal unless `force` explicitly accepts a possible re-bill.
pub async fn eval_step(
    state: &mut State,
    id: &str,
    opts: &EvalOptions,
) -> Result<(serde_json::Value, bool), WfError> {
    if !state.plan.steps.iter().any(|s| s.id == id) {
        return Err(WfError::usage(format!("unknown step {id}")));
    }
    let target = match eligible(state, id) {
        Ok(t) => t,
        Err(reason) => {
            return Ok((
                serde_json::json!({"step": id, "eval": {"status": "ineligible", "reason": reason}}),
                false,
            ));
        }
    };
    let truncated = target.brief_truncated || target.summary_truncated;
    // A stale marker from a previous task id on this step is ignored: the
    // step was re-prepared and re-recorded, so it is a new billable unit.
    if !opts.force
        && let Some(m) = state.progress.iter().find(|p| p.id == id).and_then(|p| p.eval.clone())
        && m.task_id == target.task_id
        && matches!(
            m.status.as_str(),
            "evaluated" | "skipped" | "failed" | "unknown"
        )
    {
        return Ok((
            serde_json::json!({"step": id, "taskId": target.task_id,
                "eval": {"status": m.status, "reason": m.reason, "truncated": m.truncated, "verdict": m.verdict}}),
            false,
        ));
    }
    let base = outcomes::state_base();
    // Idempotency before any network: never double-bill or double-record.
    // An unreadable store fails closed here, before any side effect.
    match outcomes::has_decider_for(&base, &target.task_id) {
        Ok(true) => {
            return Ok((
                serde_json::json!({"step": id, "taskId": target.task_id,
                    "eval": {"status": "already-recorded",
                        "reason": "a decider line already exists for this task; not billed again"}}),
                false,
            ));
        }
        Ok(false) => {}
        Err(e) => {
            return Err(if e.usage {
                WfError::usage(e.message)
            } else {
                WfError::fail(e.message)
            });
        }
    }
    if !opts.force && !sampled(&target.task_id, opts.sample) {
        let m = marker(
            "skipped",
            &target.task_id,
            Some(format!("sampled out (1-in-{})", opts.sample)),
            truncated,
        );
        set_marker(state, id, m.clone());
        return Ok((
            serde_json::json!({"step": id, "taskId": target.task_id,
                "eval": {"status": "skipped", "reason": m.reason, "truncated": truncated}}),
            true,
        ));
    }
    let input = serde_json::json!({
        "task": target.brief,
        "evidence": [{"id": "summary", "text": target.summary}],
        "performer": {"provider": target.provider, "model": target.model},
    });
    let judged = match outcomes::evaluate_json(&input.to_string()).await {
        Ok(v) => v,
        Err(e) => {
            // No usable result: report failed, never invent a verdict. The
            // call may or may not have billed, so this is terminal without
            // --force rather than silently retried.
            let mut m = marker("failed", &target.task_id, Some(short(&e.message, 200)), truncated);
            m.verdict = None;
            set_marker(state, id, m.clone());
            return Ok((
                serde_json::json!({"step": id, "taskId": target.task_id,
                    "eval": {"status": "failed", "reason": m.reason, "truncated": truncated}}),
                true,
            ));
        }
    };
    let eval = match outcomes::parse_decider_eval(&judged.to_string()) {
        Ok(e) => e,
        Err(e) => {
            let m = marker(
                "failed",
                &target.task_id,
                Some(short(&e.message, 200)),
                truncated,
            );
            set_marker(state, id, m.clone());
            return Ok((
                serde_json::json!({"step": id, "taskId": target.task_id,
                    "eval": {"status": "failed", "reason": m.reason, "truncated": truncated}}),
                true,
            ));
        }
    };
    // Crash window: the judge already billed, the line is not yet stored. If
    // this append fails, report unknown and never silently retry the billed
    // side effect; --force is the only way back, with a possible double bill.
    match outcomes::record_workflow_decider_to(
        &base,
        &outcomes::WorkflowDecider {
            task: &target.task_id,
            category: &target.category,
            provider: Some(&target.provider),
            model: &target.model,
            eval: &eval,
            now: now_secs(),
            run_id: &state.run_id,
            step: id,
        },
    ) {
        Ok(path) => {
            let mut m = marker("evaluated", &target.task_id, None, truncated);
            m.verdict = Some(eval.verdict.clone());
            set_marker(state, id, m);
            Ok((
                serde_json::json!({"step": id, "taskId": target.task_id,
                    "eval": {"status": "evaluated", "verdict": eval.verdict,
                        "confidence": eval.confidence,
                        "truncated": truncated,
                        "file": path.display().to_string()}}),
                true,
            ))
        }
        Err(e) => {
            let m = marker(
                "unknown",
                &target.task_id,
                Some(short(&e.message, 200)),
                truncated,
            );
            set_marker(state, id, m.clone());
            Ok((
                serde_json::json!({"step": id, "taskId": target.task_id,
                    "eval": {"status": "unknown",
                        "reason": "the judge was billed but the record was not stored; not retried",
                        "detail": m.reason, "truncated": truncated}}),
                true,
            ))
        }
    }
}

fn set_marker(state: &mut State, id: &str, m: EvalStatus) {
    if let Some(p) = state.progress.iter_mut().find(|p| p.id == id) {
        p.eval = Some(m);
    }
}

fn short(s: &str, max: usize) -> String {
    let t: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        format!("{t}…")
    } else {
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::{new_state, prepare, record};

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn target() -> serde_json::Value {
        serde_json::json!({"providerInstanceId": "opencode", "model": "opencode/spark"})
    }

    fn plan(task: &str) -> serde_json::Value {
        serde_json::json!({"title": "t", "steps": [{"id": "a", "task": task, "role": "implementation"}]})
    }

    fn prepared(task_brief: &str) -> State {
        let mut s = new_state(&plan(task_brief), "thread-1").unwrap();
        let t = target();
        prepare(&mut s, "a", "opencode", |_, _| {
            Ok(serde_json::json!({"target": t}))
        })
        .unwrap();
        s
    }

    fn launched(task: &str) -> serde_json::Value {
        serde_json::json!({"taskId": task, "status": "running", "workState": "working",
            "hasPendingChildRuns": false, "providerInstanceId": "opencode", "model": "opencode/spark"})
    }

    fn done(task: &str, summary: &str) -> serde_json::Value {
        serde_json::json!({"taskId": task, "status": "completed", "workState": "result_available",
            "hasPendingChildRuns": false, "providerInstanceId": "opencode",
            "model": "opencode/spark", "latestTerminalSummary": summary})
    }

    fn settled(task: &str, summary: &str) -> State {
        let mut s = prepared("do the thing");
        record(&mut s, "a", &launched(task)).unwrap();
        record(&mut s, "a", &done(task, summary)).unwrap();
        assert_eq!(
            s.progress.iter().find(|p| p.id == "a").unwrap().state,
            "completed"
        );
        s
    }

    struct Env {
        _guard: std::sync::MutexGuard<'static, ()>,
        keys: Vec<(&'static str, Option<String>)>,
    }

    impl Env {
        /// Isolated XDG_STATE_HOME plus mock judge files; restores the
        /// process environment on drop. Serialized: env is process-global.
        fn mock(tag: &str, jev_answers: serde_json::Value) -> (Self, std::path::PathBuf) {
            // A failed sibling test must not wedge the rest: keep the guard.
            let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let base = std::env::temp_dir().join(format!(
                "agentdeck-wf-eval-{}-{}",
                std::process::id(),
                tag
            ));
            let _ = std::fs::remove_dir_all(&base);
            std::fs::create_dir_all(&base).unwrap();
            let mock = base.join("mock");
            std::fs::create_dir_all(&mock).unwrap();
            std::fs::write(
                mock.join("router.json"),
                r#"{"pool": [], "accounts": {}, "version": 1, "decisionModel": "t/judge"}"#,
            )
            .unwrap();
            std::fs::write(
                mock.join("jev.json"),
                serde_json::json!({"answers": jev_answers, "usage": {"cost": 0.001}}).to_string(),
            )
            .unwrap();
            let mut env = Self {
                _guard: guard,
                keys: Vec::new(),
            };
            for (k, v) in [
                ("XDG_STATE_HOME", base.to_string_lossy().into_owned()),
                (
                    "MODEL_ROUTE_ROUTER_JSON",
                    mock.join("router.json").to_string_lossy().into_owned(),
                ),
                (
                    "AGENTDECK_ROUTE_MOCK_JEV",
                    mock.join("jev.json").to_string_lossy().into_owned(),
                ),
            ] {
                env.keys.push((k, std::env::var(k).ok()));
                // SAFETY: held mutex serializes all env mutation in this binary.
                unsafe { std::env::set_var(k, v) };
            }
            (env, base)
        }

        fn set(&mut self, k: &'static str, v: impl AsRef<std::ffi::OsStr>) {
            if !self.keys.iter().any(|(ek, _)| *ek == k) {
                self.keys.push((k, std::env::var(k).ok()));
            }
            // SAFETY: held mutex serializes all env mutation in this binary.
            unsafe { std::env::set_var(k, v) };
        }

        fn remove(&mut self, k: &'static str) {
            if !self.keys.iter().any(|(ek, _)| *ek == k) {
                self.keys.push((k, std::env::var(k).ok()));
            }
            // SAFETY: held mutex serializes all env mutation in this binary.
            unsafe { std::env::remove_var(k) };
        }
    }

    impl Drop for Env {
        fn drop(&mut self) {
            for (k, prev) in self.keys.drain(..) {
                // SAFETY: still under the mutex; restores pre-test values.
                unsafe {
                    match prev {
                        Some(v) => std::env::set_var(k, v),
                        None => std::env::remove_var(k),
                    }
                }
            }
        }
    }

    fn opts() -> EvalOptions {
        EvalOptions {
            sample: 1,
            force: false,
        }
    }

    fn pass_answers() -> serde_json::Value {
        serde_json::json!({"outcome": {"score": 2, "confidence": 0.81}})
    }

    #[test]
    fn unsettled_steps_are_ineligible_without_touching_disk() {
        let mut s = prepared("work");
        // Running, blocked and failed are never sent to the judge and never
        // reported as model failures: eligibility fails before any file read.
        record(&mut s, "a", &launched("t-1")).unwrap();
        assert!(eligible(&s, "a").is_err());
        let mut b = prepared("work");
        record(&mut b, "a", &done("t-2", "BLOCKED: no key")).unwrap();
        assert_eq!(
            b.progress.iter().find(|p| p.id == "a").unwrap().state,
            "blocked"
        );
        assert!(eligible(&b, "a").is_err());
        assert!(eligible(&s, "nope").is_err());
    }

    #[test]
    fn truncation_is_explicit_and_bounded() {
        let big_brief = "b".repeat(outcomes::MAX_EVAL_TASK + 10);
        let big_summary = "s".repeat(outcomes::MAX_EVAL_TEXT + 10);
        let s = settled_with(&big_brief, &big_summary);
        let t = eligible(&s, "a").unwrap();
        assert!(t.brief_truncated && t.summary_truncated);
        assert_eq!(t.brief.chars().count(), outcomes::MAX_EVAL_TASK);
        assert_eq!(t.summary.chars().count(), outcomes::MAX_EVAL_TEXT);
        let s = settled("t-1", "short");
        let t = eligible(&s, "a").unwrap();
        assert!(!t.brief_truncated && !t.summary_truncated);
    }

    fn settled_with(brief: &str, summary: &str) -> State {
        let mut s = prepared(brief);
        record(&mut s, "a", &launched("t-9")).unwrap();
        record(&mut s, "a", &done("t-9", summary)).unwrap();
        s
    }

    #[test]
    fn sampling_is_deterministic() {
        assert!(sampled("anything", 1));
        assert!(sampled("anything", 0));
        // 1-in-64 cannot pass every id; repeats always agree.
        assert!(!(0..200).all(|i| sampled(&format!("task-{i}"), 64)));
        for i in 0..50 {
            let id = format!("task-{i}");
            assert_eq!(sampled(&id, 7), sampled(&id, 7));
        }
    }

    fn outcomes_text(base: &std::path::Path) -> String {
        std::fs::read_to_string(base.join("model-route/outcomes.jsonl")).unwrap_or_default()
    }

    #[tokio::test]
    async fn mock_judge_records_once_then_never_rebills() {
        // Other lib tests also swap the mock env vars; retry against a fresh
        // state dir when cross-talk makes the judge unreadable.
        for attempt in 0..10 {
            let (mut env, base) =
                Env::mock(&format!("once-{attempt}"), pass_answers());
            let mut s = settled("t-once", "did the thing well");
            let (out, changed) = eval_step(&mut s, "a", &opts()).await.unwrap();
            if out["eval"]["status"] != "evaluated" {
                continue;
            }
            assert!(changed);
            assert_eq!(out["eval"]["verdict"], "pass");
            assert_eq!(s.progress.iter().find(|p| p.id == "a").unwrap().eval.as_ref().unwrap().status, "evaluated");
            let text = outcomes_text(&base);
            assert_eq!(text.lines().count(), 1);
            // Join keys and attribution present; raw summary text never stored.
            for needle in [
                "\"basis\":\"child-report\"",
                "\"source\":\"decider\"",
                "\"verdict\":\"pass\"",
                "t-once",
            ] {
                assert!(text.contains(needle), "{needle}");
            }
            assert!(!text.contains("did the thing well"));
            // Repeat with the judge unreachable: the marker replays without
            // any network, and nothing is billed or written again.
            env.remove("AGENTDECK_ROUTE_MOCK_JEV");
            let (out2, changed2) = eval_step(&mut s, "a", &opts()).await.unwrap();
            assert_eq!(out2["eval"]["status"], "evaluated");
            assert!(!changed2);
            assert_eq!(outcomes_text(&base).lines().count(), 1);
            // Without the marker, the store scan still stops a re-bill.
            s.progress.iter_mut().find(|p| p.id == "a").unwrap().eval = None;
            let (out3, changed3) = eval_step(&mut s, "a", &opts()).await.unwrap();
            assert_eq!(out3["eval"]["status"], "already-recorded");
            assert!(!changed3);
            assert_eq!(outcomes_text(&base).lines().count(), 1);
            return;
        }
        panic!("mock judge unreadable after retries (env cross-talk)");
    }

    #[tokio::test]
    async fn failed_judge_is_terminal_until_force() {
        for attempt in 0..10 {
            let (mut env, base) =
                Env::mock(&format!("fail-{attempt}"), serde_json::json!({}));
            let mut s = settled("t-fail", "some work");
            let (out, changed) = eval_step(&mut s, "a", &opts()).await.unwrap();
            if out["eval"]["status"] != "failed" {
                continue;
            }
            assert!(changed);
            assert!(outcomes_text(&base).is_empty());
            // A working judge still does not re-bill without --force.
            env.set(
                "AGENTDECK_ROUTE_MOCK_JEV",
                base.join("mock/jev.json"),
            );
            std::fs::write(
                base.join("mock/jev.json"),
                serde_json::json!({"answers": pass_answers(), "usage": {}}).to_string(),
            )
            .unwrap();
            let (out2, _) = eval_step(&mut s, "a", &opts()).await.unwrap();
            assert_eq!(out2["eval"]["status"], "failed");
            assert!(outcomes_text(&base).is_empty());
            // Explicit --force accepts a possible re-bill and recovers.
            let (out3, _) = eval_step(
                &mut s,
                "a",
                &EvalOptions {
                    sample: 1,
                    force: true,
                },
            )
            .await
            .unwrap();
            assert_eq!(out3["eval"]["status"], "evaluated");
            assert_eq!(outcomes_text(&base).lines().count(), 1);
            return;
        }
        panic!("mock judge unreadable after retries (env cross-talk)");
    }

    #[tokio::test]
    async fn billed_but_unrecorded_is_unknown_never_retried() {
        for attempt in 0..10 {
            let (mut env, base) =
                Env::mock(&format!("crash-{attempt}"), pass_answers());
            // Crash window: judge succeeds, but the store cannot accept the
            // line (dangling symlink where outcomes.jsonl goes). Reads see an
            // empty store; the append fails.
            let dir = base.join("model-route");
            std::fs::create_dir_all(&dir).unwrap();
            #[cfg(unix)]
            std::os::unix::fs::symlink(base.join("no-such-dir/outcomes.jsonl"), dir.join("outcomes.jsonl")).unwrap();
            let mut s = settled("t-crash", "finished work");
            let (out, changed) = eval_step(&mut s, "a", &opts()).await.unwrap();
            if out["eval"]["status"] != "unknown" {
                continue;
            }
            assert!(changed);
            // The workflow completion itself survives the grading failure.
            let p = s.progress.iter().find(|p| p.id == "a").unwrap();
            assert_eq!(p.state, "completed");
            assert_eq!(p.eval.as_ref().unwrap().status, "unknown");
            // Never silently retry the uncertain side effect: with the judge
            // gone, the status replays from the marker without network.
            env.remove("AGENTDECK_ROUTE_MOCK_JEV");
            let (out2, changed2) = eval_step(&mut s, "a", &opts()).await.unwrap();
            assert_eq!(out2["eval"]["status"], "unknown");
            assert!(!changed2);
            return;
        }
        panic!("mock judge unreadable after retries (env cross-talk)");
    }

    #[tokio::test]
    async fn sampled_out_skips_then_force_evaluates() {
        for attempt in 0..10 {
            let (env, base) =
                Env::mock(&format!("sample-{attempt}"), pass_answers());
            let _env = env;
            // Find a task id this N samples out (deterministic per id).
            let (tid, n) = (0..500)
                .flat_map(|i| (2..=16u64).map(move |n| (format!("sampled-{i}"), n)))
                .find(|(tid, n)| !sampled(tid, *n))
                .unwrap();
            let mut s = settled(&tid, "work done");
            let sampling = EvalOptions {
                sample: n,
                force: false,
            };
            let (out, changed) = eval_step(&mut s, "a", &sampling).await.unwrap();
            if out["eval"]["status"] != "skipped" {
                continue;
            }
            assert!(changed);
            assert!(outcomes_text(&base).is_empty());
            let (out2, _) = eval_step(
                &mut s,
                "a",
                &EvalOptions {
                    sample: n,
                    force: true,
                },
            )
            .await
            .unwrap();
            assert_eq!(out2["eval"]["status"], "evaluated");
            assert_eq!(outcomes_text(&base).lines().count(), 1);
            return;
        }
        panic!("mock judge unreadable after retries (env cross-talk)");
    }
}
