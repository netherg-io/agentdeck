//! Read-only view of router assessment metadata (`outcomes.jsonl`).
//!
//! The router appends one JSON object per coordinator or decider assessment
//! at `$XDG_STATE_HOME/model-route/outcomes.jsonl`. This module only reads the
//! file tail (bounded) and reports it for display: it never writes, deletes,
//! repairs or cleans up the file, and the card states plainly that these
//! observations do not influence routing.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Bytes scanned from the file tail; older records stay out of view.
const MAX_BYTES: u64 = 256 * 1024;
/// Records kept after parsing (newest last); the card renders fewer.
pub const MAX_KEPT: usize = 200;
/// Rows the card renders at most (newest first).
pub const MAX_SHOWN: usize = 50;

const CATEGORIES: &[&str] = &[
    "implementation",
    "research",
    "review",
    "design",
    "test",
    "general",
];
const COORD_VERDICTS: &[&str] = &["ok", "partial", "fail", "blocked", "missing", "unavailable"];
const DEC_VERDICTS: &[&str] = &["pass", "partial", "fail", "insufficient"];

/// Who produced the assessment. Shown verbatim; never ranked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Source {
    #[default]
    Coordinator,
    Decider,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Coordinator => "coordinator",
            Self::Decider => "decider",
        }
    }
}

/// One stored assessment, metadata only: no task or evidence text is kept.
#[derive(Debug, Clone)]
pub struct Entry {
    /// Unix epoch seconds from the record (`0` when absent; store predates it).
    pub t: u64,
    pub source: Source,
    pub task_id: String,
    /// `None` means the performer was unreported (decider records only).
    pub provider: Option<String>,
    pub model: String,
    pub category: String,
    pub verdict: String,
    /// Coordinator evidence kind (`direct`, …); decider rows use the count.
    pub evidence: String,
    pub evidence_count: usize,
    /// `None` means unknown (never `false`: absence of proof is not proof).
    pub self_judged: Option<bool>,
    /// `None` means the judge did not report a cost.
    pub cost: Option<f64>,
    /// Decider confidence (0-1) when the judge reported one.
    pub confidence: Option<f64>,
    /// Workflow join keys: present only when a decider record came from a
    /// workflow task evaluation (`basis` "child-report"). Plain decider and
    /// coordinator records leave both `None`.
    pub run_id: Option<String>,
    pub step: Option<String>,
    /// Decider judge as `provider/model`.
    pub judge: Option<String>,
}

/// Read outcome that keeps missing / corrupt / unreadable states apart.
#[derive(Debug, Clone, Default)]
pub enum History {
    /// No assessment file exists yet: nothing has been recorded.
    #[default]
    Missing,
    /// File exists but holds no records.
    Empty,
    /// Newest record last, at most [`MAX_KEPT`].
    Loaded(Vec<Entry>),
    /// A complete line is not an outcome record; the file was left unchanged.
    /// The line number is relative to the scanned tail when truncated.
    Corrupt {
        line: usize,
        what: String,
    },
    Unreadable(String),
}

/// Same base resolution as the router store (`XDG_STATE_HOME`, else
/// `~/.local/state`, else the temp dir).
pub fn state_base() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(std::env::temp_dir)
                .join(".local/state")
        })
}

fn path_for(base: &Path) -> PathBuf {
    base.join("model-route/outcomes.jsonl")
}

/// Read the production store. Off the UI thread via the settings `load`.
pub fn load() -> History {
    load_from_base(&state_base())
}

pub fn load_from_base(base: &Path) -> History {
    let path = path_for(base);
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return History::Missing,
        Err(e) => return History::Unreadable(format!("{}: {e}", path.display())),
    };
    read_history(file)
}

