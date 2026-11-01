use agentdeck_core::{AddSpec, Harness, Paths, t3};
use clap::{Parser, Subcommand, ValueEnum};

mod cmd_claude;
mod cmd_codex;
mod cmd_daemon;
mod cmd_harness;
mod cmd_login;
mod cmd_migrate;
mod cmd_opencode;
mod cmd_plugin;
mod cmd_pool;
mod cmd_ps;
mod cmd_router;
mod cmd_service;
mod cmd_shared;
mod cmd_sync;
mod cmd_t3;
mod cmd_usage;
mod cmd_wrapper;
mod sync_watch;

#[derive(Parser)]
#[command(name = "agentdeck", version, about = "Manage harness instances")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Manage registry instances in instances.toml.
    Instance {
        #[command(subcommand)]
        cmd: InstanceCmd,
    },
    /// Manage the [[pool]] model pool in instances.toml.
    Pool {
        #[command(subcommand)]
        cmd: cmd_pool::PoolCmd,
    },
    /// Manage [[usage]] quota sources in instances.toml.
    Usage {
        #[command(subcommand)]
        cmd: cmd_usage::UsageCmd,
    },
    /// Run the usage polling daemon (single writer).
    Daemon(cmd_daemon::DaemonArgs),
    /// Sync and drift-check secondary Claude accounts.
    Claude {
        #[command(subcommand)]
        cmd: cmd_claude::ClaudeCmd,
    },
    /// Background sync: drift status, manual runs, auto-apply flag.
    Sync {
        #[command(subcommand)]
        cmd: cmd_sync::SyncCmd,
    },
    /// Sync, check and fix Codex shadow homes.
    Codex {
        #[command(subcommand)]
        cmd: cmd_codex::CodexCmd,
    },
    /// Show OpenCode accounts and switch the active one.
    Opencode {
        #[command(subcommand)]
        cmd: cmd_opencode::OpencodeCmd,
    },
    /// Detect harness binaries and run their official updaters/installers.
    Harness {
        #[command(subcommand)]
        cmd: cmd_harness::HarnessCmd,
    },
    /// T3 integration: status, sync and import of providerInstances.
    T3 {
        #[command(subcommand)]
        cmd: cmd_t3::T3Cmd,
    },
    /// Copy-only migration from the old ai-usage tree (never moves or deletes).
    Migrate {
        #[command(subcommand)]
        cmd: cmd_migrate::MigrateCmd,
    },
    /// Per-instance terminal wrappers (claude@second) and .desktop launchers.
    Wrapper {
        #[command(subcommand)]
        cmd: cmd_wrapper::WrapperCmd,
    },
    /// Install, remove or inspect the systemd user unit. Never enables/starts.
    Service {
        #[command(subcommand)]
        cmd: cmd_service::ServiceCmd,
    },
    /// Refresh the generated router.json for the router plugin.
    Router {
        #[command(subcommand)]
        cmd: cmd_router::RouterCmd,
    },
    /// Route a delegation target through the router (port of route.mjs),
    /// or run the pre-tool hook (port of hook.mjs: `route hook --harness …`).
    Route {
        /// Passthrough router args: [--source S] [--check] [--catalog]
        /// [--validate-target PROVIDER MODEL] ["<task brief>" | -],
        /// or [hook --harness <claude|codex|agy|opencode|shell>] (hook JSON on stdin).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Enable or disable built-in plugins (agent-process, ai-usage, router).
    Plugin {
        #[command(subcommand)]
        cmd: cmd_plugin::PluginCmd,
    },
    /// Sync shared skills into every harness skill dir.
    Skills {
        #[command(subcommand)]
        cmd: cmd_shared::SkillsCmd,
    },
    /// Link global instruction files to the shared ~/AGENTS.md.
    Instructions {
        #[command(subcommand)]
        cmd: cmd_shared::InstructionsCmd,
    },
    /// Sync the canonical MCP list into secondary accounts and Codex.
    Mcp {
        #[command(subcommand)]
        cmd: cmd_shared::McpCmd,
    },
    /// Live agent-process tree (read-only; stopping stays `agent-process stop`).
    Ps(cmd_ps::PsArgs),
    /// Run the official login CLI for an instance inside a PTY.
    Login {
        /// Registry instance id to log in.
        id: String,
        /// Use device-code authorization (codex only).
        #[arg(long)]
        device_auth: bool,
        /// Open the login URL with xdg-open (otherwise only print it).
        #[arg(long)]
        open: bool,
    },
}

