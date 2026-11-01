//! `agentdeck router json`: refresh the generated `router.json` (the router
//! itself runs from the binary; no files are extracted).
//! `agentdeck router check`: one live decision through the configured provider.

use agentdeck_core::Paths;
use agentdeck_core::router;
use clap::Subcommand;

#[derive(Subcommand)]
pub enum RouterCmd {
    /// Refresh `<state_dir>/router/` (embedded files + generated router.json).
    Json {
        #[arg(long)]
        dry_run: bool,
    },
    /// Send one small test decision with the saved provider, model and key.
    Check,
}

pub async fn run(paths: &Paths, cmd: RouterCmd) -> anyhow::Result<()> {
    match cmd {
        RouterCmd::Json { dry_run } => {
            let reg = agentdeck_core::Registry::load(paths)?;
            let changes = router::plan_router_files(paths, &reg)?;
            if changes.is_empty() {
                println!("router json: up to date");
                return Ok(());
            }
            for c in &changes {
                println!("{c}", c = c.describe());
            }
            if dry_run {
                println!("dry-run: nothing changed");
                return Ok(());
            }
            agentdeck_core::apply_changes(&changes)?;
            println!("wrote {}", router::router_json_path(paths).display());
        }
        RouterCmd::Check => check(paths).await?,
    }
    Ok(())
}

async fn check(paths: &Paths) -> anyhow::Result<()> {
    let s = agentdeck_core::settings::read(&agentdeck_core::Registry::load(paths)?);
    let provider = s.decision_provider.as_deref();
    let model = s
        .decision_model
        .clone()
        .unwrap_or_else(|| agentdeck_core::settings::DEFAULT_DECISION_MODEL.to_string());
    let started = std::time::Instant::now();
    let (size, difficulty, cost) =
        agentdeck_router::route::probe_decision(provider, &model, None).await?;
    println!(
        "{} {model}: ok, size {size:.2}, difficulty {difficulty:.2}, {} ms{}",
        provider.unwrap_or(agentdeck_core::settings::PROVIDER_OPENROUTER),
        started.elapsed().as_millis(),
        cost.map(|c| format!(", ${c:.6}")).unwrap_or_default()
    );
    Ok(())
}
