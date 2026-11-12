//! Built-in plugins: the `agent-process` launcher, the `ai-usage` daemon unit,
//! the `router` (model-route copy) and `imagegen` (gateway image skill). Plugin state lives in `instances.toml`
//! under `[plugins]` (absent table = nothing enabled, so old files stay
//! valid). Enabling a plugin lays out its files and instruction block;
//! disabling removes them; plugin data is kept.

use std::fs;
use std::path::{Path, PathBuf};

use crate::changes::{Change, apply_changes, backup_dest, plan_write_file};
use crate::paths::Paths;
use crate::registry::Registry;

pub const PLUGIN_AGENT_PROCESS: &str = "agent-process";
pub const PLUGIN_AI_USAGE: &str = "ai-usage";
pub const PLUGIN_ROUTER: &str = "router";
pub const PLUGIN_IMAGEGEN: &str = "imagegen";
pub const PLUGINS: [&str; 4] = [
    PLUGIN_AGENT_PROCESS,
    PLUGIN_AI_USAGE,
    PLUGIN_ROUTER,
    PLUGIN_IMAGEGEN,
];

/// agent-process instruction block. Every mention of the launcher lives here
/// so disabling the plugin or uninstalling agentdeck leaves no stale rules;
/// the general process-ownership policy stays hand-written.
pub const AGENT_PROCESS_BLOCK: &str = "Start agent-owned long-running processes (dev/preview/watch servers, browser and other daemons) only with `agent-process start --name <name> --cwd <project> -- <command>` (`~/.local/bin/agent-process` on bash/zsh `PATH`). It tags the environment, creates a separate process group and records PID, birth time, command, cwd, log and agent-session owner in `$XDG_RUNTIME_DIR/ai-agent-processes`; that record is the launch proof for the process-ownership rules.\n\
    - Inspect ownership with `agent-process list` / `agent-process status <id>`.\n\
    - Stop your own or your delegated child's servers with `agent-process stop <id>`; signal the exact process group only when no record exists.\n\
    - Other sessions' servers may be reused; stopping them requires explicit user approval, the only permitted use of `--force`.";

pub fn is_known(name: &str) -> bool {
    PLUGINS.contains(&name)
}

fn check_name(name: &str) -> anyhow::Result<()> {
    if !is_known(name) {
        anyhow::bail!(
            "unknown plugin {name:?}: expected agent-process, ai-usage, router or imagegen"
        );
    }
    Ok(())
}

/// The router needs T3 present (it reads providerInstances and caches).
pub fn router_available() -> bool {
    crate::t3::t3_root().is_ok_and(|root| root.exists())
}

pub fn plugin_enabled(reg: &Registry, name: &str) -> bool {
    reg.plugins.get(name).is_some_and(|v| *v)
}

/// Record plugin state in `[plugins]` (backward compatible: only the named
/// key is touched). Returns the registry `Change` (`[]` when unchanged).
pub fn set_plugin_state(
    paths: &Paths,
    name: &str,
    enabled: bool,
    dry_run: bool,
) -> anyhow::Result<Vec<Change>> {
    check_name(name)?;
    let mut reg = Registry::load(paths)?;
    if plugin_enabled(&reg, name) == enabled {
        if !dry_run {
            // Still validate the round trip so typos fail loudly.
            Registry::update(paths, |reg| {
                set_plugin_flag(reg, name, enabled)?;
                Ok(())
            })?;
        }
        return Ok(vec![]);
    }
    let old = fs::read(paths.registry_file()).ok();
    set_plugin_flag(&mut reg, name, enabled)?;
    reg.validate()?;
    let new_text = reg.render()?;
    let changes = plan_write_file(paths.registry_file(), old.as_deref(), new_text.as_bytes())
        .map(|c| vec![c])
        .unwrap_or_default();
    if !dry_run {
        Registry::update(paths, |reg| {
            set_plugin_flag(reg, name, enabled)?;
            Ok(())
        })?;
    }
    Ok(changes)
}

fn set_plugin_flag(reg: &mut Registry, name: &str, enabled: bool) -> anyhow::Result<()> {
    check_name(name)?;
    if enabled {
        reg.plugins.insert(name.to_string(), true);
    } else {
        reg.plugins.remove(name);
    }
    Ok(())
}

pub fn begin_marker(name: &str) -> String {
    format!("<!-- {name}:begin -->")
}

pub fn end_marker(name: &str) -> String {
    format!("<!-- {name}:end -->")
}

