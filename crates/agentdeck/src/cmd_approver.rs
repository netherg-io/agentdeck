//! `agentdeck approver`: local approval broker CLI (ADR-0001) — status, audit,
//! Hard-Deny Perimeter eval, wire-compatible Unix-socket client, SSH reverse
//! tunnel relay, and `[approver]` configuration in `instances.toml`.

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::Command;

use agentdeck_approver::{
    ApproverConfig, ApproverSecrets, audit_log_path, eval::run_perimeter_eval, read_recent,
    ssh_reverse_tunnel_args,
};
use agentdeck_core::settings::{
    self, ApproverSettings, DEFAULT_DECISION_MODEL, PROVIDER_OPENROUTER,
};
use agentdeck_core::{Paths, Registry, plugins};
use clap::{Args, Subcommand};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

pub const DEFAULT_REMOTE_SOCKET: &str = "/run/agent-approver/broker.sock";
pub const EXIT_CONNECT_ERROR: i32 = 253;

#[derive(Subcommand)]
pub enum ApproverCmd {
    /// Show approver plugin state, socket, Telegram config, timeouts, judge and audit count.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Show recent approval audit records.
    Audit {
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Run the offline Hard-Deny Perimeter golden + adversarial evaluation suite.
    Eval {
        #[arg(long)]
        json: bool,
    },
    /// Send one JSON approval request from stdin to the local broker socket and stream responses.
    Client {
        /// Override broker Unix socket path.
        #[arg(long)]
        socket: Option<PathBuf>,
    },
    /// Open an SSH reverse Unix-socket tunnel forwarding a remote host to the local broker.
    SshRelay {
        /// SSH host alias (e.g. homeserver-agent).
        #[arg(long)]
        host: String,
        /// Remote socket path bound on the target host.
        #[arg(long, default_value = DEFAULT_REMOTE_SOCKET)]
        remote_socket: String,
        /// Override local broker socket path.
        #[arg(long)]
        local_socket: Option<PathBuf>,
    },
    /// Show or update `[approver]` settings in instances.toml.
    Config(ApproverConfigArgs),
}

#[derive(Debug, Args)]
pub struct ApproverConfigArgs {
    #[arg(long)]
    pub autonomous_timeout: Option<u64>,
    #[arg(long)]
    pub interactive_timeout: Option<u64>,
    /// Policy Judge provider (`openrouter` or `vercel`; pass `""` to reset to default).
    #[arg(long)]
    pub judge_provider: Option<String>,
    /// Policy Judge model id (pass `""` to reset to default).
    #[arg(long)]
    pub judge_model: Option<String>,
    /// Auto-approval confidence threshold in [0.0, 1.0].
    #[arg(long)]
    pub judge_threshold: Option<f64>,
    /// Custom broker Unix socket path (pass `""` to reset to default).
    #[arg(long)]
    pub socket_path: Option<String>,
    /// Add one SSH host to remote_hosts.
    #[arg(long = "add-remote-host")]
    pub add_remote_hosts: Vec<String>,
    /// Remove one SSH host from remote_hosts.
    #[arg(long = "remove-remote-host")]
    pub remove_remote_hosts: Vec<String>,
    /// Add one case-insensitive regex to extra_hard_deny_patterns.
    #[arg(long = "add-deny-pattern")]
    pub add_deny_patterns: Vec<String>,
    /// Remove one regex from extra_hard_deny_patterns.
    #[arg(long = "remove-deny-pattern")]
    pub remove_deny_patterns: Vec<String>,
    #[arg(long)]
    pub dry_run: bool,
}

impl ApproverConfigArgs {
    fn has_mutations(&self) -> bool {
        self.autonomous_timeout.is_some()
            || self.interactive_timeout.is_some()
            || self.judge_provider.is_some()
            || self.judge_model.is_some()
            || self.judge_threshold.is_some()
            || self.socket_path.is_some()
            || !self.add_remote_hosts.is_empty()
            || !self.remove_remote_hosts.is_empty()
            || !self.add_deny_patterns.is_empty()
            || !self.remove_deny_patterns.is_empty()
    }
}

/// Build an `ApproverConfig` from the registry's `[plugins]` and `[approver]` tables.
pub fn approver_config(reg: &Registry) -> anyhow::Result<ApproverConfig> {
    let s = settings::read_approver(reg)?;
    let cfg = ApproverConfig {
        enabled: plugins::plugin_enabled(reg, plugins::PLUGIN_APPROVER),
        socket_path: s.socket_path,
        autonomous_timeout_secs: s.autonomous_timeout_secs,
        interactive_timeout_secs: s.interactive_timeout_secs,
        judge_provider: s.judge_provider,
        judge_model: s.judge_model,
        judge_confidence_threshold: s.judge_confidence_threshold,
        remote_hosts: s.remote_hosts,
        extra_hard_deny_patterns: s.extra_hard_deny_patterns,
    };
    cfg.validate()?;
    Ok(cfg)
}

pub async fn run(paths: &Paths, cmd: ApproverCmd) -> anyhow::Result<i32> {
    match cmd {
        ApproverCmd::Status { json } => {
            status(paths, json)?;
            Ok(0)
        }
        ApproverCmd::Audit { limit, json } => {
            audit(paths, limit, json)?;
            Ok(0)
        }
        ApproverCmd::Eval { json } => Ok(eval(json)?),
        ApproverCmd::Client { socket } => client(paths, socket).await,
        ApproverCmd::SshRelay {
            host,
            remote_socket,
            local_socket,
        } => ssh_relay(paths, &host, &remote_socket, local_socket),
        ApproverCmd::Config(args) => {
            config(paths, args)?;
            Ok(0)
        }
    }
}

fn status(paths: &Paths, as_json: bool) -> anyhow::Result<()> {
    let reg = Registry::load(paths)?;
    let cfg = approver_config(&reg)?;
    let socket = cfg.resolved_socket_path();
    let socket_listening =
        socket.exists() && std::os::unix::net::UnixStream::connect(&socket).is_ok();
    let secrets_path = ApproverSecrets::secrets_path(&paths.config_dir);
    let secrets = ApproverSecrets::load(&paths.config_dir).unwrap_or_default();
    let telegram_configured =
        secrets.telegram_bot_token.is_some() && secrets.telegram_owner_id.is_some();
    let provider = cfg
        .judge_provider
        .as_deref()
        .unwrap_or(PROVIDER_OPENROUTER);
    let model = cfg
        .judge_model
        .as_deref()
        .unwrap_or(DEFAULT_DECISION_MODEL);
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let judge_key_present = secrets.judge_api_key(provider).is_some()
        || home
            .as_deref()
            .and_then(|h| agentdeck_core::imagegen::gateway_key(&reg, h, provider))
            .is_some();
    let audit_path = audit_log_path(&paths.state_dir);
    let recent_audit_count = read_recent(&audit_path, 1000).map(|r| r.len()).unwrap_or(0);

    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "enabled": cfg.enabled,
                "socket_path": socket,
                "socket_listening": socket_listening,
                "secrets_path": secrets_path,
                "telegram_configured": telegram_configured,
                "autonomous_timeout_secs": cfg.autonomous_timeout_secs,
                "interactive_timeout_secs": cfg.interactive_timeout_secs,
                "judge_provider": provider,
                "judge_model": model,
                "judge_confidence_threshold": cfg.judge_confidence_threshold,
                "judge_key_present": judge_key_present,
                "remote_hosts": cfg.remote_hosts,
                "extra_hard_deny_patterns": cfg.extra_hard_deny_patterns,
                "audit_path": audit_path,
                "recent_audit_count": recent_audit_count,
            }))?
        );
        return Ok(());
    }

    println!(
        "enabled: {}",
        if cfg.enabled { "yes" } else { "no" }
    );
    println!(
        "socket: {} ({})",
        socket.display(),
        if socket_listening {
            "listening"
        } else {
            "inactive"
        }
    );
    println!(
        "telegram: {} ({})",
        if telegram_configured {
            "configured"
        } else {
            "missing"
        },
        secrets_path.display()
    );
    println!(
        "timeouts: {}s autonomous / {}s interactive",
        cfg.autonomous_timeout_secs, cfg.interactive_timeout_secs
    );
    println!(
        "judge: {provider} {model} (threshold {:.2}, key {})",
        cfg.judge_confidence_threshold,
        if judge_key_present {
            "found"
        } else {
            "missing"
        }
    );
    if !cfg.remote_hosts.is_empty() {
        println!("remote hosts: {}", cfg.remote_hosts.join(", "));
    }
    if !cfg.extra_hard_deny_patterns.is_empty() {
        println!(
            "extra hard-deny patterns: {}",
            cfg.extra_hard_deny_patterns.len()
        );
    }
    println!(
        "audit: {} ({recent_audit_count} recent record(s))",
        audit_path.display()
    );
    Ok(())
}

