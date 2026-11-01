use agentdeck_core::Paths;
use agentdeck_core::pool::{self, PoolEntry};
use clap::Subcommand;

#[derive(Subcommand)]
pub enum PoolCmd {
    /// List pool entries.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Add a pool entry.
    Add {
        #[arg(long)]
        instance: String,
        #[arg(long)]
        model: String,
        #[arg(long = "option")]
        options: Vec<String>,
        #[arg(long)]
        disabled: bool,
        #[arg(long)]
        dry_run: bool,
    },
    /// Remove a pool entry.
    Remove {
        #[arg(long)]
        instance: String,
        #[arg(long)]
        model: String,
        #[arg(long = "option")]
        options: Vec<String>,
        #[arg(long)]
        dry_run: bool,
    },
    /// Enable a pool entry.
    Enable {
        #[arg(long)]
        instance: String,
        #[arg(long)]
        model: String,
        #[arg(long = "option")]
        options: Vec<String>,
        #[arg(long)]
        dry_run: bool,
    },
    /// Disable a pool entry.
    Disable {
        #[arg(long)]
        instance: String,
        #[arg(long)]
        model: String,
        #[arg(long = "option")]
        options: Vec<String>,
        #[arg(long)]
        dry_run: bool,
    },
}

pub fn run(paths: &Paths, cmd: PoolCmd) -> anyhow::Result<()> {
    match cmd {
        PoolCmd::List { json } => {
            if json {
                println!("{}", pool::pool_json(paths)?);
            } else {
                for e in pool::list_pool(paths)? {
                    println!("{}", e.describe());
                }
            }
        }
        PoolCmd::Add {
            instance,
            model,
            options,
            disabled,
            dry_run,
        } => {
            let entry = PoolEntry {
                instance: instance.clone(),
                model: model.clone(),
                options: pool::parse_options(&options)?,
                enabled: !disabled,
            };
            let (changes, warn) = pool::add_pool_entry(paths, entry, dry_run)?;
            for c in &changes {
                println!("{c}", c = c.describe());
            }
            if let Some(w) = warn {
                eprintln!("warning: {w}");
            }
            if dry_run {
                println!("dry-run: registry unchanged");
            } else {
                println!("added pool entry {instance} {model}");
            }
        }
        PoolCmd::Remove {
            instance,
            model,
            options,
            dry_run,
        } => {
            let opts = pool::parse_options(&options)?;
            let changes = pool::remove_pool_entry(paths, &instance, &model, &opts, dry_run)?;
            for c in &changes {
                println!("{c}", c = c.describe());
            }
            if dry_run {
                println!("dry-run: registry unchanged");
            } else {
                println!("removed pool entry {instance} {model}");
            }
        }
        PoolCmd::Enable {
            instance,
            model,
            options,
            dry_run,
        } => {
            let opts = pool::parse_options(&options)?;
            let changes = pool::set_pool_enabled(paths, &instance, &model, &opts, true, dry_run)?;
            for c in &changes {
                println!("{c}", c = c.describe());
            }
            if dry_run {
                println!("dry-run: registry unchanged");
            } else if changes.is_empty() {
                println!("{instance} {model}: already enabled");
            } else {
                println!("enabled pool entry {instance} {model}");
            }
        }
        PoolCmd::Disable {
            instance,
            model,
            options,
            dry_run,
        } => {
            let opts = pool::parse_options(&options)?;
            let changes = pool::set_pool_enabled(paths, &instance, &model, &opts, false, dry_run)?;
            for c in &changes {
                println!("{c}", c = c.describe());
            }
            if dry_run {
                println!("dry-run: registry unchanged");
            } else if changes.is_empty() {
                println!("{instance} {model}: already disabled");
            } else {
                println!("disabled pool entry {instance} {model}");
            }
        }
    }
    Ok(())
}
