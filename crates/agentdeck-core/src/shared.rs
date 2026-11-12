//! Shared cross-harness management: skills, global instruction files and MCP
//! servers. Every function takes an explicit `home` (plus [`Paths`] for
//! state), so tests run on temp dirs and production passes the real `$HOME`.
//!
//! Behaviour mirrors `~/.general/scripts/sync-skills`: harness skill dirs get
//! one symlink per shared skill, while agy dirs get real copies (its file tool
//! refuses symlinks pointing outside its own dirs) carrying a
//! `.sync-skills-source` marker. Entries this tool owns are tracked in a
//! manifest in the state dir; foreign entries are reported, never deleted.

use std::collections::BTreeSet;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::changes::{Change, backup_dest, describe_diff, plan_write_file};
use crate::paths::Paths;
use crate::registry::Registry;

/// Marker inside a copied skill dir, same name sync-skills uses so both tools
/// agree on which copies are managed.
pub const SKILL_MARKER: &str = ".sync-skills-source";
/// Skills that live per-harness and are never synced (sync-skills rule).
const SKIP_SKILLS: [&str; 2] = ["impeccable", "synced"];
/// Owned-entry manifest in the state dir.
const MANIFEST: &str = "shared-skills.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillMode {
    Link,
    Copy,
}

pub struct SkillTarget {
    pub dir: PathBuf,
    pub mode: SkillMode,
    pub plugin: bool,
}

/// Per-harness skill dirs, mirroring the sync-skills target list
/// (`dir:plugin:mode`). Only existing dirs are managed; missing ones are
/// skipped, never created.
pub fn skill_targets(home: &Path) -> Vec<SkillTarget> {
    let mut out = Vec::new();
    // (relative dir, gets ponytail plugin skills, link or copy)
    let fixed: &[(&str, bool, SkillMode)] = &[
        (".claude/skills", false, SkillMode::Link),
        (".claude-second/skills", false, SkillMode::Link),
        (".claude-third/skills", false, SkillMode::Link),
        (".claude-fourth/skills", false, SkillMode::Link),
        (".codex/skills", false, SkillMode::Link),
        (".codex-t3/second/skills", false, SkillMode::Link),
        (".agents/skills", true, SkillMode::Link),
        (".config/opencode/skills", true, SkillMode::Link),
        (".gemini/skills", true, SkillMode::Copy),
        (".gemini/config/skills", true, SkillMode::Copy),
    ];
    for (rel, plugin, mode) in fixed {
        out.push(SkillTarget {
            dir: home.join(rel),
            mode: *mode,
            plugin: *plugin,
        });
    }
    // One T3 antigravity profile dir per hash dir, like sync-skills.
    if let Ok(rd) = fs::read_dir(home.join(".t3/userdata/providers/antigravity")) {
        let mut subs: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path().join("config/skills"))
            .collect();
        subs.sort();
        for dir in subs {
            out.push(SkillTarget {
                dir,
                mode: SkillMode::Copy,
                plugin: true,
            });
        }
    }
    out
}

/// Skills source dirs: `[shared] skills_source` in instances.toml, a single
/// dir or a list (default `~/.general/skills` then `~/.agents/skills`, the
/// sync-skills sources in its order; later entries win on a name clash).
pub fn skills_source(paths: &Paths, home: &Path) -> Vec<PathBuf> {
    // Plugin-shipped skills come last so they win over stale hand copies.
    let mut dirs = configured_sources(paths, home);
    dirs.push(crate::plugins::plugin_skills_dir(paths));
    dirs
}

/// The configured (non-plugin) skill sources, in sync order.
pub fn configured_sources(paths: &Paths, home: &Path) -> Vec<PathBuf> {
    if let Ok(reg) = Registry::load(paths)
        && let Some(table) = reg.extra.get("shared").and_then(|v| v.as_table())
        && let Some(value) = table.get("skills_source")
    {
        let mut dirs = Vec::new();
        if let Some(dir) = value.as_str() {
            dirs.push(expand_home(dir, home));
        } else if let Some(list) = value.as_array() {
            dirs.extend(
                list.iter()
                    .filter_map(|v| v.as_str())
                    .map(|dir| expand_home(dir, home)),
            );
        }
        if !dirs.is_empty() {
            return dirs;
        }
    }
    vec![home.join(".general/skills"), home.join(".agents/skills")]
}

pub(crate) fn expand_home(value: &str, home: &Path) -> PathBuf {
    if value == "~" {
        home.to_path_buf()
    } else if let Some(rest) = value.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(value)
    }
}

struct SkillSrc {
    name: String,
    path: PathBuf,
}

/// Source skills merged over every source dir: entries with a `SKILL.md`
/// that are not symlinks and not per-harness names. On a name clash the
/// later source wins (sync-skills order). Plugin targets additionally get
/// the ponytail skills (harnesses that load the plugin themselves must not).
fn skill_sources(sources: &[PathBuf], home: &Path, plugin: bool) -> Vec<SkillSrc> {
    let mut out = Vec::new();
    for source in sources {
        collect_skills(source, &mut out);
    }
    if plugin {
        collect_skills(
            &home.join(".claude/plugins/marketplaces/ponytail/skills"),
            &mut out,
        );
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    // Stable sort keeps the later source last in each name group; keep that one.
    let mut seen = BTreeSet::new();
    let mut deduped: Vec<SkillSrc> = out
        .into_iter()
        .rev()
        .filter(|s| seen.insert(s.name.clone()))
        .collect();
    deduped.sort_by(|a, b| a.name.cmp(&b.name));
    deduped
}

fn collect_skills(dir: &Path, out: &mut Vec<SkillSrc>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if SKIP_SKILLS.contains(&name.as_str()) {
            continue;
        }
        let path = entry.path();
        if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
            continue;
        }
        if !path.join("SKILL.md").is_file() {
            continue;
        }
        out.push(SkillSrc { name, path });
    }
}

#[derive(Debug, Clone)]
pub enum SkillState {
    Ok,
    /// Target dir does not exist; skipped like sync-skills.
    NoDir,
    Missing,
    /// Symlink points elsewhere (or entry is unreadable).
    WrongLink {
        actual: Option<PathBuf>,
    },
    /// Copy content differs from the source.
    StaleCopy,
    /// Real entry this tool does not own; kept.
    Foreign,
    /// Owned entry whose source is gone; sync would remove it.
    StaleOwned,
}

impl SkillState {
    pub fn describe(&self) -> &'static str {
        match self {
            SkillState::Ok => "ok",
            SkillState::NoDir => "target dir missing, skipped",
            SkillState::Missing => "missing",
            SkillState::WrongLink { .. } => "points elsewhere",
            SkillState::StaleCopy => "copy outdated",
            SkillState::Foreign => "foreign entry, kept",
            SkillState::StaleOwned => "source gone, owned (sync removes)",
        }
    }
}

pub struct SkillStatus {
    pub target: PathBuf,
    pub skill: String,
    pub state: SkillState,
}

/// Read-only status across every target dir. Never writes.
pub fn skills_status(home: &Path, paths: &Paths) -> Vec<SkillStatus> {
    let source_dirs = skills_source(paths, home);
    let owned = read_manifest(&manifest_path(paths));
    let mut out = Vec::new();
    for target in skill_targets(home) {
        if !target.dir.is_dir() {
            out.push(SkillStatus {
                target: target.dir,
                skill: String::new(),
                state: SkillState::NoDir,
            });
            continue;
        }
        let sources = skill_sources(&source_dirs, home, target.plugin);
        let names: BTreeSet<&str> = sources.iter().map(|s| s.name.as_str()).collect();
        for src in &sources {
            let dst = target.dir.join(&src.name);
            // The source dir itself is never linked into itself.
            if dst == src.path {
                continue;
            }
            out.push(SkillStatus {
                target: target.dir.clone(),
                skill: src.name.clone(),
                state: link_state(&dst, &src.path, target.mode),
            });
        }
        // Stale owned entries and foreign entries.
        if let Ok(rd) = fs::read_dir(&target.dir) {
            for entry in rd.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if names.contains(name.as_str()) || name == ".DS_Store" {
                    continue;
                }
                let dst = entry.path();
                if source_dirs.contains(&dst) {
                    continue;
                }
                let state = if owned.contains(&dst) || managed_copy_of_gone_source(&dst) {
                    SkillState::StaleOwned
                } else {
                    SkillState::Foreign
                };
                out.push(SkillStatus {
                    target: target.dir.clone(),
                    skill: name,
                    state,
                });
            }
        }
    }
    out
}

