use std::path::Path;

use anyhow::Result;

use agentdeck_core::t3::{
    T3ProviderCache, T3UsageWindow, read_instance_enabled, read_usage_cache, t3_root,
};

use crate::model::{AccountState, FIVE_HOUR_MINS, QuotaWindow, WEEKLY_MINS, label_for_duration};

const MONTHLY_MINS: u64 = 30 * 24 * 60;
const WINDOW_SUFFIXES: [&str; 5] = ["5h", "five_hour", "five-hour", "weekly", "week"];

pub struct T3Source<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub provider: &'static str,
    pub instance: Option<&'a str>,
    pub explicit_plan: Option<&'a str>,
}

/// Resolve a `[[usage]]` entry to its T3 `providerInstanceId`.
///
/// `opencode-go` defaults to T3's `"opencode"` instance when `instance` is
/// omitted (matching `agentdeck-core::router`), so live configs keep working
/// without manual edits.
pub fn resolve_instance_slug(provider: &str, instance: Option<&str>) -> Option<String> {
    if let Some(slug) = instance.map(str::trim).filter(|s| !s.is_empty()) {
        return Some(slug.to_owned());
    }
    if provider == "opencode-go" {
        return Some("opencode".to_owned());
    }
    None
}

pub async fn fetch(source: T3Source<'_>, stale_seconds: i64) -> Result<AccountState> {
    let root = match t3_root() {
        Ok(root) => root,
        Err(err) => {
            return Ok(AccountState::error(
                source.id,
                source.name,
                source.provider,
                format!("{err}; set HOME or AGENTDECK_T3_DIR"),
            ));
        }
    };
    Ok(fetch_from_root(
        &root,
        source,
        stale_seconds,
        crate::util::unix_now(),
    ))
}

pub fn fetch_from_root(
    t3_root: &Path,
    source: T3Source<'_>,
    stale_seconds: i64,
    now: i64,
) -> AccountState {
    let make_state = |status: &str,
                      plan: Option<String>,
                      email: Option<String>,
                      windows: Vec<QuotaWindow>,
                      error: Option<String>,
                      updated_at: i64| AccountState {
        id: source.id.to_owned(),
        name: source.name.to_owned(),
        provider: source.provider.to_owned(),
        status: status.to_owned(),
        plan,
        email,
        model: None,
        windows,
        balances: Vec::new(),
        error,
        updated_at,
    };

    let Some(instance_id) = resolve_instance_slug(source.provider, source.instance) else {
        return AccountState::error(
            source.id,
            source.name,
            source.provider,
            format!(
                "Usage source {:?} ({}) has no T3 instance; set `instance = \"<t3-id>\"` in instances.toml",
                source.id, source.provider
            ),
        );
    };

    match read_instance_enabled(t3_root, &instance_id) {
        Err(err) => {
            return AccountState::error(
                source.id,
                source.name,
                source.provider,
                format!("{err}; check T3 Code installation and userdata/settings.json"),
            );
        }
        Ok(None) => {
            let msg = if source.provider == "opencode-go"
                && source.instance.is_none_or(|s| s.trim().is_empty())
            {
                "T3 instance \"opencode\" not found in T3 settings.json; configure opencode in T3 Code or set `instance` in instances.toml".to_owned()
            } else {
                format!(
                    "T3 instance {instance_id:?} not found in T3 settings.json; update `instance` in instances.toml or configure it in T3 Code"
                )
            };
            return AccountState::error(source.id, source.name, source.provider, msg);
        }
        Ok(Some(false)) => {
            return AccountState::error(
                source.id,
                source.name,
                source.provider,
                format!(
                    "T3 instance {instance_id:?} is disabled in T3 settings.json; enable it in T3 Code"
                ),
            );
        }
        Ok(Some(true)) => {}
    }

    let cache = match read_usage_cache(t3_root, &instance_id) {
        Err(err) => {
            return AccountState::error(
                source.id,
                source.name,
                source.provider,
                format!("{err}; wait for T3 Code to finish writing cache"),
            );
        }
        Ok(None) => {
            return make_state(
                "waiting",
                source.explicit_plan.map(str::to_owned),
                None,
                Vec::new(),
                Some(format!(
                    "T3 cache missing for enabled instance {instance_id:?}; open T3 Code or trigger a health refresh"
                )),
                now,
            );
        }
        Ok(Some(cache)) => cache,
    };

    let email = cache
        .auth
        .as_ref()
        .and_then(|a| a.email.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let plan = provider_plan(source.provider, source.explicit_plan, &cache);

    let auth_status = cache.auth.as_ref().and_then(|a| a.status.as_deref());
    let cache_status = cache.status.as_deref();
    let detail = cache
        .message
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    if auth_status == Some("unauthenticated") || cache_status == Some("unauthenticated") {
        let msg = detail.map_or_else(
            || format!("T3 instance {instance_id:?} is unauthenticated; sign in via T3 Code"),
            |d| {
                format!(
                    "T3 instance {instance_id:?} is unauthenticated ({d}); sign in via T3 Code"
                )
            },
        );
        return make_state("unauthenticated", plan, email, Vec::new(), Some(msg), now);
    }
    if let Some(st) = cache_status.filter(|s| matches!(*s, "error" | "unavailable" | "disabled")) {
        let msg = detail.map_or_else(
            || format!("T3 instance {instance_id:?} has status {st}; check it in T3 Code"),
            |d| format!("T3 instance {instance_id:?} error ({st}): {d}; check it in T3 Code"),
        );
        return make_state("error", plan, email, Vec::new(), Some(msg), now);
    }

    let Some(limits) = cache.usage_limits else {
        let msg = detail.map_or_else(
            || format!("T3 cache for {instance_id:?} has no usageLimits yet; wait for T3 health refresh"),
            |d| format!("T3 cache for {instance_id:?} has no usageLimits ({d}); wait for T3 health refresh"),
        );
        return make_state("waiting", plan, email, Vec::new(), Some(msg), now);
    };

    let windows = map_windows(source.provider, limits.windows);
    if windows.is_empty()
        && let Some(unavail) = limits.unavailable
    {
        let reason = unavail
            .reason
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("unknown");
        let msg = match unavail
            .message
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(extra) => {
                format!("T3 reports no subscription limits for this instance: {reason} ({extra})")
            }
            None => format!("T3 reports no subscription limits for this instance: {reason}"),
        };
        return make_state("error", plan, email, Vec::new(), Some(msg), now);
    }

    let Some(checked_at) = limits
        .checked_at
        .as_deref()
        .or(cache.checked_at.as_deref())
        .and_then(parse_rfc3339)
    else {
        return AccountState::error(
            source.id,
            source.name,
            source.provider,
            format!(
                "T3 cache for {instance_id:?} has missing or invalid usageLimits.checkedAt; wait for T3 health refresh"
            ),
        );
    };

    if windows.is_empty() {
        return make_state(
            "waiting",
            plan,
            email,
            Vec::new(),
            Some(format!(
                "T3 cache for {instance_id:?} has empty usageLimits.windows; wait for T3 health refresh"
            )),
            checked_at,
        );
    }

    let age = now.saturating_sub(checked_at);
    if checked_at > now {
        return make_state(
            "stale",
            plan,
            email,
            windows,
            Some(format!(
                "T3 cache timestamp for {instance_id:?} is in the future; wait for T3 health refresh"
            )),
            checked_at,
        );
    }
    if age > stale_seconds {
        return make_state(
            "stale",
            plan,
            email,
            windows,
            Some(format!(
                "T3 cache for {instance_id:?} updated {} ago; wait for T3 health refresh or check T3 Code",
                crate::providers::claude::humanize_age(age)
            )),
            checked_at,
        );
    }

    let depleted =
        source.provider == "opencode-go" && windows.iter().any(|w| w.remaining_percent <= 0.0);
    make_state(
        if depleted { "depleted" } else { "ok" },
        plan,
        email,
        windows,
        None,
        checked_at,
    )
}

