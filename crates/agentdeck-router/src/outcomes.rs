//! Observational coordinator outcome store (no routing effect).
//!
//! `agentdeck route outcomes record|list|evaluate`: append-only JSONL at
//! `XDG_STATE_HOME/model-route/outcomes.jsonl` (dir `0700`, file `0600`).
//! One line per assessment; repeats for the same task are revisions, newest
//! last. Only metadata is stored: no task text, summaries or evidence text.

use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use crate::policy::valid_answer;

pub const USAGE: &str = "usage: agentdeck route outcomes record --task ID --provider P --model M --category C --verdict V --evidence E [--source coordinator]\n  agentdeck route outcomes record-decider --task ID --category C --model M [--provider P] < evaluation.json\n  agentdeck route outcomes list [--task ID] [--limit N]\n  agentdeck route outcomes evaluate < evaluation.json\n\n  category: implementation|research|review|design|test|general\n  verdict: ok|partial|fail|blocked|missing|unavailable (blocked/missing/unavailable are not failures)\n  evidence: direct|spot-check|test-output|child-report\n  source is always recorded as \"coordinator\"; decider assessments use record-decider and are recorded as \"decider\".\n  repeats for one task append a new revision; appends are file-locked; corrupt lines fail reads (a torn final line is skipped), never deleted.\n\nevaluate reads one JSON object on stdin (max 16 KiB):\n  {\"task\": \"what was asked, 1-2000 chars\", \"evidence\": [{\"id\": \"e1\", \"text\": \"...1-4000 chars...\"} 1-8 items],\n   \"performer\": {\"provider\": \"P\", \"model\": \"M\"}?}\n  It sends that content to the configured decision provider/model (explicit opt-in; standard keys)\n  and prints {verdict: pass|partial|fail|insufficient, confidence, evidenceConsidered, judge{provider,model},\n  selfJudged: true|null (null means unknown), judgeCostUsd: number|null (null means unreported)}.\n  Nothing is written anywhere: raw task and evidence text are never persisted. To persist an\n  assessment, run `outcomes record` separately with validated metadata only.\n\nrecord-decider reads the JSON object printed by `outcomes evaluate` on stdin (max 16 KiB)\n  and appends validated decider metadata only: no task text, evidence text, summaries or\n  secrets are read or stored. Judge identity, selfJudged and cost come from that result\n  (never from flags); unknown stays null. Needs no network and never affects ranking.";

const CATEGORIES: &[&str] = &[
    "implementation",
    "research",
    "review",
    "design",
    "test",
    "general",
];
const VERDICTS: &[&str] = &["ok", "partial", "fail", "blocked", "missing", "unavailable"];
const EVIDENCE: &[&str] = &["direct", "spot-check", "test-output", "child-report"];

const MAX_TASK: usize = 256;
const MAX_PROVIDER: usize = 64;
const MAX_MODEL: usize = 128;
const MAX_LIST: usize = 1000;

#[derive(Debug)]
pub(crate) struct OutcomeError {
    pub(crate) usage: bool,
    pub(crate) message: String,
}

impl OutcomeError {
    fn usage(message: impl Into<String>) -> Self {
        Self {
            usage: true,
            message: message.into(),
        }
    }
    fn fail(message: impl Into<String>) -> Self {
        Self {
            usage: false,
            message: message.into(),
        }
    }
}

/// CLI entry. Returns the process exit code (0, 1, or 2 on usage errors).
pub async fn run(argv: &[String]) -> i32 {
    match run_inner(argv).await {
        Ok(value) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&value).unwrap_or_default()
            );
            0
        }
        Err(e) => {
            eprintln!("outcomes: {}", e.message);
            if e.usage { 2 } else { 1 }
        }
    }
}

async fn run_inner(argv: &[String]) -> Result<serde_json::Value, OutcomeError> {
    match argv.first().map(String::as_str) {
        Some("record") => record_cli(&argv[1..]),
        Some("record-decider") => record_decider_cli(&argv[1..]),
        Some("list") => list_cli(&argv[1..]),
        Some("evaluate") => {
            if argv.len() != 1 {
                return Err(OutcomeError::usage(USAGE));
            }
            evaluate_cli().await
        }
        _ => Err(OutcomeError::usage(USAGE)),
    }
}

fn outcomes_path(base: &Path) -> PathBuf {
    base.join("model-route/outcomes.jsonl")
}

fn ensure_parent(base: &Path) -> Result<PathBuf, OutcomeError> {
    let dir = base.join("model-route");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .map_err(|e| OutcomeError::fail(e.to_string()))?;
    // A private dir that is not 0700 (pre-existing or raced) must fail loudly,
    // never silently hold outcome data in a group/world-readable directory.
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| OutcomeError::fail(format!("cannot secure {}: {e}", dir.display())))?;
    Ok(outcomes_path(base))
}

pub(crate) fn state_base() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(std::env::temp_dir)
                .join(".local/state")
        })
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn check_id(value: &str, what: &str, max: usize, extra: &[char]) -> Result<(), OutcomeError> {
    if value.is_empty() || value.len() > max {
        return Err(OutcomeError::usage(format!("{what} must be 1-{max} chars")));
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || extra.contains(&c))
    {
        return Err(OutcomeError::usage(format!(
            "{what} has invalid characters"
        )));
    }
    Ok(())
}

fn check_enum(value: &str, what: &str, allowed: &[&str]) -> Result<(), OutcomeError> {
    if allowed.contains(&value) {
        Ok(())
    } else {
        Err(OutcomeError::usage(format!(
            "{what} must be one of {}",
            allowed.join("|")
        )))
    }
}

#[derive(Debug, Clone)]
struct Record {
    task: String,
    provider: String,
    model: String,
    category: String,
    verdict: String,
    evidence: String,
}

