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
pub const PLUGIN_MCP_SERVER: &str = "mcp-server";
pub const PLUGINS: [&str; 5] = [
    PLUGIN_AGENT_PROCESS,
    PLUGIN_AI_USAGE,
    PLUGIN_ROUTER,
    PLUGIN_IMAGEGEN,
    PLUGIN_MCP_SERVER,
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
            "unknown plugin {name:?}: expected agent-process, ai-usage, router, imagegen or mcp-server"
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
        (
            PLUGIN_MCP_SERVER,
            plugin_enabled(reg, PLUGIN_MCP_SERVER).then_some(crate::manage::MCP_BLOCK),
        ),
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

/// Enable = install the daemon unit (never start it) and remove obsolete
/// capture hooks once written by earlier setups. Disable = uninstall the unit
/// (refuses while active) and remove those obsolete hooks. Other plugin data
/// is kept either way.
pub fn plan_enable_ai_usage(paths: &Paths, home: &Path) -> anyhow::Result<Vec<Change>> {
    let mut changes: Vec<Change> = crate::service::plan_install(paths)?.into_iter().collect();
    changes.extend(plan_cleanup_legacy_hooks(paths, home)?);
    Ok(changes)
}

pub fn apply_enable_ai_usage(changes: &[Change]) -> anyhow::Result<()> {
    crate::service::apply_install(changes)
}

pub fn plan_disable_ai_usage(paths: &Paths, home: &Path) -> anyhow::Result<Vec<Change>> {
    let mut changes: Vec<Change> = crate::service::plan_uninstall()?.into_iter().collect();
    changes.extend(plan_cleanup_legacy_hooks(paths, home)?);
    Ok(changes)
}

pub fn apply_disable_ai_usage(changes: &[Change]) -> anyhow::Result<()> {
    crate::service::apply_uninstall(changes)
}

fn split_shell_words(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_token = false;
    let mut quote: Option<char> = None;
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) if c == q => quote = None,
            Some('"') if c == '\\' => {
                if let Some(next) = chars.next() {
                    cur.push(next);
                }
            }
            Some(_) => cur.push(c),
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                in_token = true;
            }
            None if c.is_whitespace() => {
                if in_token {
                    out.push(std::mem::take(&mut cur));
                    in_token = false;
                }
            }
            None if c == '\\' => {
                in_token = true;
                if let Some(next) = chars.next() {
                    cur.push(next);
                }
            }
            None => {
                in_token = true;
                cur.push(c);
            }
        }
    }
    if in_token {
        out.push(cur);
    }
    out
}

fn strip_exec(words: &[String]) -> &[String] {
    match words.first().map(String::as_str) {
        Some("exec") => &words[1..],
        _ => words,
    }
}

fn exe_is(word: &str, names: &[&str]) -> bool {
    Path::new(word)
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| names.contains(&n))
}

fn is_legacy_claude_hook_command(cmd: &str) -> bool {
    let words = split_shell_words(cmd);
    let words = strip_exec(&words);
    (words.len() >= 2 && exe_is(&words[0], &["ai-usage", "agentdeck"]) && words[1] == "claude-hook")
        || (words.len() >= 4
            && exe_is(&words[0], &["agentdeck"])
            && words[1] == "usage"
            && words[2] == "hook"
            && words[3] == "claude")
}

fn is_legacy_agy_hook_command(cmd: &str) -> bool {
    let words = split_shell_words(cmd);
    let words = strip_exec(&words);
    (words.len() >= 2 && exe_is(&words[0], &["ai-usage", "agentdeck"]) && words[1] == "agy-hook")
        || (words.len() >= 4
            && exe_is(&words[0], &["agentdeck"])
            && words[1] == "usage"
            && words[2] == "hook"
            && words[3] == "agy")
}

fn is_legacy_agy_script(bytes: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    if !text.starts_with("#!/bin/sh") {
        return false;
    }
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    lines.len() == 1 && is_legacy_agy_hook_command(lines[0])
}

fn push_unique_path(out: &mut Vec<PathBuf>, seen: &mut Vec<PathBuf>, path: PathBuf) {
    let key = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
    if !seen.contains(&key) {
        seen.push(key);
        out.push(path);
    }
}