fn link_state(dst: &Path, src: &Path, mode: SkillMode) -> SkillState {
    match fs::symlink_metadata(dst) {
        Err(e) if e.kind() == ErrorKind::NotFound => SkillState::Missing,
        Err(_) => SkillState::WrongLink { actual: None },
        Ok(m) if m.file_type().is_symlink() => {
            if mode == SkillMode::Copy {
                // A link where a copy belongs is replaced on sync.
                SkillState::StaleCopy
            } else if fs::read_link(dst).ok().as_deref() == Some(src) {
                SkillState::Ok
            } else {
                SkillState::WrongLink {
                    actual: fs::read_link(dst).ok(),
                }
            }
        }
        Ok(_) => {
            if mode == SkillMode::Copy {
                if copy_is_current(src, dst) {
                    SkillState::Ok
                } else if fs::read(dst.join(SKILL_MARKER)).is_ok() {
                    SkillState::StaleCopy
                } else {
                    SkillState::Foreign
                }
            } else {
                SkillState::Foreign
            }
        }
    }
}

/// A managed copy whose marker points at a source that no longer exists.
fn managed_copy_of_gone_source(dst: &Path) -> bool {
    match fs::read(dst.join(SKILL_MARKER)) {
        Ok(bytes) => {
            let src = PathBuf::from(String::from_utf8_lossy(&bytes).trim());
            !src.as_os_str().is_empty() && !src.join("SKILL.md").is_file()
        }
        Err(_) => false,
    }
}

/// Plan the full sync. Idempotent: a second run over applied state is `[]`
/// apart from no manifest write. Removes only manifest-owned entries (plus
/// managed copies of vanished sources); foreign entries are left alone.
pub fn plan_skills_sync(home: &Path, paths: &Paths) -> anyhow::Result<Vec<Change>> {
    let source_dirs = skills_source(paths, home);
    let manifest = manifest_path(paths);
    let owned = read_manifest(&manifest);
    let mut owned: BTreeSet<PathBuf> = owned.into_iter().collect();
    let mut changes = Vec::new();
    let mut seen = BTreeSet::new();
    for target in skill_targets(home) {
        if !target.dir.is_dir() {
            continue;
        }
        // Profile skill dirs are often links to one shared dir; planning it twice
        // would queue the same removal again and fail on the missing entry.
        if !seen.insert(fs::canonicalize(&target.dir).unwrap_or_else(|_| target.dir.clone())) {
            owned.retain(|p| !p.starts_with(&target.dir));
            continue;
        }
        let sources = skill_sources(&source_dirs, home, target.plugin);
        let names: BTreeSet<&str> = sources.iter().map(|s| s.name.as_str()).collect();
        for src in &sources {
            let dst = target.dir.join(&src.name);
            if dst == src.path {
                continue;
            }
            match target.mode {
                SkillMode::Link => match fs::symlink_metadata(&dst) {
                    Err(e) if e.kind() == ErrorKind::NotFound => {
                        changes.push(Change::Symlink {
                            link: dst.clone(),
                            target: src.path.clone(),
                        });
                        owned.insert(dst);
                    }
                    Err(e) => return Err(e.into()),
                    Ok(m) if m.file_type().is_symlink() => {
                        if fs::read_link(&dst).ok().as_deref() != Some(src.path.as_path()) {
                            changes.push(Change::Symlink {
                                link: dst.clone(),
                                target: src.path.clone(),
                            });
                            owned.insert(dst);
                        } else {
                            owned.insert(dst);
                        }
                    }
                    // A real entry where a link belongs is foreign: keep it.
                    Ok(_) => {}
                },
                SkillMode::Copy => {
                    if copy_is_current(&src.path, &dst) {
                        owned.insert(dst);
                        continue;
                    }
                    let managed = fs::read_link(&dst).is_ok()
                        || fs::read(dst.join(SKILL_MARKER)).is_ok()
                        || fs::symlink_metadata(&dst).is_err()
                        || owned.contains(&dst);
                    if !managed {
                        continue; // foreign real dir: report, never touch
                    }
                    if fs::symlink_metadata(&dst).is_ok() {
                        changes.push(Change::BackupRemove {
                            path: dst.clone(),
                            backup: backup_dest(&paths.state_dir, &src.name),
                        });
                    }
                    changes.extend(plan_copy(&src.path, &dst)?);
                    owned.insert(dst);
                }
            }
        }
        if let Ok(rd) = fs::read_dir(&target.dir) {
            for entry in rd.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if names.contains(name.as_str()) || name == ".DS_Store" {
                    continue;
                }
                let dst = entry.path();
                if source_dirs.contains(&dst) {
                    continue;
                }
                if owned.contains(&dst) || managed_copy_of_gone_source(&dst) {
                    changes.push(Change::BackupRemove {
                        path: dst.clone(),
                        backup: backup_dest(&paths.state_dir, &name),
                    });
                    owned.remove(&dst);
                }
                // Foreign entries are never removed.
            }
        }
    }
    let new_manifest = render_manifest(&owned);
    let old_manifest = fs::read(&manifest).ok();
    changes.extend(plan_write_file(
        manifest,
        old_manifest.as_deref(),
        &new_manifest,
    ));
    Ok(changes)
}

/// True when `dst` is a copy of `src`: marker names this source and every
/// file matches (the content-hash skip).
fn copy_is_current(src: &Path, dst: &Path) -> bool {
    if !dst.is_dir() || dst.is_symlink() {
        return false;
    }
    let marker = fs::read(dst.join(SKILL_MARKER));
    if marker.map(|b| b != marker_bytes(src)).unwrap_or(true) {
        return false;
    }
    let mut src_files = Vec::new();
    if !collect_files(src, src, &mut src_files) {
        return false;
    }
    for (rel, bytes) in &src_files {
        if fs::read(dst.join(rel)).ok().as_deref() != Some(bytes.as_slice()) {
            return false;
        }
    }
    // No extra files in the copy (sync-skills replaces the whole dir).
    let mut dst_files = Vec::new();
    if !collect_files(dst, dst, &mut dst_files) {
        return false;
    }
    let src_names: BTreeSet<&Path> = src_files.iter().map(|(r, _)| r.as_path()).collect();
    dst_files
        .iter()
        .all(|(r, _)| r == Path::new(SKILL_MARKER) || src_names.contains(r.as_path()))
}

fn marker_bytes(src: &Path) -> Vec<u8> {
    format!("{}\n", src.display()).into_bytes()
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) -> bool {
    let Ok(rd) = fs::read_dir(dir) else {
        return false;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else {
            return false;
        };
        if ft.is_symlink() {
            return false;
        }
        if ft.is_dir() {
            if !collect_files(root, &path, out) {
                return false;
            }
        } else if let Ok(bytes) = fs::read(&path) {
            out.push((path.strip_prefix(root).unwrap().to_path_buf(), bytes));
        } else {
            return false;
        }
    }
    true
}

