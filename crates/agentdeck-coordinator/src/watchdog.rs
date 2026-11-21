//! Reactive Watchdog tick engine (ADR-0002, Q6, Q16, Q22): pure decision
//! function over a run snapshot + a read-only thread observation, plus a
//! debounced executor that drives the loopback T3 controller.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::checkpoint::CheckpointVerdict;
use crate::config::{CoordinatorConfig, CoordinatorPoolEntry};
use crate::pool::{
    PoolKey, PoolSelection, QuotaWindowView, eligible, pool_key, select_coordinator,
    should_proactively_rotate,
};
use crate::t3_control::{ObservedThread, T3ThreadController};

/// Phase of an Autonomous Run as tracked by the daemon (mirrors the
/// `thread_state` expansion Wave 2 maps from).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunPhase {
    Running,
    SleepingForQuota,
    Paused,
    Finished,
}

/// Daemon-side snapshot of the run. Wave 2 builds this from `ThreadState`
/// (phase, sleep deadline, unblocked-work check over checklist vs Owner Inbox).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunSnapshot {
    pub thread_id: String,
    pub phase: RunPhase,
    pub sleep_until_epoch: Option<i64>,
    pub has_unblocked_work: bool,
}

/// Everything one Watchdog tick needs. `quotas` maps each pool
/// `(instance_id, model)` to its current window view (built from
/// `quota_from_t3_cache`).
#[derive(Debug, Clone)]
pub struct WatchdogThreadInput {
    pub run: RunSnapshot,
    pub observed: ObservedThread,
    pub quotas: HashMap<PoolKey, QuotaWindowView>,
    pub now_epoch_secs: i64,
    pub checkpoint_verdict: Option<CheckpointVerdict>,
}

/// Pure Watchdog decision. No I/O, no clock reads: fully unit-testable.
#[derive(Debug, Clone, PartialEq)]
pub enum WatchdogAction {
    Noop {
        reason: String,
    },
    RotateAndWake {
        thread_id: String,
        to_entry: CoordinatorPoolEntry,
        fresh_context: bool,
        message: String,
        reason: String,
    },
    Wake {
        thread_id: String,
        fresh_context: bool,
        message: String,
        reason: String,
    },
    EnterQuotaSleep {
        thread_id: String,
        wake_at_epoch_secs: i64,
        reason: String,
    },
    TriggerCheckpoint {
        thread_id: String,
        target: Option<CoordinatorPoolEntry>,
        reason: String,
    },
}

impl WatchdogAction {
    /// Stable kind key for the debounce guard.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Noop { .. } => "noop",
            Self::RotateAndWake { .. } => "rotate-wake",
            Self::Wake { .. } => "wake",
            Self::EnterQuotaSleep { .. } => "quota-sleep",
            Self::TriggerCheckpoint { .. } => "checkpoint",
        }
    }
}

/// The observed `(instance_id, model)` pair, when both are known.
fn current_key(observed: &ObservedThread) -> Option<(&str, &str)> {
    Some((
        observed.provider_instance_id.as_deref()?,
        observed.model.as_deref()?,
    ))
}

/// Quotas with the just-failed `(instance_id, model)` marked ineligible at
/// 0% 5h, so reactive rotation NEVER restarts the account that just failed
/// even if its T3 cache is stale. A credible future reset already in the
/// cache is kept; a stale or unknown one is dropped so it can never produce
/// a premature wake.
fn with_failed_marked_ineligible(
    quotas: &HashMap<PoolKey, QuotaWindowView>,
    observed: &ObservedThread,
    now_epoch_secs: i64,
) -> HashMap<PoolKey, QuotaWindowView> {
    let mut adjusted = quotas.clone();
    let keys: Vec<PoolKey> = match (
        observed.provider_instance_id.as_deref(),
        observed.model.as_deref(),
    ) {
        (Some(inst), Some(model)) => vec![pool_key(inst, model)],
        (Some(inst), None) => adjusted
            .keys()
            .filter(|(i, _)| i == inst)
            .cloned()
            .collect(),
        _ => Vec::new(),
    };
    for key in keys {
        let mut v = adjusted
            .get(&key)
            .cloned()
            .unwrap_or_else(QuotaWindowView::unknown_healthy);
        v.five_hour_remaining_pct = Some(0.0);
        if v.five_hour_resets_at.is_none_or(|r| r <= now_epoch_secs) {
            v.five_hour_resets_at = None;
        }
        adjusted.insert(key, v);
    }
    adjusted
}