fn parse_record(argv: &[String]) -> Result<Record, OutcomeError> {
    let mut task = None;
    let mut provider = None;
    let mut model = None;
    let mut category = None;
    let mut verdict = None;
    let mut evidence = None;
    let mut i = 0;
    while i < argv.len() {
        let flag = argv[i].as_str();
        let get = |i: &mut usize| -> Result<String, OutcomeError> {
            *i += 1;
            argv.get(*i)
                .cloned()
                .ok_or_else(|| OutcomeError::usage(USAGE))
        };
        match flag {
            "--task" => task = Some(get(&mut i)?),
            "--provider" => provider = Some(get(&mut i)?),
            "--model" => model = Some(get(&mut i)?),
            "--category" => category = Some(get(&mut i)?),
            "--verdict" => verdict = Some(get(&mut i)?),
            "--evidence" => evidence = Some(get(&mut i)?),
            "--source" => {
                let s = get(&mut i)?;
                if s != "coordinator" {
                    return Err(OutcomeError::usage(
                        "only --source coordinator is recorded by this store",
                    ));
                }
            }
            _ => return Err(OutcomeError::usage(USAGE)),
        }
        i += 1;
    }
    let r = Record {
        task: task.ok_or_else(|| OutcomeError::usage(USAGE))?,
        provider: provider.ok_or_else(|| OutcomeError::usage(USAGE))?,
        model: model.ok_or_else(|| OutcomeError::usage(USAGE))?,
        category: category.ok_or_else(|| OutcomeError::usage(USAGE))?,
        verdict: verdict.ok_or_else(|| OutcomeError::usage(USAGE))?,
        evidence: evidence.ok_or_else(|| OutcomeError::usage(USAGE))?,
    };
    // T3 IDs are untrusted input: strict charset + length bounds.
    // '%' covers percent-encoded T3 task IDs (e.g. command%3Amcp%3A...).
    check_id(&r.task, "task", MAX_TASK, &[':', '_', '.', '-', '%'])?;
    check_id(&r.provider, "provider", MAX_PROVIDER, &['_', '.', '-'])?;
    check_id(
        &r.model,
        "model",
        MAX_MODEL,
        &['_', '.', '-', '/', ':', '+'],
    )?;
    check_enum(&r.category, "category", CATEGORIES)?;
    check_enum(&r.verdict, "verdict", VERDICTS)?;
    check_enum(&r.evidence, "evidence", EVIDENCE)?;
    Ok(r)
}

fn record_cli(argv: &[String]) -> Result<serde_json::Value, OutcomeError> {
    let r = parse_record(argv)?;
    let path = append_to(&state_base(), &r, now_secs())?;
    Ok(serde_json::json!({"recorded": true, "file": path.display().to_string()}))
}

// flock(2) advisory lock on an open file; released on close. libc is already a
// dependency, so no new crate for one syscall. Retries on EINTR.
fn lock_file(f: &std::fs::File, op: libc::c_int, what: &str) -> Result<(), OutcomeError> {
    loop {
        // shortcut: non-blocking attempts are never used; a stuck lock just waits.
        let rc = unsafe { libc::flock(f.as_raw_fd(), op) };
        if rc == 0 {
            return Ok(());
        }
        let e = std::io::Error::last_os_error();
        if e.kind() != std::io::ErrorKind::Interrupted {
            return Err(OutcomeError::fail(format!(
                "cannot {what} outcomes file: {e}"
            )));
        }
    }
}

fn append_to(base: &Path, r: &Record, now: u64) -> Result<PathBuf, OutcomeError> {
    let line = serde_json::json!({
        "t": now,
        "source": "coordinator",
        "task_id": r.task,
        "provider": r.provider,
        "model": r.model,
        "category": r.category,
        "verdict": r.verdict,
        "evidence": r.evidence,
        "decider": null,
    });
    append_line(base, &line)
}

// Single locked, permission-checked, torn-tail-guarded append shared by both
// sources, so coordinator and decider records get identical durability.
fn append_line(base: &Path, line: &serde_json::Value) -> Result<PathBuf, OutcomeError> {
    let path = ensure_parent(base)?;
    let mut text = serde_json::to_string(&line).map_err(|e| OutcomeError::fail(e.to_string()))?;
    text.push('\n');
    // Exclusive flock serializes concurrent recorders across processes, so one
    // write_all lands as whole lines; fsync under the same lock gives each
    // record command durability. Lock releases on close, even on error paths.
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| OutcomeError::fail(e.to_string()))?;
    lock_file(&f, libc::LOCK_EX, "lock")?;
    f.set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(|e| OutcomeError::fail(format!("cannot secure {}: {e}", path.display())))?;
    if f.metadata()
        .map_err(|e| OutcomeError::fail(e.to_string()))?
        .len()
        > 0
    {
        f.seek(SeekFrom::End(-1))
            .map_err(|e| OutcomeError::fail(e.to_string()))?;
        let mut last = [0];
        f.read_exact(&mut last)
            .map_err(|e| OutcomeError::fail(e.to_string()))?;
        if last[0] != b'\n' {
            return Err(OutcomeError::fail(
                "outcomes file has an incomplete final record; file left unchanged",
            ));
        }
    }
    let r = f
        .write_all(text.as_bytes())
        .map_err(|e| OutcomeError::fail(e.to_string()))
        .and_then(|()| {
            f.sync_all()
                .map_err(|e| OutcomeError::fail(format!("cannot persist {}: {e}", path.display())))
        });
    drop(f); // releases LOCK_EX
    r?;
    std::fs::File::open(path.parent().unwrap())
        .and_then(|dir| dir.sync_all())
        .map_err(|e| OutcomeError::fail(format!("cannot persist outcomes directory: {e}")))?;
    Ok(path)
}

fn valid_record(v: &serde_json::Value) -> bool {
    let source_ok = v.get("source").and_then(|s| s.as_str()).is_some_and(|s| {
        // Decider lines use their own verdict scale; missing source predates
        // neither writer and is corruption, never silently coordinator data.
        s == "coordinator" || s == "decider"
    });
    let verdict_ok = match v.get("source").and_then(|s| s.as_str()) {
        Some("decider") => v
            .get("verdict")
            .and_then(|t| t.as_str())
            .is_some_and(|t| OUTCOME_VERDICTS.contains(&t)),
        _ => v
            .get("verdict")
            .and_then(|t| t.as_str())
            .is_some_and(|t| VERDICTS.contains(&t)),
    };
    source_ok
        && v.get("task_id")
            .and_then(|t| t.as_str())
            .is_some_and(|t| !t.is_empty())
        && verdict_ok
}

fn read_all(base: &Path) -> Result<Vec<serde_json::Value>, OutcomeError> {
    let path = outcomes_path(base);
    let mut f = match std::fs::File::open(&path) {
        // A store that was never recorded to reads as empty.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => {
            return Err(OutcomeError::fail(format!(
                "cannot read {}: {e}",
                path.display()
            )));
        }
        Ok(f) => f,
    };
    // Shared lock waits out any concurrent append, so the tail we read is whole.
    lock_file(&f, libc::LOCK_SH, "lock")?;
    let mut text = String::new();
    let r = f
        .read_to_string(&mut text)
        .map_err(|e| OutcomeError::fail(format!("cannot read {}: {e}", path.display())));
    drop(f); // releases LOCK_SH
    r?;
    // Our writer always ends lines with '\n'; a final line without one is the
    // torn tail of a crashed append and is skipped. Anything else invalid is
    // corruption: fail loudly and leave the file untouched, never treat as empty.
    let complete = text.is_empty() || text.ends_with('\n');
    let mut lines: Vec<(usize, &str)> = text.lines().enumerate().map(|(i, l)| (i + 1, l)).collect();
    let torn = if complete { None } else { lines.pop() };
    let mut items = Vec::new();
    for (n, l) in lines {
        if l.trim().is_empty() {
            continue;
        }
        let v: serde_json::Value = serde_json::from_str(l).map_err(|e| {
            OutcomeError::fail(format!(
                "{}:{n}: invalid JSON ({e}); file left unchanged",
                path.display()
            ))
        })?;
        if !valid_record(&v) {
            return Err(OutcomeError::fail(format!(
                "{}:{n}: not an outcome record; file left unchanged",
                path.display()
            )));
        }
        items.push(v);
    }
    if let Some((_, t)) = torn
        && !t.trim().is_empty()
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(t)
        && valid_record(&v)
    {
        // shortcut: a torn tail that still parses as a record is kept, not dropped.
        items.push(v);
    }
    Ok(items)
}

