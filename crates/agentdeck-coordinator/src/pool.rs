//! Coordinator Pool selection (ADR-0002, Q11, Q22): pick the healthy pool
//! entry with the fullest 5-hour window while strictly guarding the weekly
//! reserve, or sleep until the earliest blocking `resets_at + 2 min`.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::{CoordinatorConfig, CoordinatorPoolEntry};

const MAX_UTC_EPOCH: i64 = 253_402_300_799; // 9999-12-31T23:59:59Z

/// Key into the quota map: `(instance_id, model)`. Two pool entries on one
/// T3 instance (Claude on `3p-*` buckets vs Gemini on `gemini-*` buckets)
/// must NEVER share one quota view, so the model is part of the key.
pub type PoolKey = (String, String);

/// Build a [`PoolKey`] for one pool entry (or one observation).
pub fn pool_key(instance_id: &str, model: &str) -> PoolKey {
    (instance_id.to_string(), model.to_string())
}

/// Remaining-quota view for one pool entry, read from T3's usage cache.
/// `None` percents mean unknown (treated as full); `resets_at` is epoch secs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QuotaWindowView {
    pub five_hour_remaining_pct: Option<f64>,
    pub five_hour_resets_at: Option<i64>,
    pub weekly_remaining_pct: Option<f64>,
    pub weekly_resets_at: Option<i64>,
    pub healthy: bool,
}

impl QuotaWindowView {
    pub fn unknown_healthy() -> Self {
        Self {
            healthy: true,
            ..Default::default()
        }
    }

