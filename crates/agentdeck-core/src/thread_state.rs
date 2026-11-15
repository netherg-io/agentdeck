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

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ThreadState {
    pub state: String,
    pub updated: String,
    pub decisions: Vec<Decision>,
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
    let path = file(paths, id)?;
    let mut data = match load(paths, id)? {
        Some(d) => d,
        None => read(paths, t3_root, id)?
            .map(|f| f.data)
            .unwrap_or_default(),
    };
    let now = utc_now();
    if let Some(s) = state {
        data.state = s.to_string();
    }
    if let Some(d) = decision {
        data.decisions.push(Decision {
            at: now.clone(),
            text: d.to_string(),
        });
    }
    data.updated = now;
    fs::create_dir_all(path.parent().expect("threads dir"))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(&data)?)?;
    fs::rename(&tmp, &path)?;
    Ok(data)
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
        "{base}\nThis thread has saved state (updated {}, {} owner decisions{}). Goal: {goal}\n\
         Call `state_read` before acting after a handoff, compaction, limit pause or \"continue\", \
         and follow it over any summary.",
        f.data.updated,
        f.data.decisions.len(),
        f.inherited_from
            .as_deref()
            .map(|p| format!(", from forked thread {p}"))
            .unwrap_or_default(),
    )
}

fn utc_now() -> String {
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
        assert!(instructions(Some(&f)).contains("Goal: Goal: ship #71"));
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