/// Insert, replace or remove (`body = None`) a marker block. Markers must
/// each occupy a whole line; anything else (doubled begin, end without begin,
/// unterminated block) is an error and the file is left alone. Returns
/// `None` when the file already has the wanted shape.
pub fn apply_marker_block(
    old: &str,
    name: &str,
    body: Option<&str>,
) -> anyhow::Result<Option<String>> {
    let begin = begin_marker(name);
    let end = end_marker(name);
    let lines: Vec<&str> = old.lines().collect();
    let begins: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.trim() == begin)
        .map(|(i, _)| i)
        .collect();
    let ends: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.trim() == end)
        .map(|(i, _)| i)
        .collect();
    // Markers embedded in other text are malformed, not blocks.
    for line in &lines {
        let t = line.trim();
        if t.contains(&begin) && t != begin {
            anyhow::bail!("malformed {name} block: begin marker shares its line with other text");
        }
        if t.contains(&end) && t != end {
            anyhow::bail!("malformed {name} block: end marker shares its line with other text");
        }
    }
    if begins.len() > 1 || ends.len() > 1 {
        anyhow::bail!("malformed {name} block: doubled markers");
    }
    match (begins.first(), ends.first()) {
        (Some(&bi), Some(&ei)) => {
            if ei <= bi {
                anyhow::bail!("malformed {name} block: end marker comes before begin");
            }
            let current: Vec<&str> = lines[bi + 1..ei].to_vec();
            match body {
                None => {
                    // Drop the whole block lines; the insert below only ever
                    // appends after a trailing newline, so this restores the
                    // original bytes exactly.
                    let mut kept: Vec<&str> = lines[..bi].to_vec();
                    kept.extend_from_slice(&lines[ei + 1..]);
                    let mut new = kept.join("\n");
                    if !old.is_empty() && !new.is_empty() {
                        new.push('\n');
                    }
                    if new == old {
                        return Ok(None);
                    }
                    Ok(Some(new))
                }
                Some(text) => {
                    let want: Vec<&str> = text.lines().collect();
                    if current == want {
                        return Ok(None);
                    }
                    let mut kept: Vec<&str> = lines[..=bi].to_vec();
                    kept.extend(want);
                    kept.extend_from_slice(&lines[ei..]);
                    let mut new = kept.join("\n");
                    new.push('\n');
                    if new == old {
                        return Ok(None);
                    }
                    Ok(Some(new))
                }
            }
        }
        (Some(_), None) => anyhow::bail!("malformed {name} block: begin without end"),
        (None, Some(_)) => anyhow::bail!("malformed {name} block: end without begin"),
        (None, None) => match body {
            None => Ok(None),
            Some(text) => {
                let block = format!(
                    "{begin}\n{}\n{end}\n",
                    text.lines().collect::<Vec<_>>().join("\n")
                );
                if old.is_empty() {
                    return Ok(Some(block));
                }
                let mut new = old.to_string();
                if !new.ends_with('\n') {
                    new.push('\n');
                }
                new.push_str(&block);
                Ok(Some(new))
            }
        },
    }
}

/// Default instruction files: `~/AGENTS.md` and `~/.claude/CLAUDE.md`.
/// Callers pass explicit lists in tests.
pub fn default_instruction_files(home: &Path) -> Vec<PathBuf> {
    vec![home.join("AGENTS.md"), home.join(".claude/CLAUDE.md")]
}

