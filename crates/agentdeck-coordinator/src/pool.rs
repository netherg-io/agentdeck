//! Coordinator Pool selection (ADR-0002, Q11, Q22): pick the healthy pool
//! entry with the fullest 5-hour window while strictly guarding the weekly
//! reserve, or sleep until the earliest blocking `resets_at + 2 min`.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::{CoordinatorConfig, CoordinatorPoolEntry};

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
    let s = s.trim();
    let (dt, tz) = if let Some(bare) = s.strip_suffix(['Z', 'z']) {
        (bare, 0i64)
    } else {
        let i = s.rfind(['+', '-']).filter(|i| *i >= 10)?;
        let (dt, off) = (&s[..i], &s[i..]);
        let sign: i64 = if off.starts_with('-') { -1 } else { 1 };
        let digits: String = off[1..].chars().filter(|c| *c != ':').collect();
        if digits.len() != 4 {
            return None;
        }
        let (h, m) = (digits[..2].parse::<i64>().ok()?, digits[2..].parse::<i64>().ok()?);
        (dt, sign * (h * 3600 + m * 60))
    };
    let (date, time) = dt.split_once(['T', 't', ' '])?;
    let d: Vec<i64> = date.split('-').map(|p| p.parse().unwrap_or(-1)).collect();
    let t: Vec<&str> = time.split(':').collect();
    if d.len() != 3 || t.len() < 2 {
        return None;
    }
    let secs: f64 = t[2].split('.').next().unwrap_or("0").parse().ok().unwrap_or(-1.0);
    // Days since 1970-01-01 (H. Hinnant's algorithm, inverse of utc()).
    let (mut y, m, day) = (d[0], d[1], d[2]);
    if !(1..=12).contains(&m) || !(1..=31).contains(&day) || !(0.0..=60.0).contains(&secs) {
        return None;
    }
    y -= i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let hh: i64 = t[0].parse().ok()?;
    let mm: i64 = t[1].parse().ok()?;
    if !(0..24).contains(&hh) || !(0..60).contains(&mm) {
        return None;
    }
    Some(days * 86_400 + hh * 3600 + mm * 60 + secs as i64 - tz)
}

/// Does a T3 usage window apply to `model`? Antigravity keeps separate
/// buckets (Argon week vs Gemini week vs third-party Claude 5h); generic
/// windows (`session`, `primary`, `weekly`) apply to every model.
fn window_matches_model(id: &str, kind: &str, label: &str, model: &str) -> bool {
    let text = format!("{id} {kind} {label}").to_lowercase();
    let model = model.to_lowercase();
    if text.contains("argon") {
        return model.contains("argon");
    }
    if text.contains("gemini") {
        return model.contains("gemini") && !model.contains("argon");
    }
    if text.contains("3p") || text.contains("third") {
        return !model.contains("gemini");
    }
    if text.contains("claude") || text.contains("codex") || text.contains("openai") {
        let family = if text.contains("codex") || text.contains("openai") {
            "gpt"
        } else {
            "claude"
        };
        return model.contains(family)
            || model.contains("sonnet")
            || model.contains("opus")
            || model.contains("haiku")
            || model.contains("fable");
    }
    let _ = model;
    true
}