fn list_cli(argv: &[String]) -> Result<serde_json::Value, OutcomeError> {
    let mut task: Option<String> = None;
    let mut limit: usize = 50;
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--task" => {
                i += 1;
                task = Some(
                    argv.get(i)
                        .cloned()
                        .ok_or_else(|| OutcomeError::usage(USAGE))?,
                );
            }
            "--limit" => {
                i += 1;
                limit = argv
                    .get(i)
                    .and_then(|s| s.parse::<usize>().ok())
                    .filter(|&n| n > 0 && n <= MAX_LIST)
                    .ok_or_else(|| OutcomeError::usage(format!("limit must be 1-{MAX_LIST}")))?;
            }
            _ => return Err(OutcomeError::usage(USAGE)),
        }
        i += 1;
    }
    let mut items = read_all(&state_base())?;
    if let Some(t) = task {
        items.retain(|v| v.get("task_id").and_then(|x| x.as_str()) == Some(t.as_str()));
    }
    // Newest revisions last; keep the tail.
    if items.len() > limit {
        items = items.split_off(items.len() - limit);
    }
    Ok(serde_json::Value::Array(items))
}

// --- Typed decider assessment persistence (no routing effect) ----------------
// `record-decider` appends the validated metadata of an already obtained
// `evaluate` result: judge identity, self-judgment, verdict, confidence,
// reported-or-unknown cost and evidence IDs/count only. Raw task and evidence
// text are never read or stored, and no network call is made here.

const MAX_DEC_EVIDENCE: usize = 8;

#[derive(Debug, Clone)]
pub(crate) struct DeciderEval {
    pub(crate) verdict: String,
    pub(crate) confidence: f64,
    pub(crate) evidence: Vec<String>,
    pub(crate) judge_provider: String,
    pub(crate) judge_model: String,
    pub(crate) self_judged: bool,
    pub(crate) cost: Option<f64>,
}

pub(crate) fn parse_decider_eval(text: &str) -> Result<DeciderEval, OutcomeError> {
    if text.len() > MAX_EVAL_STDIN {
        return Err(OutcomeError::usage(format!(
            "evaluation result must fit in {MAX_EVAL_STDIN} bytes"
        )));
    }
    let v: serde_json::Value =
        serde_json::from_str(text).map_err(|_| OutcomeError::usage(USAGE))?;
    let verdict = v
        .get("verdict")
        .and_then(|x| x.as_str())
        .ok_or_else(|| OutcomeError::usage(USAGE))?;
    if !OUTCOME_VERDICTS.contains(&verdict) {
        return Err(OutcomeError::usage(format!(
            "verdict must be one of {}",
            OUTCOME_VERDICTS.join("|")
        )));
    }
    let confidence = v
        .get("confidence")
        .and_then(|c| c.as_f64())
        .filter(|c| (0.0..=1.0).contains(c))
        .ok_or_else(|| OutcomeError::usage("confidence must be a number in 0-1"))?;
    let ids = v
        .get("evidenceConsidered")
        .and_then(|e| e.as_array())
        .ok_or_else(|| OutcomeError::usage(USAGE))?;
    if ids.is_empty() || ids.len() > MAX_DEC_EVIDENCE {
        return Err(OutcomeError::usage(format!(
            "evidenceConsidered must hold 1-{MAX_DEC_EVIDENCE} ids"
        )));
    }
    let mut evidence = Vec::with_capacity(ids.len());
    for id in ids {
        let id = id
            .as_str()
            .ok_or_else(|| OutcomeError::usage(USAGE))?;
        check_id(id, "evidence id", MAX_EVAL_ID, &[':', '_', '.', '-'])?;
        evidence.push(id.to_string());
    }
    let mut seen = std::collections::HashSet::new();
    if !evidence.iter().all(|e| seen.insert(e.clone())) {
        return Err(OutcomeError::usage("evidence ids must be unique"));
    }
    // The judge is the model that actually produced this result; flags must
    // never override it, and an absent judge fails instead of guessing one.
    let judge = v.get("judge").ok_or_else(|| OutcomeError::usage(USAGE))?;
    let judge_provider = judge
        .get("provider")
        .and_then(|x| x.as_str())
        .ok_or_else(|| OutcomeError::usage(USAGE))?;
    check_id(
        judge_provider,
        "judge provider",
        MAX_PROVIDER,
        &['_', '.', '-'],
    )?;
    let judge_model = judge
        .get("model")
        .and_then(|x| x.as_str())
        .ok_or_else(|| OutcomeError::usage(USAGE))?;
    check_id(
        judge_model,
        "judge model",
        MAX_MODEL,
        &['_', '.', '-', '/', ':', '+'],
    )?;
    // Only true (self-evaluation) or null (unknown) persist; anything else
    // would invent certainty about the performer/judge relation.
    let self_judged = match v.get("selfJudged") {
        Some(serde_json::Value::Bool(true)) => true,
        Some(serde_json::Value::Null) | None => false,
        _ => return Err(OutcomeError::usage("selfJudged must be true or null")),
    };
    // An unreported cost stays null; a reported one must be a sane number.
    let cost = match v.get("judgeCostUsd") {
        None | Some(serde_json::Value::Null) => None,
        Some(c) => {
            let n = c
                .as_f64()
                .filter(|n| n.is_finite() && *n >= 0.0)
                .ok_or_else(|| {
                    OutcomeError::usage("judgeCostUsd must be a number >= 0 or null")
                })?;
            Some(n)
        }
    };
    Ok(DeciderEval {
        verdict: verdict.to_string(),
        confidence,
        evidence,
        judge_provider: judge_provider.to_string(),
        judge_model: judge_model.to_string(),
        self_judged,
        cost,
    })
}