fn claude_settings_candidates(home: &Path, reg: &Registry) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut seen = Vec::new();
    push_unique_path(&mut out, &mut seen, home.join(".claude/settings.json"));
    if let Ok(entries) = fs::read_dir(home) {
        let mut extra: Vec<PathBuf> = entries
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(".claude-"))
            .map(|e| e.path().join("settings.json"))
            .collect();
        extra.sort();
        for p in extra {
            push_unique_path(&mut out, &mut seen, p);
        }
    }
    for inst in &reg.instances {
        if inst.harness == crate::Harness::Claude {
            let dir = crate::sync::expand_tilde(&inst.home, home);
            push_unique_path(&mut out, &mut seen, dir.join("settings.json"));
        }
    }
    for u in &reg.usage {
        if u.kind == crate::usage::UsageKind::Claude
            && let Some(dir) = u.extra.get("config_dir").and_then(|v| v.as_str())
        {
            let dir = crate::sync::expand_tilde(dir, home);
            push_unique_path(&mut out, &mut seen, dir.join("settings.json"));
        }
    }
    out
}

fn agy_hooks_dirs(home: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut seen = Vec::new();
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        push_unique_path(
            &mut out,
            &mut seen,
            PathBuf::from(xdg).join("ai-usage/hooks"),
        );
    }
    push_unique_path(&mut out, &mut seen, home.join(".config/ai-usage/hooks"));
    out
}

fn agy_settings_candidates(home: &Path, reg: &Registry) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut seen = Vec::new();
    push_unique_path(
        &mut out,
        &mut seen,
        home.join(".gemini/antigravity-cli/settings.json"),
    );
    for inst in &reg.instances {
        if inst.harness == crate::Harness::Agy {
            let dir = crate::sync::expand_tilde(&inst.home, home);
            let gemini = if dir.file_name().and_then(|n| n.to_str()) == Some(".gemini") {
                dir
            } else {
                dir.join(".gemini")
            };
            push_unique_path(
                &mut out,
                &mut seen,
                gemini.join("antigravity-cli/settings.json"),
            );
        }
    }
    out
}

fn settings_backup_label(path: &Path, prefix: &str) -> String {
    let parent = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .map(|s| s.trim_start_matches('.'))
        .filter(|s| !s.is_empty())
        .unwrap_or("profile");
    format!("{prefix}-{parent}-settings.json")
}

fn plan_strip_status_line(
    path: PathBuf,
    state_dir: &Path,
    backup_label: &str,
    matches_cmd: impl FnOnce(&str) -> bool,
) -> anyhow::Result<Option<Change>> {
    let old = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut value: serde_json::Value = serde_json::from_slice(&old)
        .map_err(|e| anyhow::anyhow!("{} is not valid JSON: {e}", path.display()))?;
    let Some(root) = value.as_object_mut() else {
        return Ok(None);
    };
    let Some(cmd) = root
        .get("statusLine")
        .and_then(|s| s.get("command"))
        .and_then(|c| c.as_str())
    else {
        return Ok(None);
    };
    if !matches_cmd(cmd) {
        return Ok(None);
    }
    root.remove("statusLine");
    let new = (serde_json::to_string_pretty(&value)? + "\n").into_bytes();
    if new == old {
        return Ok(None);
    }
    Ok(Some(Change::WriteFile {
        path,
        contents: new,
        old: Some(old),
        backup: Some(backup_dest(state_dir, backup_label)),
    }))
}

