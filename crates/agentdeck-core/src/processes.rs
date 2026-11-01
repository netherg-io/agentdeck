//! Live /proc snapshot of agent-related processes grouped by owning session.
//!
//! Read-only except [`stop_group`]: the snapshot covers the agent-process
//! registry owner (`$XDG_RUNTIME_DIR/ai-agent-processes`, env tags
//! `AI_AGENT_PROCESS_OWNER`/`AI_AGENT_PROCESS_ID`), T3 `t3code-*.scope`
//! cgroups (mapped to T3 threads read-only when the schema allows, else the
//! scope name) and harness processes (`claude`, `codex`, `opencode`, `agy`)
//! with descendants by ancestry. Rootless podman containers (`libpod-<id>.scope`)
//! form their own group when their `conmon` carries an agent tag or sits in a
//! T3 scope: conmon inherits the launching agent's env and cgroup, while the
//! container processes do not. Unreadable counters are `None` (a dash in
//! the UI), never an error. Only the GUI calls [`stop_group`] (the owner's
//! click is the approval); the CLI stays read-only and stopping from the CLI
//! stays `agent-process stop`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Argv[0] basenames (or comm names) that mark a harness root.
const HARNESS_NAMES: &[&str] = &["claude", "codex", "opencode", "agy"];

/// Production /proc root; `AGENTDECK_PROC_DIR` overrides it (tests).
pub fn proc_root() -> PathBuf {
    std::env::var_os("AGENTDECK_PROC_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/proc"))
}

/// Production cgroup root; `AGENTDECK_CGROUP_DIR` overrides it (tests).
fn cgroup_root() -> PathBuf {
    std::env::var_os("AGENTDECK_CGROUP_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/sys/fs/cgroup"))
}

/// agent-process registry dir (same resolution as agent-process itself).
fn registry_dir() -> PathBuf {
    if let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(rt).join("ai-agent-processes");
    }
    let uid = std::fs::read_to_string("/proc/self/loginuid")
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(0);
    PathBuf::from(format!("/tmp/ai-agent-processes-{uid}"))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcInfo {
    pub pid: u32,
    pub ppid: u32,
    pub pgrp: u32,
    pub comm: String,
    pub argv: Vec<String>,
    pub cpu_percent: f64,
    pub rss_bytes: u64,
    pub read_bytes: Option<u64>,
    pub write_bytes: Option<u64>,
    pub start_ticks: u64,
    pub scope: Option<String>,
    pub owner_tag: Option<String>,
    /// Podman container id when the process runs in one.
    #[serde(default)]
    pub container: Option<String>,
}