/// `Mkdir` + one `WriteFile` per file plus the marker. Copies never carry a
/// per-file backup (they are generated); the replaced dir is backed up first.
fn plan_copy(src: &Path, dst: &Path) -> anyhow::Result<Vec<Change>> {
    let mut files = Vec::new();
    if !collect_files(src, src, &mut files) {
        anyhow::bail!("cannot read skill source {}", src.display());
    }
    let mut changes = vec![Change::Mkdir {
        path: dst.to_path_buf(),
    }];
    files.sort_by(|a, b| a.0.cmp(&b.0));
    // The old dir is removed before these writes, so every file is new:
    // diffing against it would skip unchanged files and leave them missing.
    for (rel, bytes) in files {
        changes.extend(plan_write_file(dst.join(rel), None, &bytes));
    }
    changes.extend(plan_write_file(
        dst.join(SKILL_MARKER),
        None,
        &marker_bytes(src),
    ));
    Ok(changes)
}

fn manifest_path(paths: &Paths) -> PathBuf {
    paths.state_dir.join(MANIFEST)
}

fn read_manifest(path: &Path) -> Vec<PathBuf> {
    fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v.get("owned")?.as_array().cloned())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(PathBuf::from))
                .collect()
        })
        .unwrap_or_default()
}

fn render_manifest(owned: &BTreeSet<PathBuf>) -> Vec<u8> {
    let list: Vec<&str> = owned.iter().filter_map(|p| p.to_str()).collect();
    format!(
        "{}\n",
        serde_json::to_string_pretty(&serde_json::json!({"owned": list})).unwrap_or_default()
    )
    .into_bytes()
}

// ---- instruction files ---------------------------------------------------

/// The shared instruction source every harness file links to.
pub fn instruction_source(home: &Path) -> PathBuf {
    home.join("AGENTS.md")
}

/// Global instruction files per harness and the shared source they link to:
/// Claude `CLAUDE.md`, Codex/OpenCode `AGENTS.md`, agy `GEMINI.md`.
pub fn instruction_targets(home: &Path) -> Vec<(PathBuf, PathBuf)> {
    let src = instruction_source(home);
    vec![
        (home.join(".claude/CLAUDE.md"), src.clone()),
        (home.join(".codex/AGENTS.md"), src.clone()),
        (home.join(".config/opencode/AGENTS.md"), src.clone()),
        (home.join(".gemini/GEMINI.md"), src),
    ]
}

#[derive(Debug, Clone)]
pub enum InstrState {
    Linked,
    Missing,
    WrongLink { actual: Option<PathBuf> },
    RealSame,
    RealDiff,
}

impl InstrState {
    pub fn describe(&self) -> String {
        match self {
            InstrState::Linked => "symlink to shared source".to_string(),
            InstrState::Missing => "missing".to_string(),
            InstrState::WrongLink { actual } => match actual {
                Some(a) => format!("symlink to {}, expected shared source", a.display()),
                None => "unreadable symlink".to_string(),
            },
            InstrState::RealSame => "real file, same content (link would back it up)".to_string(),
            InstrState::RealDiff => "real file, different content (needs --replace)".to_string(),
        }
    }
}

pub struct InstrStatus {
    pub path: PathBuf,
    pub state: InstrState,
}

/// Read-only status. Never writes.
pub fn instructions_status(home: &Path) -> Vec<InstrStatus> {
    instruction_targets(home)
        .into_iter()
        .map(|(path, src)| {
            let state = match fs::symlink_metadata(&path) {
                Err(e) if e.kind() == ErrorKind::NotFound => InstrState::Missing,
                Err(_) => InstrState::WrongLink { actual: None },
                Ok(m) if m.file_type().is_symlink() => {
                    if fs::read_link(&path).ok().as_deref() == Some(src.as_path()) {
                        InstrState::Linked
                    } else {
                        InstrState::WrongLink {
                            actual: fs::read_link(&path).ok(),
                        }
                    }
                }
                Ok(_) => {
                    let same = fs::read(&path)
                        .ok()
                        .zip(fs::read(&src).ok())
                        .is_some_and(|(a, b)| a == b);
                    if same {
                        InstrState::RealSame
                    } else {
                        InstrState::RealDiff
                    }
                }
            };
            InstrStatus { path, state }
        })
        .collect()
}

/// Plan linking every target at the shared source. A real file is replaced
/// only after a backup, and only when its contents match the source or
/// `replace` is given. Marker-block edits stay in [`crate::plugins`]; this
/// only manages the links.
pub fn plan_instructions_link(
    home: &Path,
    state_dir: &Path,
    replace: bool,
) -> anyhow::Result<Vec<Change>> {
    let src = instruction_source(home);
    if !src.is_file() {
        anyhow::bail!("shared source {} missing, cannot link", src.display());
    }
    let contents = fs::read(&src)?;
    let mut changes = Vec::new();
    for (path, _) in instruction_targets(home) {
        changes.extend(plan_instruction_link(
            home, state_dir, &path, &contents, replace,
        )?);
    }
    Ok(changes)
}

fn plan_instruction_link(
    home: &Path,
    state_dir: &Path,
    path: &Path,
    shared_contents: &[u8],
    replace: bool,
) -> anyhow::Result<Vec<Change>> {
    if !instruction_targets(home)
        .iter()
        .any(|(target, _)| target == path)
    {
        anyhow::bail!("{} is not a global instruction target", path.display());
    }
    let src = instruction_source(home);
    let mut changes = Vec::new();
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
        Ok(metadata) => {
            if metadata.file_type().is_symlink() && fs::read_link(path)? == src {
                return Ok(changes);
            }
            if !metadata.file_type().is_symlink() && !replace && fs::read(path)? != shared_contents
            {
                anyhow::bail!(
                    "{} is a real file with different content; remove it by hand or retry with --replace",
                    path.display()
                );
            }
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            changes.push(Change::BackupRemove {
                path: path.to_path_buf(),
                backup: backup_dest(state_dir, &name),
            });
        }
    }
    changes.push(Change::Symlink {
        link: path.to_path_buf(),
        target: src,
    });
    Ok(changes)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstructionChoice {
    UseShared,
    UseLocal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InstructionFile {
    link: Option<PathBuf>,
    contents: Option<Vec<u8>>,
}

fn read_instruction_file(path: &Path) -> anyhow::Result<InstructionFile> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Ok(InstructionFile {
                link: None,
                contents: None,
            });
        }
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file() && !metadata.file_type().is_symlink() {
        anyhow::bail!("{} is not an instruction file", path.display());
    }
    let link = if metadata.file_type().is_symlink() {
        Some(fs::read_link(path)?)
    } else {
        None
    };
    let contents = match fs::read(path) {
        Ok(contents) => Some(contents),
        Err(error) if error.kind() == ErrorKind::NotFound && link.is_some() => None,
        Err(error) => return Err(error.into()),
    };
    Ok(InstructionFile { link, contents })
}

#[derive(Debug, Clone)]
pub struct InstructionResolution {
    pub source: PathBuf,
    pub target: PathBuf,
    pub choice: InstructionChoice,
    pub diff: Vec<String>,
    pub changes: Vec<Change>,
    source_before: InstructionFile,
    target_before: InstructionFile,
}

/// Plan one selected file. Previewing never changes either file or creates a backup.
pub fn plan_instruction_resolution(
    home: &Path,
    state_dir: &Path,
    target: &Path,
    choice: InstructionChoice,
) -> anyhow::Result<InstructionResolution> {
    if !instruction_targets(home)
        .iter()
        .any(|(path, _)| path == target)
    {
        anyhow::bail!("{} is not a global instruction target", target.display());
    }
    let source = instruction_source(home);
    let source_before = read_instruction_file(&source)?;
    let target_before = read_instruction_file(target)?;
    let (before, after) = match choice {
        InstructionChoice::UseShared => (
            target_before.contents.as_deref().unwrap_or_default(),
            source_before.contents.as_deref().ok_or_else(|| {
                anyhow::anyhow!("shared instructions {} missing", source.display())
            })?,
        ),
        InstructionChoice::UseLocal => (
            source_before.contents.as_deref().unwrap_or_default(),
            target_before
                .contents
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("instructions {} missing", target.display()))?,
        ),
    };
    let diff = instruction_diff(before, after)?;
    let mut changes = Vec::new();
    if choice == InstructionChoice::UseLocal && source_before.contents.as_deref() != Some(after) {
        let mut backup = None;
        if source_before.link.is_some() {
            changes.push(Change::BackupRemove {
                path: source.clone(),
                backup: backup_dest(state_dir, "AGENTS.md"),
            });
        } else if source_before.contents.is_some() {
            backup = Some(backup_dest(state_dir, "AGENTS.md"));
        }
        changes.push(Change::WriteFile {
            path: source.clone(),
            contents: after.to_vec(),
            old: source_before.contents.clone(),
            backup,
        });
    }
    changes.extend(plan_instruction_link(home, state_dir, target, after, true)?);
    Ok(InstructionResolution {
        source,
        target: target.to_path_buf(),
        choice,
        diff,
        changes,
        source_before,
        target_before,
    })
}

