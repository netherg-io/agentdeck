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
    /// Autonomous Run phase (default Intake for pre-existing threads).
    #[serde(default)]
    pub phase: RunPhase,
    /// Structured Run Mandate granted at Intake; None before Intake completes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mandate: Option<RunMandate>,
    /// Owner Inbox: branches parked for the owner during an Autonomous Run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub owner_inbox: Vec<InboxItem>,
    /// Assumptions: reversible decisions made autonomously, with revert hints.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assumptions: Vec<Assumption>,
    /// Updated ONLY when `state` text is written, never on `checklist_update`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub state_updated_at: String,
    /// Incremented on every `checklist_update`, reset when `state` is written.
    #[serde(default)]
    pub checklist_updates_since_state: u32,
    /// RFC3339 UTC timestamp when `phase == SleepingForQuota`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sleep_until: Option<String>,
    #[serde(default)]
    pub checkpoint_count: u32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub last_checkpoint_at: String,
}

/// Autonomous Run phase. `Intake` is the default so legacy threads deserialize.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunPhase {
    #[default]
    Intake,
    Running,
    SleepingForQuota,
    Completed,
}

fn default_fix_forward_max() -> u32 {
    3
}

fn default_review_rounds_max() -> u32 {
    2
}

/// Explicit scope of authority granted by the owner at Intake.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunMandate {
    #[serde(default)]
    pub project: String,
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
    #[serde(default)]
    pub allowed_apps: Vec<String>,
    #[serde(default)]
    pub allow_releases: bool,
    #[serde(default = "default_fix_forward_max")]
    pub fix_forward_max: u32,
    #[serde(default = "default_review_rounds_max")]
    pub review_rounds_max: u32,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub granted_at: String,
}

impl Default for RunMandate {
    fn default() -> Self {
        Self {
            project: String::new(),
            allowed_hosts: Vec::new(),
            allowed_apps: Vec::new(),
            allow_releases: false,
            fix_forward_max: default_fix_forward_max(),
            review_rounds_max: default_review_rounds_max(),
            notes: String::new(),
            granted_at: String::new(),
        }
    }
}

/// One parked Owner Inbox item: a branch needing the owner or out of mandate.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct InboxItem {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub action: String,
    #[serde(default)]
    pub blocked_items: Vec<String>,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub resolved: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<String>,
}

/// Input for `inbox_add`: `id` is optional, generated as `inbox-N` when absent.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NewInboxItem {
    #[serde(default)]
    pub id: Option<String>,
    pub title: String,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub action: String,
    #[serde(default)]
    pub blocked_items: Vec<String>,
}

/// One reversible autonomous decision, recorded with a revert hint.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Assumption {
    pub id: String,
    pub decision: String,
    #[serde(default)]
    pub rationale: String,
    #[serde(default)]
    pub revert_hint: String,
    #[serde(default)]
    pub created_at: String,
}

/// Input for `assumption_add`: `id` is optional, generated as `asm-N` when absent.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NewAssumption {
    #[serde(default)]
    pub id: Option<String>,
    pub decision: String,
    #[serde(default)]
    pub rationale: String,
    #[serde(default)]
    pub revert_hint: String,
}

impl ThreadState {
    /// (done, total) over the checklist; `held` items do not count as open work.
    pub fn progress(&self) -> (usize, usize) {
        let total = self.checklist.iter().filter(|i| i.status != "held").count();
        let done = self.checklist.iter().filter(|i| i.status == "done").count();
        (done, total)
    }

    /// True when the snapshot no longer reflects recent checklist movement:
    /// 5+ checklist updates since the last `state` write, or a non-empty
    /// checklist with a blank snapshot.
    pub fn state_stale(&self) -> bool {
        self.checklist_updates_since_state >= 5
            || (self.state.trim().is_empty() && !self.checklist.is_empty())
    }