fn provider_plan(
    provider: &str,
    explicit_plan: Option<&str>,
    cache: &T3ProviderCache,
) -> Option<String> {
    if let Some(p) = explicit_plan.map(str::trim).filter(|s| !s.is_empty()) {
        return Some(p.to_owned());
    }
    match provider {
        "claude" => claude_plan(cache),
        "codex" => codex_plan(cache),
        "antigravity" => Some("Google AI Pro".to_owned()),
        "opencode-go" => Some("Go".to_owned()),
        _ => None,
    }
}

fn claude_plan(cache: &T3ProviderCache) -> Option<String> {
    let auth = cache.auth.as_ref()?;
    for raw in [&auth.label, &auth.auth_type].into_iter().flatten() {
        let mut s = raw.trim();
        for prefix in ["Claude ", "claude "] {
            if let Some(rest) = s.strip_prefix(prefix) {
                s = rest.trim();
            }
        }
        for suffix in [" Subscription", " subscription"] {
            if let Some(rest) = s.strip_suffix(suffix) {
                s = rest.trim();
            }
        }
        if s.is_empty()
            || matches!(
                s.to_ascii_lowercase().as_str(),
                "claude" | "subscription" | "ai" | "default" | "oauth"
            )
        {
            continue;
        }
        if s.contains('_') || s.contains('-') {
            let h = crate::util::humanize_plan(s);
            if !h.is_empty() {
                return Some(h);
            }
        }
        return Some(s.to_owned());
    }
    None
}

fn codex_plan(cache: &T3ProviderCache) -> Option<String> {
    let auth = cache.auth.as_ref()?;
    for raw in [&auth.label, &auth.auth_type].into_iter().flatten() {
        let mut s = raw.trim();
        for prefix in ["ChatGPT ", "chatgpt "] {
            if let Some(rest) = s.strip_prefix(prefix) {
                s = rest.trim();
            }
        }
        for suffix in [" Subscription", " subscription"] {
            if let Some(rest) = s.strip_suffix(suffix) {
                s = rest.trim();
            }
        }
        let lower = s.to_ascii_lowercase();
        if s.is_empty() || matches!(lower.as_str(), "chatgpt" | "api" | "api_key" | "apikey") {
            continue;
        }
        let normalized = match lower.as_str() {
            "pro" => "Pro 20x",
            "prolite" | "pro lite" | "pro_lite" | "pro-lite" => "Pro 5x",
            "team" | "self_serve_business_usage_based" => "Business",
            "self_serve_business_prolite" => "Business Premium",
            "business" | "ent26" | "enterprise_cbp_usage_based" => "Enterprise",
            "enterprise_cbp_automation" => "Enterprise (Automation)",
            "edu_plus" | "edu plus" => "Edu Plus",
            "edu_pro" | "edu pro" => "Edu Pro",
            "plus" => "Plus",
            "free" => "Free",
            "go" => "Go",
            _ if s.contains('_') || s.contains('-') => {
                let h = crate::util::humanize_plan(s);
                if !h.is_empty() {
                    return Some(h);
                }
                continue;
            }
            _ => return Some(s.to_owned()),
        };
        return Some(normalized.to_owned());
    }
    None
}

