//! Cross-harness management behind `agentdeck mcp-server` (plugin
//! `mcp-server`): one place where a model lists, installs and removes skills,
//! switches their invocation mode, and edits the shared instructions, so it
//! never patches only its own harness. Everything returns planned `Change`s
//! or applies through them; nothing here talks MCP.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::changes::{Change, apply_changes, backup_dest, plan_write_file};
use crate::paths::Paths;
use crate::plugins::end_marker;

/// Install marker in a skill dir agentdeck installed (holds the source spec).
pub const INSTALL_MARKER: &str = ".agentdeck-install";

// ---- skills: listing and conflicts ---------------------------------------

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SkillEntry {
    pub name: String,
    /// own | installed | plugin | project | harness | system
    pub origin: String,
    pub path: PathBuf,
    pub description: String,
    pub model_invocable: bool,
    /// Per-harness problems (missing link, stale copy, ...).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub issues: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Conflict {
    pub skill: String,
    /// shadowed | shadows-system | project-overrides | harness-copy
    pub kind: String,
    pub detail: String,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Frontmatter {
    pub name: Option<String>,
    pub description: String,
    pub disable_model_invocation: bool,
}

/// The YAML frontmatter fields agentdeck needs, read without a YAML parser
/// (single-line `key: value`, optionally quoted).
pub fn frontmatter(text: &str) -> Frontmatter {
    let mut fm = Frontmatter::default();
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some("---") {
        return fm;
    }
    for line in lines {
        if line.trim() == "---" {
            break;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value
            .trim()
            .trim_matches('"')
            .trim_matches('\'')
            .to_string();
        match key.trim() {
            "name" => fm.name = Some(value),
            "description" => fm.description = value,
            "disable-model-invocation" => fm.disable_model_invocation = value == "true",
            _ => {}
        }
    }
    fm
}

fn read_entry(dir: &Path, name: &str, origin: &str) -> Option<SkillEntry> {
    let text = fs::read_to_string(dir.join("SKILL.md")).ok()?;
    let fm = frontmatter(&text);
    Some(SkillEntry {
        name: name.to_string(),
        origin: origin.to_string(),
        path: dir.to_path_buf(),
        description: fm.description,
        model_invocable: !fm.disable_model_invocation,
        issues: Vec::new(),
    })
}

fn skill_dirs(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<(String, PathBuf)> = rd
        .flatten()
        .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
        .filter(|(n, p)| !n.starts_with('.') && p.join("SKILL.md").is_file())
        .collect();
    out.sort();
    out
}

fn codex_system_skills(home: &Path) -> BTreeSet<String> {
    skill_dirs(&home.join(".codex/skills/.system"))
        .into_iter()
        .map(|(n, _)| n)
        .collect()
}

/// Where a shared install lands: `~/.agents/skills` when it is a configured
/// source, else the last configured source.
pub fn install_dir(paths: &Paths, home: &Path) -> PathBuf {
    let configured = crate::shared::configured_sources(paths, home);
    let agents = home.join(".agents/skills");
    if configured.contains(&agents) {
        agents
    } else {
        configured.last().cloned().unwrap_or(agents)
    }
}

pub fn project_skills_dir(project: &Path) -> PathBuf {
    project.join(".agents/skills")
}

/// Every skill a model in any harness can see, with the conflicts between
/// sources. `project` adds that repository's `.agents/skills`.
pub fn list_skills(
    paths: &Paths,
    home: &Path,
    project: Option<&Path>,
) -> (Vec<SkillEntry>, Vec<Conflict>) {
    let configured = crate::shared::configured_sources(paths, home);
    let plugin_dir = crate::plugins::plugin_skills_dir(paths);
    let mut by_name: BTreeMap<String, Vec<SkillEntry>> = BTreeMap::new();
    for (i, dir) in configured.iter().enumerate() {
        let origin = if i == 0 { "own" } else { "installed" };
        for (name, path) in skill_dirs(dir) {
            if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
                continue; // a link into another source, listed there
            }
            if let Some(e) = read_entry(&path, &name, origin) {
                by_name.entry(name).or_default().push(e);
            }
        }
    }
    for (name, path) in skill_dirs(&plugin_dir) {
        if let Some(e) = read_entry(&path, &name, "plugin") {
            by_name.entry(name).or_default().push(e);
        }
    }
    let system = codex_system_skills(home);
    let mut conflicts = Vec::new();
    for (name, entries) in &by_name {
        if entries.len() > 1 {
            let winner = entries.last().map(|e| e.origin.clone()).unwrap_or_default();
            conflicts.push(Conflict {
                skill: name.clone(),
                kind: "shadowed".into(),
                detail: format!(
                    "{} copies ({}); the {winner} one is what harnesses get",
                    entries.len(),
                    entries
                        .iter()
                        .map(|e| e.origin.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            });
        }
        if system.contains(name) {
            conflicts.push(Conflict {
                skill: name.clone(),
                kind: "shadows-system".into(),
                detail: "same name as a Codex system skill: Codex loads the shared one instead"
                    .into(),
            });
        }
    }
    // Per-harness state, folded into each skill.
    let mut issues: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut harness_only: BTreeMap<String, PathBuf> = BTreeMap::new();
    for st in crate::shared::skills_status(home, paths) {
        use crate::shared::SkillState as S;
        match &st.state {
            S::Ok | S::NoDir => {}
            S::Foreign => {
                let path = st.target.join(&st.skill);
                // Installer-managed per-harness variants are intended, and a
                // source dir's own entry is not a copy.
                let intended = crate::shared::SKIP_SKILLS.contains(&st.skill.as_str())
                    || configured.contains(&st.target);
                if intended {
                    if !by_name.contains_key(&st.skill) {
                        harness_only.entry(st.skill.clone()).or_insert(path);
                    }
                } else if by_name.contains_key(&st.skill) {
                    conflicts.push(Conflict {
                        skill: st.skill.clone(),
                        kind: "harness-copy".into(),
                        detail: format!(
                            "{} holds its own copy, so that harness ignores the shared skill",
                            path.display()
                        ),
                    });
                } else {
                    harness_only.entry(st.skill.clone()).or_insert(path);
                }
            }
            other => issues.entry(st.skill.clone()).or_default().push(format!(
                "{}: {}",
                st.target.display(),
                other.describe()
            )),
        }
    }
    let mut out: Vec<SkillEntry> = by_name
        .into_values()
        .filter_map(|mut v| v.pop())
        .map(|mut e| {
            e.issues = issues.remove(&e.name).unwrap_or_default();
            e
        })
        .collect();
    for (name, path) in harness_only {
        if let Some(e) = read_entry(&path, &name, "harness") {
            out.push(e);
        }
    }
    for name in &system {
        if let Some(e) = read_entry(
            &home.join(".codex/skills/.system").join(name),
            name,
            "system",
        ) {
            out.push(e);
        }
    }
    if let Some(project) = project {
        let shared: BTreeSet<String> = out.iter().map(|e| e.name.clone()).collect();
        for (name, path) in skill_dirs(&project_skills_dir(project)) {
            if shared.contains(&name) {
                conflicts.push(Conflict {
                    skill: name.clone(),
                    kind: "project-overrides".into(),
                    detail: format!("{} overrides the global skill there", project.display()),
                });
            }
            if let Some(e) = read_entry(&path, &name, "project") {
                out.push(e);
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name).then(a.origin.cmp(&b.origin)));
    (out, conflicts)
}

// ---- skills: install / remove / invocation --------------------------------

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Fetch a skill source: an existing local dir, or GitHub as `owner/repo`,
/// `owner/repo/sub/dir` or a github.com URL (shallow clone into the state
/// dir). Returns the root to search for skills.
pub fn fetch(spec: &str, state_dir: &Path) -> anyhow::Result<PathBuf> {
    let local = PathBuf::from(spec);
    if local.is_dir() {
        return Ok(local);
    }
    let rest = spec
        .trim_start_matches("https://")
        .trim_start_matches("github.com/")
        .trim_end_matches('/')
        .trim_end_matches(".git");
    let parts: Vec<&str> = rest.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() < 2
        || !parts[..2].iter().all(|p| {
            p.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        })
    {
        anyhow::bail!("source must be a local dir, owner/repo[/path] or a github.com URL");
    }
    // `tree/<branch>/` in a browser URL is not part of the path.
    let sub: Vec<&str> = match parts.get(2) {
        Some(&"tree") | Some(&"blob") => parts.iter().skip(4).copied().collect(),
        _ => parts.iter().skip(2).copied().collect(),
    };
    let branch = matches!(parts.get(2), Some(&"tree") | Some(&"blob"))
        .then(|| parts.get(3).copied())
        .flatten();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dest = state_dir.join("skill-fetch").join(stamp.to_string());
    fs::create_dir_all(dest.parent().unwrap())?;
    let url = format!("https://github.com/{}/{}.git", parts[0], parts[1]);
    let mut cmd = std::process::Command::new("git");
    cmd.args(["clone", "--quiet", "--depth", "1"]);
    if let Some(b) = branch {
        cmd.args(["--branch", b]);
    }
    let out = cmd
        .arg(&url)
        .arg(&dest)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()?;
    if !out.status.success() {
        anyhow::bail!(
            "git clone {url} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let root = sub.iter().fold(dest.clone(), |p, s| p.join(s));
    if !root.is_dir() {
        anyhow::bail!("{} has no {}", url, sub.join("/"));
    }
    Ok(root)
}

/// Skill dirs (with a `SKILL.md`) at or under `root`, at most 4 levels deep.
pub fn find_skills(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
        if dir.join("SKILL.md").is_file() {
            out.push(dir.to_path_buf());
            return;
        }
        if depth == 0 {
            return;
        }
        let Ok(rd) = fs::read_dir(dir) else { return };
        let mut subs: Vec<PathBuf> = rd
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .filter(|e| !e.file_name().to_string_lossy().starts_with(".git"))
            .map(|e| e.path())
            .collect();
        subs.sort();
        for s in subs {
            walk(&s, depth - 1, out);
        }
    }
    let mut out = Vec::new();
    walk(root, 4, &mut out);
    out
}

fn copy_changes(src: &Path, dst: &Path, out: &mut Vec<Change>) -> anyhow::Result<()> {
    out.push(Change::Mkdir {
        path: dst.to_path_buf(),
    });
    let mut entries: Vec<_> = fs::read_dir(src)?.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name();
        if name.to_string_lossy().starts_with(".git") || name == INSTALL_MARKER {
            continue;
        }
        let ft = e.file_type()?;
        let to = dst.join(&name);
        if ft.is_dir() {
            copy_changes(&e.path(), &to, out)?;
        } else if ft.is_file() {
            let bytes = fs::read(e.path())?;
            use std::os::unix::fs::PermissionsExt;
            let exec = e.metadata()?.permissions().mode() & 0o111 != 0;
            out.push(if exec {
                Change::WriteScript {
                    path: to,
                    contents: bytes,
                    old: None,
                }
            } else {
                Change::WriteFile {
                    path: to,
                    contents: bytes,
                    old: None,
                    backup: None,
                }
            });
        }
        // Symlinks inside a fetched repo are skipped: they may point anywhere.
    }
    Ok(())
}

/// Plan installing the skill at `src` globally (shared, every harness) or
/// into `project`. Refuses names that would shadow a Codex system skill or a
/// plugin skill, and an existing skill unless `replace`.
pub fn plan_install(
    paths: &Paths,
    home: &Path,
    src: &Path,
    source_spec: &str,
    project: Option<&Path>,
    replace: bool,
) -> anyhow::Result<(String, Vec<Change>)> {
    let text = fs::read_to_string(src.join("SKILL.md"))
        .map_err(|_| anyhow::anyhow!("{} has no SKILL.md", src.display()))?;
    let fm = frontmatter(&text);
    let name = fm
        .name
        .clone()
        .or_else(|| src.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_default();
    if !valid_name(&name) {
        anyhow::bail!("skill name {name:?} must be lowercase letters, digits and dashes");
    }
    if fm.description.is_empty() {
        anyhow::bail!("{name}: SKILL.md has no description, so no model would ever load it");
    }
    if codex_system_skills(home).contains(&name) {
        anyhow::bail!("{name} would shadow the Codex system skill of the same name");
    }
    if crate::plugins::plugin_skills_dir(paths)
        .join(&name)
        .exists()
    {
        anyhow::bail!("{name} ships with an agentdeck plugin");
    }
    let dest_dir = match project {
        Some(p) => project_skills_dir(p),
        None => install_dir(paths, home),
    };
    let dest = dest_dir.join(&name);
    let mut changes = Vec::new();
    let existing: Vec<PathBuf> = match project {
        Some(_) => vec![dest.clone()],
        None => crate::shared::configured_sources(paths, home)
            .into_iter()
            .map(|d| d.join(&name))
            .collect(),
    };
    for path in existing.iter().filter(|p| fs::symlink_metadata(p).is_ok()) {
        if !replace {
            anyhow::bail!(
                "{name} already exists at {}; pass replace to overwrite it",
                path.display()
            );
        }
        if *path == dest {
            changes.push(Change::BackupRemove {
                path: path.clone(),
                backup: backup_dest(&paths.state_dir, &name),
            });
        }
    }
    copy_changes(src, &dest, &mut changes)?;
    changes.push(Change::WriteFile {
        path: dest.join(INSTALL_MARKER),
        contents: format!("{}\n", json!({ "source": source_spec })).into_bytes(),
        old: None,
        backup: None,
    });
    if let Some(p) = project {
        let link = p.join(".claude/skills").join(&name);
        if fs::symlink_metadata(&link).is_err() {
            changes.push(Change::Symlink {
                link,
                target: PathBuf::from("../../.agents/skills").join(&name),
            });
        }
    }
    Ok((name, changes))
}

/// Plan removing a shared or project skill. Plugin, harness-only and system
/// skills are refused with the way to remove them instead.
pub fn plan_remove(
    paths: &Paths,
    home: &Path,
    name: &str,
    project: Option<&Path>,
) -> anyhow::Result<Vec<Change>> {
    if let Some(p) = project {
        let dir = project_skills_dir(p).join(name);
        if !dir.join("SKILL.md").is_file() {
            anyhow::bail!("{name} is not a skill of {}", p.display());
        }
        let mut changes = vec![Change::BackupRemove {
            path: dir,
            backup: backup_dest(&paths.state_dir, name),
        }];
        let link = p.join(".claude/skills").join(name);
        if fs::symlink_metadata(&link).is_ok_and(|m| m.file_type().is_symlink()) {
            changes.push(Change::Remove { path: link });
        }
        return Ok(changes);
    }
    if crate::plugins::plugin_skills_dir(paths).join(name).exists() {
        anyhow::bail!("{name} ships with an agentdeck plugin: disable that plugin instead");
    }
    if codex_system_skills(home).contains(name) {
        anyhow::bail!("{name} is a Codex system skill and cannot be removed here");
    }
    let found: Vec<PathBuf> = crate::shared::configured_sources(paths, home)
        .into_iter()
        .map(|d| d.join(name))
        .filter(|p| p.join("SKILL.md").is_file() && !p.is_symlink())
        .collect();
    if found.is_empty() {
        anyhow::bail!("no shared skill named {name}");
    }
    Ok(found
        .into_iter()
        .map(|path| Change::BackupRemove {
            path,
            backup: backup_dest(&paths.state_dir, name),
        })
        .collect())
}

/// Plan switching a skill between model-invoked and user-invoked only:
/// Claude's `disable-model-invocation` frontmatter and Codex's
/// `agents/openai.yaml` implicit-invocation policy.
pub fn plan_invocation(dir: &Path, model_invocable: bool) -> anyhow::Result<Vec<Change>> {
    let skill = dir.join("SKILL.md");
    let text = fs::read_to_string(&skill)?;
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    if lines.first().map(|l| l.trim()) != Some("---") {
        anyhow::bail!("{} has no frontmatter", skill.display());
    }
    let close = lines
        .iter()
        .skip(1)
        .position(|l| l.trim() == "---")
        .map(|i| i + 1)
        .ok_or_else(|| anyhow::anyhow!("{} frontmatter is not closed", skill.display()))?;
    let mut head: Vec<&str> = lines[1..close]
        .iter()
        .copied()
        .filter(|l| !l.trim_start().starts_with("disable-model-invocation:"))
        .collect();
    if !model_invocable {
        head.push("disable-model-invocation: true\n");
    }
    let new_text = format!("{}{}{}", lines[0], head.concat(), lines[close..].concat());
    let mut changes: Vec<Change> =
        plan_write_file(skill, Some(text.as_bytes()), new_text.as_bytes())
            .into_iter()
            .collect();
    let yaml = dir.join("agents/openai.yaml");
    let old = fs::read_to_string(&yaml).ok();
    let policy = "policy:\n  allow_implicit_invocation: false\n";
    let stripped = old
        .as_deref()
        .unwrap_or("")
        .replace(policy, "")
        .replace("  allow_implicit_invocation: false\n", "");
    let new_yaml = if model_invocable {
        stripped
    } else {
        format!("{stripped}{policy}")
    };
    if new_yaml.trim().is_empty() {
        if old.is_some() {
            changes.push(Change::Remove { path: yaml });
        }
    } else {
        changes.extend(plan_write_file(
            yaml,
            old.as_deref().map(str::as_bytes),
            new_yaml.as_bytes(),
        ));
    }
    Ok(changes)
}

/// Apply skill changes, then relink every harness skill dir.
pub fn apply_skills(paths: &Paths, home: &Path, changes: &[Change]) -> anyhow::Result<()> {
    apply_changes(changes)?;
    let sync = crate::shared::plan_skills_sync(home, paths)?;
    apply_changes(&sync)
}

// ---- shared instructions --------------------------------------------------

/// Byte ranges of agentdeck's managed blocks in `text`.
pub fn managed_spans(text: &str) -> Vec<(usize, usize, String)> {
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(i) = text[at..].find("<!-- ") {
        let start = at + i;
        let Some(close) = text[start..].find(" -->") else {
            break;
        };
        let tag = &text[start + 5..start + close];
        if let Some(name) = tag.strip_suffix(":begin") {
            let end_tag = end_marker(name);
            if let Some(j) = text[start..].find(&end_tag) {
                let end = start + j + end_tag.len();
                out.push((start, end, name.to_string()));
                at = end;
                continue;
            }
        }
        at = start + close + 4;
    }
    out
}

#[derive(Debug, Clone, Serialize)]
pub struct InstructionsView {
    pub path: PathBuf,
    /// Hand-written text; each managed block is replaced by a one-line note.
    pub text: String,
    pub managed_blocks: Vec<String>,
    /// Harness files and whether each one links to the shared file.
    pub targets: Vec<(PathBuf, String)>,
}

pub fn read_instructions(home: &Path) -> anyhow::Result<InstructionsView> {
    let path = crate::shared::instruction_source(home);
    let raw = fs::read_to_string(&path)?;
    let mut text = String::new();
    let mut at = 0;
    let mut blocks = Vec::new();
    for (start, end, name) in managed_spans(&raw) {
        text.push_str(&raw[at..start]);
        text.push_str(&format!(
            "[agentdeck block {name}: managed, not editable here]"
        ));
        blocks.push(name);
        at = end;
    }
    text.push_str(&raw[at..]);
    let targets = crate::shared::instructions_status(home)
        .into_iter()
        .map(|s| (s.path, s.state.describe().to_string()))
        .collect();
    Ok(InstructionsView {
        path,
        text,
        managed_blocks: blocks,
        targets,
    })
}

/// Plan an edit of the shared instructions: replace the unique `old` text
/// with `new`, or (with `old` empty) append `new` before agentdeck's blocks.
/// Text inside a managed block is never touched.
pub fn plan_instructions_edit(
    home: &Path,
    state_dir: &Path,
    old: &str,
    new: &str,
) -> anyhow::Result<Vec<Change>> {
    let path = crate::shared::instruction_source(home);
    let raw = fs::read_to_string(&path)?;
    let spans = managed_spans(&raw);
    let updated = if old.is_empty() {
        if new.trim().is_empty() {
            anyhow::bail!("nothing to add");
        }
        let at = spans.first().map(|s| s.0).unwrap_or(raw.len());
        let (head, tail) = raw.split_at(at);
        let mut head = head.trim_end_matches('\n').to_string();
        head.push_str("\n\n");
        head.push_str(new.trim_end_matches('\n'));
        head.push('\n');
        if !tail.is_empty() {
            head.push('\n');
        }
        format!("{head}{tail}")
    } else {
        let hits: Vec<usize> = raw.match_indices(old).map(|(i, _)| i).collect();
        match hits.len() {
            0 => anyhow::bail!("text not found in {}", path.display()),
            1 => {}
            n => anyhow::bail!("text occurs {n} times; include more context to make it unique"),
        }
        let (s, e) = (hits[0], hits[0] + old.len());
        if spans.iter().any(|(bs, be, _)| s < *be && e > *bs) {
            anyhow::bail!(
                "that text is inside an agentdeck block; change the plugin, not the block"
            );
        }
        if new.contains(":begin -->") || new.contains(":end -->") {
            anyhow::bail!("new text must not add agentdeck block markers");
        }
        format!("{}{new}{}", &raw[..s], &raw[e..])
    };
    Ok(
        plan_write_file(path.clone(), Some(raw.as_bytes()), updated.as_bytes())
            .map(|c| match c {
                Change::WriteFile {
                    path,
                    contents,
                    old,
                    ..
                } => Change::WriteFile {
                    backup: Some(backup_dest(state_dir, "AGENTS.md")),
                    path,
                    contents,
                    old,
                },
                other => other,
            })
            .into_iter()
            .collect(),
    )
}

// ---- MCP server registration ---------------------------------------------

pub const SERVER_NAME: &str = "agentdeck";

fn claude_entry(exe: &Path) -> Value {
    json!({ "type": "stdio", "command": exe, "args": ["mcp-server"] })
}

/// Harness MCP config files the plugin edits, with how to write the entry.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// `mcpServers.<name>` = {type, command, args}
    Claude,
    /// `mcpServers.<name>` = {command, args}
    Agy,
    /// `mcp.<name>` = {type: local, command: [..], enabled}
    OpenCode,
}

fn json_targets(paths: &Paths, home: &Path) -> Vec<(PathBuf, Shape)> {
    let mut out = vec![(home.join(".claude.json"), Shape::Claude)];
    // Registered secondaries plus any other `~/.claude-*` profile, like the
    // skill and hook targets.
    let mut dirs = crate::shared::claude_secondaries(paths, home).unwrap_or_default();
    if let Ok(rd) = fs::read_dir(home) {
        dirs.extend(
            rd.flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with(".claude-"))
                .map(|e| e.path()),
        );
    }
    dirs.sort();
    dirs.dedup();
    for dir in dirs {
        out.push((dir.join(".claude.json"), Shape::Claude));
    }
    out.push((home.join(".gemini/config/mcp_config.json"), Shape::Agy));
    let oc = home.join(".config/opencode");
    let file = ["opencode.jsonc", "opencode.json"]
        .iter()
        .map(|f| oc.join(f))
        .find(|p| p.is_file())
        .unwrap_or_else(|| oc.join("opencode.json"));
    out.push((file, Shape::OpenCode));
    out
}

fn originals_path(paths: &Paths) -> PathBuf {
    paths.state_dir.join("mcp-server-originals.json")
}

fn read_originals(paths: &Paths) -> Map<String, Value> {
    fs::read(originals_path(paths))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

fn originals_change(paths: &Paths, map: &Map<String, Value>) -> Option<Change> {
    let path = originals_path(paths);
    let old = fs::read(&path).ok();
    if map.is_empty() {
        return old.map(|_| Change::Remove { path });
    }
    let new = serde_json::to_string_pretty(map).unwrap_or_default() + "\n";
    plan_write_file(path, old.as_deref(), new.as_bytes())
}

fn pretty(v: &Value) -> Vec<u8> {
    (serde_json::to_string_pretty(v).unwrap_or_default() + "\n").into_bytes()
}

fn container(v: &mut Value, shape: Shape) -> Option<&mut Map<String, Value>> {
    let key = if shape == Shape::OpenCode {
        "mcp"
    } else {
        "mcpServers"
    };
    let root = v.as_object_mut()?;
    root.entry(key)
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
}

/// Plan registering (`on`) or removing the agentdeck MCP server in every
/// harness that exists. Disable restores each file's recorded pre-enable
/// bytes when only our entry differed. A file that is not plain JSON (JSONC
/// with comments) is reported, never rewritten.
pub fn plan_register(
    paths: &Paths,
    home: &Path,
    exe: &Path,
    on: bool,
) -> anyhow::Result<(Vec<Change>, Vec<String>)> {
    let mut changes = Vec::new();
    let mut notes = Vec::new();
    let mut originals = read_originals(paths);
    let before = originals.clone();
    for (path, shape) in json_targets(paths, home) {
        if !path.parent().is_some_and(Path::is_dir) {
            continue; // harness not installed
        }
        let old = fs::read(&path).ok();
        if old.is_none() && shape == Shape::Claude {
            continue; // never create an account file
        }
        let mut v: Value = match &old {
            None => Value::Object(Map::new()),
            Some(b) => match serde_json::from_slice(b) {
                Ok(v) => v,
                Err(_) if shape == Shape::OpenCode => {
                    match plan_jsonc(&path, b, exe, on) {
                        Ok(c) => changes.extend(c),
                        Err(e) => notes.push(format!("{}: {e:#}", path.display())),
                    }
                    continue;
                }
                Err(_) => {
                    notes.push(format!(
                        "{}: not valid JSON, add `{SERVER_NAME}` by hand",
                        path.display()
                    ));
                    continue;
                }
            },
        };
        let key = path.display().to_string();
        let entry = match shape {
            Shape::Claude => claude_entry(exe),
            Shape::Agy => json!({ "command": exe, "args": ["mcp-server"] }),
            Shape::OpenCode => {
                json!({ "type": "local", "command": [exe, "mcp-server"], "enabled": true })
            }
        };
        if on {
            let Some(servers) = container(&mut v, shape) else {
                notes.push(format!("{}: MCP section is not an object", path.display()));
                continue;
            };
            if servers.get(SERVER_NAME) == Some(&entry) {
                continue;
            }
            servers.insert(SERVER_NAME.into(), entry);
            // Account files change on their own; no byte snapshot for them.
            if shape != Shape::Claude && !originals.contains_key(&key) {
                originals.insert(
                    key,
                    old.as_deref()
                        .map(|b| Value::String(String::from_utf8_lossy(b).into_owned()))
                        .unwrap_or(Value::Null),
                );
            }
            changes.push(Change::WriteFile {
                backup: old
                    .is_some()
                    .then(|| backup_dest(&paths.state_dir, &file_label(&path))),
                path,
                contents: pretty(&v),
                old,
            });
        } else {
            let Some(old) = old else { continue };
            let section = if shape == Shape::OpenCode {
                "mcp"
            } else {
                "mcpServers"
            };
            let removed = v
                .get_mut(section)
                .and_then(Value::as_object_mut)
                .is_some_and(|m| m.shift_remove(SERVER_NAME).is_some());
            let orig = originals.remove(&key);
            if !removed {
                continue;
            }
            if v.get(section)
                .and_then(Value::as_object)
                .is_some_and(Map::is_empty)
            {
                // Only drop a section enable created.
                let had = match &orig {
                    Some(Value::String(t)) => serde_json::from_str::<Value>(t)
                        .ok()
                        .is_some_and(|o| o.get(section).is_some()),
                    _ => false,
                };
                if !had {
                    v.as_object_mut().map(|r| r.shift_remove(section));
                }
            }
            let change = match orig {
                Some(Value::String(t))
                    if serde_json::from_str::<Value>(&t).ok().as_ref() == Some(&v) =>
                {
                    Change::WriteFile {
                        path: path.clone(),
                        contents: t.into_bytes(),
                        old: Some(old),
                        backup: None,
                    }
                }
                Some(Value::Null) if v.as_object().is_some_and(Map::is_empty) => {
                    Change::Remove { path: path.clone() }
                }
                _ => Change::WriteFile {
                    backup: Some(backup_dest(&paths.state_dir, &file_label(&path))),
                    path: path.clone(),
                    contents: pretty(&v),
                    old: Some(old),
                },
            };
            changes.push(change);
        }
    }
    changes.extend(plan_codex_register(home, exe, on)?);
    if originals != before {
        changes.extend(originals_change(paths, &originals));
    }
    Ok((changes, notes))
}

fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config".into())
}

/// JSONC with comments blanked to spaces (same byte offsets), so it parses
/// as JSON and positions found in it apply to the original text.
fn blank_comments(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = b.to_vec();
    let (mut i, mut in_str) = (0, false);
    while i < b.len() {
        if in_str {
            if b[i] == b'\\' {
                i += 2;
                continue;
            }
            in_str = b[i] != b'"';
            i += 1;
        } else if b[i] == b'"' {
            in_str = true;
            i += 1;
        } else if b[i..].starts_with(b"//") {
            while i < b.len() && b[i] != b'\n' {
                out[i] = b' ';
                i += 1;
            }
        } else if b[i..].starts_with(b"/*") {
            while i < b.len() && !b[i..].starts_with(b"*/") {
                if b[i] != b'\n' {
                    out[i] = b' ';
                }
                i += 1;
            }
            let end = (i + 2).min(b.len());
            out[i..end].fill(b' ');
            i += 2;
        } else {
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_default()
}

/// Byte offset just after the `{` of top-level key `key`'s object value.
fn top_level_object(clean: &str, key: &str) -> Option<usize> {
    let b = clean.as_bytes();
    let (mut depth, mut i) = (0usize, 0);
    while i < b.len() {
        match b[i] {
            b'"' => {
                let start = i;
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
                if depth == 1 && &clean[start + 1..i.min(b.len())] == key {
                    let rest = &clean[i + 1..];
                    let after = rest.trim_start();
                    if let Some(val) = after.strip_prefix(':') {
                        let val = val.trim_start();
                        if val.starts_with('{') {
                            return Some(clean.len() - val.len() + 1);
                        }
                    }
                }
            }
            b'{' | b'[' => depth += 1,
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
        i += 1;
    }
    None
}

fn jsonc_entry(exe: &Path) -> String {
    format!(
        "\n    \"{SERVER_NAME}\": {{ \"type\": \"local\", \"command\": {}, \"enabled\": true }},",
        serde_json::to_string(&json!([exe, "mcp-server"])).unwrap_or_default()
    )
}

/// OpenCode JSONC (comments kept): insert our entry as the first line of the
/// top-level `mcp` object, and on disable cut exactly that text again.
fn plan_jsonc(path: &Path, old: &[u8], exe: &Path, on: bool) -> anyhow::Result<Vec<Change>> {
    let text = std::str::from_utf8(old)?;
    let clean = blank_comments(text);
    let v: Value = serde_json::from_str(&clean)
        .map_err(|e| anyhow::anyhow!("not valid JSONC ({e}), add `{SERVER_NAME}` by hand"))?;
    let entry = jsonc_entry(exe);
    let present = v.pointer(&format!("/mcp/{SERVER_NAME}")).is_some();
    let new = if on {
        if present {
            return Ok(Vec::new());
        }
        let at = top_level_object(&clean, "mcp").ok_or_else(|| {
            anyhow::anyhow!("no top-level \"mcp\" object, add `{SERVER_NAME}` by hand")
        })?;
        let empty = v
            .get("mcp")
            .and_then(Value::as_object)
            .is_some_and(Map::is_empty);
        let snippet = if empty {
            entry.trim_end_matches(',').to_string()
        } else {
            entry
        };
        format!("{}{snippet}{}", &text[..at], &text[at..])
    } else {
        let snippet = [entry.clone(), entry.trim_end_matches(',').to_string()]
            .into_iter()
            .find(|s| text.contains(s.as_str()));
        match snippet {
            Some(s) => text.replacen(&s, "", 1),
            None if present => {
                anyhow::bail!("`{SERVER_NAME}` was edited by hand; remove it by hand")
            }
            None => return Ok(Vec::new()),
        }
    };
    Ok(
        plan_write_file(path.to_path_buf(), Some(old), new.as_bytes())
            .into_iter()
            .collect(),
    )
}

/// Codex `config.toml`: `[mcp_servers.agentdeck]`, edited with toml_edit so
/// the rest of the file stays byte-identical.
fn plan_codex_register(home: &Path, exe: &Path, on: bool) -> anyhow::Result<Vec<Change>> {
    let path = crate::shared::codex_config(home);
    let Ok(old) = fs::read(&path) else {
        return Ok(Vec::new());
    };
    let text = std::str::from_utf8(&old)?;
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e| anyhow::anyhow!("cannot parse {}: {e}", path.display()))?;
    if on {
        if !doc.contains_key("mcp_servers") {
            let mut t = toml_edit::Table::new();
            t.set_implicit(true);
            doc["mcp_servers"] = toml_edit::Item::Table(t);
        }
        let mut table = toml_edit::Table::new();
        table["command"] = toml_edit::value(exe.display().to_string());
        let mut args = toml_edit::Array::new();
        args.push("mcp-server");
        table["args"] = toml_edit::value(args);
        doc["mcp_servers"][SERVER_NAME] = toml_edit::Item::Table(table);
    } else {
        let Some(servers) = doc
            .get_mut("mcp_servers")
            .and_then(|i| i.as_table_like_mut())
        else {
            return Ok(Vec::new());
        };
        servers.remove(SERVER_NAME);
        if servers.is_empty()
            && doc["mcp_servers"]
                .as_table()
                .is_some_and(|t| t.is_implicit())
        {
            doc.remove("mcp_servers");
        }
    }
    let new = doc.to_string();
    Ok(plan_write_file(path, Some(&old), new.as_bytes())
        .into_iter()
        .collect())
}

/// Instruction block of the `mcp-server` plugin.
pub const MCP_BLOCK: &str = "Skills, the shared instructions and agentdeck drift are managed through the `agentdeck` MCP server: `skills_list` (sources, invocation, conflicts), `skill_install` / `skill_remove` (global or one project), `skill_invocation`, `instructions_read` / `instructions_edit`, `drift_list` / `drift_apply` / `drift_resolve`. Use it instead of editing a harness's skill dir, a per-harness instruction file or `~/AGENTS.md` by hand: one call updates every harness, and agentdeck's own blocks stay protected.";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::begin_marker;

    fn fx(tag: &str) -> (PathBuf, Paths, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("agentdeck-manage-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let home = dir.join("home");
        fs::create_dir_all(&home).unwrap();
        (
            dir.clone(),
            Paths::new(dir.join("config"), dir.join("state")),
            home,
        )
    }

    fn skill(dir: &Path, name: &str, extra: &str) {
        fs::create_dir_all(dir.join(name)).unwrap();
        fs::write(
            dir.join(name).join("SKILL.md"),
            format!("---\nname: {name}\ndescription: \"Does {name}\"\n{extra}---\n\n# {name}\n"),
        )
        .unwrap();
    }

    #[test]
    fn frontmatter_reads_quoted_fields_and_invocation() {
        let fm = frontmatter(
            "---\nname: x\ndescription: \"A: b\"\ndisable-model-invocation: true\n---\nbody",
        );
        assert_eq!(fm.name.as_deref(), Some("x"));
        assert_eq!(fm.description, "A: b");
        assert!(fm.disable_model_invocation);
        assert_eq!(frontmatter("no frontmatter"), Frontmatter::default());
    }

    #[test]
    fn list_reports_origins_and_conflicts() {
        let (dir, paths, home) = fx("list");
        skill(&home.join(".general/skills"), "dup", "");
        skill(&home.join(".agents/skills"), "dup", "");
        skill(&home.join(".agents/skills"), "imagegen", "");
        skill(&home.join(".codex/skills/.system"), "imagegen", "");
        skill(
            &home.join(".general/skills"),
            "quiet",
            "disable-model-invocation: true\n",
        );
        let project = dir.join("repo");
        skill(&project.join(".agents/skills"), "quiet", "");
        let (skills, conflicts) = list_skills(&paths, &home, Some(&project));
        let get = |n: &str, o: &str| {
            skills
                .iter()
                .find(|s| s.name == n && s.origin == o)
                .cloned()
        };
        assert!(get("dup", "installed").is_some(), "{skills:?}");
        assert!(!get("quiet", "own").unwrap().model_invocable);
        assert!(get("imagegen", "system").is_some());
        let kinds: Vec<(&str, &str)> = conflicts
            .iter()
            .map(|c| (c.skill.as_str(), c.kind.as_str()))
            .collect();
        assert!(kinds.contains(&("dup", "shadowed")), "{kinds:?}");
        assert!(kinds.contains(&("imagegen", "shadows-system")), "{kinds:?}");
        assert!(kinds.contains(&("quiet", "project-overrides")), "{kinds:?}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn install_remove_round_trip_with_guards() {
        let (dir, paths, home) = fx("install");
        fs::create_dir_all(home.join(".agents/skills")).unwrap();
        fs::create_dir_all(home.join(".claude/skills")).unwrap();
        skill(&home.join(".codex/skills/.system"), "imagegen", "");
        let src = dir.join("src");
        skill(&src, "fresh", "");
        fs::create_dir_all(src.join("fresh/scripts")).unwrap();
        fs::write(src.join("fresh/scripts/run.sh"), "#!/bin/sh\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            src.join("fresh/scripts/run.sh"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let (name, changes) =
            plan_install(&paths, &home, &src.join("fresh"), "local", None, false).unwrap();
        assert_eq!(name, "fresh");
        apply_skills(&paths, &home, &changes).unwrap();
        let dest = home.join(".agents/skills/fresh");
        assert!(dest.join(INSTALL_MARKER).is_file());
        let mode = fs::metadata(dest.join("scripts/run.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert!(mode & 0o111 != 0, "executables stay executable");
        assert_eq!(
            fs::read_link(home.join(".claude/skills/fresh")).unwrap(),
            dest
        );
        // Already there, and a system-skill name, are refused.
        assert!(plan_install(&paths, &home, &src.join("fresh"), "local", None, false).is_err());
        skill(&src, "imagegen", "");
        assert!(plan_install(&paths, &home, &src.join("imagegen"), "local", None, false).is_err());
        // Remove relinks: the harness link goes too.
        let changes = plan_remove(&paths, &home, "fresh", None).unwrap();
        apply_skills(&paths, &home, &changes).unwrap();
        assert!(!dest.exists());
        assert!(fs::symlink_metadata(home.join(".claude/skills/fresh")).is_err());
        assert!(plan_remove(&paths, &home, "imagegen", None).is_err());
        // Project scope: .agents/skills plus a relative Claude link.
        let project = dir.join("repo");
        let (_, changes) = plan_install(
            &paths,
            &home,
            &src.join("fresh"),
            "local",
            Some(&project),
            false,
        )
        .unwrap();
        apply_changes(&changes).unwrap();
        let link = project.join(".claude/skills/fresh");
        assert_eq!(
            fs::read_link(&link).unwrap(),
            PathBuf::from("../../.agents/skills/fresh")
        );
        assert!(link.join("SKILL.md").is_file());
        apply_changes(&plan_remove(&paths, &home, "fresh", Some(&project)).unwrap()).unwrap();
        assert!(fs::symlink_metadata(&link).is_err());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn invocation_toggles_both_harness_flags() {
        let (dir, _paths, home) = fx("invocation");
        let s = home.join("s");
        skill(&s, "x", "");
        fs::create_dir_all(s.join("x/agents")).unwrap();
        fs::write(
            s.join("x/agents/openai.yaml"),
            "interface:\n  display_name: \"X\"\n",
        )
        .unwrap();
        let before = fs::read_to_string(s.join("x/SKILL.md")).unwrap();
        apply_changes(&plan_invocation(&s.join("x"), false).unwrap()).unwrap();
        let text = fs::read_to_string(s.join("x/SKILL.md")).unwrap();
        assert!(frontmatter(&text).disable_model_invocation, "{text}");
        assert!(text.ends_with("# x\n"));
        assert!(
            fs::read_to_string(s.join("x/agents/openai.yaml"))
                .unwrap()
                .contains("allow_implicit_invocation: false")
        );
        assert!(
            plan_invocation(&s.join("x"), false).unwrap().is_empty(),
            "idempotent"
        );
        apply_changes(&plan_invocation(&s.join("x"), true).unwrap()).unwrap();
        assert_eq!(fs::read_to_string(s.join("x/SKILL.md")).unwrap(), before);
        assert_eq!(
            fs::read_to_string(s.join("x/agents/openai.yaml")).unwrap(),
            "interface:\n  display_name: \"X\"\n"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn jsonc_insert_and_remove_keep_comments_byte_exact() {
        let (dir, _paths, home) = fx("jsonc");
        let path = home.join("opencode.jsonc");
        let orig = "{\n  // keep me\n  \"mcp\": {\n    \"x\": { \"type\": \"local\" } // and me\n  },\n  \"a\": \"// not a comment\"\n}\n";
        fs::write(&path, orig).unwrap();
        let exe = Path::new("/bin/agentdeck");
        let on = plan_jsonc(&path, orig.as_bytes(), exe, true).unwrap();
        apply_changes(&on).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("// keep me") && text.contains("// and me"));
        let v: Value = serde_json::from_str(&blank_comments(&text)).unwrap();
        assert_eq!(v["mcp"]["agentdeck"]["command"][1], "mcp-server");
        assert_eq!(v["a"], "// not a comment");
        assert!(
            plan_jsonc(&path, text.as_bytes(), exe, true)
                .unwrap()
                .is_empty()
        );
        apply_changes(&plan_jsonc(&path, text.as_bytes(), exe, false).unwrap()).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), orig);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn instructions_edit_protects_managed_blocks() {
        let (dir, paths, home) = fx("instr");
        let block = format!(
            "{}\nmanaged text\n{}\n",
            begin_marker("router"),
            end_marker("router")
        );
        fs::write(
            home.join("AGENTS.md"),
            format!("# Mine\n\nrule one\n\n{block}"),
        )
        .unwrap();
        let view = read_instructions(&home).unwrap();
        assert_eq!(view.managed_blocks, ["router"]);
        assert!(!view.text.contains("managed text"));
        apply_changes(
            &plan_instructions_edit(&home, &paths.state_dir, "rule one", "rule 1").unwrap(),
        )
        .unwrap();
        apply_changes(
            &plan_instructions_edit(&home, &paths.state_dir, "", "## Added\n\n- new").unwrap(),
        )
        .unwrap();
        let text = fs::read_to_string(home.join("AGENTS.md")).unwrap();
        assert_eq!(
            text,
            format!("# Mine\n\nrule 1\n\n## Added\n\n- new\n\n{block}")
        );
        assert!(plan_instructions_edit(&home, &paths.state_dir, "managed text", "x").is_err());
        assert!(plan_instructions_edit(&home, &paths.state_dir, "absent", "x").is_err());
        let _ = fs::remove_dir_all(dir);
    }
}