fn decider_line(
    task: &str,
    category: &str,
    provider: Option<&str>,
    model: &str,
    eval: &DeciderEval,
    now: u64,
) -> Result<serde_json::Value, OutcomeError> {
    // T3 IDs are untrusted input: strict charset + length bounds, as in record.
    check_id(task, "task", MAX_TASK, &[':', '_', '.', '-', '%'])?;
    check_enum(category, "category", CATEGORIES)?;
    if let Some(p) = provider {
        check_id(p, "provider", MAX_PROVIDER, &['_', '.', '-'])?;
    }
    check_id(model, "model", MAX_MODEL, &['_', '.', '-', '/', ':', '+'])?;
    // Metadata only: IDs and counts, never task/evidence text or summaries.
    Ok(serde_json::json!({
        "t": now,
        "source": "decider",
        "task_id": task,
        "provider": provider.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null),
        "model": model,
        "category": category,
        "verdict": eval.verdict,
        "confidence": eval.confidence,
        "evidence_ids": eval.evidence,
        "evidence_count": eval.evidence.len(),
        "decider": {
            "judge": {"provider": eval.judge_provider, "model": eval.judge_model},
            "self_judged": if eval.self_judged { serde_json::Value::Bool(true) } else { serde_json::Value::Null },
            "judge_cost_usd": eval.cost.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null),
        },
    }))
}

fn record_decider_to(
    base: &Path,
    task: &str,
    category: &str,
    provider: Option<&str>,
    model: &str,
    eval: &DeciderEval,
    now: u64,
) -> Result<PathBuf, OutcomeError> {
    let line = decider_line(task, category, provider, model, eval, now)?;
    append_line(base, &line)
}

/// Workflow-triggered decider record: the same validated metadata, attributed
/// as a child report (the only evidence is the delegated agent's own summary,
/// not independent proof) and joined to the workflow run/step. No task text,
/// evidence text or summaries are stored. Additive fields only, so existing
/// readers accept these lines.
/// Workflow join keys for [`record_workflow_decider_to`]: our own run/step
/// identifiers, length-bound like every other stored id, never raw text.
pub(crate) struct WorkflowDecider<'a> {
    pub task: &'a str,
    pub category: &'a str,
    pub provider: Option<&'a str>,
    pub model: &'a str,
    pub eval: &'a DeciderEval,
    pub now: u64,
    pub run_id: &'a str,
    pub step: &'a str,
}

pub(crate) fn record_workflow_decider_to(
    base: &Path,
    w: &WorkflowDecider<'_>,
) -> Result<PathBuf, OutcomeError> {
    // Our own run/step identifiers: still length-bound, never raw text.
    if w.run_id.is_empty() || w.run_id.len() > MAX_TASK || w.step.is_empty() || w.step.len() > 64 {
        return Err(OutcomeError::usage("run/step must be 1-256/1-64 chars"));
    }
    let mut line = decider_line(w.task, w.category, w.provider, w.model, w.eval, w.now)?;
    line["basis"] = serde_json::Value::from("child-report");
    line["run_id"] = serde_json::Value::from(w.run_id);
    line["step"] = serde_json::Value::from(w.step);
    append_line(base, &line)
}

/// True when a decider line already exists for this T3 task. A corrupt or
/// unreadable store errors (fail closed) so the caller never bills a judge
/// it cannot de-duplicate against.
pub(crate) fn has_decider_for(base: &Path, task_id: &str) -> Result<bool, OutcomeError> {
    Ok(read_all(base)?.iter().any(|v| {
        v.get("source").and_then(|s| s.as_str()) == Some("decider")
            && v.get("task_id").and_then(|t| t.as_str()) == Some(task_id)
    }))
}

fn record_decider_cli(argv: &[String]) -> Result<serde_json::Value, OutcomeError> {
    let mut task = None;
    let mut category = None;
    let mut provider: Option<String> = None;
    let mut model = None;
    let mut i = 0;
    while i < argv.len() {
        let flag = argv[i].as_str();
        let get = |i: &mut usize| -> Result<String, OutcomeError> {
            *i += 1;
            argv.get(*i)
                .cloned()
                .ok_or_else(|| OutcomeError::usage(USAGE))
        };
        match flag {
            "--task" => task = Some(get(&mut i)?),
            "--category" => category = Some(get(&mut i)?),
            "--provider" => provider = Some(get(&mut i)?),
            "--model" => model = Some(get(&mut i)?),
            _ => return Err(OutcomeError::usage(USAGE)),
        }
        i += 1;
    }
    let (task, category, model) = match (task, category, model) {
        (Some(t), Some(c), Some(m)) => (t, c, m),
        _ => return Err(OutcomeError::usage(USAGE)),
    };
    let mut buf = String::new();
    std::io::stdin()
        .take((MAX_EVAL_STDIN + 1) as u64)
        .read_to_string(&mut buf)
        .map_err(|e| OutcomeError::fail(format!("cannot read stdin: {e}")))?;
    let eval = parse_decider_eval(&buf)?;
    let path = record_decider_to(
        &state_base(),
        &task,
        &category,
        provider.as_deref(),
        &model,
        &eval,
        now_secs(),
    )?;
    Ok(serde_json::json!({"recorded": true, "source": "decider", "file": path.display().to_string()}))
}

// --- Explicit on-demand outcome evaluation (no persistence) ------------------
// Reads task criteria + evidence on stdin, asks the configured decision
// provider once, prints a grounded verdict. Never writes any file: raw task
// and evidence text only travel to the explicitly invoked judge.

const MAX_EVAL_STDIN: usize = 16384;
pub(crate) const MAX_EVAL_TASK: usize = 2000;
const MAX_EVAL_EVIDENCE: usize = 8;
const MAX_EVAL_ID: usize = 64;
pub(crate) const MAX_EVAL_TEXT: usize = 4000;

const OUTCOME_VERDICTS: [&str; 4] = ["fail", "partial", "pass", "insufficient"];

#[derive(Debug, Clone)]
struct EvalEvidence {
    id: String,
    text: String,
}

#[derive(Debug, Clone)]
struct EvalInput {
    task: String,
    evidence: Vec<EvalEvidence>,
    performer_model: Option<String>,
}

fn outcome_questions() -> serde_json::Value {
    serde_json::json!({"outcome": {
        "type": "score",
        "instructions": "Judge whether the completed work meets the task requirements, using ONLY the quoted evidence. Be conservative: when the evidence is missing, vague or unrelated to the task, choose the insufficient-evidence criterion. Do not assume success from the task text alone.",
        "criteria": [
            "Failed: the evidence shows the requirements are not met.",
            "Partially met: some requirements are met but material gaps remain.",
            "Passed: the evidence shows the requirements are met.",
            "Insufficient evidence: the evidence does not let you judge.",
        ],
    }})
}

