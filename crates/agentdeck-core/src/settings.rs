//! Module settings kept in `instances.toml` beside the registry:
//! `[router] decision_provider / decision_model`, `[router.rules]`
//! user model restrictions, `[usage_poll] refresh_seconds / stale_seconds`,
//! `[approver]` broker/judge config, and `[coordinator]` pool/watchdog config.
//! Absent keys mean the built-in defaults.

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::changes::{Change, plan_write_file};
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

pub const DEFAULT_APPROVER_AUTONOMOUS_TIMEOUT_SECS: u64 = 120;
pub const DEFAULT_APPROVER_INTERACTIVE_TIMEOUT_SECS: u64 = 600;
pub const DEFAULT_APPROVER_CONFIDENCE_THRESHOLD: f64 = 0.85;

fn default_approver_autonomous_timeout() -> u64 {
    DEFAULT_APPROVER_AUTONOMOUS_TIMEOUT_SECS
}

fn default_approver_interactive_timeout() -> u64 {
    DEFAULT_APPROVER_INTERACTIVE_TIMEOUT_SECS
}

fn default_approver_confidence_threshold() -> f64 {
    DEFAULT_APPROVER_CONFIDENCE_THRESHOLD
}

/// `[approver]` settings kept in `instances.toml` (ADR-0001).
/// Mirrors `agentdeck_approver::ApproverConfig` so CLI, daemon and GUI share one schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ApproverSettings {
    pub socket_path: Option<PathBuf>,
    #[serde(default = "default_approver_autonomous_timeout")]
    pub autonomous_timeout_secs: u64,
    #[serde(default = "default_approver_interactive_timeout")]
    pub interactive_timeout_secs: u64,
    pub judge_provider: Option<String>,
    pub judge_model: Option<String>,
    #[serde(default = "default_approver_confidence_threshold")]
    pub judge_confidence_threshold: f64,
    pub remote_hosts: Vec<String>,
    pub extra_hard_deny_patterns: Vec<String>,
}

impl Default for ApproverSettings {
    fn default() -> Self {
        Self {
            socket_path: None,
            autonomous_timeout_secs: DEFAULT_APPROVER_AUTONOMOUS_TIMEOUT_SECS,
            interactive_timeout_secs: DEFAULT_APPROVER_INTERACTIVE_TIMEOUT_SECS,
            judge_provider: None,
            judge_model: None,
            judge_confidence_threshold: DEFAULT_APPROVER_CONFIDENCE_THRESHOLD,
            remote_hosts: Vec::new(),
            extra_hard_deny_patterns: Vec::new(),
        }
    }
}

impl ApproverSettings {
    pub fn validate(&self) -> anyhow::Result<()> {
        if !(5..=3600).contains(&self.autonomous_timeout_secs) {
            anyhow::bail!(
                "[approver] autonomous_timeout_secs must be within 5..=3600, got {}",
                self.autonomous_timeout_secs
            );
        }
        if !(5..=7200).contains(&self.interactive_timeout_secs) {
            anyhow::bail!(
                "[approver] interactive_timeout_secs must be within 5..=7200, got {}",
                self.interactive_timeout_secs
            );
        }
        if !self.judge_confidence_threshold.is_finite()
            || !(0.0..=1.0).contains(&self.judge_confidence_threshold)
        {
            anyhow::bail!(
                "[approver] judge_confidence_threshold must be within [0.0, 1.0], got {}",
                self.judge_confidence_threshold
            );
        }
        if let Some(m) = self.judge_model.as_deref()
            && (m.is_empty() || m.chars().any(char::is_whitespace))
        {
            anyhow::bail!("[approver] judge_model must be non-empty and contain no spaces");
        }
        if let Some(p) = self.judge_provider.as_deref() {
            if p != PROVIDER_OPENROUTER && p != PROVIDER_VERCEL {
                anyhow::bail!(
                    "[approver] judge_provider must be {PROVIDER_OPENROUTER} or {PROVIDER_VERCEL}, got {p:?}"
                );
            }
            if p == PROVIDER_VERCEL && self.judge_model.as_deref().is_none_or(str::is_empty) {
                anyhow::bail!("[approver] judge_provider \"vercel\" requires judge_model");
            }
        }
        for host in &self.remote_hosts {
            if host.is_empty() || host.chars().any(|c| c.is_whitespace() || c.is_control()) {
                anyhow::bail!("[approver] invalid remote_hosts entry {host:?}");
            }
        }
        for pat in &self.extra_hard_deny_patterns {
            if pat.is_empty() {
                anyhow::bail!("[approver] extra_hard_deny_patterns must not contain empty patterns");
            }
            regex::Regex::new(&format!("(?i){pat}")).map_err(|e| {
                anyhow::anyhow!("[approver] invalid extra_hard_deny_pattern {pat:?}: {e}")
            })?;
        }
        Ok(())
    }