/// Reject edits made after preview instead of silently replacing newly edited rules.
pub fn apply_instruction_resolution(
    home: &Path,
    resolution: &InstructionResolution,
) -> anyhow::Result<()> {
    if instruction_source(home) != resolution.source
        || !instruction_targets(home)
            .iter()
            .any(|(path, _)| path == &resolution.target)
    {
        anyhow::bail!("instruction preview belongs to another home");
    }
    if read_instruction_file(&resolution.source)? != resolution.source_before
        || read_instruction_file(&resolution.target)? != resolution.target_before
    {
        anyhow::bail!("instruction files changed after preview; compare them again");
    }
    crate::apply_changes(&resolution.changes)
}

// One ordered hunk preserves every change, including repeated or reordered lines.
fn instruction_diff(before: &[u8], after: &[u8]) -> anyhow::Result<Vec<String>> {
    let old: Vec<_> = std::str::from_utf8(before)?.split_inclusive('\n').collect();
    let new: Vec<_> = std::str::from_utf8(after)?.split_inclusive('\n').collect();
    let prefix = old
        .iter()
        .zip(&new)
        .take_while(|(left, right)| left == right)
        .count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(left, right)| left == right)
        .count();
    if before == after {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let mut append = |marker: char, line: &str| {
        out.push(format!(
            "{marker}{}",
            line.strip_suffix('\n').unwrap_or(line)
        ));
        if !line.ends_with('\n') {
            out.push("\\ No newline at end of file".into());
        }
    };
    for line in &old[prefix.saturating_sub(3)..prefix] {
        append(' ', line);
    }
    for line in &old[prefix..old.len() - suffix] {
        append('-', line);
    }
    for line in &new[prefix..new.len() - suffix] {
        append('+', line);
    }
    for line in &old[old.len() - suffix..old.len() - suffix + suffix.min(3)] {
        append(' ', line);
    }
    Ok(out)
}

// ---- MCP -----------------------------------------------------------------

/// Canonical MCP list: the primary Claude's `mcpServers` (key names only are
/// never enough here; values are needed for sync but redacted in all output).
pub fn canonical_mcp_servers(home: &Path) -> anyhow::Result<Map<String, Value>> {
    let path = home.join(".claude.json");
    let bytes = fs::read(&path)
        .map_err(|_| anyhow::anyhow!("primary {} missing, cannot sync MCP", path.display()))?;
    let doc: Value = serde_json::from_slice(&bytes)
        .map_err(|e| anyhow::anyhow!("cannot parse {}: {e}", path.display()))?;
    match doc.get("mcpServers") {
        None => anyhow::bail!("primary {} has no mcpServers", path.display()),
        Some(Value::Object(servers)) => Ok(servers.clone()),
        Some(_) => anyhow::bail!("primary {} mcpServers is not an object", path.display()),
    }
}

/// Codex MCP config (`[mcp_servers.*]`), or `$AGENTDECK_CODEX_CONFIG` in tests.
pub fn codex_config(home: &Path) -> PathBuf {
    std::env::var_os("AGENTDECK_CODEX_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".codex/config.toml"))
}

pub struct McpTargetStatus {
    pub name: String,
    pub detail: String,
}

/// Read-only MCP overview: redacted canonical list plus per-target state.
/// OpenCode and agy report unsupported: R3 names their config locations but
/// gives no entry-level mapping from Claude's schema, so syncing there would
/// be guessing.
pub fn mcp_status(
    home: &Path,
    paths: &Paths,
    secondaries: &[PathBuf],
) -> (Value, Vec<McpTargetStatus>) {
    let canonical = canonical_mcp_servers(home).unwrap_or_default();
    let redacted = crate::claude::redact_mcp_servers(&Value::Object(canonical.clone()));
    let mut targets = Vec::new();
    for secondary in secondaries {
        let path = secondary.join(".claude.json");
        let detail = match fs::read(&path) {
            Err(_) => "secondary .claude.json missing".to_string(),
            Ok(bytes) => {
                let current: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
                let current = current.get("mcpServers").cloned().unwrap_or(Value::Null);
                let want = Value::Object(canonical.clone());
                if current == want {
                    "in sync".to_string()
                } else {
                    "differs".to_string()
                }
            }
        };
        targets.push(McpTargetStatus {
            name: format!("claude:{}", secondary.display()),
            detail,
        });
    }
    let config = codex_config(home);
    targets.push(McpTargetStatus {
        name: format!("codex:{}", config.display()),
        detail: codex_status_detail(&config, &canonical),
    });
    let _ = paths;
    targets.push(McpTargetStatus {
        name: "opencode".to_string(),
        detail:
            "unsupported: no Claude->opencode entry mapping in R3 (mcp section managed by hand)"
                .to_string(),
    });
    targets.push(McpTargetStatus {
        name: "agy".to_string(),
        detail: "unsupported: IDE mcp_config.json vs CLI mcp/ dir mapping open in R3".to_string(),
    });
    (redacted, targets)
}

fn codex_status_detail(config: &Path, canonical: &Map<String, Value>) -> String {
    let Ok(bytes) = fs::read(config) else {
        return "config.toml missing".to_string();
    };
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return "config.toml is not UTF-8".to_string();
    };
    let Ok(doc) = text.parse::<toml_edit::DocumentMut>() else {
        return "config.toml does not parse".to_string();
    };
    let mut missing = Vec::new();
    let mut differ = Vec::new();
    let mut extra = Vec::new();
    let servers = doc
        .get("mcp_servers")
        .and_then(|i| i.as_table_like())
        .map(|t| {
            t.iter()
                .map(|(k, _)| k.to_string())
                .collect::<BTreeSet<String>>()
        })
        .unwrap_or_default();
    for name in canonical.keys() {
        if !servers.contains(name) {
            missing.push(name.clone());
        } else if codex_entry_matches(&doc, name, &canonical[name]) {
            // in sync
        } else {
            differ.push(name.clone());
        }
    }
    for name in &servers {
        if !canonical.contains_key(name) {
            extra.push(name.clone());
        }
    }
    if missing.is_empty() && differ.is_empty() && extra.is_empty() {
        return "in sync".to_string();
    }
    let mut parts = Vec::new();
    if !missing.is_empty() {
        parts.push(format!("missing {}", missing.join(",")));
    }
    if !differ.is_empty() {
        parts.push(format!("differ {}", differ.join(",")));
    }
    if !extra.is_empty() {
        parts.push(format!(
            "extra {} (needs --prune to remove)",
            extra.join(",")
        ));
    }
    parts.join("; ")
}

