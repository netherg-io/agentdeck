//! `agentdeck coordinator`: Autonomous Run status, Coordinator Pool quota view,
//! pre-dispatch disk hygiene check, and Checkpoint judge evaluation (ADR-0002).

use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use agentdeck_coordinator::checkpoint::{
    CheckpointInput, CheckpointJudge, HeuristicCheckpointJudge,
};
use agentdeck_coordinator::config::{CoordinatorConfig, CoordinatorPoolEntry};
use agentdeck_coordinator::hygiene::{check_disk_ok, find_stale_worktrees};
use agentdeck_coordinator::pool::{
    PoolKey, PoolSelection, QuotaWindowView, eligible, pool_key, quota_from_t3_cache,
    select_coordinator, should_proactively_rotate,
};
use agentdeck_core::settings;
use agentdeck_core::{Paths, Registry, t3, thread_state};
use clap::Subcommand;
use serde_json::{Value, json};

#[derive(Subcommand)]
pub enum CoordinatorCmd {
    /// List coordinator threads with phase, freshness, Owner Inbox, Assumptions and sleep deadline.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Show live Coordinator Pool quotas from T3 caches and the currently selected target.
    Pool {
        /// Current T3 provider instance id (for cache-preserving hysteresis & proactive rotation).
        #[arg(long)]
        current: Option<String>,
        /// Current model id (required when `--current` matches multiple pool entries).
        #[arg(long)]
        current_model: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Check free disk space against [coordinator] min_free_disk_gb and optionally list stale git worktrees.
    Hygiene {
        /// Directory to inspect for free disk space (defaults to current working directory).
        #[arg(long)]
        path: Option<PathBuf>,
        /// Directory containing git worktrees/clones to scan for clean merged checkouts.
        #[arg(long)]
        work_dir: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Run deterministic Checkpoint judge evaluation scenarios.
    Eval {
        #[arg(long)]
        json: bool,
    },
    /// Inspect and seal owner_granted=true on a thread's existing Run Mandate.
    GrantMandate {
        /// T3 thread id whose Run Mandate to seal.
        thread_id: String,
        /// Seal without interactive confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
    /// Clear owner_granted on a thread's Run Mandate without modifying its scope.
    RevokeMandate {
        /// T3 thread id whose Run Mandate to revoke.
        thread_id: String,
    },
}

pub fn now_epoch_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Format epoch seconds as RFC-3339 UTC (`YYYY-MM-DDTHH:MM:SSZ`).
pub fn format_utc_rfc3339(secs: i64) -> String {
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Convert `[coordinator]` registry settings into `agentdeck_coordinator::CoordinatorConfig`.
pub fn coordinator_config(reg: &Registry) -> anyhow::Result<CoordinatorConfig> {
    let s = settings::read_coordinator(reg)?;
    let judge_fallback = match settings::read_judge_fallback(reg) {
        Ok(fb) => fb,
        Err(e) => {
            eprintln!(
                "agentdeck coordinator: invalid [judge_fallback] config ({e:#}); continuing without fallback"
            );
            settings::JudgeFallbackSettings::default()
        }
    };
    Ok(CoordinatorConfig {
        pool: s
            .pool
            .into_iter()
            .map(|e| CoordinatorPoolEntry {
                instance_id: e.instance_id,
                model: e.model,
                options: e.options,
                enabled: e.enabled,
            })
            .collect(),
        min_weekly_reserve_pct: s.min_weekly_reserve_pct,
        proactive_rotate_below_5h_pct: s.proactive_rotate_below_5h_pct,
        quota_sleep_buffer_secs: s.quota_sleep_buffer_secs,
        idle_wake_grace_secs: s.idle_wake_grace_secs,
        checkpoint_soft_runs: s.checkpoint_soft_runs,
        checkpoint_soft_items: s.checkpoint_soft_items,
        checkpoint_confidence_threshold: s.checkpoint_confidence_threshold,
        min_free_disk_gb: s.min_free_disk_gb,
        t3_endpoint: s.t3_endpoint,
        t3_token_file: s.t3_token_file,
        judge_provider: s.judge_provider,
        judge_model: s.judge_model,
        judge_fallback,
    })
}

/// Read live `QuotaWindowView` from T3 usage caches for every entry in `cfg.pool`,
/// keyed by `(instance_id, model)` so two entries on one instance never share a view.
/// Unknown, removed, or disabled registry/T3 instances fail closed as unhealthy.
pub fn collect_pool_quotas(
    cfg: &CoordinatorConfig,
    reg: Option<&Registry>,
    t3_root: Option<&Path>,
) -> HashMap<PoolKey, QuotaWindowView> {
    let mut quotas = HashMap::new();
    for entry in &cfg.pool {
        let reg_allows = match reg {
            Some(r) if !r.instances.is_empty() => {
                r.get(&entry.instance_id).is_some_and(|i| i.enabled && i.t3)
            }
            _ => true,
        };
        let view = match (reg_allows, t3_root) {
            (true, Some(root)) => quota_from_t3_cache(root, &entry.instance_id, &entry.model),
            _ => QuotaWindowView::default(),
        };
        quotas.insert(pool_key(&entry.instance_id, &entry.model), view);
    }
    quotas
}

/// Resolve CLI `--current <instance_id>` and optional `--current-model <model>`
/// to the pool `(instance_id, model)` key, refusing ambiguous multi-model instances.
fn current_pool_key<'a>(
    cfg: &'a CoordinatorConfig,
    current: Option<&'a str>,
    current_model: Option<&'a str>,
) -> anyhow::Result<Option<(&'a str, &'a str)>> {
    let Some(cur) = current.map(str::trim).filter(|s| !s.is_empty()) else {
        if current_model.is_some_and(|m| !m.trim().is_empty()) {
            anyhow::bail!("--current-model requires --current <instance_id>");
        }
        return Ok(None);
    };
    if let Some(model) = current_model.map(str::trim).filter(|s| !s.is_empty()) {
        return Ok(Some((cur, model)));
    }
    let matches: Vec<&CoordinatorPoolEntry> =
        cfg.pool.iter().filter(|e| e.instance_id == cur).collect();
    match matches.as_slice() {
        [] => Ok(None),
        [one] => Ok(Some((one.instance_id.as_str(), one.model.as_str()))),
        many => {
            let models = many
                .iter()
                .map(|e| e.model.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::bail!(
                "--current {cur:?} matches multiple pool models ({models}); pass --current-model <model>"
            );
        }
    }
}

/// True when `current` is thin and a meaningfully fuller pool entry exists.
pub fn proactive_rotate_recommended(
    cfg: &CoordinatorConfig,
    quotas: &HashMap<PoolKey, QuotaWindowView>,
    current: Option<(&str, &str)>,
) -> bool {
    let Some((cur_inst, cur_model)) = current else {
        return false;
    };
    let Some(cur_q) = quotas.get(&pool_key(cur_inst, cur_model)) else {
        return false;
    };
    let best_other = cfg
        .pool
        .iter()
        .filter(|e| e.enabled && (e.instance_id != cur_inst || e.model != cur_model))
        .filter_map(|e| quotas.get(&pool_key(&e.instance_id, &e.model)))
        .filter(|q| eligible(cfg, q))
        .max_by(|a, b| {
            a.five_hour_remaining_pct
                .unwrap_or(100.0)
                .partial_cmp(&b.five_hour_remaining_pct.unwrap_or(100.0))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    best_other.is_some_and(|other| should_proactively_rotate(cfg, cur_q, other))
}

pub async fn run(paths: &Paths, cmd: CoordinatorCmd) -> anyhow::Result<i32> {
    match cmd {
        CoordinatorCmd::Status { json } => {
            status(paths, json)?;
            Ok(0)
        }
        CoordinatorCmd::Pool {
            current,
            current_model,
            json,
        } => {
            pool(paths, current.as_deref(), current_model.as_deref(), json)?;
            Ok(0)
        }
        CoordinatorCmd::Hygiene {
            path,
            work_dir,
            json,
        } => hygiene(paths, path, work_dir, json),
        CoordinatorCmd::Eval { json } => eval(json).await,
        CoordinatorCmd::GrantMandate { thread_id, yes } => {
            grant_mandate(paths, &thread_id, yes)?;
            Ok(0)
        }
        CoordinatorCmd::RevokeMandate { thread_id } => {
            revoke_mandate(paths, &thread_id)?;
            Ok(0)
        }
    }
}

fn phase_label(p: thread_state::RunPhase) -> &'static str {
    match p {
        thread_state::RunPhase::Intake => "intake",
        thread_state::RunPhase::Running => "running",
        thread_state::RunPhase::SleepingForQuota => "sleeping_for_quota",
        thread_state::RunPhase::Completed => "completed",
    }
}

fn status(paths: &Paths, as_json: bool) -> anyhow::Result<()> {
    let threads = thread_state::list(paths);
    if as_json {
        let rows: Vec<Value> = threads
            .iter()
            .map(|(id, st)| {
                let (done, total) = st.progress();
                json!({
                    "thread": id,
                    "project": st.project,
                    "phase": st.phase,
                    "state_stale": st.state_stale(),
                    "checklist_updates_since_state": st.checklist_updates_since_state,
                    "done": done,
                    "total": total,
                    "open_inbox_count": st.open_inbox_count(),
                    "assumptions_count": st.assumptions.len(),
                    "sleep_until": st.sleep_until,
                    "checkpoint_count": st.checkpoint_count,
                    "updated": st.updated,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if threads.is_empty() {
        println!("no coordinator threads");
        return Ok(());
    }
    for (id, st) in &threads {
        let (done, total) = st.progress();
        let sleep = st
            .sleep_until
            .as_deref()
            .map(|s| format!(" sleep_until={s}"))
            .unwrap_or_default();
        let proj = if st.project.is_empty() {
            String::new()
        } else {
            format!(" project={}", st.project)
        };
        println!(
            "{id} phase={}{proj} progress={done}/{total} stale={} inbox={} assumptions={}{sleep}",
            phase_label(st.phase),
            st.state_stale(),
            st.open_inbox_count(),
            st.assumptions.len()
        );
    }
    Ok(())
}

fn pool(
    paths: &Paths,
    current: Option<&str>,
    current_model: Option<&str>,
    as_json: bool,
) -> anyhow::Result<()> {
    let reg = Registry::load(paths)?;
    let cfg = coordinator_config(&reg)?;
    let t3_root = t3::t3_root().ok();
    let quotas = collect_pool_quotas(&cfg, Some(&reg), t3_root.as_deref());
    let now = now_epoch_secs();
    let current = current_pool_key(&cfg, current, current_model)?;
    let selection = select_coordinator(&cfg, &quotas, now, current);
    let proactive = proactive_rotate_recommended(&cfg, &quotas, current);

    if as_json {
        let entries: Vec<Value> = cfg
            .pool
            .iter()
            .map(|e| {
                let q = quotas.get(&pool_key(&e.instance_id, &e.model));
                json!({
                    "instance_id": e.instance_id,
                    "model": e.model,
                    "enabled": e.enabled,
                    "healthy": q.is_some_and(|v| v.healthy),
                    "five_hour_remaining_pct": q.and_then(|v| v.five_hour_remaining_pct),
                    "five_hour_resets_at": q.and_then(|v| v.five_hour_resets_at).map(format_utc_rfc3339),
                    "weekly_remaining_pct": q.and_then(|v| v.weekly_remaining_pct),
                    "weekly_resets_at": q.and_then(|v| v.weekly_resets_at).map(format_utc_rfc3339),
                })
            })
            .collect();
        let selection_json = match &selection {
            PoolSelection::Selected {
                entry,
                five_hour_pct,
                weekly_pct,
                reason,
            } => json!({
                "status": "selected",
                "target": entry,
                "five_hour_remaining_pct": five_hour_pct,
                "weekly_remaining_pct": weekly_pct,
                "reason": reason,
            }),
            PoolSelection::SleepForQuota {
                wake_at_epoch_secs,
                earliest_reset_epoch_secs,
                next_candidate,
                reason,
            } => json!({
                "status": "sleeping_for_quota",
                "wake_at": format_utc_rfc3339(*wake_at_epoch_secs),
                "wake_at_epoch_secs": wake_at_epoch_secs,
                "earliest_reset_epoch_secs": earliest_reset_epoch_secs,
                "next_candidate": next_candidate,
                "reason": reason,
            }),
            PoolSelection::NoPoolConfigured => json!({
                "status": "no_pool_configured",
                "reason": "no Coordinator Pool entries configured",
            }),
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "entries": entries,
                "selection": selection_json,
                "proactive_rotate": proactive,
            }))?
        );
        return Ok(());
    }

    if cfg.pool.is_empty() {
        println!("no Coordinator Pool configured in [coordinator]");
        return Ok(());
    }
    for e in &cfg.pool {
        let q = quotas.get(&pool_key(&e.instance_id, &e.model));
        let five = q
            .and_then(|v| v.five_hour_remaining_pct)
            .map(|p| format!("{p:.1}%"))
            .unwrap_or_else(|| "?".into());
        let week = q
            .and_then(|v| v.weekly_remaining_pct)
            .map(|p| format!("{p:.1}%"))
            .unwrap_or_else(|| "?".into());
        let healthy = q.is_some_and(|v| v.healthy);
        println!(
            "{} model={} enabled={} healthy={} 5h={} weekly={}",
            e.instance_id, e.model, e.enabled, healthy, five, week
        );
    }
    match selection {
        PoolSelection::Selected {
            entry,
            five_hour_pct,
            weekly_pct,
            reason,
        } => {
            println!(
                "selected: {} ({}) 5h={five_hour_pct:.1}% weekly={weekly_pct:.1}% proactive_rotate={proactive} — {reason}",
                entry.instance_id, entry.model
            );
        }
        PoolSelection::SleepForQuota {
            wake_at_epoch_secs,
            reason,
            ..
        } => {
            println!(
                "sleeping_for_quota: wake_at={} — {reason}",
                format_utc_rfc3339(wake_at_epoch_secs)
            );
        }
        PoolSelection::NoPoolConfigured => {
            println!("selected: none (no enabled pool entries)");
        }
    }
    Ok(())
}

fn hygiene(
    paths: &Paths,
    path_override: Option<PathBuf>,
    work_dir: Option<PathBuf>,
    as_json: bool,
) -> anyhow::Result<i32> {
    let reg = Registry::load(paths)?;
    let cfg = coordinator_config(&reg)?;
    let dir = match path_override {
        Some(p) => p,
        None => std::env::current_dir()?,
    };
    let check = check_disk_ok(&dir, cfg.min_free_disk_gb);
    let stale_worktrees = match work_dir.as_deref() {
        Some(wd) => {
            if !wd.is_dir() {
                anyhow::bail!("--work-dir must be an existing directory: {}", wd.display());
            }
            Some(find_stale_worktrees(wd, &HashSet::new()))
        }
        None => None,
    };
    if as_json {
        let mut out = json!({
            "ok": check.ok,
            "free_gb": check.free_gb,
            "min_gb": check.min_gb,
            "path": dir,
        });
        if let Some(stale) = &stale_worktrees {
            out["stale_worktrees"] = json!(stale);
        }
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        println!(
            "disk {}: {} GB free (min {} GB) at {}",
            if check.ok { "ok" } else { "LOW" },
            check.free_gb,
            check.min_gb,
            dir.display()
        );
        if let Some(stale) = &stale_worktrees {
            if stale.is_empty() {
                println!("stale worktrees: none");
            } else {
                println!("stale worktrees ({}):", stale.len());
                for p in stale {
                    println!("  {}", p.display());
                }
            }
        }
    }
    Ok(if check.ok { 0 } else { 1 })
}

fn print_mandate_scope(
    thread_id: &str,
    phase: thread_state::RunPhase,
    m: &thread_state::RunMandate,
) {
    let hosts = if m.allowed_hosts.is_empty() {
        "(any — unrestricted)".to_string()
    } else {
        m.allowed_hosts.join(", ")
    };
    let apps = if m.allowed_apps.is_empty() {
        "(any — unrestricted)".to_string()
    } else {
        m.allowed_apps.join(", ")
    };
    println!("thread: {thread_id} (phase={})", phase_label(phase));
    println!(
        "  project: {}",
        if m.project.is_empty() {
            "(none)"
        } else {
            &m.project
        }
    );
    println!("  allowed_hosts: {hosts}");
    println!("  allowed_apps: {apps}");
    println!("  allow_releases: {}", m.allow_releases);
    println!("  fix_forward_max: {}", m.fix_forward_max);
    println!("  review_rounds_max: {}", m.review_rounds_max);
    if !m.notes.is_empty() {
        println!("  notes: {}", m.notes);
    }
    println!("  owner_granted: {}", m.owner_granted);
}

fn grant_mandate(paths: &Paths, thread_id: &str, yes: bool) -> anyhow::Result<()> {
    let found = thread_state::read(paths, None, thread_id)?
        .ok_or_else(|| anyhow::anyhow!("thread {thread_id:?} has no saved state"))?;
    if found.data.phase == thread_state::RunPhase::Completed {
        anyhow::bail!("thread {thread_id:?} is completed: start a new run instead");
    }
    let expected = found
        .data
        .mandate
        .clone()
        .ok_or_else(|| anyhow::anyhow!("thread {thread_id:?} has no mandate to grant"))?;
    print_mandate_scope(thread_id, found.data.phase, &expected);
    if !yes {
        print!("Grant this Run Mandate? [y/N]: ");
        io::stdout().flush()?;
        let mut answer = String::new();
        io::stdin().lock().read_line(&mut answer)?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            anyhow::bail!("aborted: mandate not granted");
        }
    }
    thread_state::grant_mandate_if_matches(paths, thread_id, &expected)?;
    println!("granted mandate for {thread_id}");
    Ok(())
}

fn revoke_mandate(paths: &Paths, thread_id: &str) -> anyhow::Result<()> {
    thread_state::revoke_mandate(paths, thread_id)?;
    println!("revoked mandate for {thread_id}");
    Ok(())
}

async fn eval(as_json: bool) -> anyhow::Result<i32> {
    let judge = HeuristicCheckpointJudge;
    let cfg = CoordinatorConfig::default();
    let case = |runs, items, milestone, stale, children, events: &[&str]| CheckpointInput {
        run_count_since_checkpoint: runs,
        item_count_since_checkpoint: items,
        milestone_just_completed: milestone,
        state_stale: stale,
        active_children: children,
        recent_events_tail: events.iter().map(|s| (*s).to_string()).collect(),
    };
    let cases: Vec<(&str, CheckpointInput, bool)> = vec![
        (
            "stale_quiet_refused",
            case(0, 0, false, true, 0, &[]),
            false,
        ),
        (
            "stale_milestone_refused",
            case(0, 0, true, true, 0, &[]),
            false,
        ),
        (
            "stale_over_threshold_refused",
            case(200, 5000, false, true, 0, &[]),
            false,
        ),
        (
            "quiet_turn_preserves_cache",
            case(10, 100, false, false, 0, &[]),
            false,
        ),
        (
            "just_under_thresholds",
            case(79, 1499, false, false, 0, &[]),
            false,
        ),
        (
            "at_run_threshold_approved",
            case(80, 0, false, false, 0, &[]),
            true,
        ),
        (
            "at_item_threshold_approved",
            case(0, 1500, false, false, 0, &[]),
            true,
        ),
        (
            "clean_milestone_approved",
            case(3, 12, true, false, 0, &["shipped U1"]),
            true,
        ),
        (
            "active_children_refused",
            case(200, 5000, true, false, 2, &[]),
            false,
        ),
        (
            "merge_conflict_hazard_refused",
            case(200, 5000, true, false, 0, &["merge conflict"]),
            false,
        ),
        (
            "mid_rollback_hazard_refused",
            case(200, 5000, false, false, 0, &["mid-rollback"]),
            false,
        ),
        (
            "in_progress_deploy_refused",
            case(200, 5000, false, false, 0, &["in-progress deploy"]),
            false,
        ),
    ];

    let mut passed = 0usize;
    let mut failures = Vec::new();
    for (name, inp, expected) in &cases {
        let verdict = judge.judge(inp, &cfg).await?;
        let actual =
            verdict.should_checkpoint && verdict.confidence >= cfg.checkpoint_confidence_threshold;
        if actual == *expected {
            passed += 1;
        } else {
            failures.push(json!({
                "name": name,
                "expected": expected,
                "actual": actual,
                "confidence": verdict.confidence,
                "reason": verdict.reason,
            }));
        }
    }
    let total = cases.len();
    let ok = passed == total;
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "total": total,
                "passed": passed,
                "failures": failures,
            }))?
        );
    } else {
        println!("checkpoint eval: {passed}/{total}");
        for f in &failures {
            println!("  FAIL {}: {f}", f["name"].as_str().unwrap_or("?"));
        }
    }
    Ok(if ok { 0 } else { 1 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coordinator_config_ignores_invalid_judge_fallback_and_logs_error() {
        const CHILD: &str = "AGENTDECK_COORD_INVALID_FALLBACK_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "cmd_coordinator::tests::coordinator_config_ignores_invalid_judge_fallback_and_logs_error",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                output.status.success(),
                "child failed:\nstdout:\n{stdout}\nstderr:\n{stderr}"
            );
            assert!(
                stderr.contains("[judge_fallback]") && stderr.contains("vercel"),
                "expected invalid [judge_fallback] warning on stderr, got:\n{stderr}"
            );
            return;
        }

        let reg = Registry::from_toml(
            r#"
version = 1

[coordinator]
[[coordinator.pool]]
instance_id = "claude_a"
model = "claude-opus-5-5"

[judge_fallback]
provider = "vercel"
"#,
        )
        .unwrap();
        let cfg = coordinator_config(&reg)
            .expect("invalid [judge_fallback] must not fail coordinator_config");
        assert_eq!(cfg.pool.len(), 1);
        assert_eq!(cfg.pool[0].instance_id, "claude_a");
        assert_eq!(
            cfg.judge_fallback,
            settings::JudgeFallbackSettings::default()
        );

        // Valid [judge_fallback] is still loaded.
        let valid_reg = Registry::from_toml(
            r#"
version = 1

[judge_fallback]
provider = "vercel"
model = "openai/gpt-4o-mini"
"#,
        )
        .unwrap();
        let valid_cfg = coordinator_config(&valid_reg).unwrap();
        assert_eq!(
            valid_cfg.judge_fallback.provider.as_deref(),
            Some(settings::PROVIDER_VERCEL)
        );
        assert_eq!(
            valid_cfg.judge_fallback.model.as_deref(),
            Some("openai/gpt-4o-mini")
        );

        // Invalid [coordinator] itself must still fail.
        let bad_coord = Registry::from_toml(
            r#"
version = 1

[coordinator]
judge_provider = "vercel"
"#,
        )
        .unwrap();
        assert!(coordinator_config(&bad_coord).is_err());
    }
}