fn audit(paths: &Paths, limit: usize, as_json: bool) -> anyhow::Result<()> {
    let path = audit_log_path(&paths.state_dir);
    let records = read_recent(&path, limit)?;
    if as_json {
        println!("{}", serde_json::to_string_pretty(&records)?);
        return Ok(());
    }
    if records.is_empty() {
        println!("no audit records");
        return Ok(());
    }
    for r in records {
        let conf = r
            .judge_confidence
            .map(|c| format!(" conf={c:.2}"))
            .unwrap_or_default();
        println!(
            "{} {} host={} kind={} outcome={} source={:?} perimeter={}{conf} {}ms",
            r.timestamp, r.request_id, r.host, r.kind, r.outcome, r.source, r.perimeter, r.elapsed_ms
        );
    }
    Ok(())
}

fn eval(as_json: bool) -> anyhow::Result<i32> {
    let report = run_perimeter_eval();
    let ok = report.passed == report.total;
    if as_json {
        let failures: Vec<Value> = report
            .failures
            .iter()
            .map(|f| {
                json!({
                    "name": f.name,
                    "expected_hard_deny": f.expected_hard_deny,
                    "expected_rule": f.expected_rule,
                    "actual": format!("{:?}", f.actual),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "total": report.total,
                "passed": report.passed,
                "accuracy": report.accuracy(),
                "false_negatives": report.false_negatives,
                "false_positives": report.false_positives,
                "failures": failures,
            }))?
        );
    } else {
        println!(
            "perimeter eval: {}/{} ({:.1}%) false_negatives={} false_positives={}",
            report.passed,
            report.total,
            report.accuracy() * 100.0,
            report.false_negatives,
            report.false_positives
        );
        for f in &report.failures {
            println!(
                "  FAIL {}: expected hard_deny={} rule={:?}, got {:?}",
                f.name, f.expected_hard_deny, f.expected_rule, f.actual
            );
        }
    }
    Ok(if ok { 0 } else { 1 })
}

