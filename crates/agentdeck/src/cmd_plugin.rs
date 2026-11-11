//! `agentdeck plugin list|enable|disable`: built-in plugins (agent-process,
//! ai-usage, router). The router needs T3 present. Every write goes
//! through injectable paths and supports --dry-run; previews print first.

use std::path::PathBuf;

use agentdeck_core::{Paths, Registry, plugins};
use clap::Subcommand;

#[derive(Subcommand)]
pub enum PluginCmd {
    /// List plugins with enabled/available state. Read-only.
    List,
    /// Enable a plugin (files + unit + registry flag).
    Enable {
        /// Plugin name (agent-process, ai-usage).
        name: String,
        /// Back up a foreign file at the launcher path instead of refusing.
        #[arg(long)]
        replace: bool,
        #[arg(long)]
        dry_run: bool,
    },
    /// Disable a plugin (removes its files; data is kept).
    Disable {
        /// Plugin name (agent-process, ai-usage).
        name: String,
        #[arg(long)]
        dry_run: bool,
    },
}

fn home_dir() -> anyhow::Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("cannot resolve home: set HOME"))
}

pub fn run(paths: &Paths, cmd: PluginCmd) -> anyhow::Result<()> {
    match cmd {
        PluginCmd::List => {
            let home = home_dir()?;
            let exe = std::env::current_exe()?;
            for st in plugins::status(paths, &home, &exe) {
                println!(
                    "{} enabled={} available={}",
                    st.name,
                    if st.enabled { "yes" } else { "no" },
                    if st.available { "yes" } else { "no" }
                );
                for line in &st.detail {
                    println!("  {line}");
                }
            }
            Ok(())
        }
        PluginCmd::Enable {
            name,
            replace,
            dry_run,
        } => {
            let home = home_dir()?;
            let exe = std::env::current_exe()?;
            let files = plugins::default_instruction_files(&home);
            let changes = plugins::plan_enable(paths, &home, &files, &exe, &name, replace)?;
            if changes.is_empty() {
                println!("{name}: already enabled");
                return Ok(());
            }
            for c in &changes {
                println!("{}", c.describe());
            }
            if dry_run {
                println!("dry-run: nothing changed");
                return Ok(());
            }
            plugins::apply_all(paths, &name, true, &changes)?;
            println!("enabled {name}");
            Ok(())
        }
        PluginCmd::Disable { name, dry_run } => {
            let home = home_dir()?;
            let exe = std::env::current_exe()?;
            let files = plugins::default_instruction_files(&home);
            let changes = plugins::plan_disable(paths, &home, &files, &exe, &name)?;
            if changes.is_empty() {
                println!("{name}: already disabled");
                return Ok(());
            }
            for c in &changes {
                println!("{}", c.describe());
            }
            if dry_run {
                println!("dry-run: nothing changed");
                return Ok(());
            }
            plugins::apply_all(paths, &name, false, &changes)?;
            println!("disabled {name} (data kept)");
            Ok(())
        }
    }
}

/// Disable every enabled plugin, then drop blocks left by plugins that were
/// disabled by hand. ai-usage goes first: it refuses while the daemon runs,
/// before anything else is touched.
pub fn uninstall(paths: &Paths, dry_run: bool) -> anyhow::Result<()> {
    let home = home_dir()?;
    let exe = std::env::current_exe()?;
    let files = plugins::default_instruction_files(&home);
    let mut reg = Registry::load(paths)?;
    for name in [
        plugins::PLUGIN_AI_USAGE,
        plugins::PLUGIN_ROUTER,
        plugins::PLUGIN_AGENT_PROCESS,
    ] {
        if !plugins::plugin_enabled(&reg, name) {
            continue;
        }
        let changes = plugins::plan_disable(paths, &home, &files, &exe, name)?;
        for c in &changes {
            println!("{}", c.describe());
        }
        if !dry_run {
            plugins::apply_all(paths, name, false, &changes)?;
        }
    }
    reg.plugins.clear();
    let blocks = plugins::plan_managed_blocks(&reg, &home, &exe, &paths.state_dir)?;
    for c in &blocks {
        println!("{}", c.describe());
    }
    if dry_run {
        println!("dry-run: nothing changed");
        return Ok(());
    }
    agentdeck_core::apply_changes(&blocks)?;
    println!("agentdeck integrations removed; delete the binary to finish");
    Ok(())
}
