//! Opt-in observational evaluation of settled routed-workflow tasks.
//!
//! The coordinator runs `agentdeck route workflow eval STATE STEP --thread ID`
//! explicitly after `record` saved a settled success. Nothing here runs
//! automatically: `record` never contacts the judge, so default workflow use
//! costs nothing and sends nothing anywhere.
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
//! Cost control: explicit invocation, deterministic sampling, bounded inputs,
//! and a private per-task journal locked across processes. An `unknown` claim
//! is fsynced before checkpointing the workflow and before contacting the judge.
//! Process exit releases the lock, but leaves the claim terminal on retry.
//! `--force` accepts a possible repeat bill; concurrent calls never bypass the
//! lock. A completed outcome always prevents another request, even with force.
//!
//! Only settled completed workflow tasks are eligible. Judge failures preserve
//! completion. Outcomes reflect child reports and never influence rankings.

use crate::outcomes;
use crate::workflow::{EvalStatus, State, WfError};

/// Upper bound for `--sample N` (1-in-N deterministic sampling).
pub const MAX_SAMPLE: u64 = 1024;

#[derive(Debug, Clone)]
pub struct EvalOptions {
    pub sample: u64,
    pub force: bool,
    /// Daily judge-request budget; `None` keeps the old unbudgeted behaviour.
    /// `--force` never bypasses it, and auto-spawned evals share it.
    pub budget: Option<Budget>,
}

/// Daily spending guard for judge requests, enforced inside the eval path
/// before the durable claim and before contacting the judge. Manual evals
/// count too. Unknown request cost counts as `assume_usd_per_eval`.
#[derive(Debug, Clone)]
pub struct Budget {
    pub max_per_day: u64,
    pub max_usd_per_day: Option<f64>,
    pub assume_usd_per_eval: Option<f64>,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            max_per_day: 20,
            max_usd_per_day: None,
            assume_usd_per_eval: None,
        }
    }
}

/// Current UTC day as YYYY-MM-DD (civil-date math, no new dependency).
fn utc_day() -> String {
    let days = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| (d.as_secs() / 86400) as i64)
        .unwrap_or(0);
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{:04}-{:02}-{:02}", if m <= 2 { y + 1 } else { y }, m, d)
}

fn budget_path(base: &std::path::Path) -> std::path::PathBuf {
    base.join("model-route/workflow-eval-budget")
        .join(format!("{}.json", utc_day()))
}

fn lock_blocking(file: &std::fs::File) -> Result<(), String> {
    use std::os::fd::AsRawFd;
    loop {
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
            return Ok(());
        }
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return Err("cannot lock evaluation budget; not dispatched".to_string());
        }
    }
}

/// Open today's ledger under an exclusive cross-process lock (blocking: the
/// critical section is one small read-modify-write). Corrupt ledgers fail
/// closed like corrupt claims, so a budget is never silently exceeded.
fn budget_open(base: &std::path::Path) -> Result<(std::fs::File, u64, f64), String> {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    let path = budget_path(base);
    if let Some(dir) = path.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|_| "cannot open evaluation budget".to_string())?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| "cannot open evaluation budget".to_string())?;
    }
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .map_err(|_| "cannot open evaluation budget".to_string())?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(|_| "cannot open evaluation budget".to_string())?;
    if !file
        .metadata()
        .map_err(|_| "cannot open evaluation budget".to_string())?
        .is_file()
        || unsafe { libc::lseek(file.as_raw_fd(), 0, libc::SEEK_SET) } < 0
    {
        return Err("cannot open evaluation budget".to_string());
    }
    lock_blocking(&file)?;
    let mut text = String::new();
    file.read_to_string(&mut text)
        .map_err(|_| "evaluation budget is unreadable; not dispatched".to_string())?;
    if text.trim().is_empty() {
        return Ok((file, 0, 0.0));
    }
    let v: serde_json::Value = serde_json::from_str(&text)
        .map_err(|_| "evaluation budget is unreadable; not dispatched".to_string())?;
    let count = v
        .get("count")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| "evaluation budget is unreadable; not dispatched".to_string())?;
    let usd = v
        .get("usd")
        .and_then(serde_json::Value::as_f64)
        .filter(|n| n.is_finite() && *n >= 0.0)
        .ok_or_else(|| "evaluation budget is unreadable; not dispatched".to_string())?;
    Ok((file, count, usd))
}

fn budget_store(file: &mut std::fs::File, count: u64, usd: f64) -> Result<(), String> {
    use std::io::{Seek, Write};
    let fail = |_| "cannot persist evaluation budget".to_string();
    file.set_len(0).map_err(fail)?;
    file.rewind().map_err(fail)?;
    file.write_all(
        serde_json::to_string(&serde_json::json!({"count": count, "usd": usd}))
            .expect("budget serializes")
            .as_bytes(),
    )
    .map_err(fail)?;
    file.sync_all().map_err(fail)?;
    Ok(())
}

