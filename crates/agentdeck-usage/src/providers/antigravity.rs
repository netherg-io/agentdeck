use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::model::AccountState;
use crate::providers::t3::{self, T3Source};

pub async fn fetch(
    id: &str,
    name: &str,
    instance: Option<&str>,
    stale_seconds: i64,
) -> Result<AccountState> {
    t3::fetch(
        T3Source {
            id,
            name,
            provider: "antigravity",
            instance,
            explicit_plan: None,
        },
        stale_seconds,
    )
    .await
}

#[derive(Debug, Deserialize)]
struct StatusPayload {
    #[serde(default)]
    model: Option<ModelInfo>,
    #[serde(default)]
    quota: BTreeMap<String, QuotaInfo>,
}

#[derive(Debug, Deserialize)]
struct ModelInfo {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct QuotaInfo {
    #[serde(default)]
    remaining_fraction: Option<f64>,
}

/// Pure status-line formatter for Antigravity's stdin JSON (kept for hooks until U2).
pub fn status_line_from_payload(account_id: &str, raw: &str) -> Result<String> {
    crate::util::validate_id(account_id)?;
    let payload: StatusPayload =
        serde_json::from_str(raw).context("Некорректный JSON от agy status line")?;

    let model = payload.model.and_then(|m| {
        m.display_name
            .filter(|v| !v.trim().is_empty())
            .or_else(|| m.id.filter(|v| !v.trim().is_empty()))
    });
    let active = model.as_deref().and_then(|m| {
        let lower = m.trim().to_ascii_lowercase();
        if lower.is_empty() {
            None
        } else if lower.contains("argon") {
            Some("argon")
        } else if lower.contains("gemini") {
            Some("gemini")
        } else {
            Some("3p")
        }
    });

    let active_min = payload
        .quota
        .iter()
        .filter(|(k, _)| match active {
            Some("gemini") => k.starts_with("gemini") && !k.contains("argon"),
            Some("argon") => k.contains("argon"),
            Some("3p") => k.starts_with("3p") || k.starts_with("third"),
            _ => false,
        })
        .filter_map(|(_, q)| q.remaining_fraction.filter(|f| f.is_finite()))
        .map(|f| (f * 100.0).clamp(0.0, 100.0))
        .reduce(f64::min);

    let fallback_min = payload
        .quota
        .values()
        .filter_map(|q| q.remaining_fraction.filter(|f| f.is_finite()))
        .map(|f| (f * 100.0).clamp(0.0, 100.0))
        .reduce(f64::min);

    Ok(match active_min.or(fallback_min) {
        Some(pct) => format!("AI Usage · {}%", pct.round() as i64),
        None => "AI Usage · ожидание квоты".to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_status_line_without_writing_snapshot() {
        let line = status_line_from_payload(
            "agy-main",
            r#"{"model":{"display_name":"Gemini 3.6 Flash"},"quota":{"gemini-5h":{"remaining_fraction":0.42},"3p-5h":{"remaining_fraction":1.0}}}"#,
        )
        .unwrap();
        assert_eq!(line, "AI Usage · 42%");
    }
}