pub const WAKE_MESSAGE: &str = "Watchdog wake: unblocked checklist work remains. Read state_read and continue the Autonomous Run.";
pub const QUOTA_ROTATE_MESSAGE: &str = "Watchdog: your previous account hit its quota limit. The run now continues on a fresh account. Read state_read and continue the Autonomous Run from saved state; do not retry failed dispatches, recover their taskIds from parent activity first.";
pub const CHECKPOINT_MESSAGE: &str = "Watchdog: this is a clean Checkpoint boundary. Persist authoritative state via state_write (goal, scope, done, next), then start a fresh context window and continue the Autonomous Run.";

fn rotate(
    thread_id: &str,
    entry: CoordinatorPoolEntry,
    reason: String,
    quota: bool,
) -> WatchdogAction {
    let id = entry.instance_id.clone();
    WatchdogAction::RotateAndWake {
        thread_id: thread_id.to_string(),
        to_entry: entry,
        fresh_context: false,
        message: if quota {
            QUOTA_ROTATE_MESSAGE.to_string()
        } else {
            format!(
                "Watchdog: rotating Coordinator to {id} ({reason}). Read state_read and continue the Autonomous Run."
            )
        },
        reason,
    }
}

fn sleep_action(
    thread_id: &str,
    sel: PoolSelection,
    now_epoch_secs: i64,
) -> WatchdogAction {
    match sel {
        PoolSelection::SleepForQuota {
            wake_at_epoch_secs,
            reason,
            ..
        } => WatchdogAction::EnterQuotaSleep {
            thread_id: thread_id.to_string(),
            wake_at_epoch_secs,
            reason,
        },
        PoolSelection::NoPoolConfigured => WatchdogAction::Noop {
            reason: "quota limit hit but no Coordinator Pool is configured; owner must intervene".into(),
        },
        PoolSelection::Selected { .. } => WatchdogAction::Noop {
            reason: format!("quota selection recovered at tick {now_epoch_secs}; re-tick to rotate"),
        },
    }
}

