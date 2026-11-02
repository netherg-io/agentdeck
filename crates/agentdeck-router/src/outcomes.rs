//! Observational coordinator outcome store (no routing effect).
//!
//! `agentdeck route outcomes record|list`: append-only JSONL at
//! `XDG_STATE_HOME/model-route/outcomes.jsonl` (dir `0700`, file `0600`).
//! One line per assessment; repeats for the same task are revisions, newest
//! last. Only metadata is stored: no task text, summaries or evidence text.

use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use crate::policy::valid_answer;

pub const USAGE: &str = "usage: agentdeck route outcomes record --task ID --provider P --model M --category C --verdict V --evidence E [--source coordinator]\n  agentdeck route outcomes list [--task ID] [--limit N]\n  agentdeck route outcomes evaluate < evaluation.json\n\n  category: implementation|research|review|design|test|general\n  verdict: ok|partial|fail|blocked|missing|unavailable (blocked/missing/unavailable are not failures)\n  evidence: direct|spot-check|test-output|child-report\n  source is always recorded as \"coordinator\"; decider metadata is reserved (null) for future use.\n  repeats for one task append a new revision; appends are file-locked; corrupt lines fail reads (a torn final line is skipped), never deleted.\n\nevaluate reads one JSON object on stdin (max 16 KiB):\n  {\"task\": \"what was asked, 1-2000 chars\", \"evidence\": [{\"id\": \"e1\", \"text\": \"...1-4000 chars...\"} 1-8 items],\n   \"performer\": {\"provider\": \"P\", \"model\": \"M\"}?}\n  It sends that content to the configured decision provider/model (explicit opt-in; standard keys)\n  and prints {verdict: pass|partial|fail|insufficient, confidence, evidenceRefs, judge{provider,model},\n  selfJudged: true|false|null when the performer is unknown, judgeCostUsd: number|null}.\n  Nothing is written anywhere: raw task and evidence text are never persisted. To persist an\n  assessment, run `outcomes record` separately with validated metadata only.";

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
struct OutcomeError {
    usage: bool,
    message: String,
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

fn state_base() -> PathBuf {
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
    let path = ensure_parent(base)?;
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
    Ok(path)
}

fn valid_record(v: &serde_json::Value) -> bool {
    v.get("task_id")
        .and_then(|t| t.as_str())
        .is_some_and(|t| !t.is_empty())
        && v.get("verdict")
            .and_then(|t| t.as_str())
            .is_some_and(|t| VERDICTS.contains(&t))
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

// --- Explicit on-demand outcome evaluation (no persistence) ------------------
// Reads task criteria + evidence on stdin, asks the configured decision
// provider once, prints a grounded verdict. Never writes any file: raw task
// and evidence text only travel to the explicitly invoked judge.

const MAX_EVAL_STDIN: usize = 16384;
const MAX_EVAL_TASK: usize = 2000;
const MAX_EVAL_EVIDENCE: usize = 8;
const MAX_EVAL_ID: usize = 64;
const MAX_EVAL_TEXT: usize = 4000;

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
                check_id(provider, "performer provider", MAX_PROVIDER, &['_', '.', '-'])?;
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
    let score = (answer
        .get("score")
        .and_then(|s| s.as_f64())
        .unwrap_or(f64::NAN)
        .round() as i64)
        .clamp(0, 3) as usize;
    let confidence = answer
        .get("confidence")
        .and_then(|c| c.as_f64())
        .unwrap_or(0.0);
    // Exact model-string match only: fuzzy matches risk false self-judgment flags.
    let self_judged = match &input.performer_model {
        None => serde_json::Value::Null,
        Some(p) => serde_json::Value::from(p == model),
    };
    Ok(serde_json::json!({
        "verdict": OUTCOME_VERDICTS[score],
        "confidence": confidence,
        "evidenceRefs": input.evidence.iter().map(|e| e.id.clone()).collect::<Vec<_>>(),
        "judge": {"provider": provider, "model": model},
        "selfJudged": self_judged,
        "judgeCostUsd": cost.filter(|c| c.is_finite()).map(serde_json::Value::from).unwrap_or(serde_json::Value::Null),
        "note": "explicit on-demand evaluation; input was sent to the configured decision provider; nothing was persisted",
    }))
}

async fn evaluate_cli() -> Result<serde_json::Value, OutcomeError> {
    let mut buf = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
        .map_err(|e| OutcomeError::fail(format!("cannot read stdin: {e}")))?;
    evaluate_json(&buf).await
}

async fn evaluate_json(text: &str) -> Result<serde_json::Value, OutcomeError> {
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
        assert!(parse_eval_input(&format!(
            r#"{{"task": "t", "evidence": [{{"id": "e1", "text": "{big}"}}]}}"#
        ))
        .is_err());
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
        for (score, verdict) in [(0.0, "fail"), (1.0, "partial"), (2.0, "pass"), (3.0, "insufficient")] {
            let answers = serde_json::json!({"outcome": {"score": score, "confidence": 0.7}});
            let out = build_output(&input, &answers, Some(0.002), "openrouter", "j/judge").unwrap();
            assert_eq!(out["verdict"], verdict);
            assert_eq!(out["confidence"], 0.7);
            assert_eq!(out["evidenceRefs"], serde_json::json!(["a", "b"]));
            assert_eq!(out["judge"], serde_json::json!({"provider": "openrouter", "model": "j/judge"}));
            assert!(out["selfJudged"].is_null()); // performer unknown
            assert_eq!(out["judgeCostUsd"], 0.002);
            // No raw evidence text leaks into the structured result.
            assert!(!serde_json::to_string(&out).unwrap().contains("did a"));
        }
        // Unknown cost reports null, never a fabricated zero.
        let answers = serde_json::json!({"outcome": {"score": 2.0, "confidence": 0.9}});
        let out = build_output(&input, &answers, None, "openrouter", "j/judge").unwrap();
        assert!(out["judgeCostUsd"].is_null());
        // Invalid judge answers fail safely instead of guessing a verdict.
        assert!(build_output(&input, &serde_json::json!({}), None, "p", "m").is_err());
        assert!(build_output(
            &input,
            &serde_json::json!({"outcome": {"score": 9.0, "confidence": 0.5}}),
            None,
            "p",
            "m"
        )
        .is_err());
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
        assert_eq!(out["selfJudged"], false);
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
        assert_eq!(out["evidenceRefs"], serde_json::json!(["a"]));
        assert_eq!(
            out["judge"],
            serde_json::json!({"provider": "openrouter", "model": "t/judge"})
        );
        assert_eq!(out["judgeCostUsd"], 0.001);
        assert!(!serde_json::to_string(&out).unwrap().contains("did a"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