    pub fn exhausted(five_resets_at: Option<i64>, weekly_resets_at: Option<i64>) -> Self {
        Self {
            five_hour_remaining_pct: Some(0.0),
            five_hour_resets_at: five_resets_at,
            weekly_remaining_pct: Some(0.0),
            weekly_resets_at,
            healthy: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PoolSelection {
    Selected {
        entry: CoordinatorPoolEntry,
        five_hour_pct: f64,
        weekly_pct: f64,
        reason: String,
    },
    SleepForQuota {
        wake_at_epoch_secs: i64,
        earliest_reset_epoch_secs: Option<i64>,
        next_candidate: Option<CoordinatorPoolEntry>,
        reason: String,
    },
    NoPoolConfigured,
}

/// Parse an RFC-3339 instant (`...Z` or numeric offset, optional fraction)
/// to epoch seconds. T3 writes both `11:00:00Z` and `10:00:00.000Z`.
pub fn parse_epoch_rfc3339(s: &str) -> Option<i64> {
    fn field(s: &str, width: usize) -> Option<i64> {
        if s.len() != width || !s.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        s.parse().ok()
    }

    let s = s.trim();
    let (dt, tz) = if let Some(bare) = s.strip_suffix(['Z', 'z']) {
        (bare, 0i64)
    } else {
        let i = s.rfind(['+', '-']).filter(|i| *i >= 10)?;
        let (dt, offset) = s.split_at(i);
        let sign = if offset.starts_with('-') { -1 } else { 1 };
        let offset = &offset[1..];
        let hours = offset.get(..2)?;
        let minutes = match offset.len() {
            4 => offset.get(2..)?,
            5 => offset.get(2..)?.strip_prefix(':')?,
            _ => return None,
        };
        let (h, m) = (field(hours, 2)?, field(minutes, 2)?);
        if h > 23 || m > 59 {
            return None;
        }
        (
            dt,
            h.checked_mul(3600)?
                .checked_add(m.checked_mul(60)?)?
                .checked_mul(sign)?,
        )
    };
    let (date, time) = dt.split_once(['T', 't', ' '])?;
    let (y, date) = date.split_once('-')?;
    let (m, day) = date.split_once('-')?;
    let (y, m, day) = (field(y, 4)?, field(m, 2)?, field(day, 2)?);
    let (hh, time) = time.split_once(':')?;
    let (mm, secs) = time.split_once(':').unwrap_or((time, "00"));
    let (hh, mm) = (field(hh, 2)?, field(mm, 2)?);
    let (secs, fraction) = secs.split_once('.').unwrap_or((secs, "0"));
    if fraction.is_empty() || !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let secs = field(secs, 2)?;
    if !(1970..=9999).contains(&y)
        || !(1..=12).contains(&m)
        || !(1..=31).contains(&day)
        || hh >= 24
        || mm >= 60
        || !(0..=60).contains(&secs)
    {
        return None;
    }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let dim = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if leap { 29 } else { 28 }
        }
    };
    if day > dim {
        return None;
    }
    // Days since 1970-01-01 (H. Hinnant's algorithm, inverse of utc()).
    let y = y - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y.checked_sub(era.checked_mul(400)?)?;
    let mp = (m + 9) % 12;
    let doy = (mp.checked_mul(153)?.checked_add(2)? / 5).checked_add(day)? - 1;
    let doe = yoe
        .checked_mul(365)?
        .checked_add(yoe / 4 - yoe / 100)?
        .checked_add(doy)?;
    let days = era
        .checked_mul(146097)?
        .checked_add(doe)?
        .checked_sub(719468)?;
    days.checked_mul(86_400)?
        .checked_add(hh.checked_mul(3600)?)?
        .checked_add(mm.checked_mul(60)?)?
        .checked_add(secs)?
        .checked_sub(tz)
}

/// Does a T3 usage window apply to `model`? Antigravity keeps separate
/// buckets (Argon week vs Gemini week vs third-party Claude 5h); generic
/// windows (`session`, `primary`, `weekly`) apply to every model.
fn window_matches_model(id: &str, kind: &str, label: &str, model: &str) -> bool {
    let text = format!("{id} {kind} {label}").to_lowercase();
    let model = model.to_lowercase();
    if text.contains("codex") || text.contains("openai") {
        return model.contains("gpt") || model.contains("codex");
    }
    if text.contains("argon") {
        return model.contains("argon");
    }
    if text.contains("gemini") {
        return model.contains("gemini") && !model.contains("argon");
    }
    if text.contains("3p") || text.contains("third") {
        return !model.contains("gemini");
    }
    if text.contains("claude") {
        return model.contains("claude")
            || model.contains("sonnet")
            || model.contains("opus")
            || model.contains("haiku")
            || model.contains("fable");
    }
    true
}

/// Read `caches/<instance_id>.json` via `agentdeck-core` and extract the 5h
/// and weekly windows relevant to `model`. Only enabled T3 instances can be
/// healthy. For those, a missing cache means unknown quota (assumed full).
pub fn quota_from_t3_cache(t3_root: &Path, instance_id: &str, model: &str) -> QuotaWindowView {
    if !matches!(
        agentdeck_core::t3::read_instance_enabled(t3_root, instance_id),
        Ok(Some(true))
    ) {
        return QuotaWindowView::default();
    }
    let cache = match agentdeck_core::t3::read_usage_cache(t3_root, instance_id) {
        Ok(Some(c)) => c,
        Ok(None) => return QuotaWindowView::unknown_healthy(),
        Err(_) => return QuotaWindowView::default(),
    };
    let healthy = cache.status.as_deref() == Some("ready");
    let mut view = QuotaWindowView {
        healthy,
        ..Default::default()
    };
    let Some(limits) = cache.usage_limits else {
        return view;
    };
    // Several windows can share a duration (per-model buckets): keep the most
    // conservative remaining % among the ones matching this model.
    let mut five: Vec<(f64, Option<i64>)> = Vec::new();
    let mut weekly: Vec<(f64, Option<i64>)> = Vec::new();
    for w in &limits.windows {
        let Some(used) = w.used_percent else { continue };
        if !(0.0..=100.0).contains(&used) {
            continue;
        }
        if !window_matches_model(
            &w.id,
            w.kind.as_deref().unwrap_or(""),
            w.label.as_deref().unwrap_or(""),
            model,
        ) {
            continue;
        }
        let remaining = 100.0 - used;
        let resets = w.resets_at.as_deref().and_then(parse_epoch_rfc3339);
        let text = format!("{} {} {}", w.id, w.kind.as_deref().unwrap_or(""), w.label.as_deref().unwrap_or("")).to_lowercase();
        let is_weekly = w.window_duration_mins == Some(10080)
            || (w.window_duration_mins.is_none()
                && (text.contains("week") || text.contains("7d")));
        let is_five = w.window_duration_mins == Some(300)
            || (w.window_duration_mins.is_none()
                && (text.contains("session")
                    || text.contains("5h")
                    || text.contains("5-hour")
                    || text.contains("5_hour")));
        if is_weekly {
            weekly.push((remaining, resets));
        } else if is_five {
            five.push((remaining, resets));
        }
        // Windows with an unrelated duration (e.g. hourly caps) are ignored:
        // only the 5h and weekly windows gate rotation.
    }
    // shortcut: unknown windows stay unknown (assumed full), never 0.
    if let Some(best) = five.iter().min_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal)) {
        view.five_hour_remaining_pct = Some(best.0);
        view.five_hour_resets_at = best.1;
    }
    if let Some(best) = weekly.iter().min_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal)) {
        view.weekly_remaining_pct = Some(best.0);
        view.weekly_resets_at = best.1;
    }
    view
}