/// Reserve one judge request. Returns the assumed USD charged, settled later
/// against the reported cost. Exhaustion is a terminal `skipped`/`budget`
/// marker, never a judge call.
fn budget_reserve(base: &std::path::Path, b: &Budget) -> Result<f64, String> {
    let (mut file, count, usd) = budget_open(base)?;
    let assume = b.assume_usd_per_eval.unwrap_or(0.0);
    if count + 1 > b.max_per_day {
        return Err("budget".to_string());
    }
    // Epsilon: 0.01 * 5 prints as 0.050000000000000004 and must still fit 0.05.
    if let Some(max) = b.max_usd_per_day
        && usd + assume > max + 1e-9
    {
        return Err("budget".to_string());
    }
    budget_store(&mut file, count + 1, usd + assume)?;
    Ok(assume)
}

/// Settle the reserve against the reported cost. Best-effort and skipped when
/// nothing changed: the judge already billed, so a ledger failure must not
/// fail an otherwise recorded evaluation.
fn budget_settle(base: &std::path::Path, delta: f64) {
    if delta == 0.0 || !delta.is_finite() {
        return;
    }
    if let Ok((mut file, count, usd)) = budget_open(base) {
        let _ = budget_store(&mut file, count, (usd + delta).max(0.0));
    }
}

/// Best-effort today's ledger for status display; unreadable means zeros.
fn budget_read(base: &std::path::Path) -> (u64, f64) {
    let path = budget_path(base);
    let text = std::fs::read_to_string(path).unwrap_or_default();
    if text.trim().is_empty() {
        return (0, 0.0);
    }
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| {
            Some((
                v.get("count")?.as_u64()?,
                v.get("usd")?
                    .as_f64()
                    .filter(|n| n.is_finite() && *n >= 0.0)?,
            ))
        })
        .unwrap_or((0, 0.0))
}

/// `workflow eval-budget` output: today's count/USD against the limits.
pub fn budget_report(base: &std::path::Path, b: &Budget) -> serde_json::Value {
    let (count, usd) = budget_read(base);
    serde_json::json!({"date": utc_day(), "count": count, "usd": usd,
        "maxPerDay": b.max_per_day, "maxUsdPerDay": b.max_usd_per_day})
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
        .ok_or_else(|| format!("missing progress for step {id}"))?;
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

/// Evaluate under a durable per-task claim. The caller checkpoints the
/// workflow before dispatch; journal replay also protects stale workflow copies.
pub async fn eval_step(
    state: &mut State,
    id: &str,
    opts: &EvalOptions,
    checkpoint: impl FnMut(&State) -> Result<(), WfError>,
) -> Result<(serde_json::Value, bool), WfError> {
    eval_step_with(
        state,
        id,
        opts,
        &outcomes::state_base(),
        checkpoint,
        |input| async move { outcomes::evaluate_json(&input).await },
    )
    .await
}

async fn eval_step_with<F, Fut>(
    state: &mut State,
    id: &str,
    opts: &EvalOptions,
    base: &std::path::Path,
    mut checkpoint: impl FnMut(&State) -> Result<(), WfError>,
    judge: F,
) -> Result<(serde_json::Value, bool), WfError>
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = Result<serde_json::Value, outcomes::OutcomeError>>,
{
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
    let Some(mut claim) = Claim::open(base, &target.task_id)? else {
        return Ok((
            serde_json::json!({"step":id, "taskId":target.task_id,
            "eval":{"status":"in-flight", "reason":"another evaluation holds this task claim"}}),
            false,
        ));
    };
    // Check under the same task lock as dispatch. An unreadable store fails closed.
    if outcomes::has_decider_for(base, &target.task_id)
        .map_err(|_| WfError::fail("cannot read outcomes; evaluation was not dispatched"))?
    {
        return Ok((
            serde_json::json!({"step":id, "taskId":target.task_id,
            "eval":{"status":"already-recorded"}}),
            false,
        ));
    }
    let previous = claim.last(&target.task_id)?.or_else(|| {
        state
            .progress
            .iter()
            .find(|p| p.id == id)
            .and_then(|p| p.eval.clone())
            .filter(|m| m.task_id == target.task_id)
    });
    if !opts.force
        && let Some(m) = previous
    {
        set_marker(state, id, m.clone());
        checkpoint(state)?;
        return Ok((response(id, &m), false));
    }
    if !opts.force && !sampled(&target.task_id, opts.sample) {
        let m = marker(
            "skipped",
            &target.task_id,
            Some(format!("sampled out (1-in-{})", opts.sample)),
            truncated,
        );
        claim.save(&m)?;
        set_marker(state, id, m.clone());
        checkpoint(state)?;
        return Ok((response(id, &m), true));
    }
    // Budget is enforced here, holding the per-task claim lock and its own
    // ledger lock, before the durable unknown marker and the judge call.
    let charged = match &opts.budget {
        None => 0.0,
        Some(b) => match budget_reserve(base, b) {
            Ok(c) => c,
            Err(reason) => {
                return finish(
                    state,
                    id,
                    &mut claim,
                    &mut checkpoint,
                    marker("skipped", &target.task_id, Some(reason), truncated),
                );
            }
        },
    };
    // Unknown is deliberate: a crash cannot tell us whether the provider billed.
    let pending = marker(
        "unknown",
        &target.task_id,
        Some("judge request may have been sent; cost unknown; not retried".into()),
        truncated,
    );
    claim.save(&pending)?;
    set_marker(state, id, pending);
    checkpoint(state)?;
    let input = serde_json::json!({
        "task": target.brief,
        "evidence": [{"id": "summary", "text": target.summary}],
        "performer": {"provider": target.provider, "model": target.model},
    });
    let judged = match judge(input.to_string()).await {
        Ok(v) => v,
        Err(_) => {
            return finish(
                state,
                id,
                &mut claim,
                &mut checkpoint,
                marker(
                    "failed",
                    &target.task_id,
                    Some("judge failed; request cost unknown; not retried".into()),
                    truncated,
                ),
            );
        }
    };
    let eval = match outcomes::parse_decider_eval(&judged.to_string()) {
        Ok(e) => e,
        Err(_) => {
            return finish(
                state,
                id,
                &mut claim,
                &mut checkpoint,
                marker(
                    "failed",
                    &target.task_id,
                    Some("judge returned invalid metadata; cost unknown; not retried".into()),
                    truncated,
                ),
            );
        }
    };
    match outcomes::record_workflow_decider_to(
        base,
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
            // Unbudgeted evals (no limits configured) never touch the ledger.
            if opts.budget.is_some() {
                budget_settle(base, eval.cost.unwrap_or(charged) - charged);
            }
            let mut m = marker("evaluated", &target.task_id, None, truncated);
            m.verdict = Some(eval.verdict.clone());
            let (mut out, changed) = finish(state, id, &mut claim, &mut checkpoint, m)?;
            out["eval"]["confidence"] = serde_json::json!(eval.confidence);
            out["eval"]["file"] = serde_json::json!(path);
            Ok((out, changed))
        }
        Err(_) => finish(
            state,
            id,
            &mut claim,
            &mut checkpoint,
            marker(
                "unknown",
                &target.task_id,
                Some("outcome was not stored; request cost unknown; not retried".into()),
                truncated,
            ),
        ),
    }
}