impl ProcInfo {
    pub fn display_cmd(&self) -> String {
        if self.argv.is_empty() {
            self.comm.clone()
        } else {
            self.argv.join(" ")
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GroupKind {
    Agent {
        owner: String,
        id: String,
    },
    T3 {
        scope: String,
    },
    Harness {
        name: String,
    },
    /// A podman container started by an agent; stopped with `podman stop`.
    Container {
        id: String,
        name: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionGroup {
    pub kind: GroupKind,
    pub title: String,
    pub pid: u32,
    pub start_ticks: u64,
    pub pgrp: u32,
    /// False for T3 scopes: those are interrupted through T3, never killed.
    pub can_stop: bool,
    pub processes: Vec<ProcInfo>,
    pub cpu_percent: f64,
    pub rss_bytes: u64,
    pub read_bytes: Option<u64>,
    pub write_bytes: Option<u64>,
    /// T3 scopes only: cgroup `memory.current` when readable.
    pub scope_mem_bytes: Option<u64>,
    /// Container name when the group runs in (or is) a podman container.
    #[serde(default)]
    pub container: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub groups: Vec<SessionGroup>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopOutcome {
    Terminated,
    Killed,
    AlreadyExited,
    StillRunning,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StopReport {
    pub signaled: Vec<u32>,
    pub outcome: StopOutcome,
}

struct Stat {
    comm: String,
    state: char,
    ppid: u32,
    pgrp: u32,
    utime: u64,
    stime: u64,
    start_ticks: u64,
    rss_pages: u64,
}

/// Parse `/proc/<pid>/stat`, tolerating spaces and parens in comm by cutting
/// between the first `(` and the last `)`.
fn parse_stat(text: &str) -> Option<Stat> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    if close <= open + 1 || text[..open].trim().parse::<u32>().is_err() {
        return None;
    }
    let comm = text[open + 1..close].to_string();
    let rest: Vec<&str> = text[close + 1..].split_whitespace().collect();
    if rest.len() < 22 {
        return None;
    }
    let n = |i: usize| rest.get(i)?.parse::<u64>().ok();
    let n32 = |i: usize| rest.get(i)?.parse::<u32>().ok();
    Some(Stat {
        comm,
        state: rest[0].chars().next().unwrap_or('?'),
        ppid: n32(1)?,
        pgrp: n32(2)?,
        utime: n(11)?,
        stime: n(12)?,
        start_ticks: n(19)?,
        rss_pages: n(21)?,
    })
}

fn read_nul_list(path: &Path) -> Vec<String> {
    let raw = std::fs::read(path).unwrap_or_default();
    raw.split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}

fn env_tag(path: &Path, key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    read_nul_list(path)
        .into_iter()
        .find_map(|e| e.strip_prefix(&prefix).map(str::to_string))
}

/// First T3 unit segment (`t3code-*.scope`, `t3code.service`) in `/proc/<pid>/cgroup`.
fn scope_of(proc: &Path) -> Option<String> {
    scope_cgroup(proc).map(|(scope, _)| scope)
}

/// Scope name plus its cgroup-v2 path (after the `0::` hierarchy prefix).
fn scope_cgroup(proc: &Path) -> Option<(String, String)> {
    let text = std::fs::read_to_string(proc.join("cgroup")).ok()?;
    for line in text.lines() {
        let path = line.split_once(':').map(|(_, p)| p).unwrap_or(line);
        let path = path.rsplit_once(':').map(|(_, p)| p).unwrap_or(path);
        for seg in path.split('/') {
            let seg = seg.trim();
            // T3 session wrappers still live in the T3 service unit before they exec
            // into a per-agent scope: both are T3's, and only T3 may interrupt them.
            if (seg.starts_with("t3code-") && seg.ends_with(".scope")) || seg == "t3code.service" {
                return Some((seg.to_string(), path.to_string()));
            }
        }
    }
    None
}

/// Podman container id from a `libpod-<64 hex>.scope` cgroup segment.
fn container_of(proc: &Path) -> Option<String> {
    let text = std::fs::read_to_string(proc.join("cgroup")).ok()?;
    text.split(['/', '\n']).find_map(|seg| {
        let id = seg.strip_prefix("libpod-")?.strip_suffix(".scope")?;
        (id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit())).then(|| id.to_string())
    })
}

/// The container a `conmon` process monitors (`-c <id>` / `--cid <id>`).
fn conmon_container(comm: &str, argv: &[String]) -> Option<String> {
    if comm != "conmon" {
        return None;
    }
    argv.windows(2)
        .find(|w| w[0] == "-c" || w[0] == "--cid")
        .map(|w| w[1].clone())
}

/// Container names by full id; empty when podman is missing or slow.
fn podman_names() -> HashMap<String, String> {
    let Ok(child) = std::process::Command::new("podman")
        .args(["ps", "--no-trunc", "--format", "{{.ID}} {{.Names}}"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return HashMap::new();
    };
    let Ok(out) = child.wait_with_output() else {
        return HashMap::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_once(' '))
        .map(|(id, name)| (id.to_string(), name.trim().to_string()))
        .collect()
}

/// `podman stop` for a container group (grace in seconds, then SIGKILL).
pub fn stop_container(id: &str, grace: Duration) -> anyhow::Result<StopReport> {
    if id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        anyhow::bail!("not a container id: {id}");
    }
    let status = std::process::Command::new("podman")
        .args(["stop", "-t", &grace.as_secs().to_string(), id])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .status()?;
    if !status.success() {
        anyhow::bail!("podman stop {} failed: {status}", &id[..12]);
    }
    Ok(StopReport {
        signaled: Vec::new(),
        outcome: StopOutcome::Terminated,
    })
}

/// Per-scope resident memory from the cgroup fs; unreadable means `None`
/// (a dash), never an error.
fn scope_mem_bytes(cgroup_path: &str) -> Option<u64> {
    let path = cgroup_path.trim_start_matches('/');
    std::fs::read_to_string(cgroup_root().join(path).join("memory.current"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn io_of(proc: &Path) -> (Option<u64>, Option<u64>) {
    let text = std::fs::read_to_string(proc.join("io")).unwrap_or_default();
    let mut read = None;
    let mut write = None;
    for line in text.lines() {
        let (k, v) = line.split_once(':').unwrap_or((line, ""));
        match k.trim() {
            "read_bytes" => read = v.trim().parse().ok(),
            "write_bytes" => write = v.trim().parse().ok(),
            _ => {}
        }
    }
    (read, write)
}

fn page_size() -> u64 {
    let ps = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if ps > 0 { ps as u64 } else { 4096 }
}

fn total_ticks(root: &Path) -> u64 {
    std::fs::read_to_string(root.join("stat"))
        .ok()
        .and_then(|t| t.lines().next()?.strip_prefix("cpu ").map(str::to_string))
        .map(|line| {
            line.split_whitespace()
                .filter_map(|w| w.parse::<u64>().ok())
                .sum()
        })
        .unwrap_or(0)
}

/// One raw sample: per-pid cpu ticks + start ticks, plus the system total.
struct Sample {
    ticks: HashMap<u32, (u64, u64)>,
    total: u64,
}

fn take_sample(root: &Path) -> Sample {
    let mut ticks = HashMap::new();
    if let Ok(dir) = std::fs::read_dir(root) {
        for entry in dir.flatten() {
            let Ok(pid): Result<u32, _> = entry.file_name().to_string_lossy().parse() else {
                continue;
            };
            if let Ok(text) = std::fs::read_to_string(entry.path().join("stat"))
                && let Some(st) = parse_stat(&text)
            {
                ticks.insert(pid, (st.utime + st.stime, st.start_ticks));
            }
        }
    }
    Sample {
        ticks,
        total: total_ticks(root),
    }
}

fn harness_name(comm: &str, argv: &[String]) -> Option<String> {
    let mut cands = vec![comm.to_lowercase()];
    if let Some(first) = argv.first()
        && let Some(base) = first.rsplit('/').next()
    {
        cands.push(base.to_lowercase());
    }
    cands.into_iter().find_map(|c| {
        let base = c.split_whitespace().next().unwrap_or(&c).to_string();
        HARNESS_NAMES
            .iter()
            .find(|n| base == **n || base.starts_with(&format!("{n}-")))
            .map(|n| n.to_string())
    })
}

#[derive(Debug, Clone)]
struct RegEntry {
    id: String,
    name: String,
    owner: String,
    pid: u32,
    pgid: u32,
    start_ticks: String,
}

fn read_registry(dir: &Path) -> Vec<RegEntry> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(v): Result<serde_json::Value, _> = serde_json::from_str(&text) else {
            continue;
        };
        let get = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
        let Some(pid) = v.get("pid").and_then(|x| x.as_u64()).map(|x| x as u32) else {
            continue;
        };
        out.push(RegEntry {
            id: get("id"),
            name: get("name"),
            owner: get("owner"),
            pid,
            pgid: v
                .get("pgid")
                .and_then(|x| x.as_u64())
                .map(|x| x as u32)
                .unwrap_or(pid),
            start_ticks: get("start_ticks"),
        });
    }
    out
}

/// Live registry entries: the leader pid still exists with matching ticks.
fn live_entries(root: &Path, dir: &Path) -> Vec<RegEntry> {
    read_registry(dir)
        .into_iter()
        .filter(|e| {
            std::fs::read_to_string(root.join(format!("{}/stat", e.pid)))
                .ok()
                .and_then(|t| parse_stat(&t))
                .is_some_and(|st| st.start_ticks.to_string() == e.start_ticks)
        })
        .collect()
}

struct RawProc {
    pid: u32,
    st: Stat,
    argv: Vec<String>,
    owner_tag: Option<String>,
    id_tag: Option<String>,
    scope: Option<String>,
    scope_path: Option<String>,
    container: Option<String>,
    cpu: f64,
}

fn collect_raw(root: &Path, prev: &Sample, curr: &Sample) -> Vec<RawProc> {
    let dt = curr.total.saturating_sub(prev.total).max(1) as f64;
    let mut out = Vec::new();
    if let Ok(dir) = std::fs::read_dir(root) {
        for entry in dir.flatten() {
            let Ok(pid): Result<u32, _> = entry.file_name().to_string_lossy().parse() else {
                continue;
            };
            let base = entry.path();
            let Ok(text) = std::fs::read_to_string(base.join("stat")) else {
                continue;
            };
            let Some(st) = parse_stat(&text) else {
                continue;
            };
            let argv = read_nul_list(&base.join("cmdline"));
            let env = base.join("environ");
            let (scope, scope_path) = scope_cgroup(&base).unzip();
            let cpu = match (prev.ticks.get(&pid), curr.ticks.get(&pid)) {
                (Some((a, _)), Some((b, _))) => 100.0 * b.saturating_sub(*a) as f64 / dt,
                _ => 0.0,
            };
            out.push(RawProc {
                pid,
                st,
                argv,
                owner_tag: env_tag(&env, "AI_AGENT_PROCESS_OWNER"),
                id_tag: env_tag(&env, "AI_AGENT_PROCESS_ID"),
                scope,
                scope_path,
                container: container_of(&base),
                cpu,
            });
        }
    }
    out
}

fn to_info(root: &Path, raw: &RawProc) -> ProcInfo {
    let base = root.join(raw.pid.to_string());
    let (read_bytes, write_bytes) = io_of(&base);
    ProcInfo {
        pid: raw.pid,
        ppid: raw.st.ppid,
        pgrp: raw.st.pgrp,
        comm: raw.st.comm.clone(),
        argv: raw.argv.clone(),
        cpu_percent: raw.cpu,
        rss_bytes: raw.st.rss_pages * page_size(),
        read_bytes,
        write_bytes,
        start_ticks: raw.st.start_ticks,
        scope: raw.scope.clone(),
        owner_tag: raw.owner_tag.clone(),
        container: raw.container.clone(),
    }
}

fn snapshot_from_raw(
    root: &Path,
    reg_dir: &Path,
    raw: &[RawProc],
    names: &dyn Fn() -> HashMap<String, String>,
) -> Snapshot {
    let entries = live_entries(root, reg_dir);
    // Every entry, live or not: `podman compose up -d` exits right away, but
    // its containers' conmon still carries the entry's id tag.
    let all_entries = read_registry(reg_dir);
    // Containers whose conmon was launched by an agent: id -> owner label.
    let mut agent_containers: HashMap<String, String> = HashMap::new();
    for r in raw {
        let Some(id) = conmon_container(&r.st.comm, &r.argv) else {
            continue;
        };
        let owner = r
            .id_tag
            .as_deref()
            .and_then(|t| all_entries.iter().find(|e| e.id == t))
            .map(|e| e.name.clone())
            .filter(|n| !n.is_empty())
            .or_else(|| r.owner_tag.clone())
            .or_else(|| r.scope.clone().filter(|s| s != "t3code.service"));
        if let Some(owner) = owner {
            agent_containers.insert(id, owner);
        }
    }
    let names = if agent_containers.is_empty() {
        HashMap::new()
    } else {
        names()
    };
    let by_pid: HashMap<u32, &RawProc> = raw.iter().map(|r| (r.pid, r)).collect();
    let tag_by_pid: HashMap<u32, &str> = raw
        .iter()
        .filter_map(|r| r.id_tag.as_deref().map(|t| (r.pid, t)))
        .collect();
    let entry_pids: HashSet<u32> = entries.iter().map(|e| e.pid).collect();

    // Nearest grouped ancestor (or self): registry entry pid, else harness root.
    fn owner_of(
        pid: u32,
        by_pid: &HashMap<u32, &RawProc>,
        tag_by_pid: &HashMap<u32, &str>,
        entries: &[RegEntry],
        entry_pids: &HashSet<u32>,
    ) -> Option<String> {
        let mut cur = Some(pid);
        let mut depth = 0;
        while let Some(p) = cur {
            if let Some(id) = tag_by_pid.get(&p)
                && entries.iter().any(|e| &e.id == id)
            {
                return Some((*id).to_string());
            }
            if entry_pids.contains(&p) {
                return entries.iter().find(|e| e.pid == p).map(|e| e.id.clone());
            }
            cur = by_pid
                .get(&p)
                .map(|r| r.st.ppid)
                .filter(|pp| *pp != p && *pp > 0);
            depth += 1;
            if depth > 64 {
                break;
            }
        }
        None
    }

    fn harness_root_of(pid: u32, by_pid: &HashMap<u32, &RawProc>) -> Option<(u32, String)> {
        let mut cur = Some(pid);
        let mut depth = 0;
        let mut found = None;
        while let Some(p) = cur {
            if let Some(r) = by_pid.get(&p)
                && let Some(name) = harness_name(&r.st.comm, &r.argv)
            {
                found = Some((p, name));
            }
            cur = by_pid
                .get(&p)
                .map(|r| r.st.ppid)
                .filter(|pp| *pp != p && *pp > 0);
            depth += 1;
            if depth > 64 || cur.is_none_or(|pp| !by_pid.contains_key(&pp)) {
                break;
            }
        }
        found
    }

    // (group key, kind, leader pid/ticks/pgrp, can_stop). T3 scopes never
    // stop via signal; the GUI/CLI explain that with a localized note.
    let mut assignment: HashMap<u32, String> = HashMap::new();
    let mut meta: HashMap<String, (GroupKind, u32, u64, u32, bool)> = HashMap::new();
    for r in raw {
        let cid = r
            .container
            .clone()
            .or_else(|| conmon_container(&r.st.comm, &r.argv));
        if let Some(cid) = cid.filter(|c| agent_containers.contains_key(c)) {
            let key = format!("podman:{cid}");
            let name = names
                .get(&cid)
                .cloned()
                .unwrap_or_else(|| cid[..12.min(cid.len())].to_string());
            // Leader: the container's conmon (its stable parent), else any member.
            meta.entry(key.clone()).or_insert_with(|| {
                (
                    GroupKind::Container {
                        id: cid.clone(),
                        name,
                    },
                    r.pid,
                    r.st.start_ticks,
                    r.st.pgrp,
                    true,
                )
            });
            if conmon_container(&r.st.comm, &r.argv).is_some()
                && let Some(m) = meta.get_mut(&key)
            {
                (m.1, m.2, m.3) = (r.pid, r.st.start_ticks, r.st.pgrp);
            }
            assignment.insert(r.pid, key);
            continue;
        }
        let entry_id = r
            .id_tag
            .as_deref()
            .filter(|id| entries.iter().any(|e| &e.id == id))
            .map(str::to_string)
            .or_else(|| owner_of(r.pid, &by_pid, &tag_by_pid, &entries, &entry_pids));
        if let Some(id) = entry_id {
            let e = entries.iter().find(|e| e.id == id).expect("entry exists");
            meta.entry(format!("agent:{id}")).or_insert_with(|| {
                (
                    GroupKind::Agent {
                        owner: e.owner.clone(),
                        id: e.id.clone(),
                    },
                    e.pid,
                    e.start_ticks.parse().unwrap_or(0),
                    e.pgid,
                    true,
                )
            });
            assignment.insert(r.pid, format!("agent:{id}"));
            continue;
        }
        // In t3code.service only the launch wrapper of an agent scope is agent
        // work; the server, browser and tunnel are T3 itself.
        let scope = match r.scope.as_deref() {
            Some("t3code.service") => r.argv.iter().find_map(|a| {
                a.split_whitespace()
                    .find_map(|w| w.strip_prefix("--unit="))
                    .filter(|u| u.starts_with("t3code-") && u.ends_with(".scope"))
                    .map(str::to_string)
            }),
            other => other.map(str::to_string),
        };
        if let Some(scope) = &scope {
            let label = scope.clone();
            meta.entry(format!("t3:{scope}")).or_insert_with(|| {
                (
                    GroupKind::T3 { scope: label },
                    r.pid,
                    r.st.start_ticks,
                    r.st.pgrp,
                    false,
                )
            });
            assignment.insert(r.pid, format!("t3:{scope}"));
            continue;
        }
        if let Some((hpid, name)) = harness_root_of(r.pid, &by_pid) {
            let key = format!("harness:{name}:{hpid}");
            if let Some(h) = by_pid.get(&hpid) {
                meta.entry(key.clone()).or_insert((
                    GroupKind::Harness { name: name.clone() },
                    hpid,
                    h.st.start_ticks,
                    h.st.pgrp,
                    true,
                ));
                assignment.insert(r.pid, key);
            }
        }
    }

    let mut groups: HashMap<String, Vec<&RawProc>> = HashMap::new();
    for r in raw {
        if let Some(key) = assignment.get(&r.pid) {
            groups.entry(key.clone()).or_default().push(r);
        }
    }
    let mut snap = Snapshot::default();
    for (key, members) in groups {
        let (kind, pid, ticks, pgrp, can_stop) = meta.remove(&key).expect("meta set above");
        let infos: Vec<ProcInfo> = {
            let mut v: Vec<ProcInfo> = members.iter().map(|r| to_info(root, r)).collect();
            v.sort_by_key(|p| p.pid);
            v
        };
        let container_owner = match &kind {
            GroupKind::Container { id, .. } => agent_containers.get(id).cloned(),
            _ => None,
        };
        let title = match &kind {
            GroupKind::Agent { owner, id } => {
                // The registered name says what runs; owner/id stay for identity.
                match entries
                    .iter()
                    .find(|e| &e.id == id)
                    .map(|e| e.name.as_str())
                {
                    Some(name) if !name.is_empty() => format!("{name} ({owner} {id})"),
                    _ => format!("{owner} ({id})"),
                }
            }
            GroupKind::T3 { scope } => scope.clone(),
            GroupKind::Harness { name } => name.clone(),
            GroupKind::Container { name, .. } => match &container_owner {
                Some(owner) => format!("{name} ({owner})"),
                None => name.clone(),
            },
        };
        let container = match &kind {
            GroupKind::Container { name, .. } => Some(name.clone()),
            _ => infos.iter().find_map(|p| p.container.as_ref()).map(|id| {
                names
                    .get(id)
                    .cloned()
                    .unwrap_or_else(|| id[..12.min(id.len())].to_string())
            }),
        };
        let sum = |f: fn(&ProcInfo) -> Option<u64>| {
            let mut total = 0u64;
            let mut any = false;
            for p in &infos {
                if let Some(v) = f(p) {
                    total += v;
                    any = true;
                }
            }
            any.then_some(total)
        };
        // Cgroup memory only exists for T3 scopes; unreadable means None.
        let scope_mem_bytes = (!can_stop)
            .then(|| {
                members
                    .iter()
                    .filter_map(|r| r.scope_path.as_deref())
                    .find_map(scope_mem_bytes)
            })
            .flatten();
        snap.groups.push(SessionGroup {
            kind,
            title,
            pid,
            start_ticks: ticks,
            pgrp,
            can_stop,
            cpu_percent: infos.iter().map(|p| p.cpu_percent).sum(),
            rss_bytes: infos.iter().map(|p| p.rss_bytes).sum(),
            read_bytes: sum(|p| p.read_bytes),
            write_bytes: sum(|p| p.write_bytes),
            scope_mem_bytes,
            container,
            processes: infos,
        });
    }
    snap.groups.sort_by(|a, b| a.title.cmp(&b.title));
    snap
}

fn snapshot_with_roots(root: &Path, reg_dir: &Path) -> Snapshot {
    let prev = take_sample(root);
    std::thread::sleep(Duration::from_millis(200));
    let curr = take_sample(root);
    let raw = collect_raw(root, &prev, &curr);
    snapshot_from_raw(root, reg_dir, &raw, &podman_names)
}

/// Live snapshot (two /proc samples 200 ms apart). Read-only, never fails.
pub fn snapshot() -> Snapshot {
    snapshot_with_roots(&proc_root(), &registry_dir())
}

fn read_stat_in(root: &Path, pid: u32) -> anyhow::Result<Stat> {
    let text = std::fs::read_to_string(root.join(format!("{pid}/stat")))
        .map_err(|_| anyhow::anyhow!("process {pid} is gone"))?;
    parse_stat(&text).ok_or_else(|| anyhow::anyhow!("cannot parse stat for {pid}"))
}

/// Live processes sharing the leader's process group, after re-checking the
/// leader identity (pid + start ticks). Used by the stop confirmation.
pub fn group_members(
    proc_root: &Path,
    pid: u32,
    start_ticks: u64,
) -> anyhow::Result<Vec<ProcInfo>> {
    let st = read_stat_in(proc_root, pid)?;
    if st.start_ticks != start_ticks {
        anyhow::bail!("process {pid} identity changed; refusing to list");
    }
    let pgrp = st.pgrp;
    let mut out = Vec::new();
    let Ok(dir) = std::fs::read_dir(proc_root) else {
        return Ok(out);
    };
    for entry in dir.flatten() {
        let Ok(q): Result<u32, _> = entry.file_name().to_string_lossy().parse() else {
            continue;
        };
        let base = entry.path();
        let Ok(text) = std::fs::read_to_string(base.join("stat")) else {
            continue;
        };
        if let Some(s) = parse_stat(&text)
            && s.pgrp == pgrp
        {
            let argv = read_nul_list(&base.join("cmdline"));
            let (read_bytes, write_bytes) = io_of(&base);
            out.push(ProcInfo {
                pid: q,
                ppid: s.ppid,
                pgrp: s.pgrp,
                comm: s.comm,
                argv,
                cpu_percent: 0.0,
                rss_bytes: s.rss_pages * page_size(),
                read_bytes,
                write_bytes,
                start_ticks: s.start_ticks,
                scope: scope_of(&base),
                owner_tag: env_tag(&base.join("environ"), "AI_AGENT_PROCESS_OWNER"),
                container: container_of(&base),
            });
        }
    }
    out.sort_by_key(|p| p.pid);
    Ok(out)
}

fn signal_group(pgrp: u32, sig: libc::c_int) -> std::io::Result<()> {
    // Negative pid targets the process group.
    let r = unsafe { libc::kill(-(pgrp as i32), sig) };
    if r == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn group_alive(root: &Path, members: &[u32]) -> bool {
    // Zombies still have a stat file: only a non-zombie state counts.
    members.iter().any(|pid| {
        std::fs::read_to_string(root.join(format!("{pid}/stat")))
            .ok()
            .and_then(|t| parse_stat(&t))
            .is_some_and(|st| st.state != 'Z')
    })
}

/// SIGTERM the leader's process group (identity-checked), wait `grace`, then
/// optionally SIGKILL. Only the GUI calls this (owner approval via click).
pub fn stop_group(
    pid: u32,
    start_ticks: u64,
    grace: Duration,
    escalate: bool,
) -> anyhow::Result<StopReport> {
    stop_group_in(&proc_root(), pid, start_ticks, grace, escalate)
}

fn stop_group_in(
    root: &Path,
    pid: u32,
    start_ticks: u64,
    grace: Duration,
    escalate: bool,
) -> anyhow::Result<StopReport> {
    if pid <= 1 {
        anyhow::bail!("refusing to stop pid {pid}");
    }
    let st = read_stat_in(root, pid)?;
    if st.start_ticks != start_ticks {
        anyhow::bail!("process {pid} identity changed; refusing to signal");
    }
    if st.pgrp <= 1 {
        anyhow::bail!("refusing to signal process group {}", st.pgrp);
    }
    let members: Vec<u32> = group_members(root, pid, start_ticks)?
        .into_iter()
        .map(|p| p.pid)
        .collect();
    match signal_group(st.pgrp, libc::SIGTERM) {
        Ok(()) => {}
        Err(e) if e.raw_os_error() == Some(libc::ESRCH) => {
            return Ok(StopReport {
                signaled: vec![],
                outcome: StopOutcome::AlreadyExited,
            });
        }
        Err(e) => anyhow::bail!("SIGTERM to process group {} failed: {e}", st.pgrp),
    }
    let deadline = std::time::Instant::now() + grace;
    while std::time::Instant::now() < deadline {
        if !group_alive(root, &members) {
            return Ok(StopReport {
                signaled: members,
                outcome: StopOutcome::Terminated,
            });
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if !escalate {
        return Ok(StopReport {
            signaled: members,
            outcome: StopOutcome::StillRunning,
        });
    }
    match signal_group(st.pgrp, libc::SIGKILL) {
        Ok(()) => {}
        Err(e) if e.raw_os_error() == Some(libc::ESRCH) => {
            return Ok(StopReport {
                signaled: members,
                outcome: StopOutcome::Terminated,
            });
        }
        Err(e) => anyhow::bail!("SIGKILL to process group {} failed: {e}", st.pgrp),
    }
    let kill_deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < kill_deadline {
        if !group_alive(root, &members) {
            return Ok(StopReport {
                signaled: members,
                outcome: StopOutcome::Killed,
            });
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(StopReport {
        signaled: members,
        outcome: StopOutcome::StillRunning,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static N: AtomicU64 = AtomicU64::new(0);

    struct Guard(PathBuf);
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tmpdir(tag: &str) -> (PathBuf, Guard) {
        let dir = std::env::temp_dir().join(format!(
            "agentdeck-proc-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        (dir.clone(), Guard(dir))
    }

    /// 22 fields after comm: state ppid pgrp session + 7 + utime stime +
    /// cutime cstime prio nice threads itreal starttime vsize rss.
    fn stat_line(pid: u32, comm: &str, ppid: u32, pgrp: u32, start: u64) -> String {
        format!("{pid} ({comm}) R {ppid} {pgrp} 1 0 0 0 0 0 0 0 10 5 0 0 0 0 1 0 {start} 100 25\n")
    }

    fn fake_proc(root: &Path, pid: &str, stat: &str, argv: &[u8], env: &[u8], cgroup: &str) {
        let dir = root.join(pid);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("stat"), stat).unwrap();
        std::fs::write(dir.join("cmdline"), argv).unwrap();
        std::fs::write(dir.join("environ"), env).unwrap();
        std::fs::write(dir.join("cgroup"), cgroup).unwrap();
        std::fs::write(
            dir.join("io"),
            "rchar: 0\nread_bytes: 4096\nwrite_bytes: 8192\n",
        )
        .unwrap();
    }

    #[test]
    fn stat_parses_comm_with_spaces_and_parens() {
        // pid comes from the directory name; stat carries comm + counters.
        let st = parse_stat(&stat_line(7, "my proc (x)", 1, 7, 99)).unwrap();
        assert_eq!(st.comm, "my proc (x)");
        assert_eq!(st.ppid, 1);
        assert_eq!(st.pgrp, 7);
        assert_eq!(st.utime, 10);
        assert_eq!(st.stime, 5);
        assert_eq!(st.start_ticks, 99);
        assert_eq!(st.rss_pages, 25);
    }

    #[test]
    fn snapshot_groups_by_session_scope_and_harness() {
        let (dir, _g) = tmpdir("groups");
        let root = dir.join("proc");
        let reg = dir.join("reg");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&reg).unwrap();
        std::fs::write(
            root.join("stat"),
            "cpu 100 0 100 800 0 0 0 0 0 0\ncpu0 50 0 50 400 0 0 0 0 0 0\n",
        )
        .unwrap();
        // Registry-owned: leader pid 100 tagged + child 101.
        fake_proc(
            &root,
            "100",
            &stat_line(100, "sleep", 1, 100, 1000),
            b"sleep\0 60\0",
            b"AI_AGENT_PROCESS_OWNER=ses1\0AI_AGENT_PROCESS_ID=abc\0",
            "0::/\n",
        );
        fake_proc(
            &root,
            "101",
            &stat_line(101, "sleep", 100, 100, 1001),
            b"sleep 60\0",
            b"AI_AGENT_PROCESS_OWNER=ses1\0AI_AGENT_PROCESS_ID=abc\0",
            "0::/\n",
        );
        // T3 scope member.
        fake_proc(
            &root,
            "200",
            &stat_line(200, "node", 1, 200, 2000),
            b"node\0server\0",
            b"PATH=/x\0",
            "0::/user.slice/t3code-abc123.scope\n",
        );
        // Harness root + descendant.
        fake_proc(
            &root,
            "300",
            &stat_line(300, "codex", 1, 300, 3000),
            b"codex\0tui\0",
            b"PATH=/x\0",
            "0::/\n",
        );
        fake_proc(
            &root,
            "301",
            &stat_line(301, "sh", 300, 300, 3001),
            b"sh\0-c\0",
            b"PATH=/x\0",
            "0::/\n",
        );
        // Unrelated process: skipped.
        fake_proc(
            &root,
            "400",
            &stat_line(400, "my proc (x)", 1, 400, 4000),
            b"ls\0",
            b"PATH=/x\0",
            "0::/\n",
        );
        std::fs::write(
            reg.join("abc.json"),
            r#"{"id":"abc","name":"srv","owner":"ses1","pid":100,"pgid":100,"start_ticks":"1000"}"#,
        )
        .unwrap();
        let prev = take_sample(&root);
        let curr = take_sample(&root);
        let raw = collect_raw(&root, &prev, &curr);
        assert_eq!(raw.len(), 6);
        let snap = snapshot_from_raw(&root, &reg, &raw, &HashMap::new);
        assert_eq!(snap.groups.len(), 3);
        let agent = snap
            .groups
            .iter()
            .find(|g| matches!(&g.kind, GroupKind::Agent { .. }))
            .unwrap();
        assert_eq!(agent.processes.len(), 2);
        assert!(agent.can_stop);
        assert_eq!(agent.title, "srv (ses1 abc)");
        let t3 = snap
            .groups
            .iter()
            .find(|g| matches!(&g.kind, GroupKind::T3 { .. }))
            .unwrap();
        assert!(!t3.can_stop);
        assert!(!t3.can_stop);
        assert_eq!(t3.processes.len(), 1);
        let h = snap
            .groups
            .iter()
            .find(|g| matches!(&g.kind, GroupKind::Harness { .. }))
            .unwrap();
        assert_eq!(h.processes.len(), 2);
        assert!(h.read_bytes == Some(8192));
        assert!(h.write_bytes == Some(16384));
        // Unreadable IO is None, never an error.
        std::fs::remove_file(root.join("300/io")).unwrap();
        let raw = collect_raw(&root, &prev, &curr);
        let snap = snapshot_from_raw(&root, &reg, &raw, &HashMap::new);
        let h = snap
            .groups
            .iter()
            .find(|g| matches!(&g.kind, GroupKind::Harness { .. }))
            .unwrap();
        assert_eq!(h.read_bytes, Some(4096));
    }

    #[test]
    fn agent_podman_containers_group_through_conmon() {
        let (root, _g) = tmpdir("podman");
        let reg = root.join("reg");
        std::fs::create_dir_all(&reg).unwrap();
        let id = "a".repeat(64);
        let other = "b".repeat(64);
        let scope = "0::/user.slice/t3code-claude-1.scope\n";
        // conmon of an agent's container: dead registry entry's tag, T3 scope.
        fake_proc(
            &root,
            "500",
            &stat_line(500, "conmon", 1, 500, 5000),
            format!("conmon\0-c\0{id}\0").as_bytes(),
            b"AI_AGENT_PROCESS_ID=dead1\0AI_AGENT_PROCESS_OWNER=ses9\0",
            scope,
        );
        fake_proc(
            &root,
            "501",
            &stat_line(501, "apache2", 500, 501, 5010),
            b"apache2\0",
            b"container=podman\0",
            &format!("0::/user.slice/libpod-{id}.scope/container\n"),
        );
        // The user's own container: untagged conmon outside T3, skipped.
        fake_proc(
            &root,
            "600",
            &stat_line(600, "conmon", 1, 600, 6000),
            format!("conmon\0-c\0{other}\0").as_bytes(),
            b"PATH=/x\0",
            "0::/user.slice/app.slice\n",
        );
        fake_proc(
            &root,
            "601",
            &stat_line(601, "postgres", 600, 601, 6010),
            b"postgres\0",
            b"",
            &format!("0::/user.slice/libpod-{other}.scope/container\n"),
        );
        // T3 itself inside t3code.service: not agent work.
        fake_proc(
            &root,
            "700",
            &stat_line(700, "t3", 1, 700, 7000),
            b"t3\0serve\0",
            b"",
            "0::/user.slice/app-t3code.slice/t3code.service\n",
        );
        std::fs::write(
            reg.join("dead1.json"),
            r#"{"id":"dead1","name":"cd-wp","owner":"ses9","pid":999,"pgid":999,"start_ticks":"1"}"#,
        )
        .unwrap();
        let prev = take_sample(&root);
        let curr = take_sample(&root);
        let raw = collect_raw(&root, &prev, &curr);
        let id2 = id.clone();
        let names = move || HashMap::from([(id2.clone(), "cd_wp".to_string())]);
        let snap = snapshot_from_raw(&root, &reg, &raw, &names);
        assert_eq!(snap.groups.len(), 1, "{:?}", snap.groups);
        let g = &snap.groups[0];
        assert!(
            matches!(&g.kind, GroupKind::Container { id: i, name } if *i == id && name == "cd_wp")
        );
        assert_eq!(g.title, "cd_wp (cd-wp)");
        assert_eq!(g.pid, 500);
        assert_eq!(g.container.as_deref(), Some("cd_wp"));
        assert_eq!(g.processes.len(), 2);
    }

    #[test]
    fn scope_cgroup_mem_read_when_present() {
        let (dir, _g) = tmpdir("scope");
        let root = dir.join("proc");
        let reg = dir.join("reg");
        let cg = dir.join("cg");
        for d in [&root, &reg, &cg.join("user.slice/t3code-abc123.scope")] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(root.join("stat"), "cpu 100 0 100 800 0 0 0 0 0 0\n").unwrap();
        fake_proc(
            &root,
            "200",
            &stat_line(200, "node", 1, 200, 2000),
            b"node\0server\0",
            b"PATH=/x\0",
            "0::/user.slice/t3code-abc123.scope\n",
        );
        std::fs::write(
            cg.join("user.slice/t3code-abc123.scope/memory.current"),
            "12345678\n",
        )
        .unwrap();
        unsafe { std::env::set_var("AGENTDECK_CGROUP_DIR", &cg) };
        let prev = take_sample(&root);
        let raw = collect_raw(&root, &prev, &prev);
        let snap = snapshot_from_raw(&root, &reg, &raw, &HashMap::new);
        unsafe { std::env::remove_var("AGENTDECK_CGROUP_DIR") };
        assert_eq!(snap.groups.len(), 1);
        assert_eq!(snap.groups[0].scope_mem_bytes, Some(12345678));
    }

    /// Spawn only-test-owned processes in their own session (pre_exec
    /// setsid: the child pid is the group leader, no helper binary needed).
    /// Never signals anything the test did not start.
    fn spawn_detached(args: &[&str]) -> (std::process::Child, u32, u64) {
        use std::os::unix::process::CommandExt as _;
        let mut cmd = std::process::Command::new(args[0]);
        cmd.args(&args[1..]);
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = cmd
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn test process");
        let pid = child.id();
        // Wait until the leader is visible as its own group leader.
        let mut ticks = None;
        for _ in 0..100 {
            if let Ok(t) = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                && let Some(st) = parse_stat(&t)
                && st.pgrp == pid
            {
                ticks = Some(st.start_ticks);
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = &mut child;
        (child, pid, ticks.expect("spawned tree visible"))
    }

    #[test]
    fn stop_group_identity_check_refuses_before_signaling() {
        let (mut child, pid, ticks) = spawn_detached(&["sh", "-c", "sleep 60 & sleep 61; wait"]);
        // Wait for sh to fork both sleeps before asserting the member list.
        let members = {
            let mut members = Vec::new();
            for _ in 0..150 {
                members = group_members(&PathBuf::from("/proc"), pid, ticks).unwrap();
                if members.len() >= 2 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            members
        };
        assert!(members.len() >= 2, "tree visible: {members:?}");
        // Wrong ticks: no signal sent, tree still alive.
        assert!(
            stop_group_in(
                &PathBuf::from("/proc"),
                pid,
                ticks + 1,
                Duration::from_secs(1),
                true
            )
            .is_err()
        );
        assert!(Path::new(&format!("/proc/{pid}/stat")).exists());
        let r = stop_group_in(
            &PathBuf::from("/proc"),
            pid,
            ticks,
            Duration::from_secs(3),
            true,
        )
        .unwrap();
        assert!(
            matches!(r.outcome, StopOutcome::Terminated | StopOutcome::Killed),
            "{r:?}"
        );
        let _ = child.wait(); // reap the zombie before checking /proc
        assert!(!Path::new(&format!("/proc/{pid}/stat")).exists());
    }

    #[test]
    fn stop_group_no_escalate_leaves_ignorer_running() {
        let (mut child, pid, ticks) = spawn_detached(&["sh", "-c", "trap '' TERM; exec sleep 60"]);
        let r = stop_group_in(
            &PathBuf::from("/proc"),
            pid,
            ticks,
            Duration::from_secs(1),
            false,
        )
        .unwrap();
        assert_eq!(r.outcome, StopOutcome::StillRunning);
        // Cleanup the test-owned process with escalation.
        let r = stop_group_in(
            &PathBuf::from("/proc"),
            pid,
            ticks,
            Duration::from_secs(1),
            true,
        )
        .unwrap();
        assert!(
            matches!(r.outcome, StopOutcome::Terminated | StopOutcome::Killed),
            "{r:?}"
        );
        let _ = child.wait();
    }
}
