use agentdeck_core::Paths;
use agentdeck_core::usage::{self, UsageEntry, UsageKind};
use agentdeck_usage::daemon;
use clap::{Subcommand, ValueEnum};

#[derive(Subcommand)]
pub enum UsageCmd {
    /// List quota accounts derived from [[usage]] entries (secret names/paths only, never values).
    Accounts {
        #[arg(long)]
        json: bool,
    },
    /// Add a [[usage]] source.
    Add {
        #[arg(long)]
        id: String,
        #[arg(long, value_enum)]
        kind: UsageKindArg,
        #[arg(long)]
        instance: Option<String>,
        #[arg(long)]
        display: Option<String>,
        #[arg(long)]
        api_key_env: Option<String>,
        #[arg(long)]
        db_file: Option<String>,
        #[arg(long)]
        dry_run: bool,
    },
    /// Remove a [[usage]] source.
    Remove {
        id: String,
        #[arg(long)]
        dry_run: bool,
    },
    /// Run one polling cycle (refuses while the daemon holds the lock).
    Once {
        /// Also write the old ai-usage state paths. Off by default.
        #[arg(long, action = clap::ArgAction::SetTrue)]
        compat_write_ai_usage: bool,
        #[arg(long)]
        dry_run: bool,
    },
    /// Show sources and state freshness. Read-only, no network.
    Doctor {
        #[arg(long)]
        json: bool,
    },
    // shortcut: keeps still-registered `usage hook claude|agy --account ...`
    // commands exiting 0 quietly instead of emitting clap errors on every prompt;
    // drop after installed machines run `plugin enable ai-usage` cleanup (next major bump).
    #[command(hide = true)]
    Hook {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        _args: Vec<String>,
    },
}

#[derive(Clone, Copy, ValueEnum)]
pub enum UsageKindArg {
    Claude,
    Codex,
    Antigravity,
    Deepseek,
    Openrouter,
    Vercel,
    OpencodeGo,
}

impl UsageKindArg {
    fn as_core(self) -> UsageKind {
        match self {
            Self::Claude => UsageKind::Claude,
            Self::Codex => UsageKind::Codex,
            Self::Antigravity => UsageKind::Antigravity,
            Self::Deepseek => UsageKind::Deepseek,
            Self::Openrouter => UsageKind::Openrouter,
            Self::Vercel => UsageKind::Vercel,
            Self::OpencodeGo => UsageKind::OpencodeGo,
        }
    }
}

fn opt(s: Option<String>) -> Option<String> {
    s.filter(|v| !v.is_empty())
}

pub async fn run(paths: &Paths, cmd: UsageCmd) -> anyhow::Result<()> {
    match cmd {
        UsageCmd::Accounts { json } => {
            let reg = agentdeck_core::Registry::load(paths)?;
            let accounts = agentdeck_usage::config::accounts_from_registry(&reg.usage)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&accounts)?);
            } else {
                for a in &accounts {
                    println!("{} {} {}", a.id(), a.provider(), a.name());
                }
            }
        }
        UsageCmd::Add {
            id,
            kind,
            instance,
            display,
            api_key_env,
            db_file,
            dry_run,
        } => {
            let entry = UsageEntry {
                id: id.clone(),
                kind: kind.as_core(),
                instance: opt(instance),
                display: display.unwrap_or_default(),
                api_key_env: opt(api_key_env),
                db_file: opt(db_file),
                extra: Default::default(),
            };
            let changes = usage::add_usage_entry(paths, entry, dry_run)?;
            for c in &changes {
                println!("{c}", c = c.describe());
            }
            if dry_run {
                println!("dry-run: registry unchanged");
            } else {
                println!("added usage {id}");
            }
        }
        UsageCmd::Remove { id, dry_run } => {
            let changes = usage::remove_usage_entry(paths, &id, dry_run)?;
            for c in &changes {
                println!("{c}", c = c.describe());
            }
            if dry_run {
                println!("dry-run: registry unchanged");
            } else {
                println!("removed usage {id}");
            }
        }
        UsageCmd::Once {
            compat_write_ai_usage,
            dry_run,
        } => {
            let reg = agentdeck_core::Registry::load(paths)?;
            let accounts = agentdeck_usage::config::accounts_from_registry(&reg.usage)?;
            let stale = agentdeck_usage::config::Config::default().stale_seconds;
            let mut dpaths = daemon::DaemonPaths::for_agentdeck(
                &paths.state_dir,
                &daemon::agentdeck_runtime_dir(),
            );
            if !compat_write_ai_usage {
                dpaths = dpaths.without_compat();
            }
            let state = daemon::run_once(&dpaths, &accounts, stale, dry_run).await?;
            println!("{}", serde_json::to_string_pretty(&state)?);
            if dry_run {
                println!("dry-run: state unchanged");
            }
        }
        UsageCmd::Doctor { json } => {
            let reg = agentdeck_core::Registry::load(paths)?;
            let accounts = agentdeck_usage::config::accounts_from_registry(&reg.usage)?;
            let dpaths = daemon::DaemonPaths::for_agentdeck(
                &paths.state_dir,
                &daemon::agentdeck_runtime_dir(),
            );
            let report = daemon::doctor(&dpaths, &accounts, daemon::old_service_active);
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print_doctor(&report);
            }
        }
        UsageCmd::Hook { .. } => {
            let _ = std::io::copy(&mut std::io::stdin(), &mut std::io::sink());
        }
    }
    Ok(())
}

fn print_doctor(report: &daemon::DoctorReport) {
    println!("agentdeck usage doctor\n");
    if report.t3_health_refresh_interval_ms == Some(0) {
        println!(
            "T3 background refresh is off (providerHealthRefreshInterval=0; Settings -> Providers -> Health check interval); quotas update only on manual refresh and go stale."
        );
    }
    println!("sources: {}", report.sources.len());
    for s in &report.sources {
        let inst = s
            .instance
            .as_ref()
            .map(|i| format!(" instance={i}"))
            .unwrap_or_default();
        println!("  {} {}{} {}", s.id, s.kind, inst, s.source);
    }
    for f in [&report.canonical]
        .into_iter()
        .chain(report.compat_canonical.iter())
        .chain(report.compat_runtime.iter())
        .chain([&report.runtime])
    {
        match (f.updated_at, f.age_secs) {
            (Some(t), Some(age)) => {
                println!("  {}: updated_at={t} ({age} с ago)", f.path.display())
            }
            _ => println!("  {}: missing", f.path.display()),
        }
    }
    println!(
        "  ai-usage.service: {}",
        if report.old_service_active {
            "active (daemon refuses to start)"
        } else {
            "inactive"
        }
    );
    println!("  lock: {}", if report.lock_held { "held" } else { "free" });
}