/// Plan the marker-block edit across `files`. A file reached by symlink from
/// another listed file is written once; content outside the markers stays
/// byte-identical. Malformed markers abort the whole plan (no writes).
pub fn plan_marker_files(
    files: &[PathBuf],
    name: &str,
    body: Option<&str>,
    state_dir: &Path,
) -> anyhow::Result<Vec<Change>> {
    check_name(name)?;
    let mut seen: Vec<PathBuf> = Vec::new();
    let mut changes: Vec<Change> = Vec::new();
    for file in files {
        // Canonicalize existing files so symlinked duplicates collapse.
        let key = fs::canonicalize(file).unwrap_or_else(|_| file.clone());
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        let old = match fs::read(file) {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        let old_text = old
            .as_deref()
            .map(|b| {
                std::str::from_utf8(b)
                    .map_err(|_| anyhow::anyhow!("{} is not valid UTF-8", file.display()))
            })
            .transpose()?
            .unwrap_or("");
        let Some(new_text) = apply_marker_block(old_text, name, body)? else {
            continue;
        };
        let backup = old
            .is_some()
            .then(|| backup_dest(state_dir, file_name(file)));
        changes.push(Change::WriteFile {
            path: file.clone(),
            contents: new_text.into_bytes(),
            old,
            backup,
        });
    }
    Ok(changes)
}

fn file_name(path: &Path) -> &str {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("instructions")
}

/// Agentdeck owns its marker blocks: an enabled plugin's block must match
/// the generated text, a disabled plugin's block must be absent. Plans both
/// blocks per existing file in one write (symlinked duplicates collapse);
/// missing files are never created here.
pub fn plan_managed_blocks(
    reg: &Registry,
    home: &Path,
    exe: &Path,
    state_dir: &Path,
) -> anyhow::Result<Vec<Change>> {
    let router = plugin_enabled(reg, PLUGIN_ROUTER).then(|| crate::router::router_block(exe));
    let blocks = [
        (
            PLUGIN_AGENT_PROCESS,
            plugin_enabled(reg, PLUGIN_AGENT_PROCESS).then_some(AGENT_PROCESS_BLOCK),
        ),
        (crate::router::ROUTER_MARKER, router.as_deref()),
    ];
    let mut seen: Vec<PathBuf> = Vec::new();
    let mut changes = Vec::new();
    for file in default_instruction_files(home) {
        let Ok(key) = fs::canonicalize(&file) else {
            continue;
        };
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        let old = fs::read(&file)?;
        let old_text = std::str::from_utf8(&old)
            .map_err(|_| anyhow::anyhow!("{} is not valid UTF-8", file.display()))?;
        let mut text = old_text.to_string();
        for (name, body) in blocks {
            if let Some(new) = apply_marker_block(&text, name, body)
                .map_err(|e| anyhow::anyhow!("{}: {e}", file.display()))?
            {
                text = new;
            }
        }
        if text != old_text {
            changes.push(Change::WriteFile {
                path: file.clone(),
                contents: text.into_bytes(),
                old: Some(old),
                backup: Some(backup_dest(state_dir, file_name(&file))),
            });
        }
    }
    Ok(changes)
}

/// Skills shipped with plugins, written under the state dir and linked into
/// every harness by the shared skill sync like any other skill.
const ROUTER_SKILLS: &[(&str, &str)] = &[
    (
        "coordinator/SKILL.md",
        include_str!("../plugin-skills/coordinator/SKILL.md"),
    ),
    (
        "t3-routed-workflow/SKILL.md",
        include_str!("../plugin-skills/t3-routed-workflow/SKILL.md"),
    ),
    (
        "t3-routed-workflow/agents/openai.yaml",
        include_str!("../plugin-skills/t3-routed-workflow/agents/openai.yaml"),
    ),
];

// Not named `imagegen`: Codex ships a system skill by that name, and a shared
// one would shadow its native image tool.
const IMAGEGEN_SKILLS: &[(&str, &str)] = &[(
    "imagegen-gateway/SKILL.md",
    include_str!("../plugin-skills/imagegen-gateway/SKILL.md"),
)];

const PLUGIN_SKILLS: [(&str, &[(&str, &str)]); 2] = [
    (PLUGIN_ROUTER, ROUTER_SKILLS),
    (PLUGIN_IMAGEGEN, IMAGEGEN_SKILLS),
];

pub fn plugin_skills_dir(paths: &Paths) -> PathBuf {
    paths.state_dir.join("plugin-skills")
}

/// Shipped skill files match the binary while their plugin is enabled (hand
/// edits are reverted); a disabled plugin's skills are removed, and the dir
/// goes once no plugin ships anything.
pub fn plan_plugin_skills(reg: &Registry, paths: &Paths) -> Vec<Change> {
    let dir = plugin_skills_dir(paths);
    let (on, off): (Vec<_>, Vec<_>) = PLUGIN_SKILLS
        .iter()
        .partition(|(plugin, _)| plugin_enabled(reg, plugin));
    if on.is_empty() {
        return if fs::symlink_metadata(&dir).is_ok() {
            vec![Change::BackupRemove {
                path: dir,
                backup: backup_dest(&paths.state_dir, "plugin-skills"),
            }]
        } else {
            Vec::new()
        };
    }
    let mut changes: Vec<Change> = on
        .iter()
        .flat_map(|(_, files)| files.iter())
        .filter_map(|(rel, body)| {
            let path = dir.join(rel);
            let old = fs::read(&path).ok();
            plan_write_file(path, old.as_deref(), body.as_bytes())
        })
        .collect();
    let mut gone: Vec<&str> = off
        .iter()
        .flat_map(|(_, files)| files.iter())
        .filter_map(|(rel, _)| rel.split('/').next())
        .collect();
    gone.dedup();
    for skill in gone {
        let path = dir.join(skill);
        if fs::symlink_metadata(&path).is_ok() {
            changes.push(Change::BackupRemove {
                path,
                backup: backup_dest(&paths.state_dir, skill),
            });
        }
    }
    changes
}

/// Bring shipped skills in line with the registry, then relink harness skill
/// dirs so new skills appear and removed ones lose their links.
pub fn sync_plugin_skills(paths: &Paths, home: &Path) -> anyhow::Result<Vec<Change>> {
    let mut changes = plan_plugin_skills(&Registry::load(paths)?, paths);
    if changes.is_empty() {
        return Ok(changes);
    }
    apply_changes(&changes)?;
    let links = crate::shared::plan_skills_sync(home, paths)?;
    apply_changes(&links)?;
    changes.extend(links);
    Ok(changes)
}

/// `~/.local/bin`, or `$AGENTDECK_BIN_DIR` when set (tests inject temp dirs).
pub fn bin_dir(home: &Path) -> PathBuf {
    std::env::var_os("AGENTDECK_BIN_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/bin"))
}

pub fn agent_process_link(home: &Path) -> PathBuf {
    bin_dir(home).join("agent-process")
}

/// Ours when the link target is exactly the agentdeck binary.
fn link_is_ours(link: &Path, exe: &Path) -> bool {
    fs::read_link(link).is_ok_and(|t| t == exe)
}

/// Plan `agent-process` enable: symlink plus instruction block. An existing
/// foreign file/link refuses unless `replace` backs it up first.
pub fn plan_enable_agent_process(
    home: &Path,
    files: &[PathBuf],
    exe: &Path,
    replace: bool,
    state_dir: &Path,
) -> anyhow::Result<Vec<Change>> {
    let link = agent_process_link(home);
    let mut changes = Vec::new();
    match fs::symlink_metadata(&link) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => changes.push(Change::Symlink {
            link: link.clone(),
            target: exe.to_path_buf(),
        }),
        Err(e) => return Err(e.into()),
        Ok(_) if link_is_ours(&link, exe) => {}
        Ok(_) if replace => {
            changes.push(Change::BackupRemove {
                path: link.clone(),
                backup: backup_dest(state_dir, "agent-process"),
            });
            changes.push(Change::Symlink {
                link: link.clone(),
                target: exe.to_path_buf(),
            });
        }
        Ok(_) => anyhow::bail!(
            "{} exists and is not ours; remove it by hand or retry with --replace",
            link.display()
        ),
    }
    changes.extend(plan_marker_files(
        files,
        PLUGIN_AGENT_PROCESS,
        Some(AGENT_PROCESS_BLOCK),
        state_dir,
    )?);
    Ok(changes)
}

/// Plan `agent-process` disable: our symlink and the block go; a foreign
/// file at the link path is left alone. Registry data is untouched.
pub fn plan_disable_agent_process(
    home: &Path,
    files: &[PathBuf],
    exe: &Path,
    state_dir: &Path,
) -> anyhow::Result<Vec<Change>> {
    let link = agent_process_link(home);
    let mut changes = Vec::new();
    if link_is_ours(&link, exe) {
        changes.push(Change::Remove { path: link });
    }
    changes.extend(plan_marker_files(
        files,
        PLUGIN_AGENT_PROCESS,
        None,
        state_dir,
    )?);
    Ok(changes)
}