    pub fn resolved_socket_path(&self) -> PathBuf {
        if let Some(path) = &self.socket_path {
            return path.clone();
        }
        if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
            return PathBuf::from(dir).join("agentdeck").join("approver.sock");
        }
        std::env::temp_dir().join("agentdeck-approver.sock")
    }
}

/// Read `[approver]` from `instances.toml`; absent means defaults. Unknown keys are ignored.
pub fn read_approver(reg: &Registry) -> anyhow::Result<ApproverSettings> {
    let Some(val) = reg.extra.get("approver") else {
        return Ok(ApproverSettings::default());
    };
    if !val.is_table() {
        anyhow::bail!("[approver] must be a TOML table");
    }
    let cfg: ApproverSettings = val
        .clone()
        .try_into()
        .map_err(|e| anyhow::anyhow!("invalid [approver] config: {e}"))?;
    cfg.validate()?;
    Ok(cfg)
}

/// TOML integers are i64: a `u64` that passed validation still converts
/// checked, never with `as`, so a future bound change cannot wrap silently.
fn checked_i64(section: &str, key: &str, v: u64) -> anyhow::Result<i64> {
    i64::try_from(v)
        .map_err(|_| anyhow::anyhow!("[{section}] {key} value {v} does not fit in TOML's i64"))
}

fn apply_approver_to_registry(reg: &mut Registry, cfg: &ApproverSettings) -> anyhow::Result<()> {
    let str_arr = |items: &[String]| {
        (!items.is_empty()).then(|| {
            toml::Value::Array(items.iter().cloned().map(toml::Value::String).collect())
        })
    };
    set(
        reg,
        "approver",
        "socket_path",
        cfg.socket_path
            .as_ref()
            .map(|p| toml::Value::String(p.to_string_lossy().into_owned())),
    );
    set(
        reg,
        "approver",
        "autonomous_timeout_secs",
        Some(toml::Value::Integer(checked_i64(
            "approver",
            "autonomous_timeout_secs",
            cfg.autonomous_timeout_secs,
        )?)),
    );
    set(
        reg,
        "approver",
        "interactive_timeout_secs",
        Some(toml::Value::Integer(checked_i64(
            "approver",
            "interactive_timeout_secs",
            cfg.interactive_timeout_secs,
        )?)),
    );
    set(
        reg,
        "approver",
        "judge_provider",
        cfg.judge_provider.clone().map(toml::Value::String),
    );
    set(
        reg,
        "approver",
        "judge_model",
        cfg.judge_model.clone().map(toml::Value::String),
    );
    set(
        reg,
        "approver",
        "judge_confidence_threshold",
        Some(toml::Value::Float(cfg.judge_confidence_threshold)),
    );
    set(reg, "approver", "remote_hosts", str_arr(&cfg.remote_hosts));
    set(
        reg,
        "approver",
        "extra_hard_deny_patterns",
        str_arr(&cfg.extra_hard_deny_patterns),
    );
    Ok(())
}

/// Validated write of `[approver]` into `instances.toml`, preserving unknown keys.
pub fn write_approver(
    paths: &Paths,
    cfg: &ApproverSettings,
    dry_run: bool,
) -> anyhow::Result<Vec<Change>> {
    cfg.validate()?;
    let mut reg = Registry::load(paths)?;
    apply_approver_to_registry(&mut reg, cfg)?;
    reg.validate()?;
    let new_text = reg.render()?;
    let old = fs::read(paths.registry_file()).ok();
    let changes = plan_write_file(paths.registry_file(), old.as_deref(), new_text.as_bytes())
        .map(|c| vec![c])
        .unwrap_or_default();
    if !dry_run && !changes.is_empty() {
        Registry::update(paths, |r| {
            apply_approver_to_registry(r, cfg)?;
            Ok(())
        })?;
    }
    Ok(changes)
}

/// Read-only check whether `approver.env` defines both Telegram bot token and owner id.
pub fn approver_telegram_configured(config_dir: &std::path::Path) -> bool {
    approver_env_status(config_dir) == ApproverEnvStatus::Configured
}

/// Strict read-only parse of `approver.env`: `Missing` when the file is
/// absent or defines no Telegram keys, `Invalid` when a present key is
/// malformed (e.g. a non-numeric owner id), `Configured` only when both a
/// non-empty token and a numeric owner id are present. Never mutates anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApproverEnvStatus {
    Missing,
    Invalid,
    Configured,
}

impl ApproverEnvStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Invalid => "invalid",
            Self::Configured => "configured",
        }
    }
}

