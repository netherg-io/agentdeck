//! Read-only `agentdeck ps`: the processes snapshot as a table or JSON.
//! No stop command lives here: stopping stays `agent-process stop` (CLI
//! agents get no ownership bypass); only the GUI stop button calls
//! `processes::stop_group` under the owner's click.

use agentdeck_core::processes;

#[derive(clap::Args)]
pub struct PsArgs {
    /// Machine-readable snapshot.
    #[arg(long)]
    pub json: bool,
}

fn mega(bytes: u64) -> String {
    format!("{:.0}M", bytes as f64 / 1_048_576.0)
}

fn bytes_or_dash(v: Option<u64>) -> String {
    v.map(mega).unwrap_or_else(|| "-".to_string())
}

pub fn run(args: PsArgs) -> anyhow::Result<()> {
    let snap = processes::snapshot();
    if args.json {
        println!("{}", serde_json::to_string_pretty(&snap)?);
        return Ok(());
    }
    if snap.groups.is_empty() {
        println!("(no agent processes)");
        return Ok(());
    }
    for g in &snap.groups {
        let scope = g
            .scope_mem_bytes
            .map(|m| format!("  scope {}", mega(m)))
            .unwrap_or_default();
        let podman = g
            .container
            .as_deref()
            .map(|c| format!("  [podman {c}]"))
            .unwrap_or_default();
        println!(
            "{}{podman}  CPU {:.1}%  RSS {}  IO {}/{}{scope}",
            g.title,
            g.cpu_percent,
            mega(g.rss_bytes),
            bytes_or_dash(g.read_bytes),
            bytes_or_dash(g.write_bytes),
        );
        for p in &g.processes {
            println!(
                "  {:>7}  CPU {:>5.1}%  RSS {:>6}  IO {:>6}/{:<6}  {}",
                p.pid,
                p.cpu_percent,
                mega(p.rss_bytes),
                bytes_or_dash(p.read_bytes),
                bytes_or_dash(p.write_bytes),
                p.display_cmd(),
            );
        }
        if !g.can_stop {
            println!("  T3 sessions are interrupted through T3 when it is reachable, not killed.");
        }
    }
    Ok(())
}