fn map_windows(provider: &str, raw: Vec<T3UsageWindow>) -> Vec<QuotaWindow> {
    let mut windows: Vec<QuotaWindow> = raw
        .into_iter()
        .filter_map(|w| match provider {
            "claude" => map_claude_window(w),
            "codex" => map_codex_window(w),
            "antigravity" => map_antigravity_window(w),
            "opencode-go" => map_opencode_go_window(w),
            _ => map_passthrough_window(w),
        })
        .collect();

    match provider {
        "claude" | "codex" => {
            windows.sort_by(|a, b| {
                a.duration_mins
                    .unwrap_or(u64::MAX)
                    .cmp(&b.duration_mins.unwrap_or(u64::MAX))
                    .then_with(|| a.key.cmp(&b.key))
            });
        }
        "antigravity" => {
            windows.sort_by(|a, b| {
                let pa = u8::from(family_for_key(&a.key) != "gemini");
                let pb = u8::from(family_for_key(&b.key) != "gemini");
                pa.cmp(&pb)
                    .then_with(|| {
                        a.duration_mins
                            .unwrap_or(u64::MAX)
                            .cmp(&b.duration_mins.unwrap_or(u64::MAX))
                    })
                    .then_with(|| a.key.cmp(&b.key))
            });
        }
        _ => {}
    }
    windows
}

fn usable_percent(w: &T3UsageWindow) -> Option<f64> {
    w.used_percent.filter(|p| p.is_finite())
}

fn resets_at(w: &T3UsageWindow) -> Option<i64> {
    w.resets_at.as_deref().and_then(parse_rfc3339)
}

fn fallback_label(w: &T3UsageWindow) -> String {
    if let Some(label) = w.label.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        return label.to_owned();
    }
    let h = title_case(&w.id);
    if h.is_empty() {
        "Лимит".to_owned()
    } else {
        h
    }
}

fn map_passthrough_window(w: T3UsageWindow) -> Option<QuotaWindow> {
    let used = usable_percent(&w)?;
    let resets = resets_at(&w);
    let label = fallback_label(&w);
    Some(QuotaWindow::new(
        w.id,
        label,
        used,
        w.window_duration_mins,
        resets,
    ))
}

fn map_claude_window(w: T3UsageWindow) -> Option<QuotaWindow> {
    let used = usable_percent(&w)?;
    let resets = resets_at(&w);
    match w.id.as_str() {
        "five_hour" => Some(QuotaWindow::new(
            "session",
            "5 часов",
            used,
            w.window_duration_mins.or(Some(FIVE_HOUR_MINS)),
            resets,
        )),
        "seven_day" => Some(QuotaWindow::new(
            "weekly_all",
            "Неделя",
            used,
            w.window_duration_mins.or(Some(WEEKLY_MINS)),
            resets,
        )),
        id => {
            if let Some(model_slug) = id.strip_prefix("seven_day_").filter(|s| !s.is_empty()) {
                let scope = w
                    .label
                    .as_deref()
                    .and_then(|l| l.split_once("·").map(|(_, r)| r.trim()))
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .unwrap_or_else(|| title_case(model_slug));
                let key = format!("weekly_scoped:{}", model_slug.to_ascii_lowercase());
                let label = format!("Неделя · {scope}");
                Some(
                    QuotaWindow::new(
                        key,
                        label,
                        used,
                        w.window_duration_mins.or(Some(WEEKLY_MINS)),
                        resets,
                    )
                    .with_scope(Some(scope)),
                )
            } else {
                map_passthrough_window(w)
            }
        }
    }
}

fn map_codex_window(w: T3UsageWindow) -> Option<QuotaWindow> {
    let used = usable_percent(&w)?;
    let resets = resets_at(&w);
    match w.id.as_str() {
        "primary" | "secondary" => {
            let duration = w.window_duration_mins;
            Some(QuotaWindow::new(
                w.id,
                label_for_duration(duration),
                used,
                duration,
                resets,
            ))
        }
        _ => map_passthrough_window(w),
    }
}

fn map_antigravity_window(w: T3UsageWindow) -> Option<QuotaWindow> {
    let used = usable_percent(&w)?;
    let resets = resets_at(&w);
    let duration = w
        .window_duration_mins
        .or_else(|| agy_duration_for_key(&w.id));
    let family = family_for_key(&w.id);
    let scope = if family == "gemini" {
        None
    } else {
        scope_for_family(&family)
    };

    let label = match (&scope, duration, family.as_str()) {
        (Some(s), _, _) => format!("{s} · {}", label_for_duration(duration)),
        (None, Some(_), "gemini") => format!("Gemini · {}", label_for_duration(duration)),
        (None, Some(_), _) => format!("{} · {}", title_case(&family), label_for_duration(duration)),
        (None, None, _) => fallback_label(&w),
    };

    Some(QuotaWindow::new(w.id, label, used, duration, resets).with_scope(scope))
}

