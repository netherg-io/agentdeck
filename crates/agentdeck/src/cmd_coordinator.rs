//! `agentdeck coordinator`: Autonomous Run status, Coordinator Pool quota view,
//! pre-dispatch disk hygiene check, and Checkpoint judge evaluation (ADR-0002).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use agentdeck_coordinator::checkpoint::{
    CheckpointInput, CheckpointJudge, HeuristicCheckpointJudge,
};
use agentdeck_coordinator::config::{CoordinatorConfig, CoordinatorPoolEntry};
use agentdeck_coordinator::hygiene::check_disk_ok;
use agentdeck_coordinator::pool::{
    PoolSelection, QuotaWindowView, quota_from_t3_cache, select_coordinator,
    should_proactively_rotate,
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
        #[arg(long)]
        json: bool,
    },
    /// Check free disk space against [coordinator] min_free_disk_gb.
    Hygiene {
        /// Directory to inspect (defaults to current working directory).
        #[arg(long)]
        path: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Run deterministic Checkpoint judge evaluation scenarios.
    Eval {
        #[arg(long)]
        json: bool,
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
    })
}

/// Read live `QuotaWindowView` from T3 usage caches for every entry in `cfg.pool`.
pub fn collect_pool_quotas(
    cfg: &CoordinatorConfig,
    t3_root: Option<&Path>,
) -> HashMap<String, QuotaWindowView> {
    let mut quotas = HashMap::new();
    for entry in &cfg.pool {
        let view = match t3_root {
            Some(root) => quota_from_t3_cache(root, &entry.instance_id, &entry.model),
            None => QuotaWindowView::unknown_healthy(),
        };
        quotas.insert(entry.instance_id.clone(), view);
    }
    quotas
}

/// True when `current_instance` is thin and a meaningfully fuller pool entry exists.
pub fn proactive_rotate_recommended(
    cfg: &CoordinatorConfig,
    quotas: &HashMap<String, QuotaWindowView>,
    current_instance: Option<&str>,
) -> bool {
    let Some(cur) = current_instance else {
        return false;
    };
    let Some(cur_q) = quotas.get(cur) else {
        return false;
    };
    let best_other = cfg
        .pool
        .iter()
        .filter(|e| e.enabled && e.instance_id != cur)
        .filter_map(|e| quotas.get(&e.instance_id))
        .filter(|q| q.healthy)
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
        CoordinatorCmd::Pool { current, json } => {
            pool(paths, current.as_deref(), json)?;
            Ok(0)
        }
        CoordinatorCmd::Hygiene { path, json } => hygiene(paths, path, json),
        CoordinatorCmd::Eval { json } => eval(json).await,
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

fn pool(paths: &Paths, current: Option<&str>, as_json: bool) -> anyhow::Result<()> {
    let reg = Registry::load(paths)?;
    let cfg = coordinator_config(&reg)?;
    let t3_root = t3::t3_root().ok();
    let quotas = collect_pool_quotas(&cfg, t3_root.as_deref());
    let now = now_epoch_secs();
    let selection = select_coordinator(&cfg, &quotas, now, current);
    let proactive = proactive_rotate_recommended(&cfg, &quotas, current);

    if as_json {
        let entries: Vec<Value> = cfg
            .pool
            .iter()
            .map(|e| {
                let q = quotas.get(&e.instance_id);
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
        let q = quotas.get(&e.instance_id);
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

fn hygiene(paths: &Paths, path_override: Option<PathBuf>, as_json: bool) -> anyhow::Result<i32> {
    let reg = Registry::load(paths)?;
    let cfg = coordinator_config(&reg)?;
    let dir = match path_override {
        Some(p) => p,
        None => std::env::current_dir()?,
    };
    let check = check_disk_ok(&dir, cfg.min_free_disk_gb);
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": check.ok,
                "free_gb": check.free_gb,
                "min_gb": check.min_gb,
                "path": dir,
            }))?
        );
    } else {
        println!(
            "disk {}: {} GB free (min {} GB) at {}",
            if check.ok { "ok" } else { "LOW" },
            check.free_gb,
            check.min_gb,
            dir.display()
        );
    }
    Ok(if check.ok { 0 } else { 1 })
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
        ("stale_quiet_refused", case(0, 0, false, true, 0, &[]), false),
        ("stale_milestone_refused", case(0, 0, true, true, 0, &[]), false),
        ("stale_over_threshold_refused", case(200, 5000, false, true, 0, &[]), false),
        ("quiet_turn_preserves_cache", case(10, 100, false, false, 0, &[]), false),
        ("just_under_thresholds", case(79, 1499, false, false, 0, &[]), false),
        ("at_run_threshold_approved", case(80, 0, false, false, 0, &[]), true),
        ("at_item_threshold_approved", case(0, 1500, false, false, 0, &[]), true),
        ("clean_milestone_approved", case(3, 12, true, false, 0, &["shipped U1"]), true),
        ("active_children_refused", case(200, 5000, true, false, 2, &[]), false),
        ("merge_conflict_hazard_refused", case(200, 5000, true, false, 0, &["merge conflict"]), false),
        ("mid_rollback_hazard_refused", case(200, 5000, false, false, 0, &["mid-rollback"]), false),
        ("in_progress_deploy_refused", case(200, 5000, false, false, 0, &["in-progress deploy"]), false),
    ];

    let mut passed = 0usize;
    let mut failures = Vec::new();
    for (name, inp, expected) in &cases {
        let verdict = judge.judge(inp, &cfg).await?;
        let actual = verdict.should_checkpoint
            && verdict.confidence >= cfg.checkpoint_confidence_threshold;
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