async fn client(paths: &Paths, socket_override: Option<PathBuf>) -> anyhow::Result<i32> {
    let socket = match socket_override {
        Some(p) => p,
        None => approver_config(&Registry::load(paths)?)?.resolved_socket_path(),
    };
    let stdin = std::io::stdin();
    let mut request_line = String::new();
    for line in stdin.lock().lines() {
        let line = line?;
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            request_line = trimmed.to_string();
            break;
        }
    }
    if request_line.is_empty() {
        anyhow::bail!("empty approval request on stdin");
    }
    let _: Value = serde_json::from_str(&request_line)
        .map_err(|e| anyhow::anyhow!("invalid JSON request on stdin: {e}"))?;

    let stream = match UnixStream::connect(&socket).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("approver client: cannot connect to {}: {e}", socket.display());
            return Ok(EXIT_CONNECT_ERROR);
        }
    };
    let (read_half, mut write_half) = stream.into_split();
    let mut payload = request_line.into_bytes();
    payload.push(b'\n');
    if let Err(e) = write_half.write_all(&payload).await {
        eprintln!("approver client: write failed: {e}");
        return Ok(EXIT_CONNECT_ERROR);
    }
    if let Err(e) = write_half.flush().await {
        eprintln!("approver client: flush failed: {e}");
        return Ok(EXIT_CONNECT_ERROR);
    }
    // Keep `write_half` open while reading so the broker distinguishes a live
    // waiter from a disconnected client (POLLHUP triggers immediate cancel).
    let mut lines = BufReader::new(read_half).lines();
    let mut stdout = std::io::stdout();
    while let Some(line) = lines.next_line().await? {
        writeln!(stdout, "{line}")?;
        stdout.flush()?;
        if let Ok(v) = serde_json::from_str::<Value>(&line)
            && v.get("type").and_then(Value::as_str) == Some("result")
        {
            drop(write_half);
            return Ok(0);
        }
    }
    eprintln!("approver client: broker closed connection before sending a result");
    Ok(EXIT_CONNECT_ERROR)
}