    /// Parked Owner Inbox items still waiting on the owner.
    pub fn open_inbox_count(&self) -> usize {
        self.owner_inbox.iter().filter(|i| !i.resolved).count()
    }

    /// True if any checklist item still needs work (not done/held/blocked).
    pub fn has_unblocked_work(&self) -> bool {
        self.checklist
            .iter()
            .any(|i| matches!(i.status.as_str(), "todo" | "running" | "review"))
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
            // Fresh snapshot: it reflects all checklist movement so far.
            data.state_updated_at = now.to_string();
            data.checklist_updates_since_state = 0;
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
        // Snapshot freshness tracking: every checklist change ages `state`.
        data.checklist_updates_since_state =
            data.checklist_updates_since_state.saturating_add(1);
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

/// Set run metadata (phase, mandate, sleep deadline) and optionally record a
/// Checkpoint. When `record_checkpoint` is true the snapshot must be fresh:
/// refused while `state_stale()`. `sleep_until`: None leaves the deadline
/// unchanged, Some(None) clears it, Some(Some(t)) sets it.
pub fn set_run_meta(
    paths: &Paths,
    t3_root: Option<&Path>,
    id: &str,
    phase: Option<RunPhase>,
    mandate: Option<RunMandate>,
    sleep_until: Option<Option<String>>,
    record_checkpoint: bool,
) -> anyhow::Result<ThreadState> {
    if record_checkpoint
        && read(paths, t3_root, id)?
            .map(|f| f.data.state_stale())
            .unwrap_or(false)
    {
        anyhow::bail!(
            "state is stale (5+ checklist updates since the last state write): \
             rewrite `state` first, then record the checkpoint"
        );
    }
    mutate(paths, t3_root, id, |data, now| {
        if let Some(p) = phase {
            data.phase = p;
        }
        if let Some(m) = mandate {
            data.mandate = Some(m);
        }
        if let Some(s) = sleep_until.clone() {
            data.sleep_until = s;
        }
        if record_checkpoint {
            data.checkpoint_count = data.checkpoint_count.saturating_add(1);
            data.last_checkpoint_at = now.to_string();
        }
        Ok(())
    })
}

fn next_id(prefix: &str, used: &[String]) -> String {
    let mut n = used.len() + 1;
    loop {
        let cand = format!("{prefix}-{n}");
        if !used.iter().any(|u| u == &cand) {
            return cand;
        }
        n += 1;
    }
}

/// Park one branch in the Owner Inbox. The id is caller-provided or `inbox-N`.
pub fn inbox_add(
    paths: &Paths,
    t3_root: Option<&Path>,
    id: &str,
    item: NewInboxItem,
) -> anyhow::Result<InboxItem> {
    if item.title.trim().is_empty() {
        anyhow::bail!("inbox item needs a title");
    }
    let full = mutate(paths, t3_root, id, |data, now| {
        let used: Vec<String> = data.owner_inbox.iter().map(|i| i.id.clone()).collect();
        let item_id = item.id.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| next_id("inbox", &used));
        if item_id.len() > 64 {
            anyhow::bail!("inbox id must be 1-64 chars");
        }
        if used.iter().any(|u| u == &item_id) {
            anyhow::bail!("inbox item {item_id:?} already exists");
        }
        data.owner_inbox.push(InboxItem {
            id: item_id,
            title: item.title.clone(),
            reason: item.reason.clone(),
            action: item.action.clone(),
            blocked_items: item.blocked_items.clone(),
            created_at: now.to_string(),
            resolved: false,
            resolved_at: None,
        });
        Ok(())
    })?;
    full.owner_inbox
        .last()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("inbox item was not saved"))
}