fn map_opencode_go_window(w: T3UsageWindow) -> Option<QuotaWindow> {
    let used = usable_percent(&w)?;
    let resets = resets_at(&w);
    match w.id.as_str() {
        "go_rolling" => Some(QuotaWindow::new(
            "rolling",
            "5 часов",
            used,
            w.window_duration_mins.or(Some(FIVE_HOUR_MINS)),
            resets,
        )),
        "go_weekly" => Some(QuotaWindow::new(
            "weekly",
            "Неделя",
            used,
            w.window_duration_mins.or(Some(WEEKLY_MINS)),
            resets,
        )),
        "go_monthly" => Some(QuotaWindow::new(
            "monthly",
            "Месяц",
            used,
            w.window_duration_mins.or(Some(MONTHLY_MINS)),
            resets,
        )),
        _ => map_passthrough_window(w),
    }
}

fn agy_duration_for_key(key: &str) -> Option<u64> {
    let normalized = key.to_ascii_lowercase();
    if normalized.contains("weekly") || normalized.contains("week") {
        Some(WEEKLY_MINS)
    } else if normalized.contains("five_hour")
        || normalized.contains("five-hour")
        || normalized.contains("5h")
    {
        Some(FIVE_HOUR_MINS)
    } else {
        None
    }
}

fn family_for_key(key: &str) -> String {
    let normalized = key.to_ascii_lowercase();
    for suffix in WINDOW_SUFFIXES {
        for separator in ['-', '_'] {
            if let Some(head) = normalized.strip_suffix(&format!("{separator}{suffix}")) {
                return head.to_owned();
            }
        }
    }
    normalized
}

fn scope_for_family(family: &str) -> Option<String> {
    match family {
        "gemini" => Some("Gemini".to_owned()),
        f if f.starts_with("gemini-argon") => Some("Gemini Argon".to_owned()),
        "3p" | "third-party" | "third_party" => Some("Сторонние модели".to_owned()),
        _ => None,
    }
}