fn ssh_relay(
    paths: &Paths,
    host: &str,
    remote_socket: &str,
    local_override: Option<PathBuf>,
) -> anyhow::Result<i32> {
    if host.is_empty() || host.chars().any(|c| c.is_whitespace() || c.is_control()) {
        anyhow::bail!("invalid SSH host {host:?}");
    }
    if remote_socket.is_empty()
        || remote_socket
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
    {
        anyhow::bail!("invalid remote socket path {remote_socket:?}");
    }
    let local_socket = match local_override {
        Some(p) => p,
        None => approver_config(&Registry::load(paths)?)?.resolved_socket_path(),
    };
    let args = ssh_reverse_tunnel_args(host, &local_socket, remote_socket);
    let status = Command::new("ssh")
        .args(&args)
        .status()
        .map_err(|e| anyhow::anyhow!("failed to start ssh relay to {host}: {e}"))?;
    Ok(status.code().unwrap_or(1))
}

fn config(paths: &Paths, args: ApproverConfigArgs) -> anyhow::Result<()> {
    let reg = Registry::load(paths)?;
    let mut cfg: ApproverSettings = settings::read_approver(&reg)?;
    if !args.has_mutations() {
        println!("{}", serde_json::to_string_pretty(&cfg)?);
        return Ok(());
    }
    if let Some(v) = args.autonomous_timeout {
        cfg.autonomous_timeout_secs = v;
    }
    if let Some(v) = args.interactive_timeout {
        cfg.interactive_timeout_secs = v;
    }
    if let Some(p) = args.judge_provider {
        cfg.judge_provider = (!p.trim().is_empty()).then(|| p.trim().to_string());
    }
    if let Some(m) = args.judge_model {
        cfg.judge_model = (!m.trim().is_empty()).then(|| m.trim().to_string());
    }
    if let Some(t) = args.judge_threshold {
        cfg.judge_confidence_threshold = t;
    }
    if let Some(s) = args.socket_path {
        cfg.socket_path = (!s.trim().is_empty()).then(|| PathBuf::from(s.trim()));
    }
    for h in args.add_remote_hosts {
        if !cfg.remote_hosts.contains(&h) {
            cfg.remote_hosts.push(h);
        }
    }
    if !args.remove_remote_hosts.is_empty() {
        cfg.remote_hosts
            .retain(|h| !args.remove_remote_hosts.contains(h));
    }
    for p in args.add_deny_patterns {
        if !cfg.extra_hard_deny_patterns.contains(&p) {
            cfg.extra_hard_deny_patterns.push(p);
        }
    }
    if !args.remove_deny_patterns.is_empty() {
        cfg.extra_hard_deny_patterns
            .retain(|p| !args.remove_deny_patterns.contains(p));
    }
    let changes = settings::write_approver(paths, &cfg, args.dry_run)?;
    if changes.is_empty() {
        println!("approver config: unchanged");
        return Ok(());
    }
    for c in &changes {
        println!("{}", c.describe());
    }
    if args.dry_run {
        println!("dry-run: registry unchanged");
    } else {
        println!("updated [approver] config");
    }
    Ok(())
}
