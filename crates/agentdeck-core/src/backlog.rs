//! Coordinator backlog per project: findings, follow-ups and deferred steps
//! that outlive one session (replaces `coordinator-backlog` GitHub issues).
//! One JSON file per project under the state dir, written under an exclusive
//! lock because several coordinator threads may share a project.

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::Paths;
use crate::thread_state::{Decision, utc_now};

const MAX_TEXT: usize = 16 * 1024;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Closed {
    pub at: String,
    /// PR that fixed the item (`owner/repo#n` or URL) and the merged SHA.
    pub pr: String,
    pub sha: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Entry {
    pub id: u64,
    pub title: String,
    #[serde(default)]
    pub body: String,
    /// When the item may be closed.
    #[serde(default)]
    pub close_when: String,
    /// `open` or `closed`.
    pub status: String,
    /// Free labels such as `partial` or `next-release`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<String>,
    /// Dated notes, append-only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<Decision>,
    pub created: String,
    pub updated: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed: Option<Closed>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Backlog {
    pub next_id: u64,
    pub items: Vec<Entry>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct NewEntry {
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub close_when: String,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub links: Vec<String>,
}

/// Partial update; `note` appends a dated history line, `close` closes the
/// item (needs a PR and SHA), `reopen` opens a closed one.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Patch {
    pub title: Option<String>,
    pub body: Option<String>,
    pub close_when: Option<String>,
    pub labels: Option<Vec<String>>,
    pub links: Option<Vec<String>>,
    pub note: Option<String>,
    pub close: Option<ClosePatch>,
    #[serde(default)]
    pub reopen: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ClosePatch {
    pub pr: String,
    pub sha: String,
}

pub fn check_project(p: &str) -> anyhow::Result<()> {
    let ok = !p.is_empty()
        && p.len() <= 64
        && p.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !ok {
        anyhow::bail!("project must be 1-64 chars of a-z, 0-9 and '-', got {p:?}");
    }
    Ok(())
}

fn dir(paths: &Paths) -> PathBuf {
    paths.state_dir.join("coordinator/backlog")
}

fn file(paths: &Paths, project: &str) -> anyhow::Result<PathBuf> {
    check_project(project)?;
    Ok(dir(paths).join(format!("{project}.json")))
}

pub(crate) fn save_json<T: Serialize>(path: &Path, data: &T) -> anyhow::Result<()> {
    fs::create_dir_all(path.parent().expect("state file has a dir"))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(data)?)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn load_file(path: &Path) -> anyhow::Result<Backlog> {
    match fs::read(path) {
        Ok(b) => Ok(serde_json::from_slice(&b)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Backlog {
            next_id: 1,
            items: Vec::new(),
        }),
        Err(e) => Err(e.into()),
    }
}

fn locked(paths: &Paths, project: &str) -> anyhow::Result<(PathBuf, File)> {
    let path = file(paths, project)?;
    fs::create_dir_all(dir(paths))?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path.with_extension("lock"))?;
    lock.lock()?;
    Ok((path, lock))
}

fn check_text(t: &str) -> anyhow::Result<()> {
    if t.len() > MAX_TEXT {
        anyhow::bail!("text over {MAX_TEXT} bytes: link a report file instead");
    }
    Ok(())
}

pub fn load(paths: &Paths, project: &str) -> anyhow::Result<Backlog> {
    load_file(&file(paths, project)?)
}

/// Projects that have a backlog file.
pub fn projects(paths: &Paths) -> Vec<String> {
    let mut out: Vec<String> = fs::read_dir(dir(paths))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().into_string().ok()?;
            let p = n.strip_suffix(".json")?.to_string();
            check_project(&p).is_ok().then_some(p)
        })
        .collect();
    out.sort();
    out
}

pub fn add(paths: &Paths, project: &str, new: NewEntry) -> anyhow::Result<Entry> {
    if new.title.trim().is_empty() {
        anyhow::bail!("title is required");
    }
    check_text(&new.body)?;
    let (path, _lock) = locked(paths, project)?;
    let mut b = load_file(&path)?;
    let now = utc_now();
    let e = Entry {
        id: b.next_id.max(1),
        title: new.title,
        body: new.body,
        close_when: new.close_when,
        status: "open".into(),
        labels: new.labels,
        links: new.links,
        history: Vec::new(),
        created: now.clone(),
        updated: now,
        closed: None,
    };
    b.next_id = e.id + 1;
    b.items.push(e.clone());
    save_json(&path, &b)?;
    Ok(e)
}