/// Enable = install the daemon unit (never start it); disable = uninstall
/// (refuses while active). Plugin data is kept either way.
pub fn plan_enable_ai_usage(paths: &Paths) -> anyhow::Result<Vec<Change>> {
    Ok(crate::service::plan_install(paths)?.into_iter().collect())
}

pub fn apply_enable_ai_usage(changes: &[Change]) -> anyhow::Result<()> {
    crate::service::apply_install(changes)
}

pub fn plan_disable_ai_usage() -> anyhow::Result<Vec<Change>> {
    Ok(crate::service::plan_uninstall()?.into_iter().collect())
}

pub fn apply_disable_ai_usage(changes: &[Change]) -> anyhow::Result<()> {
    crate::service::apply_uninstall(changes)
}

#[derive(Debug, Clone)]
pub struct PluginStatus {
    pub name: &'static str,
    pub enabled: bool,
    pub available: bool,
    /// Short human lines: link/unit/block presence.
    pub detail: Vec<String>,
}

/// Read-only status for the CLI/GUI (never writes, never starts anything).
pub fn status(paths: &Paths, home: &Path, exe: &Path) -> Vec<PluginStatus> {
    let reg = Registry::load(paths).unwrap_or_default();
    let files = default_instruction_files(home);
    let mut out = Vec::new();
    out.push(PluginStatus {
        name: PLUGIN_AGENT_PROCESS,
        enabled: plugin_enabled(&reg, PLUGIN_AGENT_PROCESS),
        available: true,
        detail: agent_process_detail(home, exe, &files),
    });
    let unit = crate::service::status(paths).ok();
    out.push(PluginStatus {
        name: PLUGIN_AI_USAGE,
        enabled: plugin_enabled(&reg, PLUGIN_AI_USAGE),
        available: true,
        detail: vec![
            format!(
                "unit: {}",
                unit.as_ref()
                    .map(|u| u.installed)
                    .map(|b| if b { "installed" } else { "not installed" })
                    .unwrap_or("unknown")
            ),
            format!(
                "service: {}",
                unit.as_ref()
                    .map(|u| u.active)
                    .map(|b| if b { "active" } else { "inactive" })
                    .unwrap_or("unknown")
            ),
        ],
    });
    out.push(PluginStatus {
        name: PLUGIN_ROUTER,
        enabled: plugin_enabled(&reg, PLUGIN_ROUTER),
        available: router_available(),
        detail: router_detail(paths, home, exe),
    });
    let ig = crate::imagegen::read(&reg);
    let key = crate::imagegen::gateway_key(&reg, home, &ig.provider).is_some();
    out.push(PluginStatus {
        name: PLUGIN_IMAGEGEN,
        enabled: plugin_enabled(&reg, PLUGIN_IMAGEGEN),
        available: true,
        detail: vec![
            format!("provider: {}", ig.provider),
            format!("model: {}", ig.model),
            format!("key: {}", if key { "found" } else { "missing" }),
        ],
    });
    out
}

fn router_detail(paths: &Paths, home: &Path, exe: &Path) -> Vec<String> {
    use crate::router as r;
    let installed = if r::router_json_path(paths).exists() {
        "files: installed"
    } else {
        "files: missing"
    }
    .to_string();
    let shell_line = match fs::symlink_metadata(r::claude_t3_shell_link(home)) {
        Err(_) => "shell: missing".to_string(),
        Ok(_) if link_is_ours(&r::claude_t3_shell_link(home), exe) => {
            "shell: installed".to_string()
        }
        Ok(_) => "shell: foreign file".to_string(),
    };
    let present = r::hooks_present(home, exe);
    let hooks = format!(
        "hooks: {}/4 targets",
        present.iter().filter(|p| **p).count()
    );
    let begin = format!("<!-- {}:begin -->", r::ROUTER_MARKER);
    let mut with_block = 0;
    let mut present_files = 0;
    let mut seen: Vec<PathBuf> = Vec::new();
    for file in default_instruction_files(home) {
        let key = fs::canonicalize(&file).unwrap_or_else(|_| file.clone());
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        let Ok(bytes) = fs::read(&file) else { continue };
        present_files += 1;
        if String::from_utf8_lossy(&bytes)
            .lines()
            .any(|l| l.trim() == begin)
        {
            with_block += 1;
        }
    }
    vec![
        installed,
        shell_line,
        hooks,
        format!("block: {with_block}/{present_files} files"),
    ]
}

/// Enable order: files, then hooks, then the marker block (hooks fail closed,
/// so they must never point at a missing router).
fn plan_enable_router(
    paths: &Paths,
    home: &Path,
    files: &[PathBuf],
    exe: &Path,
    replace: bool,
) -> anyhow::Result<Vec<Change>> {
    if !router_available() {
        anyhow::bail!("router needs T3 present (no T3 root found)");
    }
    let reg = Registry::load(paths)?;
    let mut changes = crate::router::plan_router_files(paths, &reg)?;
    changes.extend(crate::router::plan_hooks_enable(
        home,
        paths,
        exe,
        &paths.state_dir,
        replace,
    )?);
    changes.extend(crate::router::plan_shell_link(
        home,
        exe,
        replace,
        &paths.state_dir,
    )?);
    changes.extend(crate::router::plan_router_marker(
        files,
        Some(&crate::router::router_block(exe)),
        &paths.state_dir,
    )?);
    Ok(changes)
}

