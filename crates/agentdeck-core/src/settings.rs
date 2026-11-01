//! Module settings kept in `instances.toml` beside the registry:
//! `[router] decision_model` and `[usage_poll] refresh_seconds / stale_seconds`.
//! Absent keys mean the built-in defaults.

use crate::paths::Paths;
use crate::registry::Registry;

/// Router's judge model when `[router] decision_model` is unset.
pub const DEFAULT_DECISION_MODEL: &str = "~typesafe/jev-latest";
pub const DEFAULT_REFRESH_SECONDS: u64 = 120;
pub const DEFAULT_STALE_SECONDS: u64 = 900;
/// Faster polling spawns a Codex subprocess per account every cycle.
pub const MIN_REFRESH_SECONDS: u64 = 15;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub decision_model: Option<String>,
    pub refresh_seconds: Option<u64>,
    pub stale_seconds: Option<u64>,
}

fn get<'a>(reg: &'a Registry, table: &str, key: &str) -> Option<&'a toml::Value> {
    reg.extra.get(table)?.as_table()?.get(key)
}

fn positive(v: Option<&toml::Value>) -> Option<u64> {
    v?.as_integer().filter(|n| *n > 0).map(|n| n as u64)
}

pub fn read(reg: &Registry) -> Settings {
    Settings {
        decision_model: get(reg, "router", "decision_model")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        refresh_seconds: positive(get(reg, "usage_poll", "refresh_seconds")),
        stale_seconds: positive(get(reg, "usage_poll", "stale_seconds")),
    }
}

/// Polling interval and staleness threshold the daemon should use now.
pub fn usage_poll(reg: &Registry) -> (u64, u64) {
    let s = read(reg);
    (
        s.refresh_seconds
            .unwrap_or(DEFAULT_REFRESH_SECONDS)
            .max(MIN_REFRESH_SECONDS),
        s.stale_seconds.unwrap_or(DEFAULT_STALE_SECONDS),
    )
}

/// Validated write; `None` fields remove the key so the default applies again.
pub fn write(paths: &Paths, s: &Settings) -> anyhow::Result<()> {
    if s.refresh_seconds.is_some_and(|n| n < MIN_REFRESH_SECONDS) {
        anyhow::bail!("refresh_seconds must be at least {MIN_REFRESH_SECONDS}");
    }
    if s.decision_model
        .as_deref()
        .is_some_and(|m| m.chars().any(char::is_whitespace))
    {
        anyhow::bail!("decision_model must not contain spaces");
    }
    Registry::update(paths, |reg| {
        set(
            reg,
            "router",
            "decision_model",
            s.decision_model.clone().map(toml::Value::String),
        );
        let int = |n: Option<u64>| n.map(|n| toml::Value::Integer(n as i64));
        set(reg, "usage_poll", "refresh_seconds", int(s.refresh_seconds));
        set(reg, "usage_poll", "stale_seconds", int(s.stale_seconds));
        Ok(())
    })
}

fn set(reg: &mut Registry, table: &str, key: &str, value: Option<toml::Value>) {
    let entry = reg
        .extra
        .entry(table.to_string())
        .or_insert_with(|| toml::Value::Table(Default::default()));
    if !entry.is_table() {
        *entry = toml::Value::Table(Default::default());
    }
    let t = entry.as_table_mut().expect("table");
    match value {
        Some(v) => {
            t.insert(key.to_string(), v);
        }
        None => {
            t.remove(key);
        }
    }
    if t.is_empty() {
        reg.extra.remove(table);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_round_trip_and_reset() {
        let mut reg = Registry::empty();
        assert_eq!(
            usage_poll(&reg),
            (DEFAULT_REFRESH_SECONDS, DEFAULT_STALE_SECONDS)
        );
        set(
            &mut reg,
            "router",
            "decision_model",
            Some(toml::Value::String("x/judge".into())),
        );
        set(
            &mut reg,
            "usage_poll",
            "refresh_seconds",
            Some(toml::Value::Integer(5)),
        );
        let s = read(&reg);
        assert_eq!(s.decision_model.as_deref(), Some("x/judge"));
        // Below the floor is clamped, never polled faster.
        assert_eq!(usage_poll(&reg).0, MIN_REFRESH_SECONDS);
        set(&mut reg, "router", "decision_model", None);
        assert!(!reg.extra.contains_key("router"));
    }
}