/// Read `caches/<instance_id>.json` via `agentdeck-core` and extract the 5h
/// and weekly windows relevant to `model`. Missing cache file means healthy
/// with unknown percents (assumed full); an unreadable/corrupt cache or a
/// non-`ready` status means unhealthy.
pub fn quota_from_t3_cache(
    t3_root: &Path,
    instance_id: &str,
    model: &str,
) -> QuotaWindowView {
    let cache = match agentdeck_core::t3::read_usage_cache(t3_root, instance_id) {
        Ok(Some(c)) => c,
        Ok(None) => return QuotaWindowView::unknown_healthy(),
        Err(_) => {
            return QuotaWindowView {
                healthy: false,
                ..Default::default()
            };
        }
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

fn view_for(
    quotas: &HashMap<String, QuotaWindowView>,
    instance_id: &str,
) -> QuotaWindowView {
    quotas.get(instance_id).cloned().unwrap_or_else(QuotaWindowView::unknown_healthy)
}

fn eligible(cfg: &CoordinatorConfig, q: &QuotaWindowView) -> bool {
    q.healthy
        && q.weekly_remaining_pct.unwrap_or(100.0) >= cfg.min_weekly_reserve_pct
        && q.five_hour_remaining_pct.unwrap_or(100.0) > 1.0
}

/// Pick the pool entry with the fullest 5-hour window (tie-break: fullest
/// weekly, then keep `current_instance` when within 2% to preserve prompt
/// cache, else pool order). Exhausted pools sleep until the earliest
/// blocking `resets_at + quota_sleep_buffer_secs`.
pub fn select_coordinator(
    cfg: &CoordinatorConfig,
    quotas: &HashMap<String, QuotaWindowView>,
    now_epoch_secs: i64,
    current_instance: Option<&str>,
) -> PoolSelection {
    let enabled: Vec<&CoordinatorPoolEntry> =
        cfg.pool.iter().filter(|e| e.enabled).collect();
    if enabled.is_empty() {
        return PoolSelection::NoPoolConfigured;
    }
    let healthy: Vec<(&CoordinatorPoolEntry, QuotaWindowView)> = enabled
        .iter()
        .map(|e| (*e, view_for(quotas, &e.instance_id)))
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
        // Cache-preserving hysteresis: stay on the current instance when it
        // is eligible and within 2% of the best 5h window.
        if let Some(cur) = current_instance {
            let cq = view_for(quotas, cur);
            if cfg.pool.iter().any(|e| e.enabled && e.instance_id == cur) && eligible(cfg, &cq) {
                let cfive = cq.five_hour_remaining_pct.unwrap_or(100.0);
                let cweekly = cq.weekly_remaining_pct.unwrap_or(100.0);
                if five - cfive <= 2.0 {
                    let entry = cfg.pool.iter().find(|e| e.instance_id == cur).expect("checked above");
                    return PoolSelection::Selected {
                        entry: entry.clone(),
                        five_hour_pct: cfive,
                        weekly_pct: cweekly,
                        reason: format!(
                            "keeping {cur} (5h {cfive:.1}% within 2% of best {five:.1}%) to preserve prompt cache"
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
    // Nobody is eligible: find the earliest reset of a blocking window.
    let mut earliest: Option<(i64, CoordinatorPoolEntry)> = None;
    for (entry, q) in &healthy {
        let blocked_5h = q.five_hour_remaining_pct.unwrap_or(100.0) <= 1.0;
        let blocked_weekly =
            q.weekly_remaining_pct.unwrap_or(100.0) < cfg.min_weekly_reserve_pct;
        let mut resets: Vec<i64> = Vec::new();
        if blocked_5h {
            resets.extend(q.five_hour_resets_at);
        }
        if blocked_weekly {
            resets.extend(q.weekly_resets_at);
        }
        // A blocked window without a known reset still counts: fall back to now.
        if (blocked_5h || blocked_weekly) && resets.is_empty() {
            resets.push(now_epoch_secs);
        }
        for r in resets {
            let take = match earliest {
                None => true,
                Some((er, _)) => r < er,
            };
            if take {
                earliest = Some((r, (*entry).clone()));
            }
        }
    }
    match earliest {
        Some((reset, next)) => PoolSelection::SleepForQuota {
            wake_at_epoch_secs: reset + cfg.quota_sleep_buffer_secs as i64,
            earliest_reset_epoch_secs: Some(reset),
            next_candidate: Some(next.clone()),
            reason: format!(
                "all pool entries exhausted; earliest reset is {} at {reset}, waking +{}s",
                next.instance_id, cfg.quota_sleep_buffer_secs
            ),
        },
        None => PoolSelection::SleepForQuota {
            // No healthy entry and no known reset: retry after one buffer.
            wake_at_epoch_secs: now_epoch_secs + cfg.quota_sleep_buffer_secs as i64,
            earliest_reset_epoch_secs: None,
            next_candidate: None,
            reason: "no healthy pool entry with a known reset; retrying after sleep buffer".into(),
        },
    }
}

/// Proactive in-turn rotation: the current account is getting thin and a
/// meaningfully fuller pool entry exists (≥15 points of 5h headroom, so the
/// daemon does not flap between near-equal accounts).
pub fn should_proactively_rotate(
    cfg: &CoordinatorConfig,
    current_quota: &QuotaWindowView,
    best_other: &QuotaWindowView,
) -> bool {
    let cur_five = current_quota.five_hour_remaining_pct.unwrap_or(100.0);
    let cur_weekly = current_quota.weekly_remaining_pct.unwrap_or(100.0);
    let thin = cur_five < cfg.proactive_rotate_below_5h_pct
        || cur_weekly < cfg.min_weekly_reserve_pct;
    thin
        && best_other.healthy
        && best_other.five_hour_remaining_pct.unwrap_or(100.0) >= cur_five + 15.0
}