/// Disable order: hooks first (fail closed), then files, then the block.
/// `router.json` is generated data and is kept.
fn plan_disable_router(
    paths: &Paths,
    home: &Path,
    files: &[PathBuf],
    exe: &Path,
) -> anyhow::Result<Vec<Change>> {
    if !router_available() {
        anyhow::bail!("router needs T3 present (no T3 root found)");
    }
    let mut changes = crate::router::plan_hooks_disable(home, paths, exe, &paths.state_dir)?;
    changes.extend(crate::router::plan_remove_shell_link(home, exe));
    changes.extend(crate::router::plan_remove_router_files(paths));
    changes.extend(crate::router::plan_router_marker(
        files,
        None,
        &paths.state_dir,
    )?);
    Ok(changes)
}

fn agent_process_detail(home: &Path, exe: &Path, files: &[PathBuf]) -> Vec<String> {
    let link = agent_process_link(home);
    let link_line = match fs::symlink_metadata(&link) {
        Err(_) => "launcher: missing".to_string(),
        Ok(_) if link_is_ours(&link, exe) => "launcher: installed".to_string(),
        Ok(_) => "launcher: foreign file".to_string(),
    };
    let begin = begin_marker(PLUGIN_AGENT_PROCESS);
    let mut with_block = 0;
    let mut present = 0;
    let mut seen: Vec<PathBuf> = Vec::new();
    for file in files {
        let key = fs::canonicalize(file).unwrap_or_else(|_| file.clone());
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        let Ok(bytes) = fs::read(file) else { continue };
        present += 1;
        if String::from_utf8_lossy(&bytes)
            .lines()
            .any(|l| l.trim() == begin)
        {
            with_block += 1;
        }
    }
    vec![link_line, format!("block: {with_block}/{present} files")]
}

/// Full enable plan (files + registry flag) for the CLI/GUI preview.
pub fn plan_enable(
    paths: &Paths,
    home: &Path,
    files: &[PathBuf],
    exe: &Path,
    name: &str,
    replace: bool,
) -> anyhow::Result<Vec<Change>> {
    check_name(name)?;
    if name == PLUGIN_ROUTER && !router_available() {
        anyhow::bail!("router needs T3 present (no T3 root found)");
    }
    let mut changes = match name {
        PLUGIN_AGENT_PROCESS => {
            plan_enable_agent_process(home, files, exe, replace, &paths.state_dir)?
        }
        PLUGIN_AI_USAGE => plan_enable_ai_usage(paths)?,
        PLUGIN_ROUTER => plan_enable_router(paths, home, files, exe, replace)?,
        // Only the flag and its skill (synced after apply).
        PLUGIN_IMAGEGEN => Vec::new(),
        _ => unreachable!(),
    };
    let old = fs::read(paths.registry_file()).ok();
    let mut reg = Registry::load(paths)?;
    set_plugin_flag(&mut reg, name, true)?;
    reg.validate()?;
    changes.extend(
        plan_write_file(
            paths.registry_file(),
            old.as_deref(),
            reg.render()?.as_bytes(),
        )
        .map(|c| vec![c])
        .unwrap_or_default(),
    );
    Ok(changes)
}

/// Full disable plan (files + registry flag) for the CLI/GUI preview.
pub fn plan_disable(
    paths: &Paths,
    home: &Path,
    files: &[PathBuf],
    exe: &Path,
    name: &str,
) -> anyhow::Result<Vec<Change>> {
    check_name(name)?;
    if name == PLUGIN_ROUTER && !router_available() {
        anyhow::bail!("router needs T3 present (no T3 root found)");
    }
    let mut changes = match name {
        PLUGIN_AGENT_PROCESS => plan_disable_agent_process(home, files, exe, &paths.state_dir)?,
        PLUGIN_AI_USAGE => plan_disable_ai_usage()?,
        PLUGIN_ROUTER => plan_disable_router(paths, home, files, exe)?,
        PLUGIN_IMAGEGEN => Vec::new(),
        _ => unreachable!(),
    };
    let old = fs::read(paths.registry_file()).ok();
    let mut reg = Registry::load(paths)?;
    set_plugin_flag(&mut reg, name, false)?;
    reg.validate()?;
    changes.extend(
        plan_write_file(
            paths.registry_file(),
            old.as_deref(),
            reg.render()?.as_bytes(),
        )
        .map(|c| vec![c])
        .unwrap_or_default(),
    );
    Ok(changes)
}

/// Apply enable/disable changes (files, symlink, registry). Service reloads
/// ride along with their own changes; nothing is ever started.
pub fn apply_all(
    paths: &Paths,
    name: &str,
    enable: bool,
    changes: &[Change],
) -> anyhow::Result<()> {
    check_name(name)?;
    let (file_changes, reg_changes): (Vec<Change>, Vec<Change>) =
        changes.iter().cloned().partition(
            |c| !matches!(c, Change::WriteFile { path, .. } if *path == paths.registry_file()),
        );
    if name == PLUGIN_AI_USAGE {
        if enable {
            crate::service::apply_install(&file_changes)?;
        } else {
            crate::service::apply_uninstall(&file_changes)?;
        }
    } else {
        apply_changes(&file_changes)?;
    }
    apply_changes(&reg_changes)?;
    Ok(())
}