pub fn update(paths: &Paths, project: &str, id: u64, patch: Patch) -> anyhow::Result<Entry> {
    for t in [&patch.body, &patch.note].into_iter().flatten() {
        check_text(t)?;
    }
    if let Some(c) = &patch.close
        && (c.pr.trim().is_empty() || c.sha.trim().len() < 7)
    {
        anyhow::bail!("closing needs the PR and the merged SHA (at least 7 hex chars)");
    }
    if patch.close.is_some() && patch.reopen {
        anyhow::bail!("pass close or reopen, not both");
    }
    let (path, _lock) = locked(paths, project)?;
    let mut b = load_file(&path)?;
    let Some(e) = b.items.iter_mut().find(|e| e.id == id) else {
        anyhow::bail!("{project} has no backlog item {id}");
    };
    let now = utc_now();
    if let Some(t) = patch.title.filter(|t| !t.trim().is_empty()) {
        e.title = t;
    }
    if let Some(v) = patch.body {
        e.body = v;
    }
    if let Some(v) = patch.close_when {
        e.close_when = v;
    }
    if let Some(v) = patch.labels {
        e.labels = v;
    }
    if let Some(v) = patch.links {
        e.links = v;
    }
    if let Some(n) = patch.note.filter(|n| !n.trim().is_empty()) {
        e.history.push(Decision {
            at: now.clone(),
            text: n,
        });
    }
    if let Some(c) = patch.close {
        e.status = "closed".into();
        e.closed = Some(Closed {
            at: now.clone(),
            pr: c.pr,
            sha: c.sha,
        });
    }
    if patch.reopen {
        e.status = "open".into();
        e.closed = None;
    }
    e.updated = now;
    let out = e.clone();
    save_json(&path, &b)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fx(tag: &str) -> (PathBuf, Paths) {
        let dir =
            std::env::temp_dir().join(format!("agentdeck-backlog-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        (
            dir.clone(),
            Paths::new(dir.join("config"), dir.join("state")),
        )
    }

    #[test]
    fn add_update_close_reopen() {
        let (dir, paths) = fx("flow");
        let a = add(
            &paths,
            "blockfield",
            NewEntry {
                title: "Flaky RC".into(),
                labels: vec!["partial".into()],
                ..NewEntry::default()
            },
        )
        .unwrap();
        let b = add(
            &paths,
            "blockfield",
            NewEntry {
                title: "Second".into(),
                ..NewEntry::default()
            },
        )
        .unwrap();
        assert_eq!((a.id, b.id), (1, 2));
        let e = update(
            &paths,
            "blockfield",
            1,
            Patch {
                note: Some("seen again".into()),
                ..Patch::default()
            },
        )
        .unwrap();
        assert_eq!(e.history.len(), 1);
        let bad = Patch {
            close: Some(ClosePatch {
                pr: "x#1".into(),
                sha: "ab".into(),
            }),
            ..Patch::default()
        };
        assert!(update(&paths, "blockfield", 1, bad).is_err());
        let closed = update(
            &paths,
            "blockfield",
            1,
            Patch {
                close: Some(ClosePatch {
                    pr: "x#1".into(),
                    sha: "abc1234".into(),
                }),
                ..Patch::default()
            },
        )
        .unwrap();
        assert_eq!(closed.status, "closed");
        let open = update(
            &paths,
            "blockfield",
            1,
            Patch {
                reopen: true,
                ..Patch::default()
            },
        )
        .unwrap();
        assert!(open.closed.is_none());
        assert_eq!(load(&paths, "blockfield").unwrap().items.len(), 2);
        assert_eq!(projects(&paths), vec!["blockfield".to_string()]);
        assert!(update(&paths, "blockfield", 9, Patch::default()).is_err());
        assert!(
            add(
                &paths,
                "../x",
                NewEntry {
                    title: "t".into(),
                    ..NewEntry::default()
                }
            )
            .is_err()
        );
        let _ = fs::remove_dir_all(dir);
    }
}