fn title_case(value: &str) -> String {
    value
        .split(['-', '_'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Minimal RFC 3339 timestamp parser returning unix seconds.
pub(crate) fn parse_rfc3339(value: &str) -> Option<i64> {
    let (date, rest) = value.trim().split_once(['T', 't'])?;
    let (time, offset) = split_offset(rest)?;

    let mut date = date.split('-');
    let year: i64 = date.next()?.parse().ok()?;
    let month: i64 = date.next()?.parse().ok()?;
    let day: i64 = date.next()?.parse().ok()?;
    if date.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    let mut time = time.split(':');
    let hour: i64 = time.next()?.parse().ok()?;
    let minute: i64 = time.next()?.parse().ok()?;
    let second: i64 = time.next().unwrap_or("0").split('.').next()?.parse().ok()?;
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }

    Some(days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second - offset)
}

fn split_offset(rest: &str) -> Option<(&str, i64)> {
    if let Some(time) = rest.strip_suffix(['Z', 'z']) {
        return Some((time, 0));
    }
    let index = rest.rfind(['+', '-'])?;
    let (time, offset) = rest.split_at(index);
    let sign = if offset.starts_with('-') { -1 } else { 1 };
    let mut parts = offset[1..].split(':');
    let hours: i64 = parts.next()?.parse().ok()?;
    let minutes: i64 = parts.next().unwrap_or("0").parse().ok()?;
    Some((time, sign * (hours * 3_600 + minutes * 60)))
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static N: AtomicU64 = AtomicU64::new(0);

    struct Guard(PathBuf);
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn setup(tag: &str, settings_json: &str) -> (PathBuf, Guard) {
        let dir = std::env::temp_dir().join(format!(
            "agentdeck-usage-t3-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("userdata")).unwrap();
        fs::create_dir_all(dir.join("caches")).unwrap();
        fs::write(dir.join("userdata/settings.json"), settings_json).unwrap();
        (dir.clone(), Guard(dir))
    }

    fn write_cache(t3_root: &Path, instance_id: &str, body: &str) {
        fs::write(agentdeck_core::t3::cache_file(t3_root, instance_id), body).unwrap();
    }

    fn src<'a>(id: &'a str, provider: &'static str, instance: Option<&'a str>) -> T3Source<'a> {
        T3Source {
            id,
            name: id,
            provider,
            instance,
            explicit_plan: None,
        }
    }

    const NOW: i64 = 1_794_570_000; // 2026-11-13T11:40:00Z
    const CHECKED_RECENT: &str = "2026-11-13T11:35:00.000Z"; // 1_794_569_700 (300s ago)
    const CHECKED_OLD: &str = "2026-11-13T10:00:00Z"; // 1_794_564_000 (6000s ago)
    const CHECKED_FUTURE: &str = "2026-11-13T12:30:00Z"; // +3000s

    const SETTINGS_ALL: &str = r#"{
  "providerInstances": {
    "claude-main": { "driver": "claudeAgent", "enabled": true },
    "codex-main": { "driver": "codex", "enabled": true },
    "antigravity": { "driver": "antigravity", "enabled": true },
    "opencode": { "driver": "opencode", "enabled": true },
    "disabled-inst": { "driver": "claudeAgent", "enabled": false }
  }
}"#;

    #[test]
    fn maps_claude_windows_plan_email_and_unknown_window() {
        let (t3, _g) = setup("claude", SETTINGS_ALL);
        write_cache(
            &t3,
            "claude-main",
            &format!(
                r#"{{
  "enabled": true,
  "status": "ready",
  "checkedAt": "{CHECKED_RECENT}",
  "auth": {{
    "status": "authenticated",
    "type": "Claude Pro",
    "label": "Claude Pro Subscription",
    "email": "test@example.com"
  }},
  "usageLimits": {{
    "checkedAt": "{CHECKED_RECENT}",
    "windows": [
      {{ "id": "seven_day_fable", "kind": "weekly", "label": "Weekly · Fable", "windowDurationMins": 10080, "usedPercent": 12.5, "resetsAt": "2026-11-20T00:00:00Z" }},
      {{ "id": "five_hour", "kind": "session", "label": "Session", "windowDurationMins": 300, "usedPercent": 4, "resetsAt": "2026-11-13T16:00:00Z" }},
      {{ "id": "seven_day", "kind": "weekly", "label": "Weekly", "windowDurationMins": 10080, "usedPercent": 31 }},
      {{ "id": "burst_extra", "kind": "other", "label": "Extra Burst", "usedPercent": 9.25 }}
    ]
  }}
}}"#
            ),
        );

        let st = fetch_from_root(
            &t3,
            src("claude-main", "claude", Some("claude-main")),
            900,
            NOW,
        );
        assert_eq!(st.status, "ok");
        assert_eq!(st.plan.as_deref(), Some("Pro"));
        assert_eq!(st.email.as_deref(), Some("test@example.com"));
        assert!(st.error.is_none());
        assert_eq!(st.updated_at, 1_794_569_700);

        let keys: Vec<_> = st.windows.iter().map(|w| w.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "session",
                "weekly_all",
                "weekly_scoped:fable",
                "burst_extra"
            ]
        );
        assert_eq!(st.windows[0].label, "5 часов");
        assert_eq!(st.windows[0].duration_mins, Some(300));
        assert_eq!(st.windows[1].label, "Неделя");
        assert_eq!(st.windows[2].label, "Неделя · Fable");
        assert_eq!(st.windows[2].scope.as_deref(), Some("Fable"));
        assert_eq!(st.windows[2].used_percent, 12.5);
        assert_eq!(st.windows[3].label, "Extra Burst");
        assert_eq!(st.windows[3].duration_mins, None);
        assert_eq!(st.windows[3].resets_at, None);
    }

    #[test]
    fn maps_codex_windows_and_plan_variants() {
        let (t3, _g) = setup("codex", SETTINGS_ALL);
        write_cache(
            &t3,
            "codex-main",
            &format!(
                r#"{{
  "enabled": true,
  "status": "ready",
  "auth": {{
    "status": "authenticated",
    "type": "chatgpt",
    "label": "ChatGPT Plus Subscription",
    "email": "codex@example.com"
  }},
  "usageLimits": {{
    "checkedAt": "{CHECKED_RECENT}",
    "windows": [
      {{ "id": "secondary", "kind": "weekly", "label": "Weekly", "windowDurationMins": 10080, "usedPercent": 22 }},
      {{ "id": "primary", "kind": "session", "label": "Session", "windowDurationMins": 300, "usedPercent": 5.5, "resetsAt": "2026-11-13T15:00:00+00:00" }}
    ]
  }}
}}"#
            ),
        );

        let st = fetch_from_root(
            &t3,
            src("codex-main", "codex", Some("codex-main")),
            900,
            NOW,
        );
        assert_eq!(st.status, "ok");
        assert_eq!(st.plan.as_deref(), Some("Plus"));
        assert_eq!(st.email.as_deref(), Some("codex@example.com"));
        let keys: Vec<_> = st.windows.iter().map(|w| w.key.as_str()).collect();
        assert_eq!(keys, ["primary", "secondary"]);
        assert_eq!(st.windows[0].label, "5 часов");
        assert_eq!(st.windows[0].used_percent, 5.5);
        assert_eq!(st.windows[1].label, "Неделя");
    }

    #[test]
    fn maps_antigravity_known_and_transient_windows() {
        let (t3, _g) = setup("agy", SETTINGS_ALL);
        write_cache(
            &t3,
            "antigravity",
            &format!(
                r#"{{
  "enabled": true,
  "status": "ready",
  "auth": {{ "status": "authenticated", "type": "oauth-personal", "label": "Google account" }},
  "usageLimits": {{
    "checkedAt": "{CHECKED_RECENT}",
    "windows": [
      {{ "id": "3p-weekly", "kind": "weekly", "label": "3P Weekly", "windowDurationMins": 10080, "usedPercent": 3 }},
      {{ "id": "gemini-5h", "kind": "session", "label": "Gemini 5h", "windowDurationMins": 300, "usedPercent": 15 }},
      {{ "id": "gemini-argon-wtu-weekly", "kind": "weekly", "label": "Argon", "windowDurationMins": 10080, "usedPercent": 0 }},
      {{ "id": "claude-quota", "kind": "other", "label": "Claude Quota", "usedPercent": 20 }}
    ]
  }}
}}"#
            ),
        );

        let st = fetch_from_root(
            &t3,
            src("agy-main", "antigravity", Some("antigravity")),
            900,
            NOW,
        );
        assert_eq!(st.status, "ok");
        assert_eq!(st.plan.as_deref(), Some("Google AI Pro"));
        let keys: Vec<_> = st.windows.iter().map(|w| w.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "gemini-5h",
                "3p-weekly",
                "gemini-argon-wtu-weekly",
                "claude-quota"
            ]
        );
        assert_eq!(st.windows[0].scope, None);
        assert_eq!(st.windows[0].label, "Gemini · 5 часов");
        assert_eq!(st.windows[1].scope.as_deref(), Some("Сторонние модели"));
        assert_eq!(st.windows[2].scope.as_deref(), Some("Gemini Argon"));
        assert_eq!(st.windows[3].scope, None);
        assert_eq!(st.windows[3].label, "Claude Quota");
        assert_eq!(st.windows[3].duration_mins, None);
    }

    #[test]
    fn maps_opencode_go_without_instance_and_depleted_state() {
        let (t3, _g) = setup("opencode", SETTINGS_ALL);
        write_cache(
            &t3,
            "opencode",
            &format!(
                r#"{{
  "enabled": true,
  "status": "ready",
  "usageLimits": {{
    "checkedAt": "{CHECKED_RECENT}",
    "windows": [
      {{ "id": "go_rolling", "kind": "session", "label": "Rolling", "windowDurationMins": 300, "usedPercent": 10 }},
      {{ "id": "go_weekly", "kind": "weekly", "label": "Weekly", "windowDurationMins": 10080, "usedPercent": 100 }},
      {{ "id": "go_monthly", "kind": "monthly", "label": "Monthly", "usedPercent": 2 }}
    ]
  }}
}}"#
            ),
        );

        let st = fetch_from_root(&t3, src("opencode-go", "opencode-go", None), 900, NOW);
        assert_eq!(st.status, "depleted");
        assert_eq!(st.plan.as_deref(), Some("Go"));
        let keys: Vec<_> = st.windows.iter().map(|w| w.key.as_str()).collect();
        assert_eq!(keys, ["rolling", "weekly", "monthly"]);
        assert_eq!(st.windows[2].label, "Месяц");
        assert_eq!(st.windows[2].duration_mins, Some(43_200));
    }

    #[test]
    fn opencode_go_without_instance_and_missing_opencode_in_settings_reports_fix() {
        let (t3, _g) = setup("no-opencode", r#"{"providerInstances":{}}"#);
        let st = fetch_from_root(&t3, src("opencode-go", "opencode-go", None), 900, NOW);
        assert_eq!(st.status, "error");
        let err = st.error.unwrap();
        assert!(
            err.contains("opencode") && err.contains("instance"),
            "{err}"
        );
    }

    #[test]
    fn stale_and_future_caches_keep_windows_and_mark_stale() {
        let (t3, _g) = setup("stale", SETTINGS_ALL);
        write_cache(
            &t3,
            "codex-main",
            &format!(
                r#"{{
  "enabled": true,
  "status": "ready",
  "usageLimits": {{
    "checkedAt": "{CHECKED_OLD}",
    "windows": [{{ "id": "primary", "usedPercent": 42 }}]
  }}
}}"#
            ),
        );
        let st = fetch_from_root(
            &t3,
            src("codex-main", "codex", Some("codex-main")),
            900,
            NOW,
        );
        assert_eq!(st.status, "stale");
        assert_eq!(st.updated_at, 1_794_564_000);
        assert!(!st.windows.is_empty());
        assert!(st.error.as_deref().unwrap().contains("T3"));

        write_cache(
            &t3,
            "codex-main",
            &format!(
                r#"{{
  "enabled": true,
  "status": "ready",
  "usageLimits": {{
    "checkedAt": "{CHECKED_FUTURE}",
    "windows": [{{ "id": "primary", "usedPercent": 42 }}]
  }}
}}"#
            ),
        );
        let future = fetch_from_root(
            &t3,
            src("codex-main", "codex", Some("codex-main")),
            900,
            NOW,
        );
        assert_eq!(future.status, "stale");
        assert!(future.error.as_deref().unwrap().contains("future"));
    }

    #[test]
    fn disabled_in_settings_overrides_cache_enabled_true() {
        let (t3, _g) = setup("disabled", SETTINGS_ALL);
        // Live quirk from rules.txt: cache file still claims enabled: true.
        write_cache(
            &t3,
            "disabled-inst",
            &format!(
                r#"{{
  "enabled": true,
  "status": "ready",
  "usageLimits": {{
    "checkedAt": "{CHECKED_RECENT}",
    "windows": [{{ "id": "five_hour", "usedPercent": 5 }}]
  }}
}}"#
            ),
        );

        let st = fetch_from_root(&t3, src("c1", "claude", Some("disabled-inst")), 900, NOW);
        assert_eq!(st.status, "error");
        assert!(st.windows.is_empty());
        assert!(st.error.as_deref().unwrap().contains("disabled"));
    }

    #[test]
    fn missing_cache_no_limits_empty_windows_malformed_and_no_instance() {
        let (t3, _g) = setup("failures", SETTINGS_ALL);

        // 1. Missing cache file on enabled instance -> waiting
        let missing = fetch_from_root(&t3, src("c1", "claude", Some("claude-main")), 900, NOW);
        assert_eq!(missing.status, "waiting");
        assert!(missing.error.unwrap().contains("T3 cache missing"));

        // 2. Unresolvable instance on non-opencode source -> error naming instance field
        let no_inst = fetch_from_root(&t3, src("c1", "claude", None), 900, NOW);
        assert_eq!(no_inst.status, "error");
        assert!(no_inst.error.unwrap().contains("instance"));

        // 3. Cache with unauthenticated auth and no usageLimits -> unauthenticated
        write_cache(
            &t3,
            "claude-main",
            r#"{"enabled":true,"status":"error","auth":{"status":"unauthenticated"}}"#,
        );
        let unauth = fetch_from_root(&t3, src("c1", "claude", Some("claude-main")), 900, NOW);
        assert_eq!(unauth.status, "unauthenticated");

        // 4. Empty windows -> waiting (not ok, not zero percent assumed)
        write_cache(
            &t3,
            "claude-main",
            &format!(
                r#"{{"enabled":true,"status":"ready","usageLimits":{{"checkedAt":"{CHECKED_RECENT}","windows":[]}}}}"#
            ),
        );
        let empty = fetch_from_root(&t3, src("c1", "claude", Some("claude-main")), 900, NOW);
        assert_eq!(empty.status, "waiting");
        assert!(empty.windows.is_empty());

        // 5. Malformed / half-written JSON -> error
        write_cache(&t3, "claude-main", "{\"status\":\"ready\",\"usageLim");
        let bad = fetch_from_root(
            &t3,
            src("claude-main", "claude", Some("claude-main")),
            900,
            NOW,
        );
        assert_eq!(bad.status, "error");
        assert!(bad.error.unwrap().contains("T3 cache"));
    }

    #[test]
    fn missing_t3_root_and_missing_or_malformed_settings_return_error() {
        let missing_root = std::env::temp_dir().join(format!(
            "agentdeck-usage-t3-absent-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&missing_root);
        let st = fetch_from_root(
            &missing_root,
            src("claude-main", "claude", Some("claude-main")),
            900,
            NOW,
        );
        assert_eq!(st.status, "error");
        assert!(st.windows.is_empty());
        assert!(st.error.as_deref().unwrap().contains("settings.json"));

        let (t3, _g) = setup("missing-settings", SETTINGS_ALL);
        fs::remove_file(t3.join("userdata/settings.json")).unwrap();
        let st = fetch_from_root(
            &t3,
            src("claude-main", "claude", Some("claude-main")),
            900,
            NOW,
        );
        assert_eq!(st.status, "error");
        assert!(st.windows.is_empty());
        assert!(st.error.as_deref().unwrap().contains("settings.json"));

        fs::write(
            t3.join("userdata/settings.json"),
            "{\"providerInstances\":{",
        )
        .unwrap();
        let st = fetch_from_root(
            &t3,
            src("claude-main", "claude", Some("claude-main")),
            900,
            NOW,
        );
        assert_eq!(st.status, "error");
        assert!(st.windows.is_empty());
        assert!(st.error.as_deref().unwrap().contains("settings.json"));
    }

    #[test]
    fn codex_monthly_only_window_and_null_duration_localize_like_legacy() {
        let (t3, _g) = setup(
            "codex-monthly",
            r#"{"providerInstances":{"codex-second":{"driver":"codex","enabled":true}}}"#,
        );
        write_cache(
            &t3,
            "codex-second",
            &format!(
                r#"{{
  "enabled": true,
  "status": "ready",
  "auth": {{
    "status": "authenticated",
    "type": "chatgpt",
    "label": "ChatGPT Free Subscription"
  }},
  "usageLimits": {{
    "checkedAt": "{CHECKED_RECENT}",
    "windows": [
      {{ "id": "primary", "kind": "monthly", "label": "Monthly", "windowDurationMins": 43200, "usedPercent": 38, "resetsAt": "2026-12-09T10:46:58Z" }}
    ]
  }}
}}"#
            ),
        );

        let st = fetch_from_root(
            &t3,
            src("codex-second", "codex", Some("codex-second")),
            900,
            NOW,
        );
        assert_eq!(st.status, "ok");
        assert_eq!(st.plan.as_deref(), Some("Free"));
        assert_eq!(st.windows.len(), 1);
        assert_eq!(st.windows[0].key, "primary");
        assert_eq!(st.windows[0].label, "30 д");
        assert_eq!(st.windows[0].duration_mins, Some(43_200));
        assert_eq!(st.windows[0].used_percent, 38.0);

        // Even if windowDurationMins is omitted on primary, localized fallback applies.
        write_cache(
            &t3,
            "codex-second",
            &format!(
                r#"{{
  "enabled": true,
  "status": "ready",
  "usageLimits": {{
    "checkedAt": "{CHECKED_RECENT}",
    "windows": [
      {{ "id": "primary", "kind": "session", "label": "Session", "usedPercent": 10 }}
    ]
  }}
}}"#
            ),
        );
        let no_dur = fetch_from_root(
            &t3,
            src("codex-second", "codex", Some("codex-second")),
            900,
            NOW,
        );
        assert_eq!(no_dur.windows[0].label, "Лимит");
    }

    #[test]
    fn opencode_go_uses_explicit_instance_when_set() {
        let (t3, _g) = setup(
            "opencode-explicit",
            r#"{"providerInstances":{"opencode":{"driver":"opencode","enabled":true},"opencode-alt":{"driver":"opencode","enabled":true}}}"#,
        );
        write_cache(
            &t3,
            "opencode",
            &format!(
                r#"{{"enabled":true,"status":"ready","usageLimits":{{"checkedAt":"{CHECKED_RECENT}","windows":[{{"id":"go_rolling","usedPercent":99}}]}}}}"#
            ),
        );
        write_cache(
            &t3,
            "opencode-alt",
            &format!(
                r#"{{"enabled":true,"status":"ready","usageLimits":{{"checkedAt":"{CHECKED_RECENT}","windows":[{{"id":"go_rolling","usedPercent":12}},{{"id":"go_monthly","usedPercent":5}}]}}}}"#
            ),
        );

        let st = fetch_from_root(
            &t3,
            src("opencode-go", "opencode-go", Some("opencode-alt")),
            900,
            NOW,
        );
        assert_eq!(st.status, "ok");
        assert_eq!(st.windows.len(), 2);
        assert_eq!(st.windows[0].key, "rolling");
        assert_eq!(st.windows[0].used_percent, 12.0);
        assert_eq!(st.windows[1].key, "monthly");
        assert_eq!(st.windows[1].duration_mins, Some(43_200));
    }

    #[test]
    fn antigravity_suffixed_unknown_family_localizes_duration_instead_of_english_t3_label() {
        let (t3, _g) = setup("agy-local", SETTINGS_ALL);
        write_cache(
            &t3,
            "antigravity",
            &format!(
                r#"{{
  "enabled": true,
  "status": "ready",
  "usageLimits": {{
    "checkedAt": "{CHECKED_RECENT}",
    "windows": [
      {{ "id": "spark-weekly", "kind": "weekly", "label": "Spark · Weekly", "windowDurationMins": 10080, "usedPercent": 15 }}
    ]
  }}
}}"#
            ),
        );
        let st = fetch_from_root(
            &t3,
            src("agy-main", "antigravity", Some("antigravity")),
            900,
            NOW,
        );
        assert_eq!(st.windows[0].label, "Spark · Неделя");
        assert_eq!(st.windows[0].duration_mins, Some(10_080));
    }

    #[test]
    fn retained_usage_limits_are_not_published_ok_when_cache_or_auth_fails() {
        let (t3, _g) = setup("retained-status", SETTINGS_ALL);

        let write_with = |status: &str, auth_status: Option<&str>, message: Option<&str>| {
            let auth_json = auth_status
                .map(|a| format!(r#","auth":{{"status":"{a}","type":"Claude Pro"}}"#))
                .unwrap_or_default();
            let msg_json = message
                .map(|m| format!(r#","message":"{m}""#))
                .unwrap_or_default();
            write_cache(
                &t3,
                "claude-main",
                &format!(
                    r#"{{"enabled":true,"status":"{status}"{auth_json}{msg_json},"usageLimits":{{"checkedAt":"{CHECKED_RECENT}","windows":[{{"id":"five_hour","kind":"session","label":"Session","windowDurationMins":300,"usedPercent":10}}]}}}}"#
                ),
            );
        };

        write_with("warning", Some("unauthenticated"), Some("login required"));
        let st = fetch_from_root(
            &t3,
            src("claude-main", "claude", Some("claude-main")),
            900,
            NOW,
        );
        assert_eq!(st.status, "unauthenticated");
        assert!(st.windows.is_empty());
        assert!(st.error.as_deref().unwrap().contains("login required"));

        write_with("unauthenticated", None, None);
        let st = fetch_from_root(
            &t3,
            src("claude-main", "claude", Some("claude-main")),
            900,
            NOW,
        );
        assert_eq!(st.status, "unauthenticated");
        assert!(st.windows.is_empty());

        for bad_status in ["error", "unavailable", "disabled"] {
            write_with(bad_status, Some("authenticated"), Some("probe failed"));
            let st = fetch_from_root(
                &t3,
                src("claude-main", "claude", Some("claude-main")),
                900,
                NOW,
            );
            assert_eq!(st.status, "error", "cache.status={bad_status}");
            assert!(st.windows.is_empty(), "cache.status={bad_status}");
            assert!(
                st.error.as_deref().unwrap().contains("probe failed"),
                "cache.status={bad_status}"
            );
        }

        // Warning with authenticated status and valid limits still publishes ok.
        write_with("warning", Some("authenticated"), None);
        let st = fetch_from_root(
            &t3,
            src("claude-main", "claude", Some("claude-main")),
            900,
            NOW,
        );
        assert_eq!(st.status, "ok");
        assert_eq!(st.windows.len(), 1);
    }

    #[test]
    fn usage_limits_unavailable_is_terminal_error_not_waiting() {
        let (t3, _g) = setup("unavailable", SETTINGS_ALL);

        write_cache(
            &t3,
            "claude-main",
            &format!(
                r#"{{"enabled":true,"status":"ready","usageLimits":{{"checkedAt":"{CHECKED_RECENT}","windows":[],"unavailable":{{"reason":"unsupported"}}}}}}"#
            ),
        );
        let st = fetch_from_root(
            &t3,
            src("claude-main", "claude", Some("claude-main")),
            900,
            NOW,
        );
        assert_eq!(st.status, "error");
        assert!(st.windows.is_empty());
        assert_eq!(
            st.error.as_deref(),
            Some("T3 reports no subscription limits for this instance: unsupported")
        );

        write_cache(
            &t3,
            "codex-main",
            &format!(
                r#"{{"enabled":true,"status":"ready","usageLimits":{{"checkedAt":"{CHECKED_RECENT}","windows":[],"unavailable":{{"reason":"unsupported","message":"use connected account"}}}}}}"#
            ),
        );
        let st = fetch_from_root(
            &t3,
            src("codex-main", "codex", Some("codex-main")),
            900,
            NOW,
        );
        assert_eq!(st.status, "error");
        assert!(st.windows.is_empty());
        let err = st.error.unwrap();
        assert!(
            err.starts_with("T3 reports no subscription limits for this instance: unsupported")
                && err.contains("use connected account"),
            "{err}"
        );

        // Even without checkedAt or reason, unavailable is still terminal.
        write_cache(
            &t3,
            "claude-main",
            r#"{"enabled":true,"status":"ready","usageLimits":{"windows":[],"unavailable":{}}}"#,
        );
        let st = fetch_from_root(
            &t3,
            src("claude-main", "claude", Some("claude-main")),
            900,
            NOW,
        );
        assert_eq!(st.status, "error");
        assert!(st.windows.is_empty());
        assert!(
            st.error
                .as_deref()
                .unwrap()
                .starts_with("T3 reports no subscription limits for this instance: ")
        );
    }
}