pub(crate) fn approver_env_status(config_dir: &std::path::Path) -> ApproverEnvStatus {
    let Ok(raw) = fs::read_to_string(config_dir.join("approver.env")) else {
        return ApproverEnvStatus::Missing;
    };
    let (mut has_token, mut has_owner, mut invalid) = (false, false, false);
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let trimmed = trimmed.strip_prefix("export ").unwrap_or(trimmed).trim();
        let Some((key, val)) = trimmed.split_once('=') else {
            continue;
        };
        let val = val.trim().trim_matches('"').trim_matches('\'');
        if val.is_empty() {
            continue;
        }
        match key.trim() {
            "TELEGRAM_BOT_TOKEN" => has_token = true,
            "TELEGRAM_OWNER_ID" => {
                if val.parse::<i64>().is_ok() {
                    has_owner = true;
                } else {
                    invalid = true;
                }
            }
            _ => {}
        }
    }
    if has_token && has_owner {
        ApproverEnvStatus::Configured
    } else if invalid {
        ApproverEnvStatus::Invalid
    } else {
        ApproverEnvStatus::Missing
    }
}

fn default_coord_weekly_reserve() -> f64 {
    10.0
}
fn default_coord_proactive_below_5h() -> f64 {
    10.0
}
fn default_coord_sleep_buffer() -> u64 {
    120
}
fn default_coord_wake_grace() -> u64 {
    180
}
fn default_coord_soft_runs() -> u32 {
    80
}
fn default_coord_soft_items() -> u32 {
    1500
}
fn default_coord_confidence() -> f64 {
    0.80
}
fn default_coord_min_disk() -> u64 {
    15
}
fn default_true_bool() -> bool {
    true
}

/// One strong-model entry in `[coordinator]`'s dedicated `Coordinator Pool`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CoordinatorPoolEntry {
    pub instance_id: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub options: serde_json::Map<String, serde_json::Value>,
    #[serde(default = "default_true_bool")]
    pub enabled: bool,
}

/// `[coordinator]` settings kept in `instances.toml` (ADR-0002).
/// Mirrors `agentdeck_coordinator::CoordinatorConfig` for CLI, daemon and GUI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CoordinatorSettings {
    #[serde(default)]
    pub pool: Vec<CoordinatorPoolEntry>,
    #[serde(default = "default_coord_weekly_reserve")]
    pub min_weekly_reserve_pct: f64,
    #[serde(default = "default_coord_proactive_below_5h")]
    pub proactive_rotate_below_5h_pct: f64,
    #[serde(default = "default_coord_sleep_buffer")]
    pub quota_sleep_buffer_secs: u64,
    #[serde(default = "default_coord_wake_grace")]
    pub idle_wake_grace_secs: u64,
    #[serde(default = "default_coord_soft_runs")]
    pub checkpoint_soft_runs: u32,
    #[serde(default = "default_coord_soft_items")]
    pub checkpoint_soft_items: u32,
    #[serde(default = "default_coord_confidence")]
    pub checkpoint_confidence_threshold: f64,
    #[serde(default = "default_coord_min_disk")]
    pub min_free_disk_gb: u64,
    #[serde(default)]
    pub t3_endpoint: Option<String>,
    #[serde(default)]
    pub t3_token_file: Option<PathBuf>,
    #[serde(default)]
    pub judge_provider: Option<String>,
    #[serde(default)]
    pub judge_model: Option<String>,
}

impl Default for CoordinatorSettings {
    fn default() -> Self {
        Self {
            pool: Vec::new(),
            min_weekly_reserve_pct: default_coord_weekly_reserve(),
            proactive_rotate_below_5h_pct: default_coord_proactive_below_5h(),
            quota_sleep_buffer_secs: default_coord_sleep_buffer(),
            idle_wake_grace_secs: default_coord_wake_grace(),
            checkpoint_soft_runs: default_coord_soft_runs(),
            checkpoint_soft_items: default_coord_soft_items(),
            checkpoint_confidence_threshold: default_coord_confidence(),
            min_free_disk_gb: default_coord_min_disk(),
            t3_endpoint: None,
            t3_token_file: None,
            judge_provider: None,
            judge_model: None,
        }
    }
}

