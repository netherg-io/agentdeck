//! Per-T3-thread working state for long sessions such as a coordinator: a
//! replaceable snapshot (goal, scope, done, next) plus an append-only list of
//! verbatim owner decisions. Keyed by the T3 thread id, which stays the same
//! across provider switches, handoffs and compaction; a fork without state of
//! its own reads its parent's, and its first write copies that state over.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};

use crate::Paths;

pub const THREAD_ENV: &str = "T3CODE_THREAD_ID";
const MAX_STATE: usize = 16 * 1024;
const MAX_DECISION: usize = 4 * 1024;
const MAX_FORK_DEPTH: usize = 8;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Decision {
    pub at: String,
    pub text: String,
}

/// Checklist statuses, in display order of progress.
pub const ITEM_STATUSES: [&str; 6] = ["todo", "running", "review", "done", "blocked", "held"];
const MAX_ITEMS: usize = 200;

/// One step of the session's goal, shown to the owner as a checklist.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Item {
    pub id: String,
    pub title: String,
    pub status: String,
    /// PR, issue or backlog references (`repo#n`, URLs, `backlog:<project>#n`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<String>,
    /// Current state in one line: what runs now, what blocks it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

/// Partial update of an item; absent fields keep their value.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ItemPatch {
    pub id: String,
    pub title: Option<String>,
    pub status: Option<String>,
    pub links: Option<Vec<String>>,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ThreadState {
    pub state: String,
    pub updated: String,
    pub decisions: Vec<Decision>,
    /// Backlog project this session works on (see `backlog`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub project: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checklist: Vec<Item>,
}

