use agentdeck_core::Paths;
use agentdeck_usage::daemon;
use clap::Args;

use crate::sync_watch;

#[derive(Debug, Args)]
pub struct DaemonArgs {
    /// Also write the old ai-usage state paths (the router and the current
    /// applet still read them). Default on; the migration release removes it.
    #[arg(long, action = clap::ArgAction::Set, default_value_t = true)]
    pub compat_write_ai_usage: bool,
    /// Run one cycle without writing anything.
    #[arg(long)]
    pub dry_run: bool,
}

pub async fn run(paths: &Paths, args: DaemonArgs) -> anyhow::Result<()> {
    let reg = agentdeck_core::Registry::load(paths)?;
    let accounts = agentdeck_usage::config::accounts_from_registry(&reg.usage)?;
    let defaults = agentdeck_usage::config::Config::default();
    let mut dpaths =
        daemon::DaemonPaths::for_agentdeck(&paths.state_dir, &daemon::agentdeck_runtime_dir());
    if !args.compat_write_ai_usage {
        dpaths.compat_canonical.clear();
        dpaths.compat_runtime.clear();
    }
    if args.dry_run {
        let state = daemon::run_once(&dpaths, &accounts, defaults.stale_seconds, true).await?;
        println!("{}", serde_json::to_string_pretty(&state)?);
        println!("dry-run: state unchanged");
        return Ok(());
    }
    // Background sync watches beside quota polling; its failure is logged
    // inside the task and never stops the poller.
    let sync_task = tokio::spawn(sync_watch::run(paths.clone()));
    // Sources are re-read every cycle so GUI/CLI edits apply without a restart;
    // a broken registry keeps the last good list instead of stopping polling.
    let mut accounts = accounts;
    let result = daemon::run_daemon(&dpaths, daemon::old_service_active, || {
        match agentdeck_core::Registry::load(paths)
            .and_then(|reg| agentdeck_usage::config::accounts_from_registry(&reg.usage))
        {
            Ok(fresh) => accounts = fresh,
            Err(e) => eprintln!("usage sources not reloaded, keeping previous: {e:#}"),
        }
        Ok((
            accounts.clone(),
            defaults.refresh_seconds,
            defaults.stale_seconds,
        ))
    })
    .await;
    sync_task.abort();
    result
}
