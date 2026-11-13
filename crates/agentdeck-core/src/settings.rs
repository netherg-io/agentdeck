//! Module settings kept in `instances.toml` beside the registry:
//! `[router] decision_provider / decision_model`, `[router.rules]`
//! user model restrictions, and `[usage_poll] refresh_seconds /
//! stale_seconds`. Absent keys mean the built-in defaults.

use crate::paths::Paths;
use crate::registry::Registry;

/// Router's judge model when `[router] decision_model` is unset (OpenRouter only).
pub const DEFAULT_DECISION_MODEL: &str = "~typesafe/jev-latest";
/// `[router] decision_provider` values; absent means OpenRouter's decisions API.
pub const PROVIDER_OPENROUTER: &str = "openrouter";
/// Vercel AI Gateway chat completions; it has no default model.
pub const PROVIDER_VERCEL: &str = "vercel";
pub const DEFAULT_REFRESH_SECONDS: u64 = 120;
pub const DEFAULT_STALE_SECONDS: u64 = 900;
/// Faster polling spawns a Codex subprocess per account every cycle.
pub const MIN_REFRESH_SECONDS: u64 = 15;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub decision_provider: Option<String>,
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
        decision_provider: get(reg, "router", "decision_provider")
            .and_then(|v| v.as_str())
            .filter(|s| *s == PROVIDER_VERCEL || *s == PROVIDER_OPENROUTER)
            .map(str::to_string),
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
    if let Some(p) = s.decision_provider.as_deref() {
        if p != PROVIDER_OPENROUTER && p != PROVIDER_VERCEL {
            anyhow::bail!("decision_provider must be {PROVIDER_OPENROUTER} or {PROVIDER_VERCEL}");
        }
        if p == PROVIDER_VERCEL && s.decision_model.is_none() {
            anyhow::bail!("Vercel AI Gateway needs a decision model, e.g. provider/model");
        }
    }
    Registry::update(paths, |reg| {
        set(
            reg,
            "router",
            "decision_provider",
            s.decision_provider.clone().map(toml::Value::String),
        );
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

/// User-configurable model restrictions (`[router.rules]` in
/// `instances.toml`). An absent table means the built-in defaults, which
/// preserve the original embedded Argon audit/review ban exactly; an
/// explicit (even empty) list overrides it, so `no_review_models = []`
/// disables the ban. `deny_models` denies matching favorites outright and
/// wins over every other rule.
pub const DEFAULT_NO_REVIEW_MODEL: &str = "gemini-.*argon";
/// Strict bounds so a typo fails closed instead of silently matching nothing.
pub const MAX_RULE_PATTERNS: usize = 32;
pub const MAX_RULE_PATTERN_LEN: usize = 200;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UserRuleSource {
    Default,
    User,
}

impl UserRuleSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::User => "user",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserRules {
    pub no_review_models: Vec<String>,
    pub deny_models: Vec<String>,
    pub source: UserRuleSource,
}

impl Default for UserRules {
    fn default() -> Self {
        Self {
            no_review_models: vec![DEFAULT_NO_REVIEW_MODEL.to_string()],
            deny_models: Vec::new(),
            source: UserRuleSource::Default,
        }
    }
}

fn rule_list(
    table: &toml::map::Map<String, toml::Value>,
    key: &str,
) -> anyhow::Result<Option<Vec<String>>> {
    match table.get(key) {
        None => Ok(None),
        Some(toml::Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_str().map(str::to_string).ok_or_else(|| {
                    anyhow::anyhow!("[router.rules] {key} must be an array of strings")
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()
            .map(Some),
        Some(_) => Err(anyhow::anyhow!(
            "[router.rules] {key} must be an array of strings"
        )),
    }
}

fn check_patterns(key: &str, patterns: &[String]) -> anyhow::Result<()> {
    if patterns.len() > MAX_RULE_PATTERNS {
        anyhow::bail!(
            "[router.rules] {key} has {} patterns, at most {MAX_RULE_PATTERNS} allowed",
            patterns.len()
        );
    }
    for p in patterns {
        if p.is_empty() {
            anyhow::bail!("[router.rules] {key} must not contain empty patterns");
        }
        if p.len() > MAX_RULE_PATTERN_LEN {
            anyhow::bail!(
                "[router.rules] {key} pattern {p:?} exceeds {MAX_RULE_PATTERN_LEN} chars"
            );
        }
        // Same case-insensitive matching as roster families; a bad pattern
        // fails closed here instead of silently matching nothing later.
        regex::Regex::new(&format!("(?i){p}")).map_err(|e| {
            anyhow::anyhow!("[router.rules] {key} pattern {p:?} is not a regex: {e}")
        })?;
    }
    Ok(())
}

impl UserRules {
    pub fn validate(&self) -> anyhow::Result<()> {
        check_patterns("no_review_models", &self.no_review_models)?;
        check_patterns("deny_models", &self.deny_models)?;
        Ok(())
    }

    fn matches_any(patterns: &[String], model: &str) -> bool {
        patterns
            .iter()
            .any(|p| regex::Regex::new(&format!("(?i){p}")).is_ok_and(|re| re.is_match(model)))
    }

    /// True when the model must never take audits or reviews (unless the
    /// user explicitly asks for it, which the hook still lifts).
    pub fn is_no_review(&self, model: &str) -> bool {
        Self::matches_any(&self.no_review_models, model)
    }

    /// Denied for automatic routing regardless of quota or fit; a verified explicit user choice may override this rule.
    pub fn is_denied(&self, model: &str) -> bool {
        Self::matches_any(&self.deny_models, model)
    }

    /// Read `[router.rules]`; absent means the built-in defaults. Unknown
    /// keys are ignored so newer configs still load.
    pub fn from_registry(reg: &Registry) -> anyhow::Result<Self> {
        let Some(table) = reg
            .extra
            .get("router")
            .and_then(|v| v.as_table())
            .and_then(|t| t.get("rules"))
            .and_then(|v| v.as_table())
        else {
            return Ok(Self::default());
        };
        let defaults = Self::default();
        let touched = table.contains_key("no_review_models") || table.contains_key("deny_models");
        let rules = Self {
            no_review_models: rule_list(table, "no_review_models")?
                .unwrap_or(defaults.no_review_models),
            deny_models: rule_list(table, "deny_models")?.unwrap_or_default(),
            source: if touched {
                UserRuleSource::User
            } else {
                UserRuleSource::Default
            },
        };
        rules.validate()?;
        Ok(rules)
    }

    /// Read the generated `router.json` form; absent or null means defaults.
    /// Unknown keys are ignored; anything malformed fails closed.
    pub fn from_json(v: Option<&serde_json::Value>) -> anyhow::Result<Self> {
        let Some(v) = v else {
            return Ok(Self::default());
        };
        if v.is_null() {
            return Ok(Self::default());
        }
        let o = v
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("userRules must be an object"))?;
        let list = |key: &str| -> anyhow::Result<Option<Vec<String>>> {
            match o.get(key) {
                None => Ok(None),
                Some(serde_json::Value::Array(items)) => items
                    .iter()
                    .map(|item| {
                        item.as_str().map(str::to_string).ok_or_else(|| {
                            anyhow::anyhow!("userRules {key} must be an array of strings")
                        })
                    })
                    .collect::<anyhow::Result<Vec<_>>>()
                    .map(Some),
                Some(_) => Err(anyhow::anyhow!(
                    "userRules {key} must be an array of strings"
                )),
            }
        };
        let defaults = Self::default();
        let touched = o.contains_key("noReviewModels") || o.contains_key("denyModels");
        let source = match o.get("source").and_then(|s| s.as_str()) {
            Some("user") => UserRuleSource::User,
            Some("default") => UserRuleSource::Default,
            Some(other) => {
                anyhow::bail!("userRules source must be \"default\" or \"user\", got {other:?}")
            }
            None => {
                if touched {
                    UserRuleSource::User
                } else {
                    UserRuleSource::Default
                }
            }
        };
        let rules = Self {
            no_review_models: list("noReviewModels")?.unwrap_or(defaults.no_review_models),
            deny_models: list("denyModels")?.unwrap_or_default(),
            source,
        };
        rules.validate()?;
        Ok(rules)
    }

    /// The `router.json` form the GUI reads: lists plus their source.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "noReviewModels": self.no_review_models,
            "denyModels": self.deny_models,
            "source": self.source.as_str(),
        })
    }
}

/// Validated write; both lists are stored explicitly so an empty
/// `no_review_models` (ban disabled) survives instead of resetting to default.
pub fn write_user_rules(paths: &Paths, rules: &UserRules) -> anyhow::Result<()> {
    rules.validate()?;
    Registry::update(paths, |reg| {
        let arr = |items: &[String]| {
            toml::Value::Array(items.iter().cloned().map(toml::Value::String).collect())
        };
        let router = reg
            .extra
            .entry("router".to_string())
            .or_insert_with(|| toml::Value::Table(Default::default()));
        if !router.is_table() {
            *router = toml::Value::Table(Default::default());
        }
        let nested = router
            .as_table_mut()
            .expect("table")
            .entry("rules".to_string())
            .or_insert_with(|| toml::Value::Table(Default::default()));
        if !nested.is_table() {
            *nested = toml::Value::Table(Default::default());
        }
        let t = nested.as_table_mut().expect("table");
        t.insert("no_review_models".to_string(), arr(&rules.no_review_models));
        t.insert("deny_models".to_string(), arr(&rules.deny_models));
        Ok(())
    })
}

pub(crate) fn set(reg: &mut Registry, table: &str, key: &str, value: Option<toml::Value>) {
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

    #[test]
    fn user_rules_default_preserves_argon_ban() {
        let rules = UserRules::from_registry(&Registry::empty()).unwrap();
        assert_eq!(rules.source, UserRuleSource::Default);
        assert_eq!(rules.no_review_models, vec![DEFAULT_NO_REVIEW_MODEL]);
        assert!(rules.deny_models.is_empty());
        assert!(rules.is_no_review("gemini-3-argon"));
        assert!(rules.is_no_review("GEMINI-X-ARGON"));
        assert!(!rules.is_no_review("gemini-2.5-pro"));
        assert!(!rules.is_denied("gemini-3-argon"));
    }

    #[test]
    fn user_rules_explicit_override_and_disable() {
        let mut reg = Registry::from_toml(
            "version = 1\n[router.rules]\nno_review_models = [\"qwen\"]\ndeny_models = [\"^gpt-\"]\n",
        )
        .unwrap();
        let rules = UserRules::from_registry(&reg).unwrap();
        assert_eq!(rules.source, UserRuleSource::User);
        assert!(!rules.is_no_review("gemini-3-argon"));
        assert!(rules.is_no_review("qwen-max"));
        assert!(rules.is_denied("gpt-6.1-sol"));
        assert!(!rules.is_denied("claude-opus"));
        // Unknown keys are ignored for forward compatibility.
        reg.extra["router"]["rules"]
            .as_table_mut()
            .unwrap()
            .insert("future_knob".into(), toml::Value::Integer(1));
        assert!(UserRules::from_registry(&reg).is_ok());
        // Explicit empty list disables the ban and survives.
        let reg =
            Registry::from_toml("version = 1\n[router.rules]\nno_review_models = []\n").unwrap();
        let rules = UserRules::from_registry(&reg).unwrap();
        assert_eq!(rules.source, UserRuleSource::User);
        assert!(rules.no_review_models.is_empty());
        assert!(!rules.is_no_review("gemini-3-argon"));
    }

    #[test]
    fn user_rules_invalid_fails_closed() {
        for text in [
            "version = 1\n[router.rules]\nno_review_models = \"gemini\"",
            "version = 1\n[router.rules]\nno_review_models = [42]",
            "version = 1\n[router.rules]\nno_review_models = [\"([\"]",
            "version = 1\n[router.rules]\ndeny_models = [\"\"]",
            "version = 1\n[router.rules]\nno_review_models = 7",
        ] {
            let reg = Registry::from_toml(text).unwrap();
            assert!(UserRules::from_registry(&reg).is_err(), "{text}");
        }
        // Too many patterns also fails.
        let many = (0..40)
            .map(|i| format!("\"m{i}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let reg = Registry::from_toml(&format!(
            "version = 1\n[router.rules]\ndeny_models = [{many}]"
        ))
        .unwrap();
        assert!(UserRules::from_registry(&reg).is_err());
    }

    #[test]
    fn user_rules_json_round_trip() {
        let rules = UserRules::default();
        let back = UserRules::from_json(Some(&rules.to_json())).unwrap();
        assert_eq!(back, rules);
        assert!(UserRules::from_json(None).unwrap().source == UserRuleSource::Default);
        assert!(
            UserRules::from_json(Some(&serde_json::Value::Null))
                .unwrap()
                .source
                == UserRuleSource::Default
        );
        assert!(UserRules::from_json(Some(&serde_json::json!({"noReviewModels": [42]}))).is_err());
        assert!(UserRules::from_json(Some(&serde_json::json!("x"))).is_err());
    }
}