fn plan_cleanup_legacy_hooks(paths: &Paths, home: &Path) -> anyhow::Result<Vec<Change>> {
    let reg = Registry::load(paths).unwrap_or_default();
    let mut changes = Vec::new();

    for path in claude_settings_candidates(home, &reg) {
        let label = settings_backup_label(&path, "claude");
        if let Some(c) = plan_strip_status_line(
            path,
            &paths.state_dir,
            &label,
            is_legacy_claude_hook_command,
        )? {
            changes.push(c);
        }
    }

    let mut removed_scripts = Vec::new();
    for hooks_dir in agy_hooks_dirs(home) {
        let Ok(rd) = fs::read_dir(&hooks_dir) else {
            continue;
        };
        let mut scripts: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("agy-") && n.ends_with(".sh"))
                    && fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_file())
            })
            .collect();
        scripts.sort();
        for path in scripts {
            if let Ok(bytes) = fs::read(&path)
                && is_legacy_agy_script(&bytes)
            {
                if let Ok(canon) = fs::canonicalize(&path) {
                    removed_scripts.push(canon);
                }
                removed_scripts.push(path.clone());
                changes.push(Change::Remove { path });
            }
        }
    }

    for path in agy_settings_candidates(home, &reg) {
        let label = settings_backup_label(&path, "agy");
        if let Some(c) = plan_strip_status_line(path, &paths.state_dir, &label, |cmd| {
            if is_legacy_agy_hook_command(cmd) {
                return true;
            }
            let cmd_path = crate::sync::expand_tilde(cmd.trim(), home);
            let canon = fs::canonicalize(&cmd_path).ok();
            removed_scripts.contains(&cmd_path)
                || canon.as_ref().is_some_and(|c| removed_scripts.contains(c))
        })? {
            changes.push(c);
        }
    }

    Ok(changes)
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
            format!("transparent model: {}", ig.transparent_model),
            format!("key: {}", if key { "found" } else { "missing" }),
        ],
    });
    out.push(PluginStatus {
        name: PLUGIN_MCP_SERVER,
        enabled: plugin_enabled(&reg, PLUGIN_MCP_SERVER),
        available: true,
        detail: match crate::manage::plan_register(paths, home, exe, true) {
            Ok((changes, notes)) => {
                let mut d = vec![format!(
                    "registration: {}",
                    if changes.is_empty() {
                        "in every harness"
                    } else {
                        "incomplete"
                    }
                )];
                d.extend(notes);
                d
            }
            Err(e) => vec![format!("registration: {e:#}")],
        },
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
        PLUGIN_AI_USAGE => plan_enable_ai_usage(paths, home)?,
        PLUGIN_ROUTER => plan_enable_router(paths, home, files, exe, replace)?,
        // Only the flag and its skill (synced after apply).
        PLUGIN_IMAGEGEN => Vec::new(),
        PLUGIN_MCP_SERVER => {
            let mut c = crate::manage::plan_register(paths, home, exe, true)?.0;
            c.extend(plan_marker_files(
                files,
                PLUGIN_MCP_SERVER,
                Some(crate::manage::MCP_BLOCK),
                &paths.state_dir,
            )?);
            c
        }
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
        PLUGIN_AI_USAGE => plan_disable_ai_usage(paths, home)?,
        PLUGIN_ROUTER => plan_disable_router(paths, home, files, exe)?,
        PLUGIN_IMAGEGEN => Vec::new(),
        PLUGIN_MCP_SERVER => {
            let mut c = crate::manage::plan_register(paths, home, exe, false)?.0;
            c.extend(plan_marker_files(
                files,
                PLUGIN_MCP_SERVER,
                None,
                &paths.state_dir,
            )?);
            c
        }
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
        let touches_unit = crate::service::unit_path().ok().is_some_and(|unit| {
            file_changes.iter().any(|c| match c {
                Change::WriteFile { path, .. } | Change::Remove { path } => *path == unit,
                _ => false,
            })
        });
        if touches_unit {
            if enable {
                crate::service::apply_install(&file_changes)?;
            } else {
                crate::service::apply_uninstall(&file_changes)?;
            }
        } else {
            apply_changes(&file_changes)?;
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
        let _env = test_env_lock();
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

    /// Every file, link and dir under `root` except agentdeck's own config
    /// and state (plugin data is kept by design).
    fn tree(root: &Path, skip: &[PathBuf]) -> BTreeMap<PathBuf, String> {
        fn walk(dir: &Path, root: &Path, skip: &[PathBuf], out: &mut BTreeMap<PathBuf, String>) {
            let Ok(rd) = fs::read_dir(dir) else { return };
            for e in rd.flatten() {
                let p = e.path();
                if skip.iter().any(|s| p.starts_with(s)) {
                    continue;
                }
                let rel = p.strip_prefix(root).unwrap().to_path_buf();
                let m = fs::symlink_metadata(&p).unwrap();
                if m.file_type().is_symlink() {
                    out.insert(rel, format!("-> {}", fs::read_link(&p).unwrap().display()));
                } else if m.is_dir() {
                    out.insert(rel, "dir".into());
                    walk(&p, root, skip, out);
                } else {
                    out.insert(
                        rel,
                        String::from_utf8_lossy(&fs::read(&p).unwrap()).into_owned(),
                    );
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(root, root, skip, &mut out);
        // Parents of agentdeck's own dirs; their other contents still count.
        for parent in skip.iter().filter_map(|s| s.parent()) {
            if let Ok(rel) = parent.strip_prefix(root) {
                out.remove(rel);
            }
        }
        out
    }

    /// Enable then disable each plugin the way the CLI does (plan, apply,
    /// plugin-skill sync): everything outside agentdeck's data must be byte
    /// for byte what it was, as if the plugin had never been there.
    #[test]
    fn every_plugin_leaves_no_trace_after_disable() {
        let _env = test_env_lock();
        let real_home = env::var_os("HOME");
        for name in PLUGINS {
            let (g, home, paths, files, exe) = setup(&format!("trace-{name}"));
            let t3 = g.0.join("t3");
            fs::create_dir_all(&t3).unwrap();
            unsafe {
                env::set_var("AGENTDECK_T3_DIR", &t3);
                // XDG dirs sit beside the fake home; only dirs under $HOME
                // are tracked, and the created-dirs log lives in our state.
                env::set_var("HOME", &g.0);
                env::set_var("AGENTDECK_STATE_DIR", &paths.state_dir);
            }
            let w = |rel: &str, body: &str| {
                let p = home.join(rel);
                fs::create_dir_all(p.parent().unwrap()).unwrap();
                fs::write(p, body).unwrap();
            };
            w("AGENTS.md", "# mine\n\nhand-written rules\n");
            w(".claude/CLAUDE.md", "claude only\n");
            w(
                ".claude/settings.json",
                "{\n  \"model\": \"opus\",\n  \"hooks\": {\n    \"PostToolUse\": [\n      {\n        \"matcher\": \"Bash\",\n        \"hooks\": [{ \"type\": \"command\", \"command\": \"true\" }]\n      }\n    ]\n  }\n}\n",
            );
            w(".claude-second/settings.json", "{\"theme\":\"dark\"}");
            w(".codex/config.toml", "model = \"gpt\"\n");
            w(
                ".gemini/config/config.json",
                "{\n  \"plugins\": {\n    \"Figma\": {\n      \"enabled\": true\n    }\n  }\n}\n",
            );
            w(
                ".config/opencode/opencode.json",
                "{\n  \"$schema\": \"https://opencode.ai/config.json\",\n  \"plugin\": [\"x\"]\n}\n",
            );
            w(
                ".claude.json",
                "{\n  \"numStartups\": 3,\n  \"mcpServers\": {\n    \"shared\": {\n      \"type\": \"http\",\n      \"url\": \"http://127.0.0.1:8080/mcp\"\n    }\n  }\n}\n",
            );
            w(
                ".gemini/config/mcp_config.json",
                "{\"mcpServers\":{\"shared\":{\"url\":\"http://x\"}}}",
            );
            for d in [
                ".claude/skills",
                ".agents/skills",
                ".config/opencode/skills",
                ".gemini/skills",
                ".local/bin",
            ] {
                fs::create_dir_all(home.join(d)).unwrap();
            }
            let skip = [
                paths.config_dir.clone(),
                paths.state_dir.clone(),
                g.0.join("calls.log"),
            ];
            let before = tree(&g.0, &skip);
            let on = plan_enable(&paths, &home, &files, &exe, name, false).unwrap();
            apply_all(&paths, name, true, &on).unwrap();
            sync_plugin_skills(&paths, &home).unwrap();
            assert_ne!(tree(&g.0, &skip), before, "{name}: enable changed nothing");
            let off = plan_disable(&paths, &home, &files, &exe, name).unwrap();
            apply_all(&paths, name, false, &off).unwrap();
            sync_plugin_skills(&paths, &home).unwrap();
            let after = tree(&g.0, &skip);
            let diff: Vec<String> = before
                .keys()
                .chain(after.keys())
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .filter(|k| before.get(*k) != after.get(*k))
                .map(|k| format!("{}: {:?} -> {:?}", k.display(), before.get(k), after.get(k)))
                .collect();
            assert!(diff.is_empty(), "{name} left traces:\n{}", diff.join("\n"));
            unsafe {
                env::remove_var("AGENTDECK_T3_DIR");
            }
        }
        unsafe {
            env::remove_var("AGENTDECK_STATE_DIR");
            match real_home {
                Some(h) => env::set_var("HOME", h),
                None => env::remove_var("HOME"),
            }
        }
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
        let (_g, home, paths, _files, _exe) = setup("aiusage");
        let log = std::env::var_os("FAKE_SYSTEMCTL_LOG")
            .map(PathBuf::from)
            .unwrap();

        // Enable plans a unit write; applying reloads but never starts.
        let changes = plan_enable_ai_usage(&paths, &home).unwrap();
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
        assert!(plan_disable_ai_usage(&paths, &home).is_err());
        assert!(unit_path().unwrap().exists());

        unsafe {
            std::env::set_var("FAKE_SYSTEMCTL_ACTIVE", "inactive");
        }
        let changes = plan_disable_ai_usage(&paths, &home).unwrap();
        assert_eq!(changes.len(), 1);
        apply_disable_ai_usage(&changes).unwrap();
        assert!(!unit_path().unwrap().exists());
    }

    #[test]
    fn ai_usage_layout_pass_removes_obsolete_hooks_idempotently() {
        let _env = test_env_lock();
        let (g, home, paths, files, exe) = setup("aiusage-cleanup");

        // Plant existing enabled plugin + unit so re-running enable tests the upgrade pass.
        let init = plan_enable(&paths, &home, &files, &exe, PLUGIN_AI_USAGE, false).unwrap();
        apply_all(&paths, PLUGIN_AI_USAGE, true, &init).unwrap();

        let claude_main = home.join(".claude/settings.json");
        let claude_second = home.join(".claude-second/settings.json");
        let claude_custom = home.join(".claude-custom/settings.json");
        let claude_null = home.join(".claude-null/settings.json");
        let agy_main_script = g.0.join("config/ai-usage/hooks/agy-agy-main.sh");
        let agy_old_script = home.join(".config/ai-usage/hooks/agy-legacy.sh");
        let agy_custom_script = g.0.join("config/ai-usage/hooks/agy-custom.sh");
        let agy_settings = home.join(".gemini/antigravity-cli/settings.json");

        for p in [
            &claude_main,
            &claude_second,
            &claude_custom,
            &claude_null,
            &agy_main_script,
            &agy_old_script,
            &agy_custom_script,
            &agy_settings,
        ] {
            fs::create_dir_all(p.parent().unwrap()).unwrap();
        }

        fs::write(
            &claude_main,
            "{\n  \"theme\": \"dark\",\n  \"statusLine\": {\n    \"type\": \"command\",\n    \"command\": \"'/opt/bin/agentdeck' usage hook claude --account 'claude-main'\",\n    \"padding\": 0\n  },\n  \"hooks\": {}\n}\n",
        )
        .unwrap();
        fs::write(
            &claude_second,
            "{\n  \"model\": \"sonnet\",\n  \"statusLine\": {\n    \"type\": \"command\",\n    \"command\": \"'/usr/local/bin/ai-usage' claude-hook --account 'claude-second'\",\n    \"padding\": 0\n  }\n}\n",
        )
        .unwrap();
        let custom_claude_bytes = "{\n  \"statusLine\": {\n    \"type\": \"command\",\n    \"command\": \"/home/user/ai-usage-scripts/mine.sh\",\n    \"padding\": 0\n  }\n}\n";
        fs::write(&claude_custom, custom_claude_bytes).unwrap();
        let null_claude_bytes = "{\n  \"theme\": \"light\",\n  \"statusLine\": null\n}\n";
        fs::write(&claude_null, null_claude_bytes).unwrap();

        fs::write(
            &agy_main_script,
            "#!/bin/sh\nexec '/opt/bin/agentdeck' usage hook agy --account 'agy-main'\n",
        )
        .unwrap();
        fs::write(
            &agy_old_script,
            "#!/bin/sh\nexec '/usr/local/bin/ai-usage' agy-hook --account 'agy-legacy'\n",
        )
        .unwrap();
        let custom_agy_script_bytes = "#!/bin/sh\necho custom-status\n";
        fs::write(&agy_custom_script, custom_agy_script_bytes).unwrap();
        fs::write(
            &agy_settings,
            format!(
                "{{\n  \"otherKey\": true,\n  \"statusLine\": {{\n    \"type\": \"command\",\n    \"command\": \"{}\",\n    \"enabled\": true,\n    \"stack_with_default\": true\n  }}\n}}\n",
                agy_main_script.display()
            ),
        )
        .unwrap();

        let changes = plan_enable(&paths, &home, &files, &exe, PLUGIN_AI_USAGE, false).unwrap();
        assert!(!changes.is_empty());
        apply_all(&paths, PLUGIN_AI_USAGE, true, &changes).unwrap();

        assert_eq!(
            fs::read_to_string(&claude_main).unwrap(),
            "{\n  \"theme\": \"dark\",\n  \"hooks\": {}\n}\n"
        );
        assert_eq!(
            fs::read_to_string(&claude_second).unwrap(),
            "{\n  \"model\": \"sonnet\"\n}\n"
        );
        assert!(!agy_main_script.exists());
        assert!(!agy_old_script.exists());
        assert_eq!(
            fs::read_to_string(&agy_settings).unwrap(),
            "{\n  \"otherKey\": true\n}\n"
        );

        // Foreign / user-written files stay byte-identical.
        assert_eq!(
            fs::read_to_string(&claude_custom).unwrap(),
            custom_claude_bytes
        );
        assert_eq!(fs::read_to_string(&claude_null).unwrap(), null_claude_bytes);
        assert_eq!(
            fs::read_to_string(&agy_custom_script).unwrap(),
            custom_agy_script_bytes
        );

        // Backups exist for modified settings files.
        assert!(fs::read_dir(paths.backup_dir()).unwrap().count() >= 3);

        // Idempotent: second run produces no changes.
        assert!(
            plan_enable(&paths, &home, &files, &exe, PLUGIN_AI_USAGE, false)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn ai_usage_disable_cleans_obsolete_hooks_and_keeps_foreign_agy_status_line() {
        let _env = test_env_lock();
        let (_g, home, paths, files, exe) = setup("aiusage-disable-cleanup");

        let claude_main = home.join(".claude/settings.json");
        let agy_settings = home.join(".gemini/antigravity-cli/settings.json");
        let custom_agy_script = home.join(".local/bin/my-statusline.sh");
        for p in [&claude_main, &agy_settings, &custom_agy_script] {
            fs::create_dir_all(p.parent().unwrap()).unwrap();
        }
        fs::write(
            &claude_main,
            "{\n  \"statusLine\": {\n    \"type\": \"command\",\n    \"command\": \"/opt/bin/agentdeck usage hook claude --account main\"\n  }\n}\n",
        )
        .unwrap();
        let foreign_agy_bytes = format!(
            "{{\n  \"statusLine\": {{\n    \"type\": \"command\",\n    \"command\": \"{}\"\n  }}\n}}\n",
            custom_agy_script.display()
        );
        fs::write(&agy_settings, &foreign_agy_bytes).unwrap();
        fs::write(&custom_agy_script, "#!/bin/sh\necho ok\n").unwrap();

        let changes = plan_disable(&paths, &home, &files, &exe, PLUGIN_AI_USAGE).unwrap();
        apply_all(&paths, PLUGIN_AI_USAGE, false, &changes).unwrap();

        assert_eq!(fs::read_to_string(&claude_main).unwrap(), "{}\n");
        assert_eq!(
            fs::read_to_string(&agy_settings).unwrap(),
            foreign_agy_bytes
        );
        assert!(custom_agy_script.exists());
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
        // settings.json did not exist before enable, so disable removes it.
        let is_settings = |c: &Change| matches!(c, Change::WriteFile { path, .. } | Change::Remove { path } if path.ends_with("settings.json"));
        let first_hook = changes.iter().position(is_settings).unwrap();
        let first_remove = changes
            .iter()
            .position(|c| matches!(c, Change::Remove { path } if !path.ends_with("settings.json")))
            .unwrap();
        assert!(first_hook < first_remove, "hooks go before files");
        apply_all(&paths, PLUGIN_ROUTER, false, &changes).unwrap();
        assert!(!home.join(".claude/settings.json").exists());
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