pub fn decide_action(
    cfg: &CoordinatorConfig,
    run: &RunSnapshot,
    observed: &ObservedThread,
    quotas: &HashMap<PoolKey, QuotaWindowView>,
    now_epoch_secs: i64,
    checkpoint_verdict: Option<&CheckpointVerdict>,
) -> WatchdogAction {
    let noop = |reason: &str| WatchdogAction::Noop {
        reason: reason.to_string(),
    };
    if !matches!(run.phase, RunPhase::Running | RunPhase::SleepingForQuota) {
        return noop("run is neither running nor sleeping-for-quota");
    }
    // Active-run and unblocked-work guards run before the quota-sleep wake
    // branch: work that completed or was parked in Owner Inbox while
    // sleeping (or a thread that already resumed) must Noop, never wake.
    if observed.is_running {
        return noop("active run in progress");
    }
    if !run.has_unblocked_work {
        return noop("no unblocked work: remaining items are done, held, or parked in Owner Inbox");
    }
    if run.phase == RunPhase::SleepingForQuota {
        if run.sleep_until_epoch.is_some_and(|until| now_epoch_secs < until) {
            return noop("still inside quota sleep window");
        }
        // Sleep over: rotate onto the first eligible entry, else re-sleep.
        return match select_coordinator(cfg, quotas, now_epoch_secs, current_key(observed)) {
            PoolSelection::Selected { entry, reason, .. } => rotate(
                &run.thread_id,
                entry,
                format!("quota sleep over: {reason}"),
                false,
            ),
            sel @ PoolSelection::SleepForQuota { .. } => {
                sleep_action(&run.thread_id, sel, now_epoch_secs)
            }
            PoolSelection::NoPoolConfigured => {
                noop("quota sleep over but no Coordinator Pool is configured")
            }
        };
    }
    if observed.hit_quota_limit {
        // The observed failure takes effect BEFORE selection: the just-failed
        // account is ineligible even when its T3 cache is stale.
        let adjusted = with_failed_marked_ineligible(quotas, observed, now_epoch_secs);
        return match select_coordinator(
            cfg,
            &adjusted,
            now_epoch_secs,
            current_key(observed),
        ) {
            PoolSelection::Selected { entry, five_hour_pct, weekly_pct, .. } => rotate(
                &run.thread_id,
                entry.clone(),
                format!(
                    "quota limit on {}: rotating to {} (5h {five_hour_pct:.1}%, weekly {weekly_pct:.1}%)",
                    observed.provider_instance_id.as_deref().unwrap_or("?"),
                    entry.instance_id,
                ),
                true,
            ),
            sel @ PoolSelection::SleepForQuota { .. } => {
                sleep_action(&run.thread_id, sel, now_epoch_secs)
            }
            PoolSelection::NoPoolConfigured => {
                noop("quota limit hit but no Coordinator Pool is configured; owner must intervene")
            }
        };
    }
    if observed.pending_background_tasks > 0 {
        return noop("background tasks are running and will wake the thread");
    }
    if now_epoch_secs - observed.updated_at_epoch < cfg.idle_wake_grace_secs as i64 {
        return noop("thread updated recently; inside idle grace period");
    }
    // Silent with unblocked work: prefer proactive rotation, then an approved
    // Checkpoint, then a plain wake.
    if let Some((cur_inst, cur_model)) = current_key(observed)
        && let Some(cur_q) = quotas.get(&pool_key(cur_inst, cur_model))
    {
        let mut best_other: Option<(&CoordinatorPoolEntry, &QuotaWindowView)> = None;
        for entry in cfg.pool.iter().filter(|e| {
            e.enabled && (e.instance_id != cur_inst || e.model != cur_model)
        }) {
            let Some(q) = quotas.get(&pool_key(&entry.instance_id, &entry.model)) else {
                continue;
            };
            // The exact pool eligibility bar: an unhealthy or 0%-weekly
            // candidate can never beat or hide a healthy eligible runner-up.
            if !eligible(cfg, q) {
                continue;
            }
            let five = q.five_hour_remaining_pct.unwrap_or(100.0);
            let take = match best_other {
                None => true,
                Some((_, bq)) => {
                    five > bq.five_hour_remaining_pct.unwrap_or(100.0)
                }
            };
            if take {
                best_other = Some((entry, q));
            }
        }
        if let Some((entry, other_q)) = best_other
            && should_proactively_rotate(cfg, cur_q, other_q)
        {
            return rotate(
                &run.thread_id,
                entry.clone(),
                format!(
                    "proactive rotation: {cur_inst} is thin, {} is fuller",
                    entry.instance_id
                ),
                false,
            );
        }
    }
    // A stale-state verdict always refuses, so `should_checkpoint &&
    // confidence >= threshold` already implies freshness.
    if let Some(v) = checkpoint_verdict
        && v.should_checkpoint
        && v.confidence >= cfg.checkpoint_confidence_threshold
    {
        return WatchdogAction::TriggerCheckpoint {
            thread_id: run.thread_id.clone(),
            target: None,
            reason: format!("judge approved checkpoint: {}", v.reason),
        };
    }
    WatchdogAction::Wake {
        thread_id: run.thread_id.clone(),
        fresh_context: false,
        message: WAKE_MESSAGE.to_string(),
        reason: "silent thread with unblocked work past idle grace".to_string(),
    }
}