fn response(id: &str, m: &EvalStatus) -> serde_json::Value {
    serde_json::json!({"step":id, "taskId":m.task_id, "eval":{
        "status":m.status, "reason":m.reason, "truncated":m.truncated, "verdict":m.verdict}})
}

fn finish(
    state: &mut State,
    id: &str,
    claim: &mut Claim,
    checkpoint: &mut impl FnMut(&State) -> Result<(), WfError>,
    m: EvalStatus,
) -> Result<(serde_json::Value, bool), WfError> {
    claim.save(&m)?;
    set_marker(state, id, m.clone());
    checkpoint(state)?;
    Ok((response(id, &m), true))
}

/// Stable inode: never rename/unlink a locked claim or truncate past an unknown
/// marker. A torn append fails closed instead of looking like a fresh task.
struct Claim {
    file: std::fs::File,
    dir: std::path::PathBuf,
}

impl Claim {
    fn open(base: &std::path::Path, task: &str) -> Result<Option<Self>, WfError> {
        use sha2::{Digest, Sha256};
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
        let dir = base.join("model-route/workflow-evals");
        let fail = |_| WfError::fail("cannot open private evaluation claim; not dispatched");
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .map_err(fail)?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).map_err(fail)?;
        let path = dir.join(format!("{:x}.jsonl", Sha256::digest(task.as_bytes())));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .map_err(fail)?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(fail)?;
        if !file.metadata().map_err(fail)?.is_file() {
            return Err(WfError::fail("evaluation claim is not a regular file"));
        }
        loop {
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(Some(Self { file, dir }));
            }
            let error = std::io::Error::last_os_error();
            match error.kind() {
                std::io::ErrorKind::WouldBlock => return Ok(None),
                std::io::ErrorKind::Interrupted => continue,
                _ => return Err(fail(error)),
            }
        }
    }

    fn last(&mut self, task: &str) -> Result<Option<EvalStatus>, WfError> {
        use std::io::Read;
        let invalid = || WfError::fail("unreadable evaluation claim; not dispatched");
        let mut text = String::new();
        (&mut self.file)
            .take(1_048_577)
            .read_to_string(&mut text)
            .map_err(|_| invalid())?;
        if text.len() > 1_048_576 || !text.is_empty() && !text.ends_with('\n') {
            return Err(invalid());
        }
        let mut last = None;
        for line in text.lines() {
            let m: EvalStatus = serde_json::from_str(line).map_err(|_| invalid())?;
            if m.task_id != task
                || !matches!(
                    m.status.as_str(),
                    "unknown" | "failed" | "skipped" | "evaluated"
                )
            {
                return Err(invalid());
            }
            last = Some(m);
        }
        Ok(last)
    }

    fn save(&mut self, m: &EvalStatus) -> Result<(), WfError> {
        use std::io::Write;
        let fail =
            |_| WfError::fail("cannot persist evaluation claim; request cost may be unknown");
        let mut text = serde_json::to_vec(m).expect("evaluation metadata serializes");
        text.push(b'\n');
        self.file.write_all(&text).map_err(fail)?;
        self.file.sync_all().map_err(fail)?;
        // Persist new directory entries as well as contents, before any network.
        for dir in [
            self.dir.as_path(),
            self.dir.parent().unwrap(),
            self.dir.parent().unwrap().parent().unwrap(),
        ] {
            std::fs::File::open(dir)
                .and_then(|f| f.sync_all())
                .map_err(fail)?;
        }
        Ok(())
    }
}