fn view_for(quotas: &HashMap<PoolKey, QuotaWindowView>, instance_id: &str, model: &str) -> QuotaWindowView {
    quotas
        .get(&pool_key(instance_id, model))
        .cloned()
        .unwrap_or_default()
}

/// Pool eligibility: the exact bar a rotation target must clear (healthy,
/// weekly reserve intact, 5h window above 1%). Both the reactive and the
/// proactive rotation paths filter through this one predicate.
pub fn eligible(cfg: &CoordinatorConfig, q: &QuotaWindowView) -> bool {
    q.healthy
        && q.weekly_remaining_pct.unwrap_or(100.0) >= cfg.min_weekly_reserve_pct
        && q.five_hour_remaining_pct.unwrap_or(100.0) > 1.0
}

/// Pick the pool entry with the fullest 5-hour window (tie-break: fullest
/// weekly, then keep `current` when within 2% to preserve prompt
/// cache, else pool order). Exhausted pools sleep until the earliest
/// future point at which a candidate clears every blocking window,
/// plus `quota_sleep_buffer_secs`; with no usable future reset the sleep
/// is a bounded `now + 300s` refresh.
pub fn select_coordinator(
    cfg: &CoordinatorConfig,
    quotas: &HashMap<PoolKey, QuotaWindowView>,
    now_epoch_secs: i64,
    current: Option<(&str, &str)>,
) -> PoolSelection {
    let enabled: Vec<&CoordinatorPoolEntry> =
        cfg.pool.iter().filter(|e| e.enabled).collect();
    if enabled.is_empty() {
        return PoolSelection::NoPoolConfigured;
    }
    let view = |e: &CoordinatorPoolEntry| view_for(quotas, &e.instance_id, &e.model);
    let healthy: Vec<(&CoordinatorPoolEntry, QuotaWindowView)> = enabled
        .iter()
        .map(|e| (*e, view(e)))
        .filter(|(_, q)| q.healthy)
        .collect();
    let mut best: Option<(&CoordinatorPoolEntry, f64, f64)> = None;
    for (entry, q) in &healthy {
        if !eligible(cfg, q) {
            continue;
        }
        let five = q.five_hour_remaining_pct.unwrap_or(100.0);
        let weekly = q.weekly_remaining_pct.unwrap_or(100.0);
        let take = match best {
            None => true,
            Some((_, bf, bw)) => five > bf || (five == bf && weekly > bw),
        };
        // Strict `>` keeps pool order on full ties.
        if take {
            best = Some((entry, five, weekly));
        }
    }
    if let Some((entry, five, weekly)) = best {
        // Cache-preserving hysteresis: stay on the current entry when it
        // is eligible and within 2% of the best 5h window.
        if let Some((cur_inst, cur_model)) = current {
            let cq = view_for(quotas, cur_inst, cur_model);
            if cfg
                .pool
                .iter()
                .any(|e| e.enabled && e.instance_id == cur_inst && e.model == cur_model)
                && eligible(cfg, &cq)
            {
                let cfive = cq.five_hour_remaining_pct.unwrap_or(100.0);
                let cweekly = cq.weekly_remaining_pct.unwrap_or(100.0);
                if five - cfive <= 2.0 {
                    let entry = cfg
                        .pool
                        .iter()
                        .find(|e| e.instance_id == cur_inst && e.model == cur_model)
                        .expect("checked above");
                    return PoolSelection::Selected {
                        entry: entry.clone(),
                        five_hour_pct: cfive,
                        weekly_pct: cweekly,
                        reason: format!(
                            "keeping {cur_inst}/{cur_model} (5h {cfive:.1}% within 2% of best {five:.1}%) to preserve prompt cache"
                        ),
                    };
                }
            }
        }
        return PoolSelection::Selected {
            entry: entry.clone(),
            five_hour_pct: five,
            weekly_pct: weekly,
            reason: format!(
                "fullest 5h window: {} at {five:.1}% (weekly {weekly:.1}%)",
                entry.instance_id
            ),
        };
    }
    // Nobody is eligible: find the earliest future point at which a blocked
    // candidate clears every window that blocks it. A candidate blocked by
    // both the 5h and the weekly window cannot run when only the earlier
    // window resets, so its usable reset is the later of the two. Stale
    // (`<= now`) and unknown (`None`) resets are never usable: a blocked
    // window without a credible future reset disqualifies the candidate.
    let mut earliest: Option<(i64, CoordinatorPoolEntry)> = None;
    for (entry, q) in &healthy {
        let blocked_5h = q.five_hour_remaining_pct.unwrap_or(100.0) <= 1.0;
        let blocked_weekly =
            q.weekly_remaining_pct.unwrap_or(100.0) < cfg.min_weekly_reserve_pct;
        if !blocked_5h && !blocked_weekly {
            continue;
        }
        let mut resets: Vec<i64> = Vec::new();
        if blocked_5h {
            resets.extend(q.five_hour_resets_at.filter(|r| *r > now_epoch_secs));
        }
        if blocked_weekly {
            resets.extend(q.weekly_resets_at.filter(|r| *r > now_epoch_secs));
        }
        let want = usize::from(blocked_5h) + usize::from(blocked_weekly);
        if resets.len() < want {
            continue;
        }
        let usable = resets.into_iter().max().expect("want >= 1");
        let take = match earliest {
            None => true,
            Some((er, _)) => usable < er,
        };
        if take {
            earliest = Some((usable, (*entry).clone()));
        }
    }
    let earliest = earliest.and_then(|(reset, next)| {
        let wake = reset.checked_add(i64::try_from(cfg.quota_sleep_buffer_secs).ok()?)?;
        (wake <= MAX_UTC_EPOCH).then_some((wake, reset, next))
    });
    match earliest {
        Some((wake, reset, next)) => PoolSelection::SleepForQuota {
            wake_at_epoch_secs: wake,
            earliest_reset_epoch_secs: Some(reset),
            next_candidate: Some(next.clone()),
            reason: format!(
                "all pool entries exhausted; earliest usable reset is {} at {reset}, waking +{}s",
                next.instance_id, cfg.quota_sleep_buffer_secs
            ),
        },
        None => PoolSelection::SleepForQuota {
            // Keep the refresh deadline within the persisted RFC3339 range.
            wake_at_epoch_secs: now_epoch_secs.saturating_add(300).min(MAX_UTC_EPOCH),
            earliest_reset_epoch_secs: None,
            next_candidate: None,
            reason: "no pool entry has a usable future reset; retrying after a bounded refresh".into(),
        },
    }
}

/// Proactive rotation requires an eligible target and either a weekly
/// reserve breach or a thin 5h window with at least 15 points of headroom.
pub fn should_proactively_rotate(
    cfg: &CoordinatorConfig,
    current_quota: &QuotaWindowView,
    best_other: &QuotaWindowView,
) -> bool {
    let cur_five = current_quota.five_hour_remaining_pct.unwrap_or(100.0);
    let cur_weekly = current_quota.weekly_remaining_pct.unwrap_or(100.0);
    eligible(cfg, best_other)
        && (cur_weekly < cfg.min_weekly_reserve_pct
            || (cur_five < cfg.proactive_rotate_below_5h_pct
                && best_other.five_hour_remaining_pct.unwrap_or(100.0) >= cur_five + 15.0))
}