/// Convenience wrapper over [`decide_action`] for a bundled tick input.
pub fn decide(cfg: &CoordinatorConfig, input: &WatchdogThreadInput) -> WatchdogAction {
    decide_action(
        cfg,
        &input.run,
        &input.observed,
        &input.quotas,
        input.now_epoch_secs,
        input.checkpoint_verdict.as_ref(),
    )
}

/// Debounce guard so the daemon never double-wakes the same thread within a
/// cooldown window (e.g. two ticks racing one silent thread).
#[derive(Debug, Default)]
pub struct ActionDebounce {
    window_secs: u64,
    last: HashMap<(String, String), i64>,
}

impl ActionDebounce {
    pub fn new(window_secs: u64) -> Self {
        Self {
            window_secs,
            last: HashMap::new(),
        }
    }

    /// `true` when this (thread, action-kind) fired recently and should be
    /// skipped. Noop and quota-sleep updates are never debounced: they are
    /// cheap and idempotent.
    pub fn should_skip(&mut self, action: &WatchdogAction, thread_id: &str, now_epoch_secs: i64) -> bool {
        if matches!(
            action,
            WatchdogAction::Noop { .. } | WatchdogAction::EnterQuotaSleep { .. }
        ) {
            return false;
        }
        let key = (thread_id.to_string(), action.kind().to_string());
        if let Some(last) = self.last.get(&key)
            && now_epoch_secs - *last < self.window_secs as i64
        {
            return true;
        }
        self.last.insert(key, now_epoch_secs);
        // Bounded growth: drop entries older than the window.
        let window = self.window_secs as i64;
        self.last.retain(|_, t| now_epoch_secs - *t < window * 4);
        false
    }
}

/// Execute one decided action against the loopback controller. Returns
/// `Ok(true)` when a T3 call (or a sleep transition the daemon must persist)
/// was issued, `Ok(false)` for Noop or a debounced repeat.
pub async fn execute_action<C: T3ThreadController>(
    controller: &C,
    action: &WatchdogAction,
    debounce: &mut ActionDebounce,
    now_epoch_secs: i64,
) -> anyhow::Result<bool> {
    match action {
        WatchdogAction::Noop { .. } => Ok(false),
        WatchdogAction::RotateAndWake {
            thread_id,
            to_entry,
            fresh_context,
            message,
            ..
        } => {
            if debounce.should_skip(action, thread_id, now_epoch_secs) {
                return Ok(false);
            }
            controller
                .configure_thread(thread_id, &to_entry.instance_id, &to_entry.model, &to_entry.options_map())
                .await?;
            controller.send_message(thread_id, message, *fresh_context).await?;
            Ok(true)
        }
        WatchdogAction::Wake {
            thread_id,
            fresh_context,
            message,
            ..
        } => {
            if debounce.should_skip(action, thread_id, now_epoch_secs) {
                return Ok(false);
            }
            controller.send_message(thread_id, message, *fresh_context).await?;
            Ok(true)
        }
        WatchdogAction::EnterQuotaSleep { .. } => {
            // No T3 call: the daemon persists the new sleep deadline itself.
            Ok(true)
        }
        WatchdogAction::TriggerCheckpoint {
            thread_id,
            target,
            reason,
        } => {
            if debounce.should_skip(action, thread_id, now_epoch_secs) {
                return Ok(false);
            }
            let message = match target {
                Some(t) => {
                    controller
                        .configure_thread(thread_id, &t.instance_id, &t.model, &t.options_map())
                        .await?;
                    format!("{} (continuing on {})", CHECKPOINT_MESSAGE, t.instance_id)
                }
                None => CHECKPOINT_MESSAGE.to_string(),
            };
            // In-context instruction: the Coordinator needs its context to
            // write state before any reset, so never fresh here.
            controller.send_message(thread_id, &format!("{message}\nReason: {reason}"), false).await?;
            Ok(true)
        }
    }
}