fn set_marker(state: &mut State, id: &str, m: EvalStatus) {
    if let Some(p) = state.progress.iter_mut().find(|p| p.id == id) {
        p.eval = Some(m);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::{new_state, prepare, record};
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command};
    use std::time::{Duration, Instant};

    struct Temp(PathBuf);
    impl Temp {
        fn new(tag: &str) -> Self {
            static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "agentdeck-eval-c2-{}-{tag}-{}",
                std::process::id(),
                N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }
    fn settled(task: &str) -> State {
        let mut s = new_state(
            &serde_json::json!({"title":"t", "steps":[{"id":"a",
            "task":"RAW-BRIEF", "role":"implementation"}]}),
            "thread",
        )
        .unwrap();
        prepare(&mut s, "a", "opencode", |_, _| {
            Ok(serde_json::json!({"target":{
            "providerInstanceId":"opencode", "model":"opencode/spark"}}))
        })
        .unwrap();
        record(
            &mut s,
            "a",
            &serde_json::json!({"taskId":task, "status":"completed",
            "workState":"result_available", "hasPendingChildRuns":false,
            "providerInstanceId":"opencode", "model":"opencode/spark",
            "latestTerminalSummary":"RAW-SUMMARY"}),
        )
        .unwrap();
        assert_eq!(s.progress[0].state, "completed");
        s
    }
    fn opts() -> EvalOptions {
        EvalOptions {
            sample: 1,
            force: false,
            budget: None,
        }
    }
    fn answer() -> serde_json::Value {
        serde_json::json!({"verdict":"pass", "confidence":0.8, "evidenceConsidered":["summary"],
            "judge":{"provider":"mock", "model":"judge"}, "selfJudged":null, "judgeCostUsd":null})
    }
    fn snapshot(dir: &Path, s: &State) -> Result<(), WfError> {
        crate::workflow::write_state(&dir.join("state.json"), s, false)
    }
    fn calls(dir: &Path) -> usize {
        std::fs::read_to_string(dir.join("calls"))
            .unwrap_or_default()
            .lines()
            .count()
    }
    fn counted(dir: &Path) {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("calls"))
            .unwrap()
            .write_all(b"request\n")
            .unwrap();
    }
    fn wait_marker(dir: &Path, marker: &str, child: &mut Child) {
        let until = Instant::now() + Duration::from_secs(10);
        while !dir.join(marker).exists() {
            assert!(Instant::now() < until, "timeout waiting for {marker}");
            assert!(
                child.try_wait().unwrap().is_none(),
                "worker exited before {marker}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn finish(child: &mut Child) -> std::process::ExitStatus {
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < until, "worker timeout");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn worker(dir: &Path, mode: &str) -> Child {
        worker_task(dir, mode, "process-task")
    }
    fn worker_task(dir: &Path, mode: &str, task: &str) -> Child {
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "workflow_eval::tests::process_worker",
                "--nocapture",
            ])
            .env("EVAL_TEST_DIR", dir)
            .env("EVAL_TEST_MODE", mode)
            .env("EVAL_TEST_TASK", task)
            .spawn()
            .unwrap()
    }
    #[tokio::test]
    async fn process_worker() {
        let Some(dir) = std::env::var_os("EVAL_TEST_DIR").map(PathBuf::from) else {
            return;
        };
        let mode = std::env::var("EVAL_TEST_MODE").unwrap();
        if mode == "cli" {
            // Only this isolated --exact child mutates its environment; no
            // sibling tests or real provider/credential paths can be reached.
            let mock = dir.join("mock.json");
            let router = dir.join("router.json");
            std::fs::write(
                &mock,
                serde_json::json!({"answers":{"outcome":{"score":2,"confidence":0.8}},"usage":{}})
                    .to_string(),
            )
            .unwrap();
            std::fs::write(
                &router,
                r#"{"pool":[],"accounts":{},"version":1,"decisionModel":"t/judge"}"#,
            )
            .unwrap();
            unsafe {
                std::env::set_var("XDG_STATE_HOME", &dir);
                std::env::set_var("MODEL_ROUTE_ROUTER_JSON", &router);
                std::env::set_var("AGENTDECK_ROUTE_MOCK_JEV", &mock);
            }
            let s = settled("cli-task");
            assert!(s.progress[0].eval.is_none());
            assert!(!dir.join("model-route").exists());
            let runs = crate::workflow::workflows_dir();
            std::fs::create_dir_all(&runs).unwrap();
            let file = runs.join(format!("{}.json", s.run_id));
            crate::workflow::write_state(&file, &s, true).unwrap();
            let args = vec![
                "eval".into(),
                file.to_string_lossy().into_owned(),
                "a".into(),
                "--thread".into(),
                "thread".into(),
            ];
            assert_eq!(crate::workflow::run(&args).await, 0);
            let disk: State = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
            assert_eq!(disk.progress[0].state, "completed");
            assert_eq!(disk.progress[0].eval.as_ref().unwrap().status, "evaluated");
            // Keep a mock configured even on regressions: absent files fail
            // locally and can never fall through to a real network request.
            std::fs::remove_file(mock).unwrap();
            assert_eq!(crate::workflow::run(&args).await, 0);
            let text = std::fs::read_to_string(dir.join("model-route/outcomes.jsonl")).unwrap();
            assert_eq!(text.lines().count(), 1);
            assert!(!text.contains("RAW-"));
            return;
        }
        // A separately loaded workflow with the same T3 task must share the claim.
        // EVAL_TEST_TASK and budget.json let budget tests isolate tasks/limits
        // per worker without touching the shared default modes above.
        let task = std::env::var("EVAL_TEST_TASK").unwrap_or_else(|_| "process-task".into());
        let mut s = settled(&task);
        let mut options = opts();
        options.force = mode == "concurrent-force";
        if let Ok(text) = std::fs::read_to_string(dir.join("budget.json")) {
            let v: serde_json::Value = serde_json::from_str(&text).unwrap();
            options.budget = Some(Budget {
                max_per_day: v.get("maxPerDay").and_then(|n| n.as_u64()).unwrap_or(20),
                max_usd_per_day: v.get("maxUsdPerDay").and_then(|n| n.as_f64()),
                assume_usd_per_eval: v.get("assumeUsdPerEval").and_then(|n| n.as_f64()),
            });
        }
        let out = eval_step_with(
            &mut s,
            "a",
            &options,
            &dir,
            |s| snapshot(&dir, s),
            |input| {
                assert!(input.contains("RAW-SUMMARY"));
                async {
                    counted(&dir);
                    std::fs::write(dir.join("entered"), b"").unwrap();
                    if mode == "crash" {
                        std::process::exit(73);
                    }
                    if mode == "hold" {
                        let until = Instant::now() + Duration::from_secs(10);
                        while !dir.join("release").exists() {
                            assert!(Instant::now() < until, "fake judge timeout");
                            std::thread::sleep(Duration::from_millis(10));
                        }
                    }
                    Ok(answer())
                }
            },
        )
        .await
        .unwrap()
        .0;
        std::fs::write(
            dir.join(format!("{task}-{mode}-result.json")),
            out.to_string(),
        )
        .unwrap();
    }
    #[test]
    fn cli_uses_durable_evaluation_and_default_record_stays_observational() {
        let dir = Temp::new("cli");
        assert!(finish(&mut worker(&dir.0, "cli")).success());
    }

    #[test]
    fn interrupted_process_never_rebills_on_retry() {
        let dir = Temp::new("crash");
        let mut first = worker(&dir.0, "crash");
        assert_eq!(finish(&mut first).code(), Some(73));
        assert!(finish(&mut worker(&dir.0, "retry")).success());
        assert_eq!(
            calls(&dir.0),
            1,
            "retry billed an interrupted request twice"
        );
        let out: serde_json::Value = serde_json::from_slice(
            &std::fs::read(dir.0.join("process-task-retry-result.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(out["eval"]["status"], "unknown");
    }
    #[test]
    fn simultaneous_processes_send_exactly_one_request() {
        let dir = Temp::new("concurrent");
        let mut first = worker(&dir.0, "hold");
        wait_marker(&dir.0, "entered", &mut first);
        let mut second = worker(&dir.0, "concurrent");
        // Second must return while the first judge is still in flight.
        let second_status = finish(&mut second);
        let forced_status = finish(&mut worker(&dir.0, "concurrent-force"));
        assert!(forced_status.success());
        std::fs::write(dir.0.join("release"), b"").unwrap();
        assert!(finish(&mut first).success());
        assert!(second_status.success());
        assert_eq!(calls(&dir.0), 1, "concurrent evaluators both billed");
        assert!(finish(&mut worker(&dir.0, "retry")).success());
        assert_eq!(calls(&dir.0), 1);
    }
    fn budgeted(max_per_day: u64, max_usd: Option<f64>, assume: Option<f64>) -> EvalOptions {
        EvalOptions {
            sample: 1,
            force: false,
            budget: Some(Budget {
                max_per_day,
                max_usd_per_day: max_usd,
                assume_usd_per_eval: assume,
            }),
        }
    }
    fn costed(cost: serde_json::Value) -> serde_json::Value {
        serde_json::json!({"verdict":"pass", "confidence":0.8, "evidenceConsidered":["summary"],
            "judge":{"provider":"mock", "model":"judge"}, "selfJudged":null, "judgeCostUsd":cost})
    }
    async fn budgeted_eval(
        dir: &Path,
        task: &str,
        opts: &EvalOptions,
        judge: impl FnOnce(
            String,
        )
            -> std::future::Ready<Result<serde_json::Value, outcomes::OutcomeError>>,
    ) -> serde_json::Value {
        let mut s = settled(task);
        eval_step_with(&mut s, "a", opts, dir, |s| snapshot(dir, s), judge)
            .await
            .unwrap()
            .0
    }

    #[tokio::test]
    async fn budget_exhausted_skips_without_judge_call() {
        let dir = Temp::new("budget");
        let out = budgeted_eval(&dir.0, "budget-a", &budgeted(1, None, None), |_| {
            std::future::ready({
                counted(&dir.0);
                Ok(answer())
            })
        })
        .await;
        assert_eq!(out["eval"]["status"], "evaluated");
        let out = budgeted_eval(&dir.0, "budget-b", &budgeted(1, None, None), |_| {
            std::future::ready({
                counted(&dir.0);
                Ok(answer())
            })
        })
        .await;
        assert_eq!(out["eval"]["status"], "skipped");
        assert_eq!(out["eval"]["reason"], "budget");
        assert_eq!(calls(&dir.0), 1, "exhausted budget billed the judge");
        assert_eq!(budget_read(&dir.0), (1, 0.0));
        // The skip is terminal: replays and --force never bill either.
        for task in ["budget-b", "budget-c"] {
            let mut force = budgeted(1, None, None);
            force.force = true;
            let out = budgeted_eval(&dir.0, task, &force, |_| {
                std::future::ready({
                    counted(&dir.0);
                    Ok(answer())
                })
            })
            .await;
            assert_eq!(out["eval"]["status"], "skipped");
            assert_eq!(out["eval"]["reason"], "budget");
        }
        assert_eq!(calls(&dir.0), 1);
    }

    #[tokio::test]
    async fn unknown_cost_counts_assume_against_usd_cap() {
        let dir = Temp::new("budget-usd");
        // answer() reports no cost, so each request charges the assume value.
        let out = budgeted_eval(
            &dir.0,
            "usd-a",
            &budgeted(100, Some(0.015), Some(0.01)),
            |_| {
                std::future::ready({
                    counted(&dir.0);
                    Ok(answer())
                })
            },
        )
        .await;
        assert_eq!(out["eval"]["status"], "evaluated");
        assert_eq!(budget_read(&dir.0), (1, 0.01));
        let out = budgeted_eval(
            &dir.0,
            "usd-b",
            &budgeted(100, Some(0.015), Some(0.01)),
            |_| {
                std::future::ready({
                    counted(&dir.0);
                    Ok(answer())
                })
            },
        )
        .await;
        assert_eq!(out["eval"]["status"], "skipped");
        assert_eq!(out["eval"]["reason"], "budget");
        assert_eq!(calls(&dir.0), 1);
    }

    #[tokio::test]
    async fn reported_cost_settles_against_assume() {
        let dir = Temp::new("budget-settle");
        let out = budgeted_eval(
            &dir.0,
            "settle-a",
            &budgeted(100, Some(1.0), Some(0.01)),
            |_| {
                std::future::ready({
                    counted(&dir.0);
                    Ok(costed(serde_json::json!(0.002)))
                })
            },
        )
        .await;
        assert_eq!(out["eval"]["status"], "evaluated");
        let (count, usd) = budget_read(&dir.0);
        assert_eq!(count, 1);
        assert!((usd - 0.002).abs() < 1e-12, "ledger usd is {usd}");
    }

    #[test]
    fn concurrent_processes_share_one_budget_slot() {
        let dir = Temp::new("budget-race");
        std::fs::write(dir.0.join("budget.json"), r#"{"maxPerDay":1}"#).unwrap();
        let mut first = worker_task(&dir.0, "hold", "race-a");
        wait_marker(&dir.0, "entered", &mut first);
        // The first worker holds the only daily slot inside its fake judge;
        // the second must skip on budget without ever calling the judge.
        let mut second = worker_task(&dir.0, "race", "race-b");
        let second_status = finish(&mut second);
        std::fs::write(dir.0.join("release"), b"").unwrap();
        assert!(finish(&mut first).success());
        assert!(second_status.success());
        assert_eq!(
            calls(&dir.0),
            1,
            "budget allowed two concurrent judge requests"
        );
        let out: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.0.join("race-b-race-result.json")).unwrap())
                .unwrap();
        assert_eq!(out["eval"]["status"], "skipped");
        assert_eq!(out["eval"]["reason"], "budget");
    }

    #[tokio::test]
    async fn durable_checkpoint_precedes_judge_and_state_errors_fail_closed() {
        let dir = Temp::new("checkpoint");
        let mut s = settled("checkpoint-task");
        let out = eval_step_with(
            &mut s,
            "a",
            &opts(),
            &dir.0,
            |s| snapshot(&dir.0, s),
            |_| async {
                let disk: State =
                    serde_json::from_slice(&std::fs::read(dir.0.join("state.json")).unwrap())
                        .unwrap();
                assert_eq!(disk.progress[0].eval.as_ref().unwrap().status, "unknown");
                Ok(answer())
            },
        )
        .await
        .unwrap()
        .0;
        assert_eq!(out["eval"]["status"], "evaluated");
        let mut s = settled("write-error-task");
        snapshot(&dir.0, &s).unwrap();
        let blocked = dir
            .0
            .join("state.json")
            .with_extension(format!("json.{}.tmp", std::process::id()));
        std::fs::create_dir(&blocked).unwrap();
        let out = eval_step_with(
            &mut s,
            "a",
            &opts(),
            &dir.0,
            |s| snapshot(&dir.0, s),
            |_| async {
                panic!("judge called after failed workflow checkpoint");
            },
        )
        .await;
        assert!(out.is_err());
        assert_eq!(s.progress[0].state, "completed");
        let disk: State =
            serde_json::from_slice(&std::fs::read(dir.0.join("state.json")).unwrap()).unwrap();
        assert_eq!(
            disk.progress[0].task_id.as_deref(),
            Some("write-error-task")
        );
        assert_eq!(disk.progress[0].state, "completed");
        std::fs::remove_dir(blocked).unwrap();
        let out = eval_step_with(
            &mut s,
            "a",
            &opts(),
            &dir.0,
            |s| snapshot(&dir.0, s),
            |_| async { panic!("checkpoint failure retried") },
        )
        .await
        .unwrap()
        .0;
        assert_eq!(out["eval"]["status"], "unknown");
    }
    fn artifacts(dir: &Path) -> String {
        let mut text = String::new();
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                text.push_str(&artifacts(&path));
            } else if path.extension().is_some_and(|e| e == "jsonl") {
                text.push_str(&std::fs::read_to_string(path).unwrap());
            }
        }
        text
    }

    #[tokio::test]
    async fn judge_failures_are_terminal_private_and_preserve_completion() {
        let dir = Temp::new("failures");
        for (task, invalid) in [("judge-error", false), ("invalid-answer", true)] {
            let mut s = settled(task);
            let out = eval_step_with(
                &mut s,
                "a",
                &opts(),
                &dir.0,
                |s| snapshot(&dir.0, s),
                |_| async {
                    counted(&dir.0);
                    if invalid {
                        Ok(serde_json::json!({"verdict":"RAW-JUDGE-RESPONSE"}))
                    } else {
                        Err(outcomes::OutcomeError {
                            usage: false,
                            message: "RAW-JUDGE-ERROR RAW-SUMMARY".into(),
                        })
                    }
                },
            )
            .await
            .unwrap()
            .0;
            assert_eq!(out["eval"]["status"], "failed");
            assert!(!out.to_string().contains("RAW-"));
            assert_eq!(s.progress[0].state, "completed");
            let disk: State =
                serde_json::from_slice(&std::fs::read(dir.0.join("state.json")).unwrap()).unwrap();
            assert_eq!(disk.progress[0].state, "completed");
            let mut stale = settled(task);
            let replay = eval_step_with(
                &mut stale,
                "a",
                &opts(),
                &dir.0,
                |s| snapshot(&dir.0, s),
                |_| async {
                    panic!("failed judge retried");
                },
            )
            .await
            .unwrap()
            .0;
            assert_eq!(replay["eval"]["status"], "failed");
        }
        assert_eq!(calls(&dir.0), 2);
        assert!(!artifacts(&dir.0).contains("RAW-"));
        // Force is the only way to accept another possibly billed request.
        let mut force = opts();
        force.force = true;
        let mut s = settled("judge-error");
        let out = eval_step_with(
            &mut s,
            "a",
            &force,
            &dir.0,
            |s| snapshot(&dir.0, s),
            |_| async { Ok(answer()) },
        )
        .await
        .unwrap()
        .0;
        assert_eq!(out["eval"]["status"], "evaluated");
    }

    #[tokio::test]
    async fn sampling_replays_per_task_and_new_task_can_evaluate() {
        let dir = Temp::new("sample");
        let tid = (0..100)
            .map(|n| format!("sample-{n}"))
            .find(|t| !sampled(t, 16))
            .unwrap();
        let mut s = settled(&tid);
        let sampling = EvalOptions {
            sample: 16,
            force: false,
            budget: None,
        };
        let out = eval_step_with(
            &mut s,
            "a",
            &sampling,
            &dir.0,
            |s| snapshot(&dir.0, s),
            |_| async { panic!("sampled-out judge called") },
        )
        .await
        .unwrap()
        .0;
        assert_eq!(out["eval"]["status"], "skipped");
        let mut stale = settled(&tid);
        let out = eval_step_with(
            &mut stale,
            "a",
            &opts(),
            &dir.0,
            |s| snapshot(&dir.0, s),
            |_| async { panic!("changed sample retried task") },
        )
        .await
        .unwrap()
        .0;
        assert_eq!(out["eval"]["status"], "skipped");
        let mut fresh = settled("new-task");
        fresh.progress[0].eval = s.progress[0].eval.clone();
        let out = eval_step_with(
            &mut fresh,
            "a",
            &opts(),
            &dir.0,
            |s| snapshot(&dir.0, s),
            |_| async {
                counted(&dir.0);
                Ok(answer())
            },
        )
        .await
        .unwrap()
        .0;
        assert_eq!(out["eval"]["status"], "evaluated");
        assert_eq!(calls(&dir.0), 1);
        let text = artifacts(&dir.0);
        assert!(text.contains("\"basis\":\"child-report\""));
        assert!(text.contains("\"judge_cost_usd\":null"));
        assert!(!text.contains("RAW-"));
    }

    #[tokio::test]
    async fn append_failure_and_final_state_failure_never_rebill() {
        let dir = Temp::new("store-error");
        std::fs::create_dir_all(dir.0.join("model-route")).unwrap();
        // Read sees no store; append cannot follow this dangling destination.
        std::os::unix::fs::symlink(
            dir.0.join("absent/outcomes"),
            dir.0.join("model-route/outcomes.jsonl"),
        )
        .unwrap();
        let mut s = settled("append-error");
        let out = eval_step_with(
            &mut s,
            "a",
            &opts(),
            &dir.0,
            |s| snapshot(&dir.0, s),
            |_| async {
                counted(&dir.0);
                Ok(answer())
            },
        )
        .await
        .unwrap()
        .0;
        assert_eq!(out["eval"]["status"], "unknown");
        assert_eq!(s.progress[0].state, "completed");
        std::fs::remove_file(dir.0.join("model-route/outcomes.jsonl")).unwrap();
        let mut stale = settled("append-error");
        let out = eval_step_with(
            &mut stale,
            "a",
            &opts(),
            &dir.0,
            |s| snapshot(&dir.0, s),
            |_| async { panic!("append failure retried") },
        )
        .await
        .unwrap()
        .0;
        assert_eq!(out["eval"]["status"], "unknown");
        let mut s = settled("final-state-error");
        let blocked = dir
            .0
            .join("state.json")
            .with_extension(format!("json.{}.tmp", std::process::id()));
        let out = eval_step_with(
            &mut s,
            "a",
            &opts(),
            &dir.0,
            |s| snapshot(&dir.0, s),
            |_| async {
                counted(&dir.0);
                std::fs::create_dir(&blocked).unwrap();
                Ok(answer())
            },
        )
        .await;
        assert!(out.is_err());
        let disk: State =
            serde_json::from_slice(&std::fs::read(dir.0.join("state.json")).unwrap()).unwrap();
        assert_eq!(disk.progress[0].state, "completed");
        assert_eq!(disk.progress[0].eval.as_ref().unwrap().status, "unknown");
        std::fs::remove_dir(blocked).unwrap();
        let mut stale = settled("final-state-error");
        let mut force = opts();
        force.force = true;
        let out = eval_step_with(
            &mut stale,
            "a",
            &force,
            &dir.0,
            |s| snapshot(&dir.0, s),
            |_| async { panic!("recorded outcome retried even with force") },
        )
        .await
        .unwrap()
        .0;
        assert_eq!(out["eval"]["status"], "already-recorded");
        assert_eq!(calls(&dir.0), 2);
    }

    #[tokio::test]
    async fn corrupt_claim_and_unreadable_store_fail_closed() {
        let dir = Temp::new("corrupt");
        let mut claim = Claim::open(&dir.0, "corrupt-task").unwrap().unwrap();
        use std::io::Write;
        claim.file.write_all(b"{torn").unwrap();
        drop(claim);
        let mut s = settled("corrupt-task");
        assert!(
            eval_step_with(
                &mut s,
                "a",
                &opts(),
                &dir.0,
                |_| Ok(()),
                |_| async { panic!("corrupt claim dispatched") }
            )
            .await
            .is_err()
        );
        std::fs::create_dir(dir.0.join("model-route/outcomes.jsonl")).unwrap();
        let mut s = settled("unreadable-store");
        assert!(
            eval_step_with(
                &mut s,
                "a",
                &opts(),
                &dir.0,
                |_| Ok(()),
                |_| async { panic!("unreadable store dispatched") }
            )
            .await
            .is_err()
        );
        assert_eq!(s.progress[0].state, "completed");
    }

    #[test]
    fn missing_progress_is_reported() {
        let mut s = settled("task");
        s.progress.clear();
        assert!(eligible(&s, "a").is_err());
    }

    #[tokio::test]
    async fn ineligible_steps_do_not_touch_disk_and_inputs_remain_bounded() {
        let dir = Temp::new("ineligible");
        let mut s = settled("task");
        for status in ["running", "blocked", "failed", "interrupted"] {
            s.progress[0].state = status.into();
            let out = eval_step_with(
                &mut s,
                "a",
                &opts(),
                &dir.0,
                |_| panic!("ineligible checkpoint"),
                |_| async { panic!("ineligible judge") },
            )
            .await
            .unwrap()
            .0;
            assert_eq!(out["eval"]["status"], "ineligible");
        }
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 0);
        s.progress[0].state = "completed".into();
        s.progress[0].payload.as_mut().unwrap()["task"] =
            serde_json::json!("é".repeat(outcomes::MAX_EVAL_TASK + 1));
        s.progress[0].summary = Some(serde_json::json!("界".repeat(outcomes::MAX_EVAL_TEXT + 1)));
        let target = eligible(&s, "a").unwrap();
        assert!(target.brief_truncated && target.summary_truncated);
        assert_eq!(target.brief.chars().count(), outcomes::MAX_EVAL_TASK);
        assert_eq!(target.summary.chars().count(), outcomes::MAX_EVAL_TEXT);
    }
}