/// Process-wide lock for tests mutating `XDG_CONFIG_HOME`/`AGENTDECK_*` env.
pub fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::unit_path;
    use std::collections::BTreeMap;
    use std::env;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    static N: AtomicU64 = AtomicU64::new(0);

    struct Guard(PathBuf);
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Temp HOME with `AGENTDECK_BIN_DIR` pointed inside it and a fake
    /// systemctl on PATH-vars. Callers hold [`test_env_lock`] across the
    /// whole test body. Returns (guard, home, paths, files, exe).
    #[test]
    fn plugin_skills_follow_their_own_plugin() {
        let (_g, _home, paths, _files, _exe) = setup("plugin-skills");
        let dir = plugin_skills_dir(&paths);
        let set = |name: &str, on: bool| {
            Registry::update(&paths, |r| {
                if on {
                    r.plugins.insert(name.into(), true);
                } else {
                    r.plugins.remove(name);
                }
                Ok(())
            })
            .unwrap();
            let reg = Registry::load(&paths).unwrap();
            apply_changes(&plan_plugin_skills(&reg, &paths)).unwrap();
        };
        set(PLUGIN_IMAGEGEN, true);
        assert!(dir.join("imagegen-gateway/SKILL.md").is_file());
        assert!(!dir.join("coordinator").exists());
        set(PLUGIN_ROUTER, true);
        assert!(dir.join("coordinator/SKILL.md").is_file());
        // Disabling one plugin removes only its skills.
        set(PLUGIN_IMAGEGEN, false);
        assert!(!dir.join("imagegen-gateway").exists());
        assert!(dir.join("coordinator/SKILL.md").is_file());
        set(PLUGIN_ROUTER, false);
        assert!(!dir.exists());
        let reg = Registry::load(&paths).unwrap();
        assert!(plan_plugin_skills(&reg, &paths).is_empty());
    }

    fn setup(tag: &str) -> (Guard, PathBuf, Paths, Vec<PathBuf>, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "agentdeck-plugins-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&dir);
        let home = dir.join("home");
        let exe = dir.join("bin/agentdeck");
        fs::create_dir_all(exe.parent().unwrap()).unwrap();
        fs::write(&exe, "#!/bin/sh\necho hi\n").unwrap();
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
        // Fake systemctl: is-active from $FAKE_SYSTEMCTL_ACTIVE, log the rest.
        let script = dir.join("systemctl");
        fs::write(
            &script,
            "#!/bin/sh\n\
             if [ \"$1\" = \"--user\" ] && [ \"$2\" = \"is-active\" ]; then\n\
             echo \"$FAKE_SYSTEMCTL_ACTIVE\"\n\
             [ \"$FAKE_SYSTEMCTL_ACTIVE\" = \"active\" ]\n\
             exit\n\
             fi\n\
             echo \"$@\" >> \"$FAKE_SYSTEMCTL_LOG\"\n",
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        unsafe {
            env::set_var("XDG_CONFIG_HOME", dir.join("config"));
            env::set_var("AGENTDECK_SYSTEMCTL", &script);
            env::set_var("FAKE_SYSTEMCTL_ACTIVE", "inactive");
            env::set_var("FAKE_SYSTEMCTL_LOG", dir.join("calls.log"));
            env::set_var("AGENTDECK_BIN_DIR", home.join(".local/bin"));
        }
        let paths = Paths::new(dir.join("config/agentdeck"), dir.join("state/agentdeck"));
        let files = default_instruction_files(&home);
        (Guard(dir), home, paths, files, exe)
    }

    #[test]
    fn marker_insert_replace_remove_round_trip() {
        assert_eq!(apply_marker_block("a\n", "x", None).unwrap(), None);
        let added = apply_marker_block("a\n", "x", Some("one"))
            .unwrap()
            .unwrap();
        assert_eq!(added, "a\n<!-- x:begin -->\none\n<!-- x:end -->\n");
        assert_eq!(apply_marker_block(&added, "x", Some("one")).unwrap(), None);
        let swapped = apply_marker_block(&added, "x", Some("two"))
            .unwrap()
            .unwrap();
        assert_eq!(swapped, "a\n<!-- x:begin -->\ntwo\n<!-- x:end -->\n");
        let removed = apply_marker_block(&swapped, "x", None).unwrap().unwrap();
        assert_eq!(removed, "a\n");
        // Missing trailing newline still round-trips.
        let added = apply_marker_block("a", "x", Some("b")).unwrap().unwrap();
        assert_eq!(added, "a\n<!-- x:begin -->\nb\n<!-- x:end -->\n");
        assert_eq!(
            apply_marker_block(&added, "x", None).unwrap().unwrap(),
            "a\n"
        );
        // Empty file.
        assert_eq!(
            apply_marker_block("", "x", Some("b")).unwrap().unwrap(),
            "<!-- x:begin -->\nb\n<!-- x:end -->\n"
        );
    }

    #[test]
    fn marker_malformed_is_an_error() {
        for bad in [
            "<!-- x:begin -->\n<!-- x:begin -->\nb\n<!-- x:end -->\n",
            "b\n<!-- x:end -->\n",
            "<!-- x:begin -->\nb\n",
            "<!-- x:end -->\n<!-- x:begin -->\nb\n",
            "text <!-- x:begin -->\nb\n<!-- x:end -->\n",
        ] {
            assert!(apply_marker_block(bad, "x", Some("n")).is_err(), "{bad:?}");
            assert!(apply_marker_block(bad, "x", None).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn agent_process_round_trip_restores_bytes() {
        let _env = test_env_lock();
        let (_g, home, paths, files, exe) = setup("roundtrip");
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::write(files[0].clone(), "handwritten\n").unwrap();
        fs::write(files[1].clone(), "# claude\n").unwrap();
        let before: BTreeMap<_, _> = files
            .iter()
            .map(|f| (f.clone(), fs::read(f).unwrap()))
            .collect();

        let changes =
            plan_enable_agent_process(&home, &files, &exe, false, &paths.state_dir).unwrap();
        assert!(!changes.is_empty());
        apply_changes(&changes).unwrap();
        Registry::update(&paths, |r| set_plugin_flag(r, PLUGIN_AGENT_PROCESS, true)).unwrap();
        assert!(fs::read_link(agent_process_link(&home)).unwrap() == exe);
        for f in &files {
            let text = fs::read_to_string(f).unwrap();
            assert!(text.contains(&begin_marker(PLUGIN_AGENT_PROCESS)), "{f:?}");
            assert!(text.starts_with(
                String::from_utf8(before[f].clone()).unwrap()[..].trim_end_matches('\n')
            ));
        }
        // Enabling twice plans nothing.
        assert!(
            plan_enable_agent_process(&home, &files, &exe, false, &paths.state_dir)
                .unwrap()
                .is_empty()
        );

        let changes = plan_disable_agent_process(&home, &files, &exe, &paths.state_dir).unwrap();
        assert!(!changes.is_empty());
        apply_changes(&changes).unwrap();
        assert!(!agent_process_link(&home).exists());
        for f in &files {
            assert_eq!(fs::read(f).unwrap(), before[f], "{f:?}");
        }
    }

    #[test]
    fn symlinked_instruction_file_written_once() {
        let _env = test_env_lock();
        let (_g, home, paths, files, exe) = setup("symlink");
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::write(&files[0], "shared\n").unwrap();
        std::os::unix::fs::symlink(&files[0], &files[1]).unwrap();
        let changes =
            plan_enable_agent_process(&home, &files, &exe, false, &paths.state_dir).unwrap();
        let writes = changes
            .iter()
            .filter(|c| matches!(c, Change::WriteFile { .. }))
            .count();
        assert_eq!(writes, 1);
        apply_changes(&changes).unwrap();
        let text = fs::read_to_string(&files[0]).unwrap();
        assert_eq!(text.matches(&begin_marker(PLUGIN_AGENT_PROCESS)).count(), 1);
    }

    #[test]
    fn foreign_launcher_refused_without_replace() {
        let _env = test_env_lock();
        let (_g, home, paths, files, exe) = setup("foreign");
        let link = agent_process_link(&home);
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        fs::write(&link, "not ours").unwrap();
        assert!(plan_enable_agent_process(&home, &files, &exe, false, &paths.state_dir).is_err());
        // --replace backs the foreign file up, then links.
        let changes =
            plan_enable_agent_process(&home, &files, &exe, true, &paths.state_dir).unwrap();
        assert!(
            changes
                .iter()
                .any(|c| matches!(c, Change::BackupRemove { .. }))
        );
        apply_changes(&changes).unwrap();
        assert!(fs::read_link(&link).unwrap() == exe);
        assert!(fs::read_dir(paths.backup_dir()).unwrap().count() >= 1);
        // Disable leaves a foreign file alone (ours is gone after replace, so
        // plant a new foreign one first).
        fs::remove_file(&link).unwrap();
        fs::write(&link, "foreign again").unwrap();
        let other = _g.0.join("bin/other");
        let changes = plan_disable_agent_process(&home, &files, &other, &paths.state_dir).unwrap();
        assert!(changes.iter().all(|c| !matches!(c, Change::Remove { .. })));
        assert!(fs::read(&link).unwrap() == b"foreign again");
    }

    #[test]
    fn ai_usage_enable_never_starts_disable_refuses_while_active() {
        let _env = test_env_lock();
        let (_g, _home, paths, _files, _exe) = setup("aiusage");
        let log = std::env::var_os("FAKE_SYSTEMCTL_LOG")
            .map(PathBuf::from)
            .unwrap();

        // Enable plans a unit write; applying reloads but never starts.
        let changes = plan_enable_ai_usage(&paths).unwrap();
        assert_eq!(changes.len(), 1);
        apply_enable_ai_usage(&changes).unwrap();
        let calls = fs::read_to_string(&log).unwrap();
        assert!(calls.contains("daemon-reload"), "{calls}");
        assert!(!calls.contains("enable"), "{calls}");
        assert!(!calls.contains("start"), "{calls}");

        // Disable refuses while active; data (the unit file) is kept.
        unsafe {
            std::env::set_var("FAKE_SYSTEMCTL_ACTIVE", "active");
        }
        assert!(plan_disable_ai_usage().is_err());
        assert!(unit_path().unwrap().exists());

        unsafe {
            std::env::set_var("FAKE_SYSTEMCTL_ACTIVE", "inactive");
        }
        let changes = plan_disable_ai_usage().unwrap();
        assert_eq!(changes.len(), 1);
        apply_disable_ai_usage(&changes).unwrap();
        assert!(!unit_path().unwrap().exists());
    }

    #[test]
    fn router_enable_disable_round_trip() {
        let _env = test_env_lock();
        let (_g, home, paths, files, exe) = setup("router");
        let t3 = _g.0.join("t3");
        std::fs::create_dir_all(&t3).unwrap();
        unsafe {
            env::set_var("AGENTDECK_T3_DIR", &t3);
        }
        assert!(router_available());
        fs::create_dir_all(&home).unwrap();
        fs::write(&files[0], "handwritten\n").unwrap();
        // Enable: files, then hooks, then marker (+ registry flag last).
        let changes = plan_enable(&paths, &home, &files, &exe, PLUGIN_ROUTER, false).unwrap();
        assert!(!changes.is_empty());
        let dir = crate::router::router_dir(&paths);
        let first_router = changes
            .iter()
            .position(|c| matches!(c, Change::WriteFile { path, .. } if path.starts_with(&dir)))
            .unwrap();
        let first_other = changes
            .iter()
            .position(|c| {
                matches!(c, Change::WriteFile { path, .. }
                    if !path.starts_with(&dir) && *path != paths.registry_file())
            })
            .unwrap();
        assert!(first_router < first_other, "files come before hooks/marker");
        apply_all(&paths, PLUGIN_ROUTER, true, &changes).unwrap();
        assert!(dir.join("router.json").exists());
        let settings = fs::read_to_string(home.join(".claude/settings.json")).unwrap();
        assert!(
            settings.contains("route hook --harness claude"),
            "{settings}"
        );
        assert!(!settings.contains(".mjs"), "{settings}");
        let plugin =
            fs::read_to_string(home.join(".config/opencode/plugin/model-route.ts")).unwrap();
        assert!(
            plugin.contains(r#"["route", "hook", "--harness", "opencode"]"#),
            "{plugin}"
        );
        let worker = home.join(".config/opencode/agent/agentdeck-worker.md");
        assert!(
            fs::read_to_string(&worker)
                .unwrap()
                .contains("temperature: 0.1")
        );
        let text = fs::read_to_string(&files[0]).unwrap();
        assert!(text.contains("<!-- model-route:begin -->"));
        assert!(text.contains("route \"<goal"), "{text}");
        assert!(
            plan_enable(&paths, &home, &files, &exe, PLUGIN_ROUTER, false)
                .unwrap()
                .is_empty()
        );
        // Disable: hooks first (fail closed), then files, then the block.
        let changes = plan_disable(&paths, &home, &files, &exe, PLUGIN_ROUTER).unwrap();
        assert!(!changes.is_empty());
        let is_settings = |c: &Change| matches!(c, Change::WriteFile { path, .. } if path.ends_with("settings.json"));
        let first_hook = changes.iter().position(is_settings).unwrap();
        let first_remove = changes
            .iter()
            .position(|c| matches!(c, Change::Remove { .. }))
            .unwrap();
        assert!(first_hook < first_remove, "hooks go before files");
        apply_all(&paths, PLUGIN_ROUTER, false, &changes).unwrap();
        let settings = fs::read_to_string(home.join(".claude/settings.json")).unwrap();
        assert!(!settings.contains("route hook"));
        assert!(!settings.contains("hook.mjs"));
        assert!(!home.join(".config/opencode/plugin/model-route.ts").exists());
        assert!(!worker.exists());
        assert!(dir.join("router.json").exists(), "data is kept");
        assert_eq!(fs::read(&files[0]).unwrap(), b"handwritten\n");
        unsafe {
            env::remove_var("AGENTDECK_T3_DIR");
        }
    }

    #[test]
    fn router_shell_link_round_trip() {
        let _env = test_env_lock();
        let (_g, home, paths, files, exe) = setup("shelllink");
        let t3 = _g.0.join("t3");
        std::fs::create_dir_all(&t3).unwrap();
        unsafe {
            env::set_var("AGENTDECK_T3_DIR", &t3);
        }
        fs::create_dir_all(&home).unwrap();
        fs::write(&files[0], "handwritten\n").unwrap();
        let link = crate::router::claude_t3_shell_link(&home);
        // Foreign file refuses without --replace.
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        fs::write(&link, "not ours").unwrap();
        assert!(plan_enable(&paths, &home, &files, &exe, PLUGIN_ROUTER, false).is_err());
        // --replace backs the foreign file up, then links.
        let changes = plan_enable(&paths, &home, &files, &exe, PLUGIN_ROUTER, true).unwrap();
        assert!(
            changes
                .iter()
                .any(|c| matches!(c, Change::BackupRemove { .. }))
        );
        apply_all(&paths, PLUGIN_ROUTER, true, &changes).unwrap();
        assert!(fs::read_link(&link).unwrap() == exe);
        assert!(fs::read_dir(paths.backup_dir()).unwrap().count() >= 1);
        let detail = status(&paths, &home, &exe);
        assert!(
            detail
                .iter()
                .find(|s| s.name == PLUGIN_ROUTER)
                .unwrap()
                .detail
                .iter()
                .any(|l| l == "shell: installed")
        );
        // Disable removes our symlink (data kept); a foreign file is left alone.
        let changes = plan_disable(&paths, &home, &files, &exe, PLUGIN_ROUTER).unwrap();
        assert!(changes.iter().any(|c| matches!(c, Change::Remove { .. })));
        apply_all(&paths, PLUGIN_ROUTER, false, &changes).unwrap();
        assert!(!link.exists());
        fs::write(&link, "foreign again").unwrap();
        let other = _g.0.join("bin/other");
        let changes = plan_disable(&paths, &home, &files, &other, PLUGIN_ROUTER).unwrap();
        assert!(changes.iter().all(|c| !matches!(c, Change::Remove { .. })));
        assert!(fs::read(&link).unwrap() == b"foreign again");
        unsafe {
            env::remove_var("AGENTDECK_T3_DIR");
        }
    }

    #[test]
    fn malformed_block_aborts_plan_with_no_writes() {
        let _env = test_env_lock();
        let (_g, home, paths, files, exe) = setup("malformed");
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::write(&files[0], "ok\n").unwrap();
        fs::write(&files[1], "<!-- agent-process:end -->\nstray\n").unwrap();
        assert!(plan_enable_agent_process(&home, &files, &exe, false, &paths.state_dir).is_err());
        assert!(!agent_process_link(&home).exists());
        assert_eq!(fs::read(&files[0]).unwrap(), b"ok\n");
    }
}