/// Strict loopback check for `[coordinator] t3_endpoint`: an `http(s)://` URL
/// on exactly `localhost`, `127.0.0.1` or `[::1]`, with an optional numeric
/// 1-65535 port and no userinfo, query or fragment. A hand-rolled splitter
/// here once accepted `http://[::1].attacker.com/mcp` and non-numeric ports.
pub fn validate_loopback_http(endpoint: &str) -> anyhow::Result<()> {
    if endpoint
        .chars()
        .any(|c| c.is_control() || c == ' ' || c == '\u{7f}')
    {
        anyhow::bail!(
            "[coordinator] t3_endpoint must not contain spaces or control characters"
        );
    }
    let rest = endpoint
        .strip_prefix("http://")
        .or_else(|| endpoint.strip_prefix("https://"))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "[coordinator] t3_endpoint must be a loopback http(s) URL (got {endpoint:?})"
            )
        })?;
    if rest.contains(['?', '#']) {
        anyhow::bail!(
            "[coordinator] t3_endpoint must be loopback http(s) without query or fragment"
        );
    }
    let authority = rest.split('/').next().unwrap_or("");
    if authority.contains('@') {
        anyhow::bail!("[coordinator] t3_endpoint must not contain userinfo");
    }
    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        let (inside, after) = bracketed.split_once(']').ok_or_else(|| {
            anyhow::anyhow!("[coordinator] t3_endpoint has a malformed IPv6 host")
        })?;
        if inside != "::1" {
            anyhow::bail!("[coordinator] t3_endpoint must be loopback (127.0.0.1, localhost or [::1])");
        }
        let port = match after {
            "" => None,
            tail => {
                let digits = tail.strip_prefix(':').ok_or_else(|| {
                    anyhow::anyhow!(
                        "[coordinator] t3_endpoint must be loopback (127.0.0.1, localhost or [::1])"
                    )
                })?;
                Some(digits)
            }
        };
        ("[::1]", port)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => (h, Some(p)),
            Some(_) => {
                anyhow::bail!("[coordinator] t3_endpoint port must be numeric (1-65535)");
            }
            None => (authority, None),
        }
    };
    if host != "127.0.0.1" && host != "localhost" && host != "[::1]" {
        anyhow::bail!(
            "[coordinator] t3_endpoint must be loopback (127.0.0.1, localhost or [::1])"
        );
    }
    if let Some(p) = port {
        let n: u32 = p.parse().map_err(|_| {
            anyhow::anyhow!("[coordinator] t3_endpoint port must be numeric (1-65535)")
        })?;
        if !(1..=65535).contains(&n) {
            anyhow::bail!("[coordinator] t3_endpoint port must be within 1-65535");
        }
    }
    Ok(())
}

impl CoordinatorSettings {
    pub fn validate(&self) -> anyhow::Result<()> {
        if !self.min_weekly_reserve_pct.is_finite()
            || !(0.0..=100.0).contains(&self.min_weekly_reserve_pct)
        {
            anyhow::bail!(
                "[coordinator] min_weekly_reserve_pct must be within [0.0, 100.0], got {}",
                self.min_weekly_reserve_pct
            );
        }
        if !self.proactive_rotate_below_5h_pct.is_finite()
            || !(0.0..=100.0).contains(&self.proactive_rotate_below_5h_pct)
        {
            anyhow::bail!(
                "[coordinator] proactive_rotate_below_5h_pct must be within [0.0, 100.0], got {}",
                self.proactive_rotate_below_5h_pct
            );
        }
        if !self.checkpoint_confidence_threshold.is_finite()
            || !(0.0..=1.0).contains(&self.checkpoint_confidence_threshold)
        {
            anyhow::bail!(
                "[coordinator] checkpoint_confidence_threshold must be within [0.0, 1.0], got {}",
                self.checkpoint_confidence_threshold
            );
        }
        if !(0..=3600).contains(&self.quota_sleep_buffer_secs) {
            anyhow::bail!(
                "[coordinator] quota_sleep_buffer_secs must be within 0..=3600, got {}",
                self.quota_sleep_buffer_secs
            );
        }
        if !(0..=86400).contains(&self.idle_wake_grace_secs) {
            anyhow::bail!(
                "[coordinator] idle_wake_grace_secs must be within 0..=86400, got {}",
                self.idle_wake_grace_secs
            );
        }
        if !(1..=10_000).contains(&self.checkpoint_soft_runs) {
            anyhow::bail!(
                "[coordinator] checkpoint_soft_runs must be within 1..=10000, got {}",
                self.checkpoint_soft_runs
            );
        }
        if !(10..=1_000_000).contains(&self.checkpoint_soft_items) {
            anyhow::bail!(
                "[coordinator] checkpoint_soft_items must be within 10..=1000000, got {}",
                self.checkpoint_soft_items
            );
        }
        if !(1..=100_000).contains(&self.min_free_disk_gb) {
            anyhow::bail!(
                "[coordinator] min_free_disk_gb must be within 1..=100000, got {}",
                self.min_free_disk_gb
            );
        }
        if self.checkpoint_soft_runs == 0 {
            anyhow::bail!("[coordinator] checkpoint_soft_runs must be positive");
        }
        if self.checkpoint_soft_items == 0 {
            anyhow::bail!("[coordinator] checkpoint_soft_items must be positive");
        }
        for (i, entry) in self.pool.iter().enumerate() {
            if entry.instance_id.is_empty()
                || entry.instance_id.chars().any(char::is_whitespace)
            {
                anyhow::bail!(
                    "[coordinator] pool entry has invalid instance_id {:?}",
                    entry.instance_id
                );
            }
            if entry.model.is_empty() || entry.model.chars().any(char::is_whitespace) {
                anyhow::bail!(
                    "[coordinator] pool entry {:?} has invalid model {:?}",
                    entry.instance_id,
                    entry.model
                );
            }
            if self.pool[..i].iter().any(|e| {
                e.instance_id == entry.instance_id && e.model == entry.model
            }) {
                anyhow::bail!(
                    "[coordinator] duplicate pool entry ({:?}, {:?})",
                    entry.instance_id,
                    entry.model
                );
            }
        }
        if let Some(ep) = self.t3_endpoint.as_deref() {
            validate_loopback_http(ep)?;
        }
        if let Some(m) = self.judge_model.as_deref()
            && (m.is_empty() || m.chars().any(char::is_whitespace))
        {
            anyhow::bail!("[coordinator] judge_model must be non-empty and contain no spaces");
        }
        if let Some(p) = self.judge_provider.as_deref() {
            if p != PROVIDER_OPENROUTER && p != PROVIDER_VERCEL {
                anyhow::bail!(
                    "[coordinator] judge_provider must be {PROVIDER_OPENROUTER} or {PROVIDER_VERCEL}, got {p:?}"
                );
            }
            if p == PROVIDER_VERCEL && self.judge_model.as_deref().is_none_or(str::is_empty) {
                anyhow::bail!("[coordinator] judge_provider \"vercel\" requires judge_model");
            }
        }
        Ok(())
    }
}

