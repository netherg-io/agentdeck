//! `agentdeck wrapper install|remove|list`: per-instance terminal wrappers
//! (`claude@second`) and legacy launcher cleanup.

use std::path::PathBuf;

use agentdeck_core::{Paths, Registry, wrappers};
use clap::Subcommand;

use super::cmd_claude::expand_tilde;

#[derive(Subcommand)]
pub enum WrapperCmd {
    /// Install the terminal wrapper script for an instance.
    Install {
        /// Registry instance id.
        id: String,
        /// Wrapper file name (default `<binary>@<suffix>`, e.g. `claude@second`).
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        dry_run: bool,
    },
    /// Remove the wrapper script and .desktop launcher of an instance.
    Remove {
        /// Registry instance id (matched by marker, so registry membership is
        /// not required).
        id: String,
        #[arg(long)]
        dry_run: bool,
    },
    /// Remove old Agentdeck provider launchers from the application menu.
    RemoveLaunchers {
        #[arg(long)]
        dry_run: bool,
    },
    /// List wrapper/desktop state for every registry instance.
    List,
}

pub fn run(paths: &Paths, cmd: WrapperCmd) -> anyhow::Result<()> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("cannot resolve home: HOME is not set"))?;
    let bin_dir = wrappers::bin_dir(&home);
    let apps_dir = wrappers::apps_dir(&home);
    match cmd {
        WrapperCmd::Install { id, name, dry_run } => {
            let reg = Registry::load(paths)?;
            let inst = reg
                .get(&id)
                .ok_or_else(|| anyhow::anyhow!("unknown instance id {id:?}"))?;
            let home_dir = expand_tilde(&inst.home, &home);
            let changes =
                wrappers::plan_install(inst, &home_dir, &bin_dir, &apps_dir, name.as_deref())?;
            for c in &changes {
                println!("{c}", c = c.describe());
            }
            if dry_run {
                println!("dry-run: wrappers for {id} unchanged");
            } else if changes.is_empty() {
                let label = name.unwrap_or_else(|| {
                    wrappers::resolve_wrapper_name(inst, None)
                        .ok()
                        .flatten()
                        .unwrap_or_default()
                });
                println!("{id}: {label} already installed");
            } else {
                agentdeck_core::apply_changes(&changes)?;
                println!("installed wrappers for {id}");
            }
        }
        WrapperCmd::Remove { id, dry_run } => {
            let changes = wrappers::plan_remove(&id, &bin_dir, &apps_dir)?;
            for c in &changes {
                println!("{c}", c = c.describe());
            }
            if dry_run {
                println!("dry-run: wrappers for {id} unchanged");
            } else {
                agentdeck_core::apply_changes(&changes)?;
                println!("removed wrappers for {id}");
            }
        }
        WrapperCmd::RemoveLaunchers { dry_run } => {
            let changes = wrappers::plan_remove_launchers(&apps_dir)?;
            for c in &changes {
                println!("{}", c.describe());
            }
            if !dry_run {
                agentdeck_core::apply_changes(&changes)?;
            }
        }
        WrapperCmd::List => {
            let instances = Registry::load(paths)?.instances;
            for row in wrappers::status(&instances, &bin_dir, &apps_dir)? {
                match row.name {
                    None => println!("{}: primary account, no wrapper", row.id),
                    Some(name) => println!(
                        "{}: {name}: wrapper {}, desktop {}",
                        row.id,
                        row.wrapper.as_str(),
                        row.desktop.as_str()
                    ),
                }
            }
        }
    }
    Ok(())
}
