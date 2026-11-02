//! Observational coordinator outcome store (no routing effect).
//!
//! `agentdeck route outcomes record|list`: append-only JSONL at
//! `XDG_STATE_HOME/model-route/outcomes.jsonl` (dir `0700`, file `0600`).
//! One line per assessment; repeats for the same task are revisions, newest
//! last. Only metadata is stored: no task text, summaries or evidence text.

use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

pub const USAGE: &str = "usage: agentdeck route outcomes record --task ID --provider P --model M --category C --verdict V --evidence E [--source coordinator]\n  agentdeck route outcomes list [--task ID] [--limit N]\n\n  category: implementation|research|review|design|test|general\n  verdict: ok|partial|fail|blocked|missing|unavailable (blocked/missing/unavailable are not failures)\n  evidence: direct|spot-check|test-output|child-report\n  source is always recorded as \"coordinator\"; decider metadata is reserved (null) for future use.\n  repeats for one task append a new revision; appends are file-locked; corrupt lines fail reads (a torn final line is skipped), never deleted.";

const CATEGORIES: &[&str] = &[
    "implementation",
    "research",
    "review",
    "design",
    "test",
    "general",
];
const VERDICTS: &[&str] = &[
    "ok",
    "partial",
    "fail",
    "blocked",
    "missing",
    "unavailable",
];
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
pub fn run(argv: &[String]) -> i32 {
    match run_inner(argv) {
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

fn run_inner(argv: &[String]) -> Result<serde_json::Value, OutcomeError> {
    match argv.first().map(String::as_str) {
        Some("record") => record_cli(&argv[1..]),
        Some("list") => list_cli(&argv[1..]),
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
        return Err(OutcomeError::usage(format!(
            "{what} must be 1-{max} chars"
        )));
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || extra.contains(&c))
    {
        return Err(OutcomeError::usage(format!("{what} has invalid characters")));
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
            return Err(OutcomeError::fail(format!("cannot {what} outcomes file: {e}")));
        }
    }
}

fn append_to(
    base: &Path,
    r: &Record,
    now: u64,
) -> Result<PathBuf, OutcomeError> {
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
        .mode(0o600)
        .open(&path)
        .map_err(|e| OutcomeError::fail(e.to_string()))?;
    lock_file(&f, libc::LOCK_EX, "lock")?;
    let r = f
        .write_all(text.as_bytes())
        .map_err(|e| OutcomeError::fail(e.to_string()))
        .and_then(|()| {
            f.sync_all()
                .map_err(|e| OutcomeError::fail(format!("cannot persist {}: {e}", path.display())))
        })
        .and_then(|()| {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).map_err(|e| {
                OutcomeError::fail(format!("cannot secure {}: {e}", path.display()))
            })
        });
    drop(f); // releases LOCK_EX
    r?;
    Ok(path)
}

fn valid_record(v: &serde_json::Value) -> bool {
    v.get("task_id").and_then(|t| t.as_str()).is_some_and(|t| !t.is_empty())
        && v.get("verdict").and_then(|t| t.as_str()).is_some_and(|t| VERDICTS.contains(&t))
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
            OutcomeError::fail(format!("{}:{n}: invalid JSON ({e}); file left unchanged", path.display()))
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
                    .ok_or_else(|| {
                        OutcomeError::usage(format!("limit must be 1-{MAX_LIST}"))
                    })?;
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
        assert!(parse_record(&["--task", "t", "--provider", "p", "--model", "m", "--category", "nope", "--verdict", "ok", "--evidence", "direct"].map(String::from)).is_err());
        assert!(parse_record(&["--task", "t", "--provider", "p", "--model", "m", "--category", "test", "--verdict", "great", "--evidence", "direct"].map(String::from)).is_err());
        assert!(parse_record(&["--task", "../evil", "--provider", "p", "--model", "m", "--category", "test", "--verdict", "ok", "--evidence", "direct"].map(String::from)).is_err());
        assert!(parse_record(&["--task", "", "--provider", "p", "--model", "m", "--category", "test", "--verdict", "ok", "--evidence", "direct"].map(String::from)).is_err());
        // missing/blocked/unavailable are valid and distinct from fail
        for v in ["ok", "partial", "fail", "blocked", "missing", "unavailable"] {
            let a = ["--task", "t", "--provider", "p", "--model", "m", "--category", "test", "--verdict", v, "--evidence", "direct"].map(String::from);
            assert!(parse_record(&a).is_ok(), "{v}");
        }
    }

    #[test]
    fn accepts_percent_encoded_t3_task_ids() {
        // Literal observed T3 delegated-task ID (127 chars): must be accepted.
        let id = "node:delegated-task:command%3Amcp%3Ad4694301-3125-4b66-8d54-2b3158afcf5a%3Adelegate-task%3A500f3994-4df3-4306-a536-05d9bc83f263";
        assert_eq!(id.len(), 127);
        let a = ["--task", id, "--provider", "p", "--model", "m", "--category", "test", "--verdict", "ok", "--evidence", "direct"].map(String::from);
        assert!(parse_record(&a).is_ok());
        // Bounds still hold: 256 ok, 257 rejected; traversal still rejected.
        let max = "t".repeat(256);
        let over = "t".repeat(257);
        for (task, ok) in [(max.as_str(), true), (over.as_str(), false), ("a/b", false), ("a%b%3A", true)] {
            let a = ["--task", task, "--provider", "p", "--model", "m", "--category", "test", "--verdict", "ok", "--evidence", "direct"].map(String::from);
            assert_eq!(parse_record(&a).is_ok(), ok, "{task}");
        }
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
        assert_eq!(std::fs::read(outcomes_path(&base)).unwrap()[..before.len()], before[..]);
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
}