impl ThreadState {
    /// (done, total) over the checklist; `held` items do not count as open work.
    pub fn progress(&self) -> (usize, usize) {
        let total = self.checklist.iter().filter(|i| i.status != "held").count();
        let done = self.checklist.iter().filter(|i| i.status == "done").count();
        (done, total)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Found {
    pub thread: String,
    /// Set when the state belongs to an ancestor this thread was forked from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inherited_from: Option<String>,
    #[serde(flatten)]
    pub data: ThreadState,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn file(paths: &Paths, id: &str) -> anyhow::Result<PathBuf> {
    if !valid_id(id) {
        anyhow::bail!("invalid thread id {id:?}");
    }
    Ok(paths.state_dir.join("threads").join(format!("{id}.json")))
}

fn load(paths: &Paths, id: &str) -> anyhow::Result<Option<ThreadState>> {
    match fs::read(file(paths, id)?) {
        Ok(b) => Ok(Some(serde_json::from_slice(&b)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Fork parent from T3's read-only `statev2.sqlite`; `None` when T3, the
/// thread or its lineage is absent.
fn parent(t3_root: Option<&Path>, id: &str) -> Option<String> {
    let db = t3_root?.join("userdata/statev2.sqlite");
    let conn = Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    conn.query_row(
        "SELECT json_extract(payload_json, '$.lineage.parentThreadId') \
         FROM orchestration_v2_projection_threads WHERE thread_id = ?1",
        [id],
        |row| row.get::<_, Option<String>>(0),
    )
    .ok()
    .flatten()
    .filter(|p| valid_id(p))
}

/// T3's title for a thread (read-only), for display.
pub fn thread_title(t3_root: Option<&Path>, id: &str) -> Option<String> {
    let db = t3_root?.join("userdata/statev2.sqlite");
    let conn = Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    conn.query_row(
        "SELECT title FROM orchestration_v2_projection_threads WHERE thread_id = ?1",
        [id],
        |row| row.get::<_, String>(0),
    )
    .ok()
}

/// This thread's state, else the nearest forked-from ancestor's.
pub fn read(paths: &Paths, t3_root: Option<&Path>, id: &str) -> anyhow::Result<Option<Found>> {
    let mut cur = id.to_string();
    for depth in 0..=MAX_FORK_DEPTH {
        if let Some(data) = load(paths, &cur)? {
            return Ok(Some(Found {
                thread: id.to_string(),
                inherited_from: (depth > 0).then_some(cur),
                data,
            }));
        }
        match parent(t3_root, &cur) {
            Some(p) => cur = p,
            None => break,
        }
    }
    Ok(None)
}

/// Replace the snapshot and/or append one verbatim decision. Decisions are
/// never dropped: rewriting the snapshot cannot lose an owner's answer.
pub fn write(
    paths: &Paths,
    t3_root: Option<&Path>,
    id: &str,
    state: Option<&str>,
    decision: Option<&str>,
) -> anyhow::Result<ThreadState> {
    if state.is_none() && decision.is_none() {
        anyhow::bail!("pass state, decision or both");
    }
    if state.is_some_and(|s| s.len() > MAX_STATE) {
        anyhow::bail!(
            "state over {MAX_STATE} bytes: keep it a summary that cites issues and files"
        );
    }
    if decision.is_some_and(|d| d.len() > MAX_DECISION) {
        anyhow::bail!("decision over {MAX_DECISION} bytes");
    }
    mutate(paths, t3_root, id, |data, now| {
        if let Some(s) = state {
            data.state = s.to_string();
        }
        if let Some(d) = decision {
            data.decisions.push(Decision {
                at: now.to_string(),
                text: d.to_string(),
            });
        }
        Ok(())
    })
}

/// One status change of a checklist item.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Change {
    pub title: String,
    /// Empty for a new item.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub from: String,
    pub to: String,
}

/// A progress report: written whenever a checklist update changes a status.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Report {
    pub at: String,
    pub thread: String,
    pub goal: String,
    pub done: usize,
    pub total: usize,
    pub changes: Vec<Change>,
}

impl Report {
    /// Two-line text for a desktop notification.
    pub fn text(&self) -> String {
        let changes: Vec<String> = self
            .changes
            .iter()
            .map(|c| format!("{}: {}", c.title, c.to))
            .collect();
        format!(
            "{} · {}/{}\n{}",
            self.goal,
            self.done,
            self.total,
            changes.join("; ")
        )
    }
}

const MAX_REPORTS: usize = 500;

fn reports_file(paths: &Paths) -> PathBuf {
    paths.state_dir.join("coordinator/reports.jsonl")
}

fn append_report(paths: &Paths, r: &Report) -> anyhow::Result<()> {
    let path = reports_file(paths);
    let mut lines: Vec<String> = fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    lines.push(serde_json::to_string(r)?);
    let keep = lines.len().saturating_sub(MAX_REPORTS);
    crate::backlog::save_text(&path, &(lines[keep..].join("\n") + "\n"))
}

/// Newest reports first.
pub fn reports(paths: &Paths, limit: usize) -> Vec<Report> {
    let mut out: Vec<Report> = fs::read_to_string(reports_file(paths))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    out.reverse();
    out.truncate(limit);
    out
}

/// Upsert checklist items by id (new ids need a title and get `todo` unless a
/// status is given), drop `remove` ids, and optionally bind the backlog project.
pub fn update_checklist(
    paths: &Paths,
    t3_root: Option<&Path>,
    id: &str,
    project: Option<&str>,
    items: &[ItemPatch],
    remove: &[String],
) -> anyhow::Result<(ThreadState, Option<Report>)> {
    if let Some(p) = project {
        crate::backlog::check_project(p)?;
    }
    for it in items {
        if it.id.is_empty() || it.id.len() > 64 {
            anyhow::bail!("item id must be 1-64 chars");
        }
        if let Some(st) = &it.status
            && !ITEM_STATUSES.contains(&st.as_str())
        {
            anyhow::bail!("status {st:?} is not one of {}", ITEM_STATUSES.join(", "));
        }
    }
    let mut before = Vec::new();
    let state = mutate(paths, t3_root, id, |data, _| {
        before = data
            .checklist
            .iter()
            .map(|i| (i.id.clone(), i.status.clone()))
            .collect();
        if let Some(p) = project {
            data.project = p.to_string();
        }
        data.checklist.retain(|i| !remove.contains(&i.id));
        for it in items {
            let pos = data.checklist.iter().position(|i| i.id == it.id);
            let entry = match pos {
                Some(n) => &mut data.checklist[n],
                None => {
                    let Some(title) = it.title.clone().filter(|t| !t.is_empty()) else {
                        anyhow::bail!("new item {} needs a title", it.id);
                    };
                    data.checklist.push(Item {
                        id: it.id.clone(),
                        title,
                        status: "todo".into(),
                        ..Item::default()
                    });
                    data.checklist.last_mut().expect("just pushed")
                }
            };
            if let Some(t) = it.title.clone().filter(|t| !t.is_empty()) {
                entry.title = t;
            }
            if let Some(st) = &it.status {
                entry.status = st.clone();
            }
            if let Some(l) = &it.links {
                entry.links = l.clone();
            }
            if let Some(n) = &it.note {
                entry.note = n.clone();
            }
        }
        if data.checklist.len() > MAX_ITEMS {
            anyhow::bail!("checklist over {MAX_ITEMS} items: group steps");
        }
        Ok(())
    })?;
    let changes: Vec<Change> = state
        .checklist
        .iter()
        .filter_map(|i| {
            let from = before
                .iter()
                .find(|(id, _)| *id == i.id)
                .map(|(_, s)| s.clone())
                .unwrap_or_default();
            (from != i.status).then(|| Change {
                title: i.title.clone(),
                from,
                to: i.status.clone(),
            })
        })
        .collect();
    if changes.is_empty() {
        return Ok((state, None));
    }
    let (done, total) = state.progress();
    let report = Report {
        at: state.updated.clone(),
        thread: id.to_string(),
        goal: state
            .state
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(160)
            .collect(),
        done,
        total,
        changes,
    };
    // A lost report must not fail the checklist write the coordinator relies on.
    if let Err(e) = append_report(paths, &report) {
        eprintln!("coordinator report not saved: {e:#}");
    }
    Ok((state, Some(report)))
}

/// Load this thread's state (or seed it from a forked-from ancestor), apply
/// `f`, stamp it and save atomically.
fn mutate(
    paths: &Paths,
    t3_root: Option<&Path>,
    id: &str,
    f: impl FnOnce(&mut ThreadState, &str) -> anyhow::Result<()>,
) -> anyhow::Result<ThreadState> {
    let path = file(paths, id)?;
    let mut data = match load(paths, id)? {
        Some(d) => d,
        None => read(paths, t3_root, id)?
            .map(|f| f.data)
            .unwrap_or_default(),
    };
    let now = utc_now();
    f(&mut data, &now)?;
    data.updated = now;
    crate::backlog::save_json(&path, &data)?;
    Ok(data)
}

/// Every thread with saved state, newest update first (for the GUI and applet).
pub fn list(paths: &Paths) -> Vec<(String, ThreadState)> {
    let mut out: Vec<(String, ThreadState)> = fs::read_dir(paths.state_dir.join("threads"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let id = name.strip_suffix(".json")?.to_string();
            let data = serde_json::from_slice(&fs::read(e.path()).ok()?).ok()?;
            valid_id(&id).then_some((id, data))
        })
        .collect();
    out.sort_by(|a, b| b.1.updated.cmp(&a.1.updated));
    out
}

/// Short text for MCP `initialize` instructions: the snapshot's first line
/// (the goal) and how to get the rest. Harnesses truncate long instructions,
/// so the full state is a tool call away instead.
pub fn instructions(found: Option<&Found>) -> String {
    let base = "Long sessions keep their goal, scope, progress and the owner's verbatim decisions with \
        `state_read` / `state_write` (keyed by this T3 thread, kept across provider switches, handoffs \
        and compaction). Write the snapshot at each checkpoint and every owner decision the moment it is given.";
    let Some(f) = found else {
        return base.to_string();
    };
    let goal: String = f
        .data
        .state
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(300)
        .collect();
    format!(
        "{base}\nThis thread has saved state (updated {}, {} owner decisions{}{}). First line: {goal}\n\
         Call `state_read` before acting after a handoff, compaction, limit pause or \"continue\", \
         and follow it over any summary.",
        f.data.updated,
        f.data.decisions.len(),
        match f.data.progress() {
            (_, 0) => String::new(),
            (d, t) => format!(", checklist {d}/{t}"),
        },
        f.inherited_from
            .as_deref()
            .map(|p| format!(", from forked thread {p}"))
            .unwrap_or_default(),
    )
}

pub(crate) fn utc_now() -> String {
    utc(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0))
}

fn utc(secs: i64) -> String {
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil date from days since 1970-01-01 (H. Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fx(tag: &str) -> (PathBuf, Paths) {
        let dir =
            std::env::temp_dir().join(format!("agentdeck-tstate-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let paths = Paths::new(dir.join("config"), dir.join("state"));
        (dir, paths)
    }

    fn t3_with_fork(dir: &Path, child: &str, parent: &str) -> PathBuf {
        let root = dir.join("t3");
        fs::create_dir_all(root.join("userdata")).unwrap();
        let c = Connection::open(root.join("userdata/statev2.sqlite")).unwrap();
        c.execute_batch(
            "CREATE TABLE orchestration_v2_projection_threads (thread_id TEXT PRIMARY KEY, payload_json TEXT NOT NULL);",
        )
        .unwrap();
        c.execute(
            "INSERT INTO orchestration_v2_projection_threads VALUES (?1, ?2)",
            [
                child,
                &format!(r#"{{"lineage":{{"parentThreadId":"{parent}"}}}}"#),
            ],
        )
        .unwrap();
        root
    }

    #[test]
    fn snapshot_replaces_decisions_accumulate() {
        let (dir, paths) = fx("acc");
        write(
            &paths,
            None,
            "t1",
            Some("Goal: ship #71\nnext: deploy"),
            None,
        )
        .unwrap();
        write(&paths, None, "t1", None, Some("«Подтверждаю» deploys")).unwrap();
        let s = write(
            &paths,
            None,
            "t1",
            Some("Goal: ship #71\nnext: verify"),
            None,
        )
        .unwrap();
        assert_eq!(s.state, "Goal: ship #71\nnext: verify");
        assert_eq!(s.decisions.len(), 1);
        assert_eq!(s.decisions[0].text, "«Подтверждаю» deploys");
        let f = read(&paths, None, "t1").unwrap().unwrap();
        assert!(f.inherited_from.is_none());
        assert!(instructions(Some(&f)).contains("First line: Goal: ship #71"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn fork_inherits_then_owns() {
        let (dir, paths) = fx("fork");
        let t3 = t3_with_fork(&dir, "child", "parent");
        write(&paths, None, "parent", Some("Goal: A"), Some("owner: yes")).unwrap();
        let f = read(&paths, Some(&t3), "child").unwrap().unwrap();
        assert_eq!(f.inherited_from.as_deref(), Some("parent"));
        let s = write(&paths, Some(&t3), "child", None, Some("owner: also B")).unwrap();
        assert_eq!(s.state, "Goal: A");
        assert_eq!(s.decisions.len(), 2);
        assert_eq!(load(&paths, "parent").unwrap().unwrap().decisions.len(), 1);
        assert!(
            read(&paths, Some(&t3), "child")
                .unwrap()
                .unwrap()
                .inherited_from
                .is_none()
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn checklist_upserts_and_counts() {
        let (dir, paths) = fx("check");
        let p = |id: &str, title: Option<&str>, status: Option<&str>| ItemPatch {
            id: id.into(),
            title: title.map(Into::into),
            status: status.map(Into::into),
            ..ItemPatch::default()
        };
        assert!(update_checklist(&paths, None, "t", None, &[p("a", None, None)], &[]).is_err());
        assert!(
            update_checklist(
                &paths,
                None,
                "t",
                None,
                &[p("a", Some("A"), Some("nope"))],
                &[]
            )
            .is_err()
        );
        let s = update_checklist(
            &paths,
            None,
            "t",
            Some("blockfield"),
            &[
                p("a", Some("A"), None),
                p("b", Some("B"), Some("done")),
                p("c", Some("C"), Some("held")),
            ],
            &[],
        )
        .unwrap();
        assert_eq!(
            (s.0.project.as_str(), s.0.progress()),
            ("blockfield", (1, 2))
        );
        assert_eq!(s.1.unwrap().changes.len(), 3);
        let s = update_checklist(
            &paths,
            None,
            "t",
            None,
            &[p("a", None, Some("done"))],
            &["c".into()],
        )
        .unwrap();
        assert_eq!(s.0.progress(), (2, 2));
        assert_eq!(s.0.checklist[0].title, "A");
        let r = s.1.unwrap();
        assert_eq!(
            r.changes,
            vec![Change {
                title: "A".into(),
                from: "todo".into(),
                to: "done".into()
            }]
        );
        assert_eq!((r.done, r.total), (2, 2));
        // A note-only update is no report.
        let note = ItemPatch {
            id: "a".into(),
            note: Some("merged".into()),
            ..ItemPatch::default()
        };
        assert!(
            update_checklist(&paths, None, "t", None, &[note], &[])
                .unwrap()
                .1
                .is_none()
        );
        let saved = reports(&paths, 10);
        assert_eq!(saved.len(), 2);
        assert_eq!(saved[0].changes[0].to, "done");
        write(&paths, None, "t", Some("Goal: x"), None).unwrap();
        let listed = list(&paths);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].1.checklist.len(), 2);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_bad_ids_and_sizes() {
        let (dir, paths) = fx("bad");
        assert!(write(&paths, None, "../x", Some("s"), None).is_err());
        assert!(write(&paths, None, "t", None, None).is_err());
        assert!(write(&paths, None, "t", Some(&"x".repeat(MAX_STATE + 1)), None).is_err());
        assert!(read(&paths, None, "none").unwrap().is_none());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn utc_formats_known_instants() {
        assert_eq!(utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(utc(1_794_787_199), "2026-11-15T23:59:59Z");
    }
}
