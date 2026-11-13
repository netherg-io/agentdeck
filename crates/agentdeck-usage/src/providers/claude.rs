use anyhow::{Context, Result};
use serde_json::Value;

use crate::model::AccountState;
use crate::providers::t3::{self, T3Source};

pub const DEFAULT_STALE_SECONDS: i64 = 900;

pub async fn fetch(
    id: &str,
    name: &str,
    plan: Option<String>,
    instance: Option<&str>,
    stale_seconds: i64,
) -> Result<AccountState> {
    t3::fetch(
        T3Source {
            id,
            name,
            provider: "claude",
            instance,
            explicit_plan: plan.as_deref(),
        },
        stale_seconds,
    )
    .await
}

pub fn humanize_age(seconds: i64) -> String {
    let minutes = seconds / 60;
    match minutes {
        ..=0 => "меньше минуты".to_owned(),
        1..=59 => format!("{minutes} мин"),
        _ => format!("{} ч {} мин", minutes / 60, minutes % 60),
    }
}

/// Pure status-line formatter for Claude Code's stdin JSON (kept for hooks until U2).
pub fn status_line_from_payload(raw: &str) -> Result<String> {
    let payload: Value =
        serde_json::from_str(raw).context("Claude hook получил некорректный JSON")?;

    let mut parts = Vec::new();
    if let Some(model) = payload
        .pointer("/model/display_name")
        .and_then(Value::as_str)
    {
        parts.push(format!("[{model}]"));
    }

    if let Some(limits) = payload.get("rate_limits").and_then(Value::as_object) {
        let mut entries: Vec<_> = limits
            .iter()
            .filter_map(|(key, value)| {
                let used = value.get("used_percentage")?.as_f64()?;
                Some((key.clone(), 100.0 - used.clamp(0.0, 100.0)))
            })
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        for (key, remaining) in entries {
            parts.push(format!("{} {remaining:.0}% left", short_key(&key)));
        }
    }

    Ok(parts.join(" · "))
}

fn short_key(key: &str) -> String {
    let (base, suffix) = if let Some(rest) = key.strip_prefix("five_hour") {
        ("5h", rest)
    } else if let Some(rest) = key.strip_prefix("seven_day") {
        ("7d", rest)
    } else {
        return key.to_owned();
    };
    match suffix.strip_prefix('_') {
        Some(suffix) if !suffix.is_empty() => format!("{base}/{suffix}"),
        _ => base.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_status_line_from_claude_stdin() {
        let line = status_line_from_payload(
            r#"{"model":{"display_name":"Sonnet"},"rate_limits":{"five_hour":{"used_percentage":12.4},"seven_day_sonnet":{"used_percentage":80}}}"#,
        )
        .unwrap();
        assert_eq!(line, "[Sonnet] · 5h 88% left · 7d/sonnet 20% left");
    }

    #[test]
    fn humanizes_age_values() {
        assert_eq!(humanize_age(0), "меньше минуты");
        assert_eq!(humanize_age(-5), "меньше минуты");
        assert_eq!(humanize_age(59), "меньше минуты");
        assert_eq!(humanize_age(60), "1 мин");
        assert_eq!(humanize_age(3_540), "59 мин");
        assert_eq!(humanize_age(3_600), "1 ч 0 мин");
        assert_eq!(humanize_age(19_860), "5 ч 31 мин");
    }
}