fn parse_eval_input(text: &str) -> Result<EvalInput, OutcomeError> {
    if text.len() > MAX_EVAL_STDIN {
        return Err(OutcomeError::usage(format!(
            "evaluation input must fit in {MAX_EVAL_STDIN} bytes"
        )));
    }
    let v: serde_json::Value =
        serde_json::from_str(text).map_err(|_| OutcomeError::usage(USAGE))?;
    let task = v
        .get("task")
        .and_then(|t| t.as_str())
        .ok_or_else(|| OutcomeError::usage(USAGE))?;
    if task.chars().count() == 0 || task.chars().count() > MAX_EVAL_TASK {
        return Err(OutcomeError::usage(format!(
            "task must be 1-{MAX_EVAL_TASK} chars"
        )));
    }
    let items = v
        .get("evidence")
        .and_then(|e| e.as_array())
        .ok_or_else(|| OutcomeError::usage(USAGE))?;
    if items.is_empty() || items.len() > MAX_EVAL_EVIDENCE {
        return Err(OutcomeError::usage(format!(
            "evidence must hold 1-{MAX_EVAL_EVIDENCE} items"
        )));
    }
    let mut evidence = Vec::with_capacity(items.len());
    for e in items {
        let id = e
            .get("id")
            .and_then(|i| i.as_str())
            .ok_or_else(|| OutcomeError::usage(USAGE))?;
        check_id(id, "evidence id", MAX_EVAL_ID, &[':', '_', '.', '-'])?;
        let etext = e
            .get("text")
            .and_then(|t| t.as_str())
            .ok_or_else(|| OutcomeError::usage(USAGE))?;
        if etext.chars().count() == 0 || etext.chars().count() > MAX_EVAL_TEXT {
            return Err(OutcomeError::usage(format!(
                "evidence text must be 1-{MAX_EVAL_TEXT} chars"
            )));
        }
        evidence.push(EvalEvidence {
            id: id.to_string(),
            text: etext.to_string(),
        });
    }
    let performer_model = match v.get("performer") {
        None => None,
        Some(p) => {
            let model = p
                .get("model")
                .and_then(|m| m.as_str())
                .ok_or_else(|| OutcomeError::usage(USAGE))?;
            check_id(
                model,
                "performer model",
                MAX_MODEL,
                &['_', '.', '-', '/', ':', '+'],
            )?;
            if let Some(provider) = p.get("provider").and_then(|x| x.as_str()) {
                check_id(
                    provider,
                    "performer provider",
                    MAX_PROVIDER,
                    &['_', '.', '-'],
                )?;
            }
            Some(model.to_string())
        }
    };
    // Duplicate evidence ids would make references ambiguous.
    let mut seen = std::collections::HashSet::new();
    if !evidence.iter().all(|e| seen.insert(e.id.clone())) {
        return Err(OutcomeError::usage("evidence ids must be unique"));
    }
    Ok(EvalInput {
        task: task.to_string(),
        evidence,
        performer_model,
    })
}

fn build_output(
    input: &EvalInput,
    answers: &serde_json::Value,
    cost: Option<f64>,
    provider: &str,
    model: &str,
) -> Result<serde_json::Value, OutcomeError> {
    let answer = answers
        .get("outcome")
        .filter(|a| valid_answer(a, false))
        .ok_or_else(|| {
            OutcomeError::fail("judge returned no usable outcome answer; nothing was recorded")
        })?;
    let raw_score = answer["score"].as_f64().unwrap();
    // The last criterion is non-ordinal: averaging scores cannot identify it.
    let score = if raw_score.fract() == 0.0 {
        raw_score as usize
    } else {
        3
    };
    let confidence = answer
        .get("confidence")
        .and_then(|c| c.as_f64())
        .unwrap_or(0.0);
    // Different provider IDs may be aliases for the same underlying model.
    let self_judged = match &input.performer_model {
        Some(p) if p.eq_ignore_ascii_case(model) => serde_json::Value::Bool(true),
        _ => serde_json::Value::Null,
    };
    Ok(serde_json::json!({
        "verdict": OUTCOME_VERDICTS[score],
        "confidence": confidence,
        "evidenceConsidered": input.evidence.iter().map(|e| e.id.clone()).collect::<Vec<_>>(),
        "judge": {"provider": provider, "model": model},
        "selfJudged": self_judged,
        "judgeCostUsd": cost.filter(|c| c.is_finite()).map(serde_json::Value::from).unwrap_or(serde_json::Value::Null),
        "note": "explicit on-demand evaluation; input was sent to the configured decision provider; nothing was persisted",
    }))
}

async fn evaluate_cli() -> Result<serde_json::Value, OutcomeError> {
    let mut buf = String::new();
    std::io::stdin()
        .take((MAX_EVAL_STDIN + 1) as u64)
        .read_to_string(&mut buf)
        .map_err(|e| OutcomeError::fail(format!("cannot read stdin: {e}")))?;
    evaluate_json(&buf).await
}