fn read_history(mut file: std::fs::File) -> History {
    let len = file.metadata().map(|m| m.len()).unwrap_or(u64::MAX);
    let mut text = String::new();
    // Bounded tail: skip whole, then drop the partial first line.
    let skipped = len.saturating_sub(MAX_BYTES);
    if skipped > 0 {
        if file.seek(SeekFrom::Start(skipped)).is_err() {
            return History::Unreadable("could not seek assessment file".to_string());
        }
        // shortcut: the tail window starts mid-line; resync at the next newline.
        let mut leaders = Vec::new();
        if file.take(MAX_BYTES + 1).read_to_end(&mut leaders).is_err()
            || String::from_utf8(leaders.clone()).is_err()
        {
            return History::Unreadable("could not read assessment file".to_string());
        }
        let s = String::from_utf8(leaders).unwrap_or_default();
        text = s
            .split_once('\n')
            .map(|(_, rest)| rest)
            .unwrap_or("")
            .to_string();
    } else if file.read_to_string(&mut text).is_err() {
        return History::Unreadable("could not read assessment file".to_string());
    }
    parse_text(&text)
}

/// Split lines like the store: a torn final line (no trailing `\n`) is kept
/// only when it still parses as a record, otherwise dropped silently.
fn parse_text(text: &str) -> History {
    if text.is_empty() {
        return History::Empty;
    }
    let complete = text.ends_with('\n');
    let mut lines: Vec<&str> = text.lines().collect();
    let torn = if complete { None } else { lines.pop() };
    let mut entries = Vec::new();
    for (n, line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match parse_line(line) {
            Ok(entry) => entries.push(entry),
            Err(what) => return History::Corrupt { line: n + 1, what },
        }
    }
    if let Some(tail) = torn
        && !tail.trim().is_empty()
        && let Ok(entry) = parse_line(tail)
    {
        entries.push(entry);
    }
    if entries.is_empty() {
        return History::Empty;
    }
    if entries.len() > MAX_KEPT {
        entries = entries.split_off(entries.len() - MAX_KEPT);
    }
    History::Loaded(entries)
}

fn str_field(v: &serde_json::Value, key: &str) -> Result<String, String> {
    v.get(key)
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("missing {key}"))
}