/// Read `[coordinator]` from `instances.toml`; absent means defaults. Unknown keys are ignored.
pub fn read_coordinator(reg: &Registry) -> anyhow::Result<CoordinatorSettings> {
    let Some(val) = reg.extra.get("coordinator") else {
        return Ok(CoordinatorSettings::default());
    };
    if !val.is_table() {
        anyhow::bail!("[coordinator] must be a TOML table");
    }
    let cfg: CoordinatorSettings = val
        .clone()
        .try_into()
        .map_err(|e| anyhow::anyhow!("invalid [coordinator] config: {e}"))?;
    cfg.validate()?;
    Ok(cfg)
}

fn apply_coordinator_to_registry(
    reg: &mut Registry,
    cfg: &CoordinatorSettings,
) -> anyhow::Result<()> {
    let pool_val = if cfg.pool.is_empty() {
        None
    } else {
        Some(
            toml::Value::try_from(&cfg.pool)
                .map_err(|e| anyhow::anyhow!("cannot serialize [coordinator] pool: {e}"))?,
        )
    };
    set(reg, "coordinator", "pool", pool_val);
    set(
        reg,
        "coordinator",
        "min_weekly_reserve_pct",
        Some(toml::Value::Float(cfg.min_weekly_reserve_pct)),
    );
    set(
        reg,
        "coordinator",
        "proactive_rotate_below_5h_pct",
        Some(toml::Value::Float(cfg.proactive_rotate_below_5h_pct)),
    );
    set(
        reg,
        "coordinator",
        "quota_sleep_buffer_secs",
        Some(toml::Value::Integer(checked_i64(
            "coordinator",
            "quota_sleep_buffer_secs",
            cfg.quota_sleep_buffer_secs,
        )?)),
    );
    set(
        reg,
        "coordinator",
        "idle_wake_grace_secs",
        Some(toml::Value::Integer(checked_i64(
            "coordinator",
            "idle_wake_grace_secs",
            cfg.idle_wake_grace_secs,
        )?)),
    );
    set(
        reg,
        "coordinator",
        "checkpoint_soft_runs",
        Some(toml::Value::Integer(checked_i64(
            "coordinator",
            "checkpoint_soft_runs",
            u64::from(cfg.checkpoint_soft_runs),
        )?)),
    );
    set(
        reg,
        "coordinator",
        "checkpoint_soft_items",
        Some(toml::Value::Integer(checked_i64(
            "coordinator",
            "checkpoint_soft_items",
            u64::from(cfg.checkpoint_soft_items),
        )?)),
    );
    set(
        reg,
        "coordinator",
        "checkpoint_confidence_threshold",
        Some(toml::Value::Float(cfg.checkpoint_confidence_threshold)),
    );
    set(
        reg,
        "coordinator",
        "min_free_disk_gb",
        Some(toml::Value::Integer(checked_i64(
            "coordinator",
            "min_free_disk_gb",
            cfg.min_free_disk_gb,
        )?)),
    );
    set(
        reg,
        "coordinator",
        "t3_endpoint",
        cfg.t3_endpoint.clone().map(toml::Value::String),
    );
    set(
        reg,
        "coordinator",
        "t3_token_file",
        cfg.t3_token_file
            .as_ref()
            .map(|p| toml::Value::String(p.to_string_lossy().into_owned())),
    );
    set(
        reg,
        "coordinator",
        "judge_provider",
        cfg.judge_provider.clone().map(toml::Value::String),
    );
    set(
        reg,
        "coordinator",
        "judge_model",
        cfg.judge_model.clone().map(toml::Value::String),
    );
    Ok(())
}

/// Validated write of `[coordinator]` into `instances.toml`, preserving unknown keys.
pub fn write_coordinator(
    paths: &Paths,
    cfg: &CoordinatorSettings,
    dry_run: bool,
) -> anyhow::Result<Vec<Change>> {
    cfg.validate()?;
    let mut reg = Registry::load(paths)?;
    apply_coordinator_to_registry(&mut reg, cfg)?;
    reg.validate()?;
    let new_text = reg.render()?;
    let old = fs::read(paths.registry_file()).ok();
    let changes = plan_write_file(paths.registry_file(), old.as_deref(), new_text.as_bytes())
        .map(|c| vec![c])
        .unwrap_or_default();
    if !dry_run && !changes.is_empty() {
        Registry::update(paths, |r| apply_coordinator_to_registry(r, cfg))?;
    }
    Ok(changes)
}