fn codex_entry_matches(doc: &toml_edit::DocumentMut, name: &str, spec: &Value) -> bool {
    let Some(entry) = doc
        .get("mcp_servers")
        .and_then(|t| t.get(name))
        .and_then(|i| i.as_table_like())
    else {
        return false;
    };
    if let Some(url) = spec.get("url").and_then(|v| v.as_str()) {
        return entry.get("url").and_then(|i| i.as_str()) == Some(url);
    }
    let Some(cmd) = spec.get("command").and_then(|v| v.as_str()) else {
        return false;
    };
    if entry.get("command").and_then(|i| i.as_str()) != Some(cmd) {
        return false;
    }
    let want_args: Vec<&str> = spec
        .get("args")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    let have_args: Vec<&str> = entry
        .get("args")
        .and_then(|i| i.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    if want_args != have_args {
        return false;
    }
    // Env lives in a sibling `[mcp_servers.<name>.env]` table.
    let want_env: Map<String, Value> = spec
        .get("env")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    let env_table = entry.get("env").and_then(|i| i.as_table_like());
    match (want_env.is_empty(), env_table) {
        (true, None) => true,
        (true, Some(_)) => false,
        (false, None) => false,
        (false, Some(t)) => {
            let have: BTreeSet<String> = t.iter().map(|(k, _)| k.to_string()).collect();
            let want: BTreeSet<String> = want_env.keys().cloned().collect();
            if have != want {
                return false;
            }
            want_env.iter().all(|(k, v)| {
                let want = match v {
                    Value::String(s) => s.clone(),
                    _ => v.to_string(),
                };
                t.get(k).and_then(|i| i.as_str()) == Some(want.as_str())
            })
        }
    }
}

/// Non-primary Claude instance homes: the MCP sync targets.
pub fn claude_secondaries(paths: &Paths, home: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let reg = crate::Registry::load(paths)?;
    let primary = home.join(".claude");
    Ok(reg
        .instances
        .iter()
        .filter(|i| i.harness == crate::Harness::Claude)
        .map(|i| crate::sync::expand_tilde(&i.home, home))
        .filter(|dir| *dir != primary)
        .collect())
}

/// One MCP change as redacted text (server values never reach output).
pub fn describe_mcp_change(change: &Change) -> String {
    let Change::WriteFile {
        path,
        contents,
        old,
        backup,
    } = change
    else {
        return change.describe();
    };
    if path.file_name().is_some_and(|n| n == ".claude.json") {
        return crate::claude::describe_change(change);
    }
    if path.file_name().is_some_and(|n| n == "config.toml") {
        let suffix = match backup {
            Some(dest) => format!(" (backup {})", dest.display()),
            None => String::new(),
        };
        return format!(
            "{}{suffix}",
            describe_codex_change(path, old.as_deref(), contents)
        );
    }
    change.describe()
}

/// Planned MCP sync plus server names skipped for an unknown shape.
#[allow(clippy::type_complexity)]
pub fn plan_mcp_sync(
    home: &Path,
    state_dir: &Path,
    secondaries: &[PathBuf],
    prune: bool,
) -> anyhow::Result<(Vec<Change>, Vec<String>)> {
    let primary_json = home.join(".claude.json");
    let canonical = canonical_mcp_servers(home)?;
    let mut changes = Vec::new();
    for secondary in secondaries {
        if let Some(c) = crate::claude::plan_mcp_merge(&primary_json, secondary, state_dir)? {
            changes.push(c);
        }
    }
    let (codex_change, skipped) =
        plan_codex_mcp(&codex_config(home), &canonical, state_dir, prune)?;
    changes.extend(codex_change);
    Ok((changes, skipped))
}

/// Merge canonical servers into Codex `config.toml` with toml_edit so
/// comments and unmanaged keys survive byte-identical. Only `command`/`args`
/// (or `url`) and the `env` table are managed; `startup_timeout_sec`,
/// `env_vars` and approval keys are kept. Removal needs `prune`.
fn plan_codex_mcp(
    config: &Path,
    canonical: &Map<String, Value>,
    state_dir: &Path,
    prune: bool,
) -> anyhow::Result<(Option<Change>, Vec<String>)> {
    let old_bytes = fs::read(config)
        .map_err(|_| anyhow::anyhow!("codex {} missing, not creating it", config.display()))?;
    let old_text = std::str::from_utf8(&old_bytes)
        .map_err(|_| anyhow::anyhow!("{} is not valid UTF-8", config.display()))?;
    let mut doc: toml_edit::DocumentMut = old_text
        .parse()
        .map_err(|e| anyhow::anyhow!("cannot parse {}: {e}", config.display()))?;
    if !doc.contains_key("mcp_servers") {
        doc["mcp_servers"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    if !doc["mcp_servers"].is_table() {
        anyhow::bail!("{} mcp_servers is not a table", config.display());
    }
    let mut skipped = Vec::new();
    let mut names: Vec<&String> = canonical.keys().collect();
    names.sort();
    for name in names {
        let spec = &canonical[name];
        if let Some(url) = spec.get("url").and_then(|v| v.as_str()) {
            let table = server_table(&mut doc, name, config)?;
            table.remove("command");
            table.remove("args");
            table.remove("env");
            table["url"] = toml_edit::value(url);
        } else if let Some(cmd) = spec.get("command").and_then(|v| v.as_str()) {
            let table = server_table(&mut doc, name, config)?;
            table.remove("url");
            table["command"] = toml_edit::value(cmd);
            let mut args = toml_edit::Array::new();
            if let Some(list) = spec.get("args").and_then(|v| v.as_array()) {
                for arg in list.iter().filter_map(|v| v.as_str()) {
                    args.push(arg);
                }
            }
            table["args"] = toml_edit::value(args);
            match spec.get("env").and_then(|v| v.as_object()) {
                None => {
                    table.remove("env");
                }
                Some(env) if env.is_empty() => {
                    table.remove("env");
                }
                Some(env) => {
                    let mut env_table = toml_edit::Table::new();
                    let mut keys: Vec<&String> = env.keys().collect();
                    keys.sort();
                    for key in keys {
                        let value = match &env[key] {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        };
                        env_table[key] = toml_edit::value(value);
                    }
                    table["env"] = toml_edit::Item::Table(env_table);
                }
            }
        } else {
            skipped.push(name.clone());
        }
    }
    if prune {
        let existing: Vec<String> = doc["mcp_servers"]
            .as_table_like()
            .map(|t| t.iter().map(|(k, _)| k.to_string()).collect())
            .unwrap_or_default();
        for name in existing {
            if !canonical.contains_key(&name) {
                doc["mcp_servers"]
                    .as_table_like_mut()
                    .map(|t| t.remove(&name));
            }
        }
    }
    let new_text = doc.to_string();
    if new_text == old_text {
        return Ok((None, skipped));
    }
    let backup = config
        .file_name()
        .map(|n| backup_dest(state_dir, &n.to_string_lossy()));
    Ok((
        Some(Change::WriteFile {
            path: config.to_path_buf(),
            contents: new_text.into_bytes(),
            old: Some(old_bytes),
            backup,
        }),
        skipped,
    ))
}

fn server_table<'d>(
    doc: &'d mut toml_edit::DocumentMut,
    name: &str,
    config: &Path,
) -> anyhow::Result<&'d mut toml_edit::Table> {
    if !doc["mcp_servers"]
        .as_table_like()
        .is_some_and(|t| t.contains_key(name))
    {
        doc["mcp_servers"][name] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    doc["mcp_servers"][name]
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("{} mcp_servers.{name} is not a table", config.display()))
}

/// Describe a Codex config change with `env` values redacted (keys stay).
pub fn describe_codex_change(path: &Path, old: Option<&[u8]>, new: &[u8]) -> String {
    let mask = |bytes: &[u8]| -> String {
        let text = String::from_utf8_lossy(bytes);
        let mut out = Vec::new();
        let mut in_env = false;
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') {
                in_env = trimmed.starts_with("[mcp_servers.") && trimmed.ends_with(".env]");
                out.push(line.to_string());
                continue;
            }
            if in_env && let Some((key, _)) = trimmed.split_once('=') {
                let indent: String = line.chars().take_while(|c| c.is_whitespace()).collect();
                out.push(format!("{indent}{} = \"<redacted>\"", key.trim()));
                continue;
            }
            out.push(line.to_string());
        }
        out.join("\n")
    };
    let old_text = old.map(mask).unwrap_or_default();
    let new_text = mask(new);
    if old.is_some_and(|b| b == new) {
        return format!("write {} (unchanged)", path.display());
    }
    match old {
        None => format!("write {} (new file, {} bytes)", path.display(), new.len()),
        Some(_) => describe_diff(path, old_text.as_bytes(), new_text.as_bytes()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes::apply_changes;
    use std::sync::atomic::{AtomicU64, Ordering};

    static N: AtomicU64 = AtomicU64::new(0);

    struct Guard(PathBuf);
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct Fx {
        _g: Guard,
        home: PathBuf,
        paths: Paths,
    }

    fn fixture(tag: &str) -> Fx {
        let dir = std::env::temp_dir().join(format!(
            "agentdeck-shared-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&dir);
        let home = dir.join("home");
        fs::create_dir_all(&home).unwrap();
        let paths = Paths::new(dir.join("config"), dir.join("state"));
        Fx {
            _g: Guard(dir),
            home,
            paths,
        }
    }

    fn write_skill(dir: &Path, name: &str, body: &str) {
        fs::create_dir_all(dir.join(name)).unwrap();
        fs::write(dir.join(name).join("SKILL.md"), body).unwrap();
    }

    /// Fixture mirroring the sync-skills layout: source skills, link targets,
    /// an agy copy target, a foreign real dir and names sync-skills skips.
    fn skills_fixture() -> Fx {
        let fx = fixture("skills");
        let source = fx.home.join(".general/skills");
        write_skill(&source, "alpha", "# alpha\n");
        write_skill(&source, "beta", "# beta\n");
        fs::create_dir_all(source.join("impeccable")).unwrap();
        fs::write(source.join("impeccable/SKILL.md"), "skip\n").unwrap();
        fs::create_dir_all(source.join("synced")).unwrap();
        fs::write(source.join("synced/SKILL.md"), "skip\n").unwrap();
        std::os::unix::fs::symlink("/elsewhere", source.join("linked")).unwrap();
        fs::create_dir_all(fx.home.join(".claude")).unwrap();
        fs::create_dir_all(fx.home.join(".claude/skills")).unwrap();
        fs::create_dir_all(fx.home.join(".config/opencode/skills")).unwrap();
        fs::create_dir_all(fx.home.join(".gemini/skills")).unwrap();
        // Foreign real dir in a link target: kept, never touched.
        fs::create_dir_all(fx.home.join(".claude/skills/mine")).unwrap();
        fs::write(fx.home.join(".claude/skills/mine/SKILL.md"), "mine\n").unwrap();
        fx
    }

    fn sync(fx: &Fx) {
        let changes = plan_skills_sync(&fx.home, &fx.paths).unwrap();
        apply_changes(&changes).unwrap();
    }

    #[test]
    fn aliased_skill_dirs_are_synced_once() {
        let fx = skills_fixture();
        fs::create_dir_all(fx.home.join(".claude-second")).unwrap();
        std::os::unix::fs::symlink(
            fx.home.join(".claude/skills"),
            fx.home.join(".claude-second/skills"),
        )
        .unwrap();
        sync(&fx);
        // Removing a source must not queue the same removal once per alias.
        fs::remove_dir_all(fx.home.join(".general/skills/beta")).unwrap();
        sync(&fx);
        assert!(fs::symlink_metadata(fx.home.join(".claude/skills/beta")).is_err());
        assert!(fs::read_link(fx.home.join(".claude/skills/alpha")).is_ok());
        assert!(
            plan_skills_sync(&fx.home, &fx.paths)
                .unwrap()
                .iter()
                .all(|c| !matches!(c, Change::BackupRemove { .. }))
        );
    }

    #[test]
    fn skills_sync_links_and_copies_then_idempotent() {
        let fx = skills_fixture();
        let changes = plan_skills_sync(&fx.home, &fx.paths).unwrap();
        assert!(!changes.is_empty());
        apply_changes(&changes).unwrap();
        for skill in ["alpha", "beta"] {
            assert_eq!(
                fs::read_link(fx.home.join(".claude/skills").join(skill)).unwrap(),
                fx.home.join(".general/skills").join(skill)
            );
            assert_eq!(
                fs::read(fx.home.join(".gemini/skills").join(skill).join("SKILL.md")).unwrap(),
                format!("# {skill}\n").into_bytes()
            );
        }
        for skipped in ["impeccable", "synced", "linked"] {
            assert!(
                fs::symlink_metadata(fx.home.join(".claude/skills").join(skipped)).is_err(),
                "{skipped} must not sync"
            );
        }
        // Foreign dir kept; manifest tracks what we own.
        assert!(fs::read(fx.home.join(".claude/skills/mine/SKILL.md")).is_ok());
        assert!(manifest_path(&fx.paths).is_file());
        // Second run plans nothing (manifest write included).
        assert!(plan_skills_sync(&fx.home, &fx.paths).unwrap().is_empty());
        assert!(
            skills_status(&fx.home, &fx.paths).iter().all(|s| matches!(
                s.state,
                SkillState::Ok | SkillState::Foreign | SkillState::NoDir
            )),
            "{:?}",
            skills_status(&fx.home, &fx.paths)
                .iter()
                .map(|s| (s.skill.clone(), s.state.describe()))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn skills_source_single_string_and_list() {
        let fx = fixture("source-cfg");
        // Shipped plugin skills always come last.
        let plugin_dir = crate::plugins::plugin_skills_dir(&fx.paths);
        fs::create_dir_all(&fx.paths.config_dir).unwrap();
        // Single string still loads.
        fs::write(
            fx.paths.registry_file(),
            "version = 1\n[shared]\nskills_source = \"~/mys/base\"\n",
        )
        .unwrap();
        assert_eq!(
            skills_source(&fx.paths, &fx.home),
            vec![fx.home.join("mys/base"), plugin_dir.clone()]
        );
        // List form loads in order; empty list falls back to the default.
        fs::write(
            fx.paths.registry_file(),
            "version = 1\n[shared]\nskills_source = [\"~/a\", \"~/b\"]\n",
        )
        .unwrap();
        assert_eq!(
            skills_source(&fx.paths, &fx.home),
            vec![fx.home.join("a"), fx.home.join("b"), plugin_dir.clone()]
        );
        fs::write(
            fx.paths.registry_file(),
            "version = 1\n[shared]\nskills_source = []\n",
        )
        .unwrap();
        assert_eq!(
            skills_source(&fx.paths, &fx.home),
            vec![
                fx.home.join(".general/skills"),
                fx.home.join(".agents/skills"),
                plugin_dir
            ]
        );
    }

    #[test]
    fn skills_parity_with_sync_skills() {
        let script = PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
            .join(".general/scripts/sync-skills");
        if !script.is_file() {
            return; // owner's helper absent (CI): fixture assertions above still hold
        }
        let fx = skills_fixture();
        // Second source, like sync-skills (~/.agents/skills after
        // ~/.general/skills). Disjoint names: sync-skills --check reports
        // drift for either link when a name exists in both sources, so a
        // clash cannot be --check-clean (precedence is pinned below).
        write_skill(&fx.home.join(".agents/skills"), "gamma", "# gamma\n");
        sync(&fx);
        let status = std::process::Command::new(&script)
            .arg("--check")
            .env("HOME", &fx.home)
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&status.stdout),
            String::from_utf8_lossy(&status.stderr)
        );
        // sync-skills --check exits 1 on drift; our sync must leave none.
        // Foreign entries are reported by both tools, so compare drift lines.
        let drift: Vec<&str> = text
            .lines()
            .filter(|l| {
                l.starts_with("link ") || l.starts_with("copy ") || l.starts_with("relink ")
            })
            .collect();
        assert!(
            status.status.success() || drift.is_empty(),
            "drift:\n{text}"
        );
    }

    #[test]
    fn skills_later_source_wins_on_name_clash() {
        // Same name in both sources: the later one (~/.agents/skills, the
        // sync-skills order) wins, matching a sync-skills run's steady state.
        let fx = skills_fixture();
        write_skill(
            &fx.home.join(".agents/skills"),
            "beta",
            "# beta from agents\n",
        );
        sync(&fx);
        assert_eq!(
            fs::read_link(fx.home.join(".claude/skills/beta")).unwrap(),
            fx.home.join(".agents/skills/beta")
        );
        assert_eq!(
            fs::read(fx.home.join(".gemini/skills/beta/SKILL.md")).unwrap(),
            b"# beta from agents\n"
        );
    }

    #[test]
    fn stale_copy_refreshes_by_content_and_foreign_copy_kept() {
        let fx = skills_fixture();
        sync(&fx);
        // Outdated copy refreshes.
        fs::write(fx.home.join(".gemini/skills/alpha/SKILL.md"), "old\n").unwrap();
        // Foreign real dir in a copy target is kept.
        fs::create_dir_all(fx.home.join(".gemini/skills/mine")).unwrap();
        fs::write(fx.home.join(".gemini/skills/mine/SKILL.md"), "mine\n").unwrap();
        let drift: Vec<String> = skills_status(&fx.home, &fx.paths)
            .into_iter()
            .filter_map(|s| match s.state {
                SkillState::Ok | SkillState::NoDir => None,
                other => Some(format!(
                    "{}/{}: {}",
                    s.target.display(),
                    s.skill,
                    other.describe()
                )),
            })
            .collect();
        assert!(
            drift
                .iter()
                .any(|d| d.contains("alpha") && d.contains("outdated")),
            "{drift:?}"
        );
        assert!(
            drift
                .iter()
                .any(|d| d.contains("mine") && d.contains("foreign")),
            "{drift:?}"
        );
        sync(&fx);
        assert_eq!(
            fs::read(fx.home.join(".gemini/skills/alpha/SKILL.md")).unwrap(),
            b"# alpha\n"
        );
        assert!(fs::read(fx.home.join(".gemini/skills/mine/SKILL.md")).is_ok());
        // A stray file makes the copy stale while SKILL.md is unchanged; the
        // refresh must still rewrite SKILL.md into the replaced dir.
        fs::write(fx.home.join(".gemini/skills/alpha/stray.md"), "x\n").unwrap();
        sync(&fx);
        assert_eq!(
            fs::read(fx.home.join(".gemini/skills/alpha/SKILL.md")).unwrap(),
            b"# alpha\n"
        );
        assert!(!fx.home.join(".gemini/skills/alpha/stray.md").exists());
        // Unchanged copies are skipped (marker mtime untouched).
        let before = fs::metadata(fx.home.join(".gemini/skills/beta/SKILL.md"))
            .unwrap()
            .modified()
            .unwrap();
        sync(&fx);
        assert_eq!(
            fs::metadata(fx.home.join(".gemini/skills/beta/SKILL.md"))
                .unwrap()
                .modified()
                .unwrap(),
            before
        );
    }

    #[test]
    fn owned_stale_entries_removed_foreign_kept() {
        let fx = skills_fixture();
        sync(&fx);
        // Drop a source skill: our link and copy become owned-stale.
        fs::remove_dir_all(fx.home.join(".general/skills/beta")).unwrap();
        let states: Vec<String> = skills_status(&fx.home, &fx.paths)
            .into_iter()
            .filter(|s| s.skill == "beta")
            .map(|s| s.state.describe().to_string())
            .collect();
        assert!(states.iter().any(|s| s.contains("owned")), "{states:?}");
        let changes = plan_skills_sync(&fx.home, &fx.paths).unwrap();
        assert!(
            changes.iter().any(|c| matches!(
                c, Change::BackupRemove { path, .. } if path.ends_with("skills/beta")
            )),
            "{changes:?}"
        );
        apply_changes(&changes).unwrap();
        assert!(fs::symlink_metadata(fx.home.join(".claude/skills/beta")).is_err());
        assert!(fs::symlink_metadata(fx.home.join(".gemini/skills/beta")).is_err());
        // Backed up outside the homes.
        assert!(fs::read_dir(fx.paths.backup_dir()).unwrap().count() >= 1);
        // Foreign dir still there.
        assert!(fs::read(fx.home.join(".claude/skills/mine/SKILL.md")).is_ok());
        assert!(plan_skills_sync(&fx.home, &fx.paths).unwrap().is_empty());
    }

    #[test]
    fn instructions_link_with_backup_and_replace() {
        let fx = fixture("instr");
        fs::write(fx.home.join("AGENTS.md"), "shared\n").unwrap();
        fs::create_dir_all(fx.home.join(".claude")).unwrap();
        // Real file, same content: links after backup.
        fs::write(fx.home.join(".claude/CLAUDE.md"), "shared\n").unwrap();
        // Real file, different content: refuses without --replace.
        fs::create_dir_all(fx.home.join(".codex")).unwrap();
        fs::write(fx.home.join(".codex/AGENTS.md"), "mine\n").unwrap();
        assert!(plan_instructions_link(&fx.home, &fx.paths.state_dir, false).is_err());
        let changes = plan_instructions_link(&fx.home, &fx.paths.state_dir, true).unwrap();
        assert!(
            changes
                .iter()
                .any(|c| matches!(c, Change::BackupRemove { .. })),
            "{changes:?}"
        );
        apply_changes(&changes).unwrap();
        for target in instruction_targets(&fx.home).iter().map(|(p, _)| p) {
            assert_eq!(fs::read_link(target).unwrap(), instruction_source(&fx.home));
        }
        let backups: Vec<PathBuf> = fs::read_dir(fx.paths.backup_dir())
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(backups.len(), 2);
        assert!(
            plan_instructions_link(&fx.home, &fx.paths.state_dir, false)
                .unwrap()
                .is_empty()
        );
        assert!(
            instructions_status(&fx.home)
                .iter()
                .all(|s| matches!(s.state, InstrState::Linked))
        );
    }

    fn instruction_resolution_fixture() -> (Fx, PathBuf) {
        let fx = fixture("instruction-resolution");
        fs::write(instruction_source(&fx.home), "shared rules\n").unwrap();
        let target = fx.home.join(".claude/CLAUDE.md");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, "local rules\n").unwrap();
        (fx, target)
    }

    #[test]
    fn instruction_resolution_previews_and_replaces_only_the_selected_file() {
        let (fx, target) = instruction_resolution_fixture();
        let other = fx.home.join(".codex/AGENTS.md");
        fs::create_dir_all(other.parent().unwrap()).unwrap();
        fs::write(&other, "other rules\n").unwrap();
        let plan = plan_instruction_resolution(
            &fx.home,
            &fx.paths.state_dir,
            &target,
            InstructionChoice::UseShared,
        )
        .unwrap();
        assert!(plan.diff.iter().any(|line| line == "-local rules"));
        assert!(plan.diff.iter().any(|line| line == "+shared rules"));
        assert_eq!(fs::read_to_string(&target).unwrap(), "local rules\n");
        assert!(!fx.paths.backup_dir().exists());
        apply_instruction_resolution(&fx.home, &plan).unwrap();
        assert_eq!(
            fs::read_link(&target).unwrap(),
            instruction_source(&fx.home)
        );
        assert_eq!(fs::read_to_string(&other).unwrap(), "other rules\n");
        let backup = plan
            .changes
            .iter()
            .find_map(|change| match change {
                Change::BackupRemove { path, backup } if path == &target => Some(backup),
                _ => None,
            })
            .unwrap();
        assert_eq!(fs::read_to_string(backup).unwrap(), "local rules\n");
    }

    #[test]
    fn instruction_import_updates_shared_rules_and_preserves_both_backups() {
        let (fx, target) = instruction_resolution_fixture();
        let linked = fx.home.join(".codex/AGENTS.md");
        fs::create_dir_all(linked.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(instruction_source(&fx.home), &linked).unwrap();
        let plan = plan_instruction_resolution(
            &fx.home,
            &fx.paths.state_dir,
            &target,
            InstructionChoice::UseLocal,
        )
        .unwrap();
        apply_instruction_resolution(&fx.home, &plan).unwrap();
        assert_eq!(
            fs::read_to_string(instruction_source(&fx.home)).unwrap(),
            "local rules\n"
        );
        assert_eq!(fs::read_to_string(&linked).unwrap(), "local rules\n");
        assert_eq!(
            fs::read_link(&target).unwrap(),
            instruction_source(&fx.home)
        );
        let backups: Vec<_> = fs::read_dir(fx.paths.backup_dir())
            .unwrap()
            .map(|entry| fs::read_to_string(entry.unwrap().path()).unwrap())
            .collect();
        assert_eq!(backups.len(), 2);
        assert!(backups.contains(&"shared rules\n".to_string()));
        assert!(backups.contains(&"local rules\n".to_string()));
    }

    #[test]
    fn instruction_resolution_rejects_stale_previews_and_foreign_paths() {
        for edit_source in [true, false] {
            let (fx, target) = instruction_resolution_fixture();
            let plan = plan_instruction_resolution(
                &fx.home,
                &fx.paths.state_dir,
                &target,
                InstructionChoice::UseShared,
            )
            .unwrap();
            let changed = if edit_source {
                instruction_source(&fx.home)
            } else {
                target.clone()
            };
            fs::write(&changed, "edited after preview\n").unwrap();
            assert!(apply_instruction_resolution(&fx.home, &plan).is_err());
            assert_eq!(
                fs::read_to_string(&changed).unwrap(),
                "edited after preview\n"
            );
            assert!(!fx.paths.backup_dir().exists());
            assert!(!target.is_symlink());
            assert!(
                plan_instruction_resolution(
                    &fx.home,
                    &fx.paths.state_dir,
                    &fx.home.join("private.md"),
                    InstructionChoice::UseShared
                )
                .is_err()
            );
        }
    }

    #[test]
    fn instruction_resolution_handles_missing_source_and_dangling_links() {
        let (fx, target) = instruction_resolution_fixture();
        fs::remove_file(instruction_source(&fx.home)).unwrap();
        let plan = plan_instruction_resolution(
            &fx.home,
            &fx.paths.state_dir,
            &target,
            InstructionChoice::UseLocal,
        )
        .unwrap();
        apply_instruction_resolution(&fx.home, &plan).unwrap();
        assert_eq!(
            fs::read_to_string(instruction_source(&fx.home)).unwrap(),
            "local rules\n"
        );
        fs::remove_file(&target).unwrap();
        let missing = fx.home.join("missing.md");
        std::os::unix::fs::symlink(&missing, &target).unwrap();
        let plan = plan_instruction_resolution(
            &fx.home,
            &fx.paths.state_dir,
            &target,
            InstructionChoice::UseShared,
        )
        .unwrap();
        apply_instruction_resolution(&fx.home, &plan).unwrap();
        let backup = plan
            .changes
            .iter()
            .find_map(|change| match change {
                Change::BackupRemove { backup, .. } => Some(backup),
                _ => None,
            })
            .unwrap();
        assert_eq!(fs::read_link(backup).unwrap(), missing);
        assert_eq!(
            fs::read_link(&target).unwrap(),
            instruction_source(&fx.home)
        );
    }

    #[test]
    fn instruction_diff_preserves_order_repeated_lines_and_large_changes() {
        let diff = instruction_diff(b"A\nB\nA\n", b"A\nA\nB\n").unwrap();
        assert_eq!(diff, [" A", "-B", "-A", "+A", "+B"]);
        let before = (0..100).map(|i| format!("old {i}\n")).collect::<String>();
        let after = (0..100).map(|i| format!("new {i}\n")).collect::<String>();
        let diff = instruction_diff(before.as_bytes(), after.as_bytes()).unwrap();
        assert_eq!(diff.len(), 200);
        assert!(diff.contains(&"-old 99".to_string()));
        assert!(diff.contains(&"+new 99".to_string()));
        assert!(
            instruction_diff(b"line", b"line\n")
                .unwrap()
                .iter()
                .any(|line| line.contains("No newline"))
        );
    }

    fn mcp_fixture() -> Fx {
        let fx = fixture("mcp");
        fs::write(
            fx.home.join(".claude.json"),
            serde_json::json!({
                "oauthAccount": {"accountUuid": "P-UUID"},
                "mcpServers": {
                    "std": {"type": "stdio", "command": "/bin/srv",
                             "args": ["a"], "env": {"TOKEN": "s3cr3t"}},
                    "web": {"type": "http", "url": "https://example.com/mcp"}
                }
            })
            .to_string(),
        )
        .unwrap();
        fs::create_dir_all(fx.home.join(".codex")).unwrap();
        fs::write(
            fx.home.join(".codex/config.toml"),
            "# keep me\nmodel = \"x\"\n\n[mcp_servers.std]\n\
             command = \"/bin/old\"\nargs = [\"a\"]\nstartup_timeout_sec = 20.0\n\
             # tail comment\n\n[mcp_servers.stale]\ncommand = \"/bin/gone\"\n",
        )
        .unwrap();
        fx
    }

    #[test]
    fn mcp_sync_adds_updates_and_redacts() {
        let fx = mcp_fixture();
        unsafe {
            std::env::set_var("AGENTDECK_CODEX_CONFIG", fx.home.join(".codex/config.toml"));
        }
        let (changes, skipped) = plan_mcp_sync(&fx.home, &fx.paths.state_dir, &[], false).unwrap();
        unsafe {
            std::env::remove_var("AGENTDECK_CODEX_CONFIG");
        }
        assert!(skipped.is_empty(), "{skipped:?}");
        assert_eq!(changes.len(), 1);
        let text = match &changes[0] {
            Change::WriteFile {
                path,
                contents,
                old,
                ..
            } => describe_codex_change(path, old.as_deref(), contents),
            other => panic!("{other:?}"),
        };
        assert!(text.contains("https://example.com/mcp"), "{text}");
        for secret in ["s3cr3t", "P-UUID"] {
            assert!(!text.contains(secret), "leak in {text}");
        }
        apply_changes(&changes).unwrap();
        let after = fs::read_to_string(fx.home.join(".codex/config.toml")).unwrap();
        assert!(
            after.contains("# keep me") && after.contains("# tail comment"),
            "{after}"
        );
        assert!(after.contains("/bin/srv"), "{after}");
        assert!(after.contains("stale"), "no prune: extra kept");
        assert!(after.contains("startup_timeout_sec"), "unmanaged keys kept");
        assert!(
            plan_mcp_sync(&fx.home, &fx.paths.state_dir, &[], false)
                .unwrap()
                .0
                .is_empty()
        );
        // --prune removes the extra server (with backup).
        let (changes, _) = plan_mcp_sync(&fx.home, &fx.paths.state_dir, &[], true).unwrap();
        assert_eq!(changes.len(), 1);
        assert!(matches!(
            &changes[0],
            Change::WriteFile {
                backup: Some(_),
                ..
            }
        ));
        apply_changes(&changes).unwrap();
        let after = fs::read_to_string(fx.home.join(".codex/config.toml")).unwrap();
        assert!(!after.contains("stale"), "{after}");
    }

    #[test]
    fn mcp_status_lists_redacted_canonical_and_unsupported() {
        let fx = mcp_fixture();
        let (redacted, targets) = mcp_status(&fx.home, &fx.paths, &[]);
        let text = serde_json::to_string(&redacted).unwrap();
        assert!(!text.contains("s3cr3t"), "{text}");
        assert!(text.contains("<redacted>"), "{text}");
        assert!(
            targets
                .iter()
                .any(|t| t.name == "opencode" && t.detail.contains("unsupported"))
        );
        assert!(
            targets
                .iter()
                .any(|t| t.name == "agy" && t.detail.contains("unsupported"))
        );
        assert!(
            targets
                .iter()
                .any(|t| t.name.starts_with("codex:") && t.detail.contains("missing"))
        );
    }
}