#[derive(Subcommand)]
enum InstanceCmd {
    /// List instances.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Add an instance.
    Add {
        #[arg(long)]
        id: String,
        #[arg(long, value_enum)]
        harness: HarnessArg,
        #[arg(long)]
        home: String,
        #[arg(long)]
        display_name: Option<String>,
        #[arg(long)]
        binary: Option<String>,
        #[arg(long)]
        no_t3: bool,
        #[arg(long)]
        dry_run: bool,
    },
    /// Remove an instance from the registry (the home dir is kept).
    Remove {
        id: String,
        #[arg(long)]
        dry_run: bool,
    },
    /// Change the display name without changing the instance id.
    Rename {
        id: String,
        name: String,
        #[arg(long)]
        dry_run: bool,
    },
    /// Change instance flags.
    Set {
        id: String,
        #[arg(long, action = clap::ArgAction::Set)]
        enabled: bool,
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum HarnessArg {
    Claude,
    Codex,
    Opencode,
    Agy,
}

impl HarnessArg {
    fn as_core(self) -> Harness {
        match self {
            Self::Claude => Harness::Claude,
            Self::Codex => Harness::Codex,
            Self::Opencode => Harness::Opencode,
            Self::Agy => Harness::Agy,
        }
    }
}

fn main() -> anyhow::Result<()> {
    // Installed as a symlink named `agent-process`, the binary is that tool (identical CLI).
    let mut args = std::env::args_os();
    let argv0 = args.next().unwrap_or_default();
    if std::path::Path::new(&argv0).file_name() == Some("agent-process".as_ref()) {
        // SAFETY: single-threaded here; `| head` ends the listing quietly, as in the shell original.
        unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
        std::process::exit(agent_process::main(args.collect()));
    }
    // Installed as a symlink named `claude-t3-shell`, the binary is the T3
    // Claude launcher proxy (same CLI as `agentdeck route claude-shell`).
    if std::path::Path::new(&argv0).file_name() == Some("claude-t3-shell".as_ref()) {
        std::process::exit(agentdeck_router::shell::run(
            &args
                .collect::<Vec<_>>()
                .iter()
                .map(|s| s.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
        ));
    }
    // Plain CLI output ends quietly at `| head`. Daemon, GUI and the route proxies
    // write to child pipes and must keep handling EPIPE themselves.
    let first = std::env::args().nth(1).unwrap_or_default();
    let plain = !first.is_empty() && !matches!(first.as_str(), "daemon" | "route")
        || std::env::args().any(|a| a == "--check");
    if plain {
        // SAFETY: single-threaded here; restoring the default disposition.
        unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    }
    let cli = Cli::parse();
    // No subcommand opens the GUI; every subcommand keeps its CLI behavior.
    let Some(cmd) = cli.cmd else {
        return run_gui();
    };
    let paths = Paths::from_env()?;
    // Providers write cooldowns/snapshots via util::persistent_state_dir();
    // point it at the new tree before anything runs. Unset, everything keeps
    // the old ai-usage paths (old binary, compat copies, migrator sources).
    agentdeck_usage::util::set_state_dir_override(paths.state_dir.join("usage"));
    agentdeck_usage::daemon::load_agentdeck_secrets(&paths.config_dir)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    match runtime.block_on(async_main(&paths, cmd)) {
        Ok(()) => Ok(()),
        Err(error) => {
            // Single-writer lock and old-service refusal need stable exit codes.
            if error
                .downcast_ref::<agentdeck_usage::daemon::LockHeld>()
                .is_some()
            {
                eprintln!("agentdeck: {error:#}");
                std::process::exit(3);
            }
            if error
                .downcast_ref::<agentdeck_usage::daemon::OldServiceActive>()
                .is_some()
            {
                eprintln!("agentdeck: {error:#}");
                std::process::exit(4);
            }
            Err(error)
        }
    }
}

fn run_gui() -> anyhow::Result<()> {
    // A headless shell has no window backend: fail with a clear error instead
    // of letting winit panic inside iced.
    let has_display = ["WAYLAND_DISPLAY", "WAYLAND_SOCKET", "DISPLAY"]
        .iter()
        .any(|var| std::env::var_os(var).is_some_and(|v| !v.is_empty()));
    if !has_display {
        anyhow::bail!(
            "cannot open the GUI: no display (neither WAYLAND_DISPLAY nor DISPLAY is set); \
             run in a desktop session or use a subcommand, see `agentdeck --help`"
        );
    }
    // Display vars set but unreachable (or any other startup panic): still a
    // clean error, never a panic.
    match std::panic::catch_unwind(agentdeck_gui::run) {
        Ok(inner) => inner.map_err(|error| anyhow::anyhow!("cannot open the GUI: {error}")),
        Err(_) => anyhow::bail!("cannot open the GUI: the window backend failed to start"),
    }
}

async fn async_main(paths: &Paths, cmd: Cmd) -> anyhow::Result<()> {
    match cmd {
        Cmd::Pool { cmd } => cmd_pool::run(paths, cmd)?,
        Cmd::Router { cmd } => cmd_router::run(paths, cmd)?,
        Cmd::Route { args } => {
            std::process::exit(agentdeck_router::route::run(args).await);
        }
        Cmd::Ps(args) => cmd_ps::run(args)?,
        Cmd::Service { cmd } => cmd_service::run(paths, cmd)?,
        Cmd::Plugin { cmd } => cmd_plugin::run(paths, cmd)?,
        Cmd::Skills { cmd } => cmd_shared::run_skills(paths, cmd)?,
        Cmd::Instructions { cmd } => cmd_shared::run_instructions(paths, cmd)?,
        Cmd::Mcp { cmd } => cmd_shared::run_mcp(paths, cmd)?,
        Cmd::Usage { cmd } => cmd_usage::run(paths, cmd).await?,
        Cmd::Daemon(args) => cmd_daemon::run(paths, args).await?,
        Cmd::Wrapper { cmd } => cmd_wrapper::run(paths, cmd)?,
        Cmd::Login {
            id,
            device_auth,
            open,
        } => cmd_login::run(paths, &id, device_auth, open)?,
        Cmd::Claude { cmd } => cmd_claude::run(&cmd)?,
        Cmd::Sync { cmd } => cmd_sync::run(&cmd)?,
        Cmd::Codex { cmd } => cmd_codex::run(&cmd)?,
        Cmd::Opencode { cmd } => cmd_opencode::run(&cmd)?,
        Cmd::Harness { cmd } => cmd_harness::run(&cmd)?,
        Cmd::T3 { cmd } => cmd_t3::run_t3(paths, cmd)?,
        Cmd::Migrate { cmd } => cmd_migrate::run(paths, cmd)?,
        Cmd::Instance { cmd } => match cmd {
            InstanceCmd::List { json } => {
                if json {
                    println!("{}", agentdeck_core::registry::instances_json(paths)?);
                } else {
                    for i in agentdeck_core::registry::list_instances(paths)? {
                        println!(
                            "{} {} {} enabled={} t3={}",
                            i.id,
                            i.harness.as_str(),
                            i.home,
                            i.enabled,
                            i.t3
                        );
                    }
                }
            }
            InstanceCmd::Add {
                id,
                harness,
                home,
                display_name,
                binary,
                no_t3,
                dry_run,
            } => {
                let changes = agentdeck_core::registry::add_instance(
                    paths,
                    AddSpec {
                        id: id.clone(),
                        harness: harness.as_core(),
                        display_name,
                        home,
                        binary,
                        no_t3,
                    },
                    dry_run,
                )?;
                for c in &changes {
                    println!("{c}", c = c.describe());
                }
                if dry_run {
                    println!("dry-run: registry unchanged");
                } else {
                    println!("added {id}");
                }
            }
            InstanceCmd::Remove { id, dry_run } => {
                // Warn when T3 still holds thread history for the id (kept, but orphaned).
                let threads = t3::t3_root()
                    .ok()
                    .and_then(|root| t3::thread_count(&root, &id).unwrap_or(None));
                if let Some(n) = threads.filter(|&n| n > 0) {
                    eprintln!(
                        "warning: T3 keeps {n} thread(s) for {id:?}; history stays but cannot launch"
                    );
                }
                let (changes, home) =
                    agentdeck_core::registry::remove_instance(paths, &id, dry_run)?;
                for c in &changes {
                    println!("{c}", c = c.describe());
                }
                if dry_run {
                    println!("dry-run: registry unchanged");
                } else {
                    println!("removed {id} (home dir kept: {home})");
                }
            }
            InstanceCmd::Rename { id, name, dry_run } => {
                let root = agentdeck_core::t3::t3_root().ok();
                let changes = agentdeck_core::t3::rename_instance(
                    paths,
                    root.as_deref(),
                    &id,
                    &name,
                    dry_run,
                )?;
                for c in changes {
                    println!("{}", c.describe());
                }
            }
            InstanceCmd::Set {
                id,
                enabled,
                dry_run,
            } => {
                let changes = agentdeck_core::registry::set_enabled(paths, &id, enabled, dry_run)?;
                for c in &changes {
                    println!("{c}", c = c.describe());
                }
                if dry_run {
                    println!("dry-run: registry unchanged");
                } else if changes.is_empty() {
                    println!("{id}: already enabled={enabled}");
                } else {
                    println!("set {id} enabled={enabled}");
                }
            }
        },
    }
    Ok(())
}