/// Metadata only: IDs and counts, never task or evidence text.
fn parse_line(line: &str) -> Result<Entry, String> {
    let v: serde_json::Value =
        serde_json::from_str(line).map_err(|e| format!("invalid JSON ({e})"))?;
    let source = match v.get("source").and_then(|s| s.as_str()) {
        Some("coordinator") => Source::Coordinator,
        Some("decider") => Source::Decider,
        _ => return Err("not an outcome record: bad source".to_string()),
    };
    let task_id = str_field(&v, "task_id")?;
    let category = str_field(&v, "category")?;
    if !CATEGORIES.contains(&category.as_str()) {
        return Err("not an outcome record: bad category".to_string());
    }
    let verdict = str_field(&v, "verdict")?;
    let provider = v
        .get("provider")
        .and_then(|p| p.as_str())
        .filter(|p| !p.is_empty())
        .map(str::to_string);
    let model = str_field(&v, "model")?;
    let t = v.get("t").and_then(|x| x.as_u64()).unwrap_or(0);
    match source {
        Source::Coordinator => {
            if !COORD_VERDICTS.contains(&verdict.as_str()) {
                return Err("not an outcome record: bad verdict".to_string());
            }
            Ok(Entry {
                t,
                source,
                task_id,
                provider,
                model,
                category,
                verdict,
                evidence: str_field(&v, "evidence")?,
                evidence_count: 1,
                self_judged: None,
                cost: None,
                confidence: None,
                run_id: None,
                step: None,
                judge: None,
            })
        }
        Source::Decider => {
            if !DEC_VERDICTS.contains(&verdict.as_str()) {
                return Err("not an outcome record: bad verdict".to_string());
            }
            // Confidence must be a sane number when present; it is validated
            // here but not displayed.
            if let Some(c) = v.get("confidence")
                && !c.as_f64().is_some_and(|c| (0.0..=1.0).contains(&c))
            {
                return Err("not an outcome record: bad confidence".to_string());
            }
            let ids = v
                .get("evidence_ids")
                .and_then(|e| e.as_array())
                .ok_or_else(|| "not an outcome record: bad evidence ids".to_string())?;
            if ids.is_empty() {
                return Err("not an outcome record: bad evidence ids".to_string());
            }
            let decider = v
                .get("decider")
                .ok_or_else(|| "not an outcome record: missing decider".to_string())?;
            let judge = decider
                .get("judge")
                .ok_or_else(|| "not an outcome record: missing judge".to_string())?;
            let judge = format!(
                "{}/{}",
                judge
                    .get("provider")
                    .and_then(|p| p.as_str())
                    .filter(|p| !p.is_empty())
                    .ok_or_else(|| "not an outcome record: bad judge".to_string())?,
                judge
                    .get("model")
                    .and_then(|m| m.as_str())
                    .filter(|m| !m.is_empty())
                    .ok_or_else(|| "not an outcome record: bad judge".to_string())?,
            );
            let self_judged = match decider.get("self_judged") {
                Some(serde_json::Value::Bool(true)) => Some(true),
                Some(serde_json::Value::Null) | None => None,
                _ => return Err("not an outcome record: bad self_judged".to_string()),
            };
            let cost = match decider.get("judge_cost_usd") {
                None | Some(serde_json::Value::Null) => None,
                Some(c) => Some(
                    c.as_f64()
                        .filter(|n| n.is_finite() && *n >= 0.0)
                        .ok_or_else(|| "not an outcome record: bad cost".to_string())?,
                ),
            };
            // Additive workflow keys: only the exact child-report shape marks
            // a workflow evaluation; anything else stays a plain decider row.
            let (run_id, step) = match (
                v.get("basis").and_then(|b| b.as_str()),
                v.get("run_id").and_then(|r| r.as_str()),
                v.get("step").and_then(|s| s.as_str()),
            ) {
                (Some("child-report"), Some(run), Some(step))
                    if !run.is_empty() && !step.is_empty() =>
                {
                    (Some(run.to_string()), Some(step.to_string()))
                }
                _ => (None, None),
            };
            Ok(Entry {
                t,
                source,
                task_id,
                provider,
                model,
                category,
                verdict,
                evidence: String::new(),
                evidence_count: ids.len(),
                self_judged,
                cost,
                confidence: v.get("confidence").and_then(|c| c.as_f64()),
                run_id,
                step,
                judge: Some(judge),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_base(tag: &str) -> PathBuf {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let base = std::env::temp_dir().join(format!(
            "agentdeck-eval-history-{}-{}-{}",
            std::process::id(),
            tag,
            N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&base);
        base
    }

    fn write(base: &Path, text: &str) {
        let path = path_for(base);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
    }

    #[test]
    fn missing_file_is_missing_not_an_error() {
        assert!(matches!(
            load_from_base(&tmp_base("missing")),
            History::Missing
        ));
    }

    #[test]
    fn empty_and_blank_files_are_empty() {
        for (tag, text) in [("empty", ""), ("blank", "\n  \n")] {
            let base = tmp_base(tag);
            write(&base, text);
            assert!(matches!(load_from_base(&base), History::Empty), "{tag}");
            let _ = std::fs::remove_dir_all(&base);
        }
    }

    #[test]
    fn coordinator_and_decider_roundtrip() {
        let base = tmp_base("roundtrip");
        write(
            &base,
            "{\"t\":7,\"source\":\"coordinator\",\"task_id\":\"task-1\",\"provider\":\"p\",\"model\":\"perf/m\",\"category\":\"test\",\"verdict\":\"ok\",\"evidence\":\"direct\",\"decider\":null}\n{\"t\":8,\"source\":\"decider\",\"task_id\":\"task-1\",\"provider\":null,\"model\":\"perf/m\",\"category\":\"test\",\"verdict\":\"pass\",\"confidence\":0.8,\"evidence_ids\":[\"a\",\"b\"],\"decider\":{\"judge\":{\"provider\":\"openrouter\",\"model\":\"j/judge\"},\"self_judged\":null,\"judge_cost_usd\":null}}\n",
        );
        let History::Loaded(entries) = load_from_base(&base) else {
            panic!("expected records")
        };
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].source, Source::Coordinator);
        assert_eq!(entries[0].evidence, "direct");
        assert!(entries[0].self_judged.is_none());
        assert!(entries[0].cost.is_none());
        let dec = &entries[1];
        assert_eq!(dec.source, Source::Decider);
        assert!(dec.provider.is_none());
        assert_eq!(dec.evidence_count, 2);
        assert_eq!(dec.confidence, Some(0.8));
        assert_eq!(dec.judge.as_deref(), Some("openrouter/j/judge"));
        assert!(dec.run_id.is_none());
        assert!(dec.step.is_none());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn workflow_decider_keeps_join_keys_plain_does_not() {
        let base = tmp_base("workflow");
        write(
            &base,
            "{\"t\":3,\"source\":\"decider\",\"task_id\":\"wf-task\",\"provider\":null,\"model\":\"perf/m\",\"category\":\"test\",\"verdict\":\"pass\",\"confidence\":0.9,\"evidence_ids\":[\"r\"],\"evidence_count\":1,\"decider\":{\"judge\":{\"provider\":\"openrouter\",\"model\":\"wf/judge\"},\"self_judged\":true,\"judge_cost_usd\":null},\"basis\":\"child-report\",\"run_id\":\"run-7\",\"step\":\"3\"}\n{\"t\":4,\"source\":\"decider\",\"task_id\":\"plain\",\"provider\":null,\"model\":\"perf/m\",\"category\":\"test\",\"verdict\":\"pass\",\"confidence\":0.5,\"evidence_ids\":[\"a\"],\"evidence_count\":1,\"decider\":{\"judge\":{\"provider\":\"p\",\"model\":\"m\"},\"self_judged\":null,\"judge_cost_usd\":null}}\n",
        );
        let History::Loaded(entries) = load_from_base(&base) else {
            panic!("expected records")
        };
        assert_eq!(entries.len(), 2);
        let wf = &entries[0];
        assert_eq!(wf.run_id.as_deref(), Some("run-7"));
        assert_eq!(wf.step.as_deref(), Some("3"));
        assert_eq!(wf.confidence, Some(0.9));
        let plain = &entries[1];
        assert!(plain.run_id.is_none());
        assert!(plain.step.is_none());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn corrupt_line_fails_and_file_survives() {
        let base = tmp_base("corrupt");
        let good = "{\"t\":1,\"source\":\"coordinator\",\"task_id\":\"t\",\"provider\":\"p\",\"model\":\"m\",\"category\":\"test\",\"verdict\":\"ok\",\"evidence\":\"direct\",\"decider\":null}\n";
        write(&base, &format!("{good}{{\"task_id\": broken\n"));
        let before = std::fs::read(path_for(&base)).unwrap();
        let History::Corrupt { line, .. } = load_from_base(&base) else {
            panic!("expected corrupt")
        };
        assert_eq!(line, 2);
        assert_eq!(std::fs::read(path_for(&base)).unwrap(), before);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn torn_tail_is_skipped() {
        let base = tmp_base("torn");
        write(
            &base,
            "{\"t\":1,\"source\":\"coordinator\",\"task_id\":\"t\",\"provider\":\"p\",\"model\":\"m\",\"category\":\"test\",\"verdict\":\"ok\",\"evidence\":\"direct\",\"decider\":null}\n{\"t\":2,\"sour",
        );
        let History::Loaded(entries) = load_from_base(&base) else {
            panic!("expected one record")
        };
        assert_eq!(entries.len(), 1);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn directory_at_path_is_unreadable() {
        let base = tmp_base("dir");
        std::fs::create_dir_all(path_for(&base)).unwrap();
        assert!(matches!(load_from_base(&base), History::Unreadable(_)));
        let _ = std::fs::remove_dir_all(&base);
    }
}
