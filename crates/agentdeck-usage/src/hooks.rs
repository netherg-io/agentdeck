//! Hook entry points shared by `agentdeck usage hook` / `usage hooks plan`.
//!
//! The new CLI reads stdin and writes snapshots through this module, so hooks
//! land in the state dir the daemon reads (the process-wide override from
//! [`crate::util`]). Installing or replacing hooks in real settings is NOT
//! done here: [`plan`] only reports current vs desired commands for the owner
//! to apply.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Serialize;

use crate::config::AccountConfig;

/// Claude status-line hook: pure formatting of Claude's stdin JSON, no writes.
pub fn run_claude_hook(account: &str) -> Result<()> {
    crate::util::validate_id(account)?;
    println!(
        "{}",
        crate::providers::claude::status_line_from_payload(&read_stdin()?)?
    );
    Ok(())
}

/// Agy status-line hook: stores the quota snapshot, then prints the line.
pub fn run_agy_hook(account: &str) -> Result<()> {
    crate::util::validate_id(account)?;
    println!(
        "{}",
        crate::providers::antigravity::status_line_from_payload(account, &read_stdin()?)?
    );
    Ok(())
}

fn read_stdin() -> Result<String> {
    let mut raw = String::new();
    std::io::stdin().read_to_string(&mut raw)?;
    Ok(raw)
}

/// Desired `statusLine.command` for a Claude account: the agentdeck binary.
pub fn claude_hook_command(exe: &Path, account: &str) -> String {
    format!(
        "{} usage hook claude --account {}",
        crate::util::shell_single_quote(&exe.to_string_lossy()),
        crate::util::shell_single_quote(account)
    )
}

/// Desired agy hook command: the `exec` line inside the wrapper script.
pub fn agy_hook_command(exe: &Path, account: &str) -> String {
    format!(
        "{} usage hook agy --account {}",
        crate::util::shell_single_quote(&exe.to_string_lossy()),
        crate::util::shell_single_quote(account)
    )
}

/// Hook script location. Unchanged from the old binary so existing
/// `/statusline` installs keep working; relocation is a later owner step.
pub fn agy_hook_script_path(config_base: &Path, account: &str) -> PathBuf {
    config_base
        .join("ai-usage")
        .join("hooks")
        .join(format!("agy-{account}.sh"))
}

/// Current `statusLine.command` in a Claude config dir, if one is set.
pub fn current_claude_command(config_dir: &Path) -> Option<String> {
    let raw =
        std::fs::read_to_string(crate::util::expand_home(config_dir).join("settings.json")).ok()?;
    serde_json::from_str::<serde_json::Value>(&raw)
        .ok()?
        .pointer("/statusLine/command")?
        .as_str()
        .map(str::to_owned)
}

/// Current agy hook command: the `exec` line of the wrapper script, if any.
pub fn current_agy_command(config_base: &Path, account: &str) -> Option<String> {
    let raw = std::fs::read_to_string(agy_hook_script_path(config_base, account)).ok()?;
    if let Some(line) = raw
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("exec "))
    {
        return Some(line.to_owned());
    }
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

#[derive(Debug, Clone, Serialize)]
pub struct HookPlanEntry {
    pub id: String,
    pub kind: &'static str,
    pub location: PathBuf,
    pub current: Option<String>,
    pub desired: String,
    pub up_to_date: bool,
}

/// Per Claude/agy account: current vs desired hook command. Read-only.
pub fn plan(exe: &Path, config_base: &Path, accounts: &[AccountConfig]) -> Vec<HookPlanEntry> {
    let mut out = Vec::new();
    for account in accounts {
        match account {
            AccountConfig::Claude { id, config_dir, .. } => {
                let desired = claude_hook_command(exe, id);
                let current = current_claude_command(config_dir);
                out.push(HookPlanEntry {
                    id: id.clone(),
                    kind: "claude",
                    location: crate::util::expand_home(config_dir).join("settings.json"),
                    up_to_date: current.as_deref() == Some(desired.as_str()),
                    current,
                    desired,
                });
            }
            AccountConfig::Antigravity { id, .. } => {
                let desired = agy_hook_command(exe, id);
                let current = current_agy_command(config_base, id);
                out.push(HookPlanEntry {
                    id: id.clone(),
                    kind: "antigravity",
                    location: agy_hook_script_path(config_base, id),
                    up_to_date: current.as_deref() == Some(desired.as_str()),
                    current,
                    desired,
                });
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agentdeck-hooks-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn claude(id: &str, dir: &Path) -> AccountConfig {
        AccountConfig::Claude {
            id: id.to_owned(),
            name: id.to_owned(),
            instance: None,
            config_dir: dir.to_path_buf(),
            plan: None,
        }
    }

    #[test]
    fn reports_current_and_desired_hook_commands() {
        let dir = temp_dir("plan");
        let exe = Path::new("/usr/bin/agentdeck");
        // Claude settings with the OLD binary's hook command installed.
        let claude_dir = dir.join("claude");
        fs::create_dir_all(&claude_dir).unwrap();
        fs::write(
            claude_dir.join("settings.json"),
            r#"{"statusLine": {"type": "command", "command": "'/usr/bin/ai-usage' claude-hook --account 'claude-main'", "padding": 0}}"#,
        )
        .unwrap();
        // Agy wrapper installed by the old binary (legacy body, byte-checked).
        let config_base = dir.join("config");
        let legacy_body = "#!/bin/sh\nexec '/usr/bin/ai-usage' agy-hook --account 'agy-main'\n";
        fs::create_dir_all(config_base.join("ai-usage/hooks")).unwrap();
        fs::write(agy_hook_script_path(&config_base, "agy-main"), legacy_body).unwrap();
        assert!(legacy_body.contains("agy-hook --account"));

        let accounts = vec![
            claude("claude-main", &claude_dir),
            AccountConfig::Antigravity {
                id: "agy-main".to_owned(),
                name: "Agy".to_owned(),
                instance: None,
                token_file: None,
            },
        ];
        let entries = plan(exe, &config_base, &accounts);
        assert_eq!(entries.len(), 2);

        let claude = &entries[0];
        assert_eq!(
            claude.current.as_deref(),
            Some("'/usr/bin/ai-usage' claude-hook --account 'claude-main'")
        );
        assert_eq!(
            claude.desired,
            "'/usr/bin/agentdeck' usage hook claude --account 'claude-main'"
        );
        assert!(!claude.up_to_date);

        let agy = &entries[1];
        assert_eq!(
            agy.current.as_deref(),
            Some("'/usr/bin/ai-usage' agy-hook --account 'agy-main'")
        );
        assert!(agy.desired.contains("usage hook agy --account"));
        assert!(!agy.up_to_date);

        // Non-hook kinds are skipped.
        let codex = AccountConfig::Codex {
            id: "codex-main".to_owned(),
            name: "Codex".to_owned(),
            instance: None,
            codex_home: PathBuf::from("/fake/.codex"),
            command: "codex".to_owned(),
            limit_id: "codex".to_owned(),
        };
        assert!(plan(exe, &config_base, &[codex]).is_empty());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_hooks_plan_as_not_installed() {
        let dir = temp_dir("missing");
        let entries = plan(
            Path::new("/usr/bin/agentdeck"),
            &dir,
            &[claude("c", &dir.join("claude"))],
        );
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].current, None);
        assert!(!entries[0].up_to_date);
        fs::remove_dir_all(&dir).unwrap();
    }
}