/// Locked delta update of `[approver]`: reads the latest settings inside the
/// registry lock, applies `f`, validates and writes back, so concurrent
/// single-field edits never overwrite each other. Returns the saved settings.
pub fn update_approver(
    paths: &Paths,
    f: impl FnOnce(&mut ApproverSettings) -> anyhow::Result<()>,
) -> anyhow::Result<ApproverSettings> {
    Registry::update_locked(paths, |reg| {
        let mut cfg = read_approver(reg)?;
        f(&mut cfg)?;
        cfg.validate()?;
        apply_approver_to_registry(reg, &cfg)?;
        Ok(cfg)
    })
}

/// Locked delta update of `[coordinator]`: same pattern as
/// [`update_approver`]. Returns the saved settings.
pub fn update_coordinator(
    paths: &Paths,
    f: impl FnOnce(&mut CoordinatorSettings) -> anyhow::Result<()>,
) -> anyhow::Result<CoordinatorSettings> {
    Registry::update_locked(paths, |reg| {
        let mut cfg = read_coordinator(reg)?;
        f(&mut cfg)?;
        cfg.validate()?;
        apply_coordinator_to_registry(reg, &cfg)?;
        Ok(cfg)
    })
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

    #[test]
    fn approver_settings_round_trip_preserves_unknown_keys_and_validates() {
        let dir = std::env::temp_dir().join(format!(
            "agentdeck-settings-approver-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let paths = Paths::new(dir.join("config"), dir.join("state"));
        assert_eq!(
            read_approver(&Registry::empty()).unwrap(),
            ApproverSettings::default()
        );
        fs::create_dir_all(&paths.config_dir).unwrap();
        fs::write(
            paths.registry_file(),
            "version = 1\n[approver]\nfuture_flag = true\n",
        )
        .unwrap();
        let cfg = ApproverSettings {
            socket_path: Some(PathBuf::from("/tmp/custom-approver.sock")),
            autonomous_timeout_secs: 90,
            interactive_timeout_secs: 300,
            judge_provider: Some(PROVIDER_OPENROUTER.into()),
            judge_model: Some("~typesafe/jev-latest".into()),
            judge_confidence_threshold: 0.9,
            remote_hosts: vec!["homeserver".into()],
            extra_hard_deny_patterns: vec!["drop\\s+schema".into()],
        };
        // dry-run leaves file untouched.
        let planned = write_approver(&paths, &cfg, true).unwrap();
        assert_eq!(planned.len(), 1);
        assert_eq!(
            read_approver(&Registry::load(&paths).unwrap()).unwrap(),
            ApproverSettings::default()
        );
        write_approver(&paths, &cfg, false).unwrap();
        let reg = Registry::load(&paths).unwrap();
        assert_eq!(read_approver(&reg).unwrap(), cfg);
        assert_eq!(
            reg.extra["approver"]["future_flag"].as_bool(),
            Some(true),
            "unknown keys in [approver] must survive writes"
        );
        // Invalid configs fail closed.
        let bad_regex = ApproverSettings {
            extra_hard_deny_patterns: vec!["([".into()],
            ..cfg.clone()
        };
        assert!(write_approver(&paths, &bad_regex, false).is_err());
        let bad_threshold = ApproverSettings {
            judge_confidence_threshold: 1.5,
            ..cfg.clone()
        };
        assert!(write_approver(&paths, &bad_threshold, false).is_err());
        let bad_vercel = ApproverSettings {
            judge_provider: Some(PROVIDER_VERCEL.into()),
            judge_model: None,
            ..cfg
        };
        assert!(write_approver(&paths, &bad_vercel, false).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn coordinator_settings_round_trip_preserves_unknown_keys_and_validates() {
        let dir = std::env::temp_dir().join(format!(
            "agentdeck-settings-coord-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let paths = Paths::new(dir.join("config"), dir.join("state"));
        assert_eq!(
            read_coordinator(&Registry::empty()).unwrap(),
            CoordinatorSettings::default()
        );
        fs::create_dir_all(&paths.config_dir).unwrap();
        fs::write(
            paths.registry_file(),
            "version = 1\n[coordinator]\nfuture_knob = \"keep\"\n",
        )
        .unwrap();
        let mut opts = serde_json::Map::new();
        opts.insert("effort".into(), serde_json::json!("high"));
        let cfg = CoordinatorSettings {
            pool: vec![
                CoordinatorPoolEntry {
                    instance_id: "claude_a".into(),
                    model: "claude-opus-4-6".into(),
                    options: opts,
                    enabled: true,
                },
                CoordinatorPoolEntry {
                    instance_id: "codex_b".into(),
                    model: "gpt-6.2-sol".into(),
                    options: Default::default(),
                    enabled: false,
                },
            ],
            min_weekly_reserve_pct: 12.0,
            proactive_rotate_below_5h_pct: 15.0,
            quota_sleep_buffer_secs: 180,
            idle_wake_grace_secs: 240,
            checkpoint_soft_runs: 60,
            checkpoint_soft_items: 1200,
            checkpoint_confidence_threshold: 0.82,
            min_free_disk_gb: 20,
            t3_endpoint: Some("http://127.0.0.1:3773/mcp".into()),
            t3_token_file: Some(PathBuf::from("/tmp/t3.token")),
            judge_provider: Some(PROVIDER_OPENROUTER.into()),
            judge_model: Some("~typesafe/jev-latest".into()),
        };
        write_coordinator(&paths, &cfg, false).unwrap();
        let reg = Registry::load(&paths).unwrap();
        assert_eq!(read_coordinator(&reg).unwrap(), cfg);
        assert_eq!(
            reg.extra["coordinator"]["future_knob"].as_str(),
            Some("keep")
        );
        // Remote endpoint rejected.
        let bad_ep = CoordinatorSettings {
            t3_endpoint: Some("http://example.com/mcp".into()),
            ..cfg.clone()
        };
        assert!(write_coordinator(&paths, &bad_ep, false).is_err());
        let bad_pct = CoordinatorSettings {
            min_weekly_reserve_pct: 101.0,
            ..cfg
        };
        assert!(write_coordinator(&paths, &bad_pct, false).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn loopback_validator_rejects_bypass_forms() {
        for good in [
            "http://127.0.0.1:3773/mcp",
            "http://127.0.0.1/mcp",
            "http://localhost:8080/",
            "http://localhost/",
            "https://localhost:1/x",
            "http://[::1]/mcp",
            "https://[::1]:443/mcp",
        ] {
            assert!(validate_loopback_http(good).is_ok(), "{good}");
        }
        for bad in [
            "http://[::1].attacker.com/mcp",
            "http://127.0.0.1:notaport/mcp",
            "http://127.0.0.1:/mcp",
            "http://127.0.0.1:0/mcp",
            "http://127.0.0.1:65536/mcp",
            "http://127.0.0.1:99999/mcp",
            "http://user@127.0.0.1/mcp",
            "http://user:pass@localhost:8080/mcp",
            "http://example.com/mcp",
            "http://localhost.evil.com/mcp",
            "http://127.0.0.1.evil.com/mcp",
            "http://[::2]/mcp",
            "http://[::1/mcp",
            "http://[::1]evil/mcp",
            "ftp://127.0.0.1/mcp",
            "HTTP://127.0.0.1/mcp",
            "http://127.0.0.1:8080/mcp?x=1",
            "http://127.0.0.1:8080/mcp#frag",
            "http://127.0.0.1:8080/m cp",
            "http://127.0.0.1:8080/\n",
            "http://::1/mcp",
            "",
        ] {
            assert!(validate_loopback_http(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn numeric_bounds_reject_edges() {
        let huge = u64::MAX;
        let over_i64 = i64::MAX as u64 + 1;
        for v in [0, huge, over_i64] {
            assert!(
                ApproverSettings {
                    autonomous_timeout_secs: v,
                    ..ApproverSettings::default()
                }
                .validate()
                .is_err(),
                "autonomous {v}"
            );
            assert!(
                ApproverSettings {
                    interactive_timeout_secs: v,
                    ..ApproverSettings::default()
                }
                .validate()
                .is_err(),
                "interactive {v}"
            );
            // 0 is valid for the buffer/grace ranges, huge values are not.
            assert_eq!(
                CoordinatorSettings {
                    quota_sleep_buffer_secs: v,
                    checkpoint_soft_runs: 80,
                    checkpoint_soft_items: 1500,
                    min_free_disk_gb: 15,
                    ..CoordinatorSettings::default()
                }
                .validate()
                .is_err(),
                v != 0,
                "buffer {v}"
            );
            assert_eq!(
                CoordinatorSettings {
                    idle_wake_grace_secs: v,
                    ..CoordinatorSettings::default()
                }
                .validate()
                .is_err(),
                v != 0,
                "grace {v}"
            );
            assert!(
                CoordinatorSettings {
                    checkpoint_soft_runs: v.min(u32::MAX as u64) as u32,
                    ..CoordinatorSettings::default()
                }
                .validate()
                .is_err(),
                "soft_runs {v}"
            );
            assert!(
                CoordinatorSettings {
                    checkpoint_soft_items: v.min(u32::MAX as u64) as u32,
                    ..CoordinatorSettings::default()
                }
                .validate()
                .is_err(),
                "soft_items {v}"
            );
            assert!(
                CoordinatorSettings {
                    min_free_disk_gb: v,
                    ..CoordinatorSettings::default()
                }
                .validate()
                .is_err(),
                "disk {v}"
            );
        }
        // Valid edges pass.
        assert!(
            ApproverSettings {
                autonomous_timeout_secs: 5,
                interactive_timeout_secs: 7200,
                ..ApproverSettings::default()
            }
            .validate()
            .is_ok()
        );
        // Just over the top of each bounded range fails.
        for cfg in [
            CoordinatorSettings {
                quota_sleep_buffer_secs: 3601,
                ..CoordinatorSettings::default()
            },
            CoordinatorSettings {
                idle_wake_grace_secs: 86401,
                ..CoordinatorSettings::default()
            },
            CoordinatorSettings {
                checkpoint_soft_runs: 10_001,
                ..CoordinatorSettings::default()
            },
            CoordinatorSettings {
                checkpoint_soft_items: 1_000_001,
                ..CoordinatorSettings::default()
            },
            CoordinatorSettings {
                checkpoint_soft_items: 9,
                ..CoordinatorSettings::default()
            },
            CoordinatorSettings {
                min_free_disk_gb: 100_001,
                ..CoordinatorSettings::default()
            },
            CoordinatorSettings {
                proactive_rotate_below_5h_pct: 100.5,
                ..CoordinatorSettings::default()
            },
        ] {
            assert!(cfg.validate().is_err(), "{cfg:?}");
        }
        assert!(
            CoordinatorSettings {
                quota_sleep_buffer_secs: 0,
                idle_wake_grace_secs: 86400,
                checkpoint_soft_runs: 1,
                checkpoint_soft_items: 10,
                min_free_disk_gb: 1,
                ..CoordinatorSettings::default()
            }
            .validate()
            .is_ok()
        );
        // Non-finite floats fail.
        assert!(
            CoordinatorSettings {
                min_weekly_reserve_pct: f64::NAN,
                ..CoordinatorSettings::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            ApproverSettings {
                judge_confidence_threshold: f64::INFINITY,
                ..ApproverSettings::default()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn vercel_requires_judge_model_in_both_sections() {
        assert!(
            CoordinatorSettings {
                judge_provider: Some(PROVIDER_VERCEL.into()),
                judge_model: None,
                ..CoordinatorSettings::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            CoordinatorSettings {
                judge_provider: Some(PROVIDER_VERCEL.into()),
                judge_model: Some("".into()),
                ..CoordinatorSettings::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            CoordinatorSettings {
                judge_provider: Some(PROVIDER_VERCEL.into()),
                judge_model: Some("openai/gpt-5".into()),
                ..CoordinatorSettings::default()
            }
            .validate()
            .is_ok()
        );
    }

    #[test]
    fn duplicate_pool_keys_rejected() {
        let entry = |instance_id: &str, model: &str| CoordinatorPoolEntry {
            instance_id: instance_id.into(),
            model: model.into(),
            options: Default::default(),
            enabled: true,
        };
        assert!(
            CoordinatorSettings {
                pool: vec![entry("agy", "m1"), entry("agy", "m1")],
                ..CoordinatorSettings::default()
            }
            .validate()
            .is_err()
        );
        // Same model on different instances is fine.
        assert!(
            CoordinatorSettings {
                pool: vec![entry("a", "m1"), entry("b", "m1")],
                ..CoordinatorSettings::default()
            }
            .validate()
            .is_ok()
        );
    }

    #[test]
    fn locked_delta_updates_preserve_untouched_fields() {
        let dir = std::env::temp_dir().join(format!(
            "agentdeck-settings-delta-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let paths = Paths::new(dir.join("config"), dir.join("state"));
        update_approver(&paths, |cfg| {
            cfg.autonomous_timeout_secs = 90;
            cfg.remote_hosts = vec!["homeserver".into()];
            Ok(())
        })
        .unwrap();
        // An interleaved edit on another thread keeps the first edit's fields.
        std::thread::scope(|s| {
            let p = Paths::new(paths.config_dir.clone(), paths.state_dir.clone());
            s.spawn(move || {
                update_approver(&p, |cfg| {
                    cfg.interactive_timeout_secs = 300;
                    Ok(())
                })
                .unwrap();
            });
            update_coordinator(&paths, |cfg| {
                cfg.min_free_disk_gb = 20;
                Ok(())
            })
            .unwrap();
        });
        let reg = Registry::load(&paths).unwrap();
        let app = read_approver(&reg).unwrap();
        assert_eq!(app.autonomous_timeout_secs, 90);
        assert_eq!(app.interactive_timeout_secs, 300);
        assert_eq!(app.remote_hosts, vec!["homeserver".to_string()]);
        let coord = read_coordinator(&reg).unwrap();
        assert_eq!(coord.min_free_disk_gb, 20);
        assert_eq!(coord.checkpoint_soft_runs, default_coord_soft_runs());
        // Invalid deltas fail without writing.
        assert!(
            update_approver(&paths, |cfg| {
                cfg.autonomous_timeout_secs = 0;
                Ok(())
            })
            .is_err()
        );
        assert_eq!(
            read_approver(&Registry::load(&paths).unwrap()).unwrap().autonomous_timeout_secs,
            90
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