pub(crate) async fn evaluate_json(text: &str) -> Result<serde_json::Value, OutcomeError> {
    let input = parse_eval_input(text)?;
    let state = serde_json::json!({
        "task": input.task,
        "evidence": input.evidence.iter().map(|e| serde_json::json!({"id": e.id, "text": e.text})).collect::<Vec<_>>(),
    });
    let (answers, cost, provider, model) =
        crate::route::judge_evaluation(state, outcome_questions())
            .await
            .map_err(|e| OutcomeError::fail(format!("judge unavailable: {e}")))?;
    build_output(&input, &answers, cost, &provider, &model)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_base(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "agentdeck-outcomes-test-{}-{}",
            std::process::id(),
            tag
        ));
        let _ = std::fs::remove_dir_all(&base);
        base
    }

    fn rec(task: &str) -> Record {
        Record {
            task: task.into(),
            provider: "testprov".into(),
            model: "test-model".into(),
            category: "implementation".into(),
            verdict: "ok".into(),
            evidence: "direct".into(),
        }
    }

    #[test]
    fn rejects_bad_enums_and_ids() {
        assert!(
            parse_record(
                &[
                    "--task",
                    "t",
                    "--provider",
                    "p",
                    "--model",
                    "m",
                    "--category",
                    "nope",
                    "--verdict",
                    "ok",
                    "--evidence",
                    "direct"
                ]
                .map(String::from)
            )
            .is_err()
        );
        assert!(
            parse_record(
                &[
                    "--task",
                    "t",
                    "--provider",
                    "p",
                    "--model",
                    "m",
                    "--category",
                    "test",
                    "--verdict",
                    "great",
                    "--evidence",
                    "direct"
                ]
                .map(String::from)
            )
            .is_err()
        );
        assert!(
            parse_record(
                &[
                    "--task",
                    "../evil",
                    "--provider",
                    "p",
                    "--model",
                    "m",
                    "--category",
                    "test",
                    "--verdict",
                    "ok",
                    "--evidence",
                    "direct"
                ]
                .map(String::from)
            )
            .is_err()
        );
        assert!(
            parse_record(
                &[
                    "--task",
                    "",
                    "--provider",
                    "p",
                    "--model",
                    "m",
                    "--category",
                    "test",
                    "--verdict",
                    "ok",
                    "--evidence",
                    "direct"
                ]
                .map(String::from)
            )
            .is_err()
        );
        // missing/blocked/unavailable are valid and distinct from fail
        for v in ["ok", "partial", "fail", "blocked", "missing", "unavailable"] {
            let a = [
                "--task",
                "t",
                "--provider",
                "p",
                "--model",
                "m",
                "--category",
                "test",
                "--verdict",
                v,
                "--evidence",
                "direct",
            ]
            .map(String::from);
            assert!(parse_record(&a).is_ok(), "{v}");
        }
    }

    #[test]
    fn accepts_percent_encoded_t3_task_ids() {
        // Literal observed T3 delegated-task ID (127 chars): must be accepted.
        let id = "node:delegated-task:command%3Amcp%3Ad4694301-3125-4b66-8d54-2b3158afcf5a%3Adelegate-task%3A500f3994-4df3-4306-a536-05d9bc83f263";
        assert_eq!(id.len(), 127);
        let a = [
            "--task",
            id,
            "--provider",
            "p",
            "--model",
            "m",
            "--category",
            "test",
            "--verdict",
            "ok",
            "--evidence",
            "direct",
        ]
        .map(String::from);
        assert!(parse_record(&a).is_ok());
        // Bounds still hold: 256 ok, 257 rejected; traversal still rejected.
        let max = "t".repeat(256);
        let over = "t".repeat(257);
        for (task, ok) in [
            (max.as_str(), true),
            (over.as_str(), false),
            ("a/b", false),
            ("a%b%3A", true),
        ] {
            let a = [
                "--task",
                task,
                "--provider",
                "p",
                "--model",
                "m",
                "--category",
                "test",
                "--verdict",
                "ok",
                "--evidence",
                "direct",
            ]
            .map(String::from);
            assert_eq!(parse_record(&a).is_ok(), ok, "{task}");
        }
    }

    #[test]
    fn append_refuses_incomplete_tail_without_changing_it() {
        let base = tmp_base("append-torn");
        let path = ensure_parent(&base).unwrap();
        std::fs::write(&path, b"{\"partial\":").unwrap();
        assert!(append_to(&base, &rec("task-1"), 1).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"{\"partial\":");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn roundtrip_keeps_revisions() {
        let base = tmp_base("roundtrip");
        append_to(&base, &rec("task-1"), 1).unwrap();
        append_to(&base, &rec("task-1"), 2).unwrap(); // revision
        let items = read_all(&base).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[1]["t"], 2);
        assert_eq!(items[0]["source"], "coordinator");
        assert!(items[0].get("decider").is_some());
        // no free-text payload stored
        assert!(items[0].get("summary").is_none());
        let _ = std::fs::remove_dir_all(&base);
    }

    fn decider_eval(
        verdict: &str,
        confidence: serde_json::Value,
        evidence: &[&str],
        judge: serde_json::Value,
        self_judged: serde_json::Value,
        cost: serde_json::Value,
    ) -> String {
        serde_json::json!({
            "verdict": verdict,
            "confidence": confidence,
            "evidenceConsidered": evidence,
            "judge": judge,
            "selfJudged": self_judged,
            "judgeCostUsd": cost,
        })
        .to_string()
    }

    fn good_eval() -> String {
        decider_eval(
            "pass",
            serde_json::json!(0.81),
            &["a", "b"],
            serde_json::json!({"provider": "openrouter", "model": "t/judge"}),
            serde_json::Value::Null,
            serde_json::json!(0.001),
        )
    }

    #[test]
    fn decider_roundtrip_stores_metadata_only() {
        let base = tmp_base("decider-roundtrip");
        let eval = parse_decider_eval(&good_eval()).unwrap();
        record_decider_to(&base, "task-9", "test", Some("p"), "perf/m", &eval, 7).unwrap();
        let items = read_all(&base).unwrap();
        assert_eq!(items.len(), 1);
        let v = &items[0];
        assert_eq!(v["source"], "decider");
        assert_eq!(v["task_id"], "task-9");
        assert_eq!(v["category"], "test");
        assert_eq!(v["provider"], "p");
        assert_eq!(v["model"], "perf/m");
        assert_eq!(v["verdict"], "pass");
        assert_eq!(v["confidence"], 0.81);
        assert_eq!(v["evidence_ids"], serde_json::json!(["a", "b"]));
        assert_eq!(v["evidence_count"], 2);
        assert_eq!(
            v["decider"]["judge"],
            serde_json::json!({"provider": "openrouter", "model": "t/judge"})
        );
        assert!(v["decider"]["self_judged"].is_null());
        assert_eq!(v["decider"]["judge_cost_usd"], 0.001);
        // No raw text, summaries or secrets leak into the stored line.
        let raw = std::fs::read_to_string(outcomes_path(&base)).unwrap();
        assert!(!raw.contains("did a"));
        for key in ["task\"", "summary", "secret", "text"] {
            assert!(!raw.contains(&format!("\"{key}")), "{key}");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn coordinator_and_decider_records_stay_distinct() {
        let base = tmp_base("decider-distinct");
        append_to(&base, &rec("task-1"), 1).unwrap();
        let eval = parse_decider_eval(&good_eval()).unwrap();
        record_decider_to(&base, "task-1", "implementation", Some("testprov"), "test-model", &eval, 2)
            .unwrap();
        let items = read_all(&base).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["source"], "coordinator");
        assert!(items[0]["decider"].is_null());
        assert_eq!(items[1]["source"], "decider");
        assert_eq!(items[1]["verdict"], "pass"); // decider scale, incl. insufficient
        let insufficient = decider_eval(
            "insufficient",
            serde_json::json!(0.2),
            &["a"],
            serde_json::json!({"provider": "openrouter", "model": "t/judge"}),
            serde_json::Value::Null,
            serde_json::Value::Null,
        );
        let eval = parse_decider_eval(&insufficient).unwrap();
        record_decider_to(&base, "task-2", "test", None, "m", &eval, 3).unwrap();
        let items = read_all(&base).unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[2]["verdict"], "insufficient");
        assert!(items[2]["provider"].is_null()); // unknown performer stays null
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn decider_preserves_unknown_self_judgment_and_cost() {
        let base = tmp_base("decider-unknown");
        // Self-evaluation keeps true; unknown stays null, never false.
        let same = decider_eval(
            "pass",
            serde_json::json!(0.6),
            &["e1"],
            serde_json::json!({"provider": "p", "model": "j/judge"}),
            serde_json::json!(true),
            serde_json::Value::Null,
        );
        let eval = parse_decider_eval(&same).unwrap();
        assert!(eval.self_judged);
        assert!(eval.cost.is_none());
        record_decider_to(&base, "t", "test", Some("p"), "j/judge", &eval, 1).unwrap();
        let items = read_all(&base).unwrap();
        assert_eq!(items[0]["decider"]["self_judged"], true);
        assert!(items[0]["decider"]["judge_cost_usd"].is_null());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn decider_rejects_bad_results_without_writing() {
        let judge = serde_json::json!({"provider": "openrouter", "model": "t/judge"});
        let bad = [
            decider_eval("great", serde_json::json!(0.5), &["a"], judge.clone(), serde_json::Value::Null, serde_json::Value::Null),
            decider_eval("pass", serde_json::json!(2.0), &["a"], judge.clone(), serde_json::Value::Null, serde_json::Value::Null),
            decider_eval("pass", serde_json::json!("high"), &["a"], judge.clone(), serde_json::Value::Null, serde_json::Value::Null),
            decider_eval("pass", serde_json::json!(0.5), &[], judge.clone(), serde_json::Value::Null, serde_json::Value::Null),
            decider_eval("pass", serde_json::json!(0.5), &["a/b"], judge.clone(), serde_json::Value::Null, serde_json::Value::Null),
            decider_eval("pass", serde_json::json!(0.5), &["a", "a"], judge.clone(), serde_json::Value::Null, serde_json::Value::Null),
            decider_eval("pass", serde_json::json!(0.5), &["a"], serde_json::json!({"provider": "p"}), serde_json::Value::Null, serde_json::Value::Null),
            decider_eval("pass", serde_json::json!(0.5), &["a"], judge.clone(), serde_json::json!(false), serde_json::Value::Null),
            decider_eval("pass", serde_json::json!(0.5), &["a"], judge.clone(), serde_json::json!("no"), serde_json::Value::Null),
            decider_eval("pass", serde_json::json!(0.5), &["a"], judge.clone(), serde_json::Value::Null, serde_json::json!(-0.1)),
            decider_eval("pass", serde_json::json!(0.5), &["a"], judge.clone(), serde_json::Value::Null, serde_json::json!("cheap")),
            "not json".to_string(),
        ];
        for (n, text) in bad.iter().enumerate() {
            assert!(parse_decider_eval(text).is_err(), "case {n}: {text}");
        }
        // Nothing reaches the file: validation happens before any append.
        let base = tmp_base("decider-invalid");
        for text in &bad {
            if let Ok(eval) = parse_decider_eval(text) {
                panic!("should not parse: {eval:?}");
            }
        }
        assert!(!outcomes_path(&base).exists());
        // Bad task/category/performer flags also fail without writing.
        let eval = parse_decider_eval(&good_eval()).unwrap();
        assert!(
            record_decider_to(&base, "../evil", "test", Some("p"), "m", &eval, 1).is_err()
        );
        assert!(
            record_decider_to(&base, "t", "nope", Some("p"), "m", &eval, 1).is_err()
        );
        assert!(!outcomes_path(&base).exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn decider_accepts_real_t3_ids() {
        let id = "node:delegated-task:command%3Amcp%3Ad4694301-3125-4b66-8d54-2b3158afcf5a%3Adelegate-task%3A500f3994-4df3-4306-a536-05d9bc83f263";
        let base = tmp_base("decider-t3ids");
        let eval = parse_decider_eval(&good_eval()).unwrap();
        record_decider_to(
            &base,
            id,
            "implementation",
            Some("opencode-go"),
            "opencode-go/muse-spark-1.3-contributor",
            &eval,
            1,
        )
        .unwrap();
        let items = read_all(&base).unwrap();
        assert_eq!(items[0]["task_id"], id);
        assert_eq!(items[0]["model"], "opencode-go/muse-spark-1.3-contributor");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn corrupt_lines_fail_and_data_survives() {
        let base = tmp_base("corrupt");
        append_to(&base, &rec("task-1"), 1).unwrap();
        let before = std::fs::read(outcomes_path(&base)).unwrap();
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(outcomes_path(&base))
                .unwrap();
            f.write_all(b"{\"task_id\": broken\n").unwrap();
        }
        let err = read_all(&base).unwrap_err();
        assert!(err.message.contains(":2:"), "{}", err.message);
        // Failed reads never delete or modify existing data.
        assert_eq!(
            std::fs::read(outcomes_path(&base)).unwrap()[..before.len()],
            before[..]
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn torn_final_line_is_skipped() {
        let base = tmp_base("torn");
        append_to(&base, &rec("task-1"), 1).unwrap();
        // Torn tail of a crashed append: no trailing newline, must not break reads.
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(outcomes_path(&base))
                .unwrap();
            f.write_all(b"{\"task_id\": tor").unwrap();
        }
        let items = read_all(&base).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["task_id"], "task-1");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn missing_store_is_empty_but_unreadable_is_an_error() {
        let base = tmp_base("missing");
        assert!(read_all(&base).unwrap().is_empty());
        // outcomes.jsonl path blocked by a directory: read must fail, not read empty.
        std::fs::create_dir_all(outcomes_path(&base)).unwrap();
        assert!(read_all(&base).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn private_permissions() {
        use std::os::unix::fs::MetadataExt;
        let base = tmp_base("perms");
        append_to(&base, &rec("t"), 1).unwrap();
        let mode = std::fs::metadata(outcomes_path(&base)).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let dir_mode = std::fs::metadata(base.join("model-route")).unwrap().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn concurrent_appends_keep_whole_lines() {
        let base = tmp_base("concurrent");
        std::thread::scope(|s| {
            for n in 0..16 {
                let b = base.clone();
                s.spawn(move || {
                    append_to(&b, &rec(&format!("task-{n}")), 1).unwrap();
                });
            }
        });
        let items = read_all(&base).unwrap();
        assert_eq!(items.len(), 16);
        let _ = std::fs::remove_dir_all(&base);
    }

    fn eval_input(task: &str, ids: &[&str]) -> String {
        let ev: Vec<String> = ids
            .iter()
            .map(|id| format!(r#"{{"id": "{id}", "text": "did {id}"}}"#))
            .collect();
        format!(r#"{{"task": "{task}", "evidence": [{}]}}"#, ev.join(","))
    }

    #[test]
    fn evaluate_rejects_bad_input() {
        assert!(parse_eval_input("not json").is_err());
        assert!(parse_eval_input(&eval_input("", &["e1"])).is_err());
        assert!(parse_eval_input(&eval_input("t", &[])).is_err());
        let many = ["e"; 9];
        assert!(parse_eval_input(&eval_input("t", &many)).is_err());
        assert!(parse_eval_input(&eval_input("t", &["a/b"])).is_err());
        assert!(parse_eval_input(&eval_input("t", &["e1", "e1"])).is_err());
        assert!(parse_eval_input(r#"{"task": "t"}"#).is_err());
        // Oversize stdin and evidence text are usage errors, never sent anywhere.
        assert!(parse_eval_input(&"x".repeat(MAX_EVAL_STDIN + 1)).is_err());
        let big = "y".repeat(MAX_EVAL_TEXT + 1);
        assert!(
            parse_eval_input(&format!(
                r#"{{"task": "t", "evidence": [{{"id": "e1", "text": "{big}"}}]}}"#
            ))
            .is_err()
        );
        // Bad performer identity is rejected before any judge call.
        assert!(parse_eval_input(
            r#"{"task": "t", "evidence": [{"id": "e1", "text": "x"}], "performer": {"model": "a/b c"}}"#
        )
        .is_err());
        assert!(parse_eval_input(&eval_input("do it", &["e1"])).is_ok());
    }

    #[test]
    fn evaluate_maps_verdicts_and_never_echoes_raw_text() {
        let input = parse_eval_input(&eval_input("ship it", &["a", "b"])).unwrap();
        for (score, verdict) in [
            (0.0, "fail"),
            (1.0, "partial"),
            (2.0, "pass"),
            (3.0, "insufficient"),
        ] {
            let answers = serde_json::json!({"outcome": {"score": score, "confidence": 0.7}});
            let out = build_output(&input, &answers, Some(0.002), "openrouter", "j/judge").unwrap();
            assert_eq!(out["verdict"], verdict);
            assert_eq!(out["confidence"], 0.7);
            assert_eq!(out["evidenceConsidered"], serde_json::json!(["a", "b"]));
            assert_eq!(
                out["judge"],
                serde_json::json!({"provider": "openrouter", "model": "j/judge"})
            );
            assert!(out["selfJudged"].is_null()); // performer unknown
            assert_eq!(out["judgeCostUsd"], 0.002);
            // No raw evidence text leaks into the structured result.
            assert!(!serde_json::to_string(&out).unwrap().contains("did a"));
        }
        let ambiguous = serde_json::json!({"outcome": {"score": 1.5, "confidence": 0.7}});
        assert_eq!(
            build_output(&input, &ambiguous, None, "p", "m").unwrap()["verdict"],
            "insufficient"
        );
        let ambiguous = serde_json::json!({"outcome": {"score": 2.9, "confidence": 0.7}});
        assert_eq!(
            build_output(&input, &ambiguous, None, "p", "m").unwrap()["verdict"],
            "insufficient"
        );
        // Unknown cost reports null, never a fabricated zero.
        let answers = serde_json::json!({"outcome": {"score": 2.0, "confidence": 0.9}});
        let out = build_output(&input, &answers, None, "openrouter", "j/judge").unwrap();
        assert!(out["judgeCostUsd"].is_null());
        // Invalid judge answers fail safely instead of guessing a verdict.
        assert!(build_output(&input, &serde_json::json!({}), None, "p", "m").is_err());
        assert!(
            build_output(
                &input,
                &serde_json::json!({"outcome": {"score": 9.0, "confidence": 0.5}}),
                None,
                "p",
                "m"
            )
            .is_err()
        );
    }

    #[test]
    fn evaluate_flags_self_judgment_on_exact_model_match() {
        let same = parse_eval_input(
            r#"{"task": "t", "evidence": [{"id": "e1", "text": "x"}], "performer": {"model": "j/judge"}}"#,
        )
        .unwrap();
        let other = parse_eval_input(
            r#"{"task": "t", "evidence": [{"id": "e1", "text": "x"}], "performer": {"provider": "p", "model": "other/m"}}"#,
        )
        .unwrap();
        let answers = serde_json::json!({"outcome": {"score": 2.0, "confidence": 0.6}});
        let out = build_output(&same, &answers, None, "openrouter", "j/judge").unwrap();
        assert_eq!(out["selfJudged"], true);
        let out = build_output(&other, &answers, None, "openrouter", "j/judge").unwrap();
        assert!(out["selfJudged"].is_null());
    }

    #[tokio::test]
    async fn evaluate_end_to_end_through_mock_judge() {
        let dir = std::env::temp_dir().join(format!("agentdeck-eval-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("router.json"),
            r#"{"pool": [], "accounts": {}, "version": 1, "decisionModel": "t/judge"}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("jev.json"),
            r#"{"answers": {"size": {"score": 0, "confidence": 0.9}, "difficulty": {"score": 0, "confidence": 0.8}, "outcome": {"score": 2, "confidence": 0.81}}, "usage": {"cost": 0.001}}"#,
        )
        .unwrap();
        // SAFETY: test-only overrides read by judge_evaluation/jev_fetch; restored below.
        // The mock file also carries routing answers so a concurrent probe test
        // reading it still passes; retries cover us reading the probe's file.
        unsafe {
            std::env::set_var("MODEL_ROUTE_ROUTER_JSON", dir.join("router.json"));
            std::env::set_var("AGENTDECK_ROUTE_MOCK_JEV", dir.join("jev.json"));
        }
        let mut out = Err("unset".to_string());
        for _ in 0..10 {
            match evaluate_json(&eval_input("ship it", &["a"])).await {
                Ok(v) => {
                    out = Ok(v);
                    break;
                }
                Err(e) => out = Err(e.message.clone()),
            }
            unsafe {
                std::env::set_var("AGENTDECK_ROUTE_MOCK_JEV", dir.join("jev.json"));
            }
        }
        unsafe {
            std::env::remove_var("MODEL_ROUTE_ROUTER_JSON");
            std::env::remove_var("AGENTDECK_ROUTE_MOCK_JEV");
        }
        let out = out.unwrap();
        assert_eq!(out["verdict"], "pass");
        assert_eq!(out["evidenceConsidered"], serde_json::json!(["a"]));
        assert_eq!(
            out["judge"],
            serde_json::json!({"provider": "openrouter", "model": "t/judge"})
        );
        assert_eq!(out["judgeCostUsd"], 0.001);
        assert!(!serde_json::to_string(&out).unwrap().contains("did a"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
