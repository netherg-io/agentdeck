//! `agentdeck t3` commands: read-only status, validated sync, registry import,
//! and the false→observe→true re-enable toggle for failed instances.

use agentdeck_core::{Paths, t3};
use clap::Subcommand;

#[derive(Subcommand)]
pub enum T3Cmd {
    /// Read-only per-instance status: registry link, driver, cache health, T3 thread count.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Merge registry instances into T3 `providerInstances` (validated, atomic, verified).
    Sync {
        #[arg(long)]
        dry_run: bool,
    },
    /// Add T3 claudeAgent/codex/opencode/antigravity instances missing from the
    /// registry into it, with a `[[usage]]` entry for claude/codex/antigravity.
    Import {
        #[arg(long)]
        dry_run: bool,
    },
    /// Re-enable a failed instance: toggle enabled false→true in settings,
    /// observing a fresh disabled cache result before writing true, then wait
    /// for a fresh probe. Exits 0 = recovered, 1 = failed again, 2 = timeout.
    Reenable {
        id: String,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        force: bool,
        #[arg(long, default_value_t = 30)]
        timeout_secs: u64,
    },
}

pub fn run_t3(paths: &Paths, cmd: T3Cmd) -> anyhow::Result<()> {
    let root = t3::t3_root()?;
    match cmd {
        T3Cmd::Status { json } => {
            let rows = t3::t3_status(paths, &root)?;
            if json {
                let out: Vec<_> = rows
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "id": r.id,
                            "driver": r.driver,
                            "in_registry": r.in_registry,
                            "enabled": r.enabled,
                            "cache": r.cache.as_ref().map(|c| serde_json::json!({
                                "status": c.status, "checkedAt": c.checked_at,
                            })),
                            "threads": r.threads,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else {
                for r in &rows {
                    let cache = r.cache.as_ref().map_or("n/a".to_string(), |c| {
                        format!("{} @ {}", c.status, c.checked_at.as_deref().unwrap_or("?"))
                    });
                    println!(
                        "{} driver={} registry={} enabled={} cache=[{}] threads={}",
                        r.id,
                        r.driver,
                        if r.in_registry { "yes" } else { "no" },
                        r.enabled,
                        cache,
                        r.threads.map_or("n/a".to_string(), |n| n.to_string())
                    );
                }
            }
        }
        T3Cmd::Sync { dry_run } => {
            let entries = t3::sync_t3(paths, &root, dry_run)?;
            if dry_run {
                for e in &entries {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "id": e.id,
                            "action": e.action,
                            "before": e.before.as_ref().map(t3::redact),
                            "after": t3::redact(&e.after),
                        }))?
                    );
                }
                println!("dry-run: settings.json unchanged");
            } else {
                for e in &entries {
                    println!("{}: {}", e.id, e.action);
                }
            }
        }
        T3Cmd::Import { dry_run } => {
            let found = t3::import_t3(paths, &root, dry_run)?;
            for f in &found {
                let usage = f
                    .usage_id
                    .as_ref()
                    .map_or(String::new(), |u| format!(" usage={u}"));
                println!("{} {} {} t3=true{usage}", f.id, f.harness.as_str(), f.home);
            }
            if found.is_empty() {
                println!("nothing to import");
            } else if dry_run {
                println!("dry-run: registry unchanged");
            }
        }
        T3Cmd::Reenable {
            id,
            dry_run,
            force,
            timeout_secs,
        } => {
            let report = t3::reenable(
                paths,
                &root,
                &id,
                &t3::ReenableOpts {
                    timeout_secs,
                    force,
                    dry_run,
                },
            )?;
            for e in &report.events {
                println!("{e}");
            }
            println!("{}", report.final_line);
            match report.outcome {
                t3::ReenableOutcome::Recovered | t3::ReenableOutcome::DryRun => {}
                t3::ReenableOutcome::FailedAgain => std::process::exit(1),
                t3::ReenableOutcome::Timeout => std::process::exit(2),
            }
        }
    }
    Ok(())
}