/// Mark one Owner Inbox item resolved (the owner handled it or it lapsed).
pub fn inbox_resolve(
    paths: &Paths,
    t3_root: Option<&Path>,
    id: &str,
    item_id: &str,
) -> anyhow::Result<InboxItem> {
    let full = mutate(paths, t3_root, id, |data, now| {
        let item = data
            .owner_inbox
            .iter_mut()
            .find(|i| i.id == item_id)
            .ok_or_else(|| anyhow::anyhow!("no inbox item {item_id:?}"))?;
        item.resolved = true;
        item.resolved_at = Some(now.to_string());
        Ok(())
    })?;
    full.owner_inbox
        .into_iter()
        .find(|i| i.id == item_id)
        .ok_or_else(|| anyhow::anyhow!("no inbox item {item_id:?}"))
}

/// Record one reversible autonomous decision with a revert hint.
pub fn assumption_add(
    paths: &Paths,
    t3_root: Option<&Path>,
    id: &str,
    item: NewAssumption,
) -> anyhow::Result<Assumption> {
    if item.decision.trim().is_empty() {
        anyhow::bail!("assumption needs a decision");
    }
    let full = mutate(paths, t3_root, id, |data, now| {
        let used: Vec<String> = data.assumptions.iter().map(|a| a.id.clone()).collect();
        let asm_id = item.id.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| next_id("asm", &used));
        if asm_id.len() > 64 {
            anyhow::bail!("assumption id must be 1-64 chars");
        }
        if used.iter().any(|u| u == &asm_id) {
            anyhow::bail!("assumption {asm_id:?} already exists");
        }
        data.assumptions.push(Assumption {
            id: asm_id,
            decision: item.decision.clone(),
            rationale: item.rationale.clone(),
            revert_hint: item.revert_hint.clone(),
            created_at: now.to_string(),
        });
        Ok(())
    })?;
    full.assumptions
        .last()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("assumption was not saved"))
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

    #[test]
    fn legacy_json_without_new_fields_still_loads() {
        let (dir, paths) = fx("legacy");
        let legacy = serde_json::json!({
            "state": "Goal: old",
            "updated": "2026-01-01T00:00:00Z",
            "decisions": [],
            "checklist": [{"id": "a", "title": "A", "status": "todo"}]
        });
        let path = paths.state_dir.join("threads").join("old.json");
        crate::backlog::save_json(&path, &legacy).unwrap();
        let f = read(&paths, None, "old").unwrap().unwrap();
        assert_eq!(f.data.phase, RunPhase::Intake);
        assert!(f.data.mandate.is_none());
        assert!(f.data.owner_inbox.is_empty());
        assert!(f.data.sleep_until.is_none());
        assert_eq!(f.data.checklist_updates_since_state, 0);
        assert_eq!(f.data.checkpoint_count, 0);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn freshness_tracks_checklist_updates_until_state_write() {
        let (dir, paths) = fx("stale");
        let p = |id: &str, title: Option<&str>| ItemPatch {
            id: id.into(),
            title: title.map(Into::into),
            ..ItemPatch::default()
        };
        // Blank snapshot + checklist = stale.
        update_checklist(&paths, None, "t", None, &[p("a", Some("A"))], &[]).unwrap();
        let s = load(&paths, "t").unwrap().unwrap();
        assert!(s.state_stale());
        assert_eq!(s.checklist_updates_since_state, 1);
        assert!(s.state_updated_at.is_empty());
        // A fresh snapshot clears the counter and stamps state_updated_at.
        let s = write(&paths, None, "t", Some("Goal: x"), None).unwrap();
        assert!(!s.state_stale());
        assert_eq!(s.checklist_updates_since_state, 0);
        assert!(!s.state_updated_at.is_empty());
        // Five checklist updates without a state write = stale.
        for _ in 0..5 {
            let note = ItemPatch {
                id: "a".into(),
                note: Some("still running".into()),
                ..ItemPatch::default()
            };
            update_checklist(&paths, None, "t", None, &[note], &[]).unwrap();
        }
        let s = load(&paths, "t").unwrap().unwrap();
        assert_eq!(s.checklist_updates_since_state, 5);
        assert!(s.state_stale());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn inbox_and_assumptions_round_trip() {
        let (dir, paths) = fx("inbox");
        let item = inbox_add(
            &paths,
            None,
            "t",
            NewInboxItem {
                title: "Need token".into(),
                reason: "owner must paste it".into(),
                action: "paste into chat".into(),
                blocked_items: vec!["b".into()],
                ..NewInboxItem::default()
            },
        )
        .unwrap();
        assert_eq!(item.id, "inbox-1");
        assert!(!item.resolved);
        let s = load(&paths, "t").unwrap().unwrap();
        assert_eq!(s.open_inbox_count(), 1);
        let done = inbox_resolve(&paths, None, "t", &item.id).unwrap();
        assert!(done.resolved && done.resolved_at.is_some());
        assert!(inbox_resolve(&paths, None, "t", "absent").is_err());
        let asm = assumption_add(
            &paths,
            None,
            "t",
            NewAssumption {
                decision: "Use X".into(),
                rationale: "fits".into(),
                revert_hint: "switch to Y".into(),
                ..NewAssumption::default()
            },
        )
        .unwrap();
        assert_eq!(asm.id, "asm-1");
        assert!(inbox_add(&paths, None, "t", NewInboxItem {
            id: Some(item.id.clone()),
            title: "dup".into(),
            ..NewInboxItem::default()
        })
        .is_err());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn run_meta_sets_fields_and_gates_checkpoints_on_freshness() {
        let (dir, paths) = fx("meta");
        write(&paths, None, "t", Some("Goal: x"), None).unwrap();
        let s = set_run_meta(
            &paths,
            None,
            "t",
            Some(RunPhase::Running),
            Some(RunMandate {
                project: "blockfield".into(),
                ..RunMandate::default()
            }),
            Some(Some("2026-11-22T00:00:00Z".into())),
            true,
        )
        .unwrap();
        assert_eq!(s.phase, RunPhase::Running);
        assert_eq!(s.mandate.as_ref().unwrap().fix_forward_max, 3);
        assert_eq!(s.checkpoint_count, 1);
        assert!(!s.last_checkpoint_at.is_empty());
        // Age the snapshot, then a checkpoint must be refused.
        let p = |id: &str, title: Option<&str>| ItemPatch {
            id: id.into(),
            title: title.map(Into::into),
            ..ItemPatch::default()
        };
        update_checklist(&paths, None, "t", None, &[p("a", Some("A"))], &[]).unwrap();
        for _ in 0..4 {
            let note = ItemPatch {
                id: "a".into(),
                note: Some("n".into()),
                ..ItemPatch::default()
            };
            update_checklist(&paths, None, "t", None, &[note], &[]).unwrap();
        }
        assert!(
            set_run_meta(&paths, None, "t", None, None, None, true).is_err(),
            "stale checkpoint must be refused"
        );
        // Non-checkpoint meta changes still work while stale.
        let s = set_run_meta(
            &paths,
            None,
            "t",
            Some(RunPhase::SleepingForQuota),
            None,
            None,
            false,
        )
        .unwrap();
        assert_eq!(s.phase, RunPhase::SleepingForQuota);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn unblocked_work_ignores_done_held_blocked() {
        let (dir, paths) = fx("unblocked");
        let p = |id: &str, title: &str, status: &str| ItemPatch {
            id: id.into(),
            title: Some(title.into()),
            status: Some(status.into()),
            ..ItemPatch::default()
        };
        update_checklist(
            &paths,
            None,
            "t",
            None,
            &[
                p("a", "A", "done"),
                p("b", "B", "held"),
                p("c", "C", "blocked"),
            ],
            &[],
        )
        .unwrap();
        assert!(!load(&paths, "t").unwrap().unwrap().has_unblocked_work());
        update_checklist(&paths, None, "t", None, &[p("d", "D", "review")], &[]).unwrap();
        assert!(load(&paths, "t").unwrap().unwrap().has_unblocked_work());
        let _ = fs::remove_dir_all(dir);
    }
}
