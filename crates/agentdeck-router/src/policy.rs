//! Port of `policy.mjs`: router JSON loading, quota math and ranking.

use regex::Regex;
use serde_json::Value;
use std::collections::HashMap;

use agentdeck_core::settings::UserRules;

pub const MIN_FIT: f64 = 0.7;
pub const TARGET_MARGIN: f64 = 0.02;
pub type CostsOf<'a> = dyn Fn(&str, Option<&str>) -> ([f64; 4], f64) + 'a;
pub const NO_REVIEW_BAN: &str = "never for audits or reviews (user rule 2026-10-17)";

const ROSTER_JSON: &str = include_str!("../../../crates/agentdeck-core/assets/router/roster.json");

pub fn roster() -> &'static Value {
    static CELL: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    CELL.get_or_init(|| serde_json::from_str(ROSTER_JSON).expect("embedded roster.json is valid"))
}

pub fn routing_weights() -> (f64, f64, f64, f64) {
    let r = &roster()["routing"];
    (
        r["sharedQuotaPenalty"].as_f64().unwrap_or(0.0),
        r["paidCreditPenalty"].as_f64().unwrap_or(0.0),
        r["cheapModelBonus"].as_f64().unwrap_or(0.0),
        r["weeklyReservePercent"].as_f64().unwrap_or(0.0),
    )
}

#[derive(Clone, Debug)]
pub struct Family {
    pub burn: f64,
    pub profile: Option<String>,
    pub min_effort: Option<String>,
    pub spend_to_zero: bool,
    pub no_review: bool,
    pub brief_hint: Option<String>,
}

fn families() -> Vec<(String, Family)> {
    roster()["families"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|f| {
            Some((
                f.get("match")?.as_str()?.to_string(),
                Family {
                    burn: f.get("burn")?.as_f64()?,
                    profile: f
                        .get("profile")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                    min_effort: f
                        .get("minEffort")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                    spend_to_zero: f
                        .get("spendToZero")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    no_review: f.get("noReview").and_then(Value::as_bool).unwrap_or(false),
                    brief_hint: f
                        .get("briefHint")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                },
            ))
        })
        .collect()
}

/// First roster family whose `match` regex hits `model`, case-insensitively.
pub fn family_of(model: &str) -> Option<Family> {
    // shortcut: patterns are recompiled per call; the router runs once per
    // invocation and the roster holds ~16 entries, so no cache is worth it.
    for (pat, fam) in families() {
        if Regex::new(&format!("(?i){pat}")).is_ok_and(|re| re.is_match(model)) {
            return Some(fam);
        }
    }
    None
}

pub fn max_burn() -> f64 {
    families()
        .into_iter()
        .map(|(_, f)| f.burn)
        .fold(1.0_f64, f64::max)
}

pub fn roster_brief_hint() -> String {
    roster()["briefHint"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

pub fn hint_of(model: &str) -> Option<String> {
    let parts = [
        Some(roster_brief_hint()),
        family_of(model).and_then(|f| f.brief_hint),
    ]
    .into_iter()
    .flatten()
    .filter(|s| !s.is_empty())
    .collect::<Vec<_>>();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    }
}

fn review_re() -> Regex {
    static CELL: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    CELL.get_or_init(|| {
        // JS `\b` is ASCII-only; mirror it exactly rather than using Unicode `\b`.
        Regex::new(r"(?i)(?:^|[^A-Za-z0-9_])(?:audit|review)|аудит|ревью|рев['’ʼ]ю")
            .expect("review regex")
    })
    .clone()
}

pub fn is_review_task(task: &str) -> bool {
    review_re().is_match(task)
}

/// Opt-in automatic evaluation of settled workflow tasks (`workflowEval` in
/// the router JSON). Default off. `error` disables auto spawns with a reason
/// but never fails config loading; manual `workflow eval` then uses default
/// budget limits.
#[derive(Clone, Debug)]
pub struct WorkflowEvalConfig {
    pub auto: bool,
    pub sample: u64,
    pub max_per_day: u64,
    pub max_usd_per_day: Option<f64>,
    pub assume_usd_per_eval: Option<f64>,
    pub error: Option<String>,
}

impl Default for WorkflowEvalConfig {
    fn default() -> Self {
        Self {
            auto: false,
            sample: 1,
            max_per_day: 20,
            max_usd_per_day: None,
            assume_usd_per_eval: None,
            error: None,
        }
    }
}

fn parse_workflow_eval(data: &Value) -> WorkflowEvalConfig {
    let bad = |why: &str| WorkflowEvalConfig {
        error: Some(why.to_string()),
        ..WorkflowEvalConfig::default()
    };
    let Some(v) = data.get("workflowEval") else {
        return WorkflowEvalConfig::default();
    };
    let o = match v.as_object() {
        Some(o) => o,
        None => return bad("workflowEval must be an object"),
    };
    let auto = match o.get("auto") {
        None => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return bad("workflowEval.auto must be a boolean"),
    };
    let sample = match o.get("sample") {
        None => 1,
        Some(n) => match n.as_u64() {
            Some(n) if (1..=crate::workflow_eval::MAX_SAMPLE).contains(&n) => n,
            _ => {
                return bad("workflowEval.sample must be 1-1024");
            }
        },
    };
    let max_per_day = match o.get("maxPerDay") {
        None => 20,
        Some(n) => match n.as_u64() {
            Some(n) if n >= 1 => n,
            _ => return bad("workflowEval.maxPerDay must be a positive integer"),
        },
    };
    let usd = |key: &str| -> Result<Option<f64>, String> {
        match o.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(n) => match n.as_f64() {
                Some(n) if n.is_finite() && n >= 0.0 => Ok(Some(n)),
                _ => Err(format!("workflowEval.{key} must be a number >= 0 or null")),
            },
        }
    };
    let max_usd_per_day = match usd("maxUsdPerDay") {
        Ok(v) => v,
        Err(e) => return bad(&e),
    };
    let assume_usd_per_eval = match usd("assumeUsdPerEval") {
        Ok(v) => v,
        Err(e) => return bad(&e),
    };
    if max_usd_per_day.is_some() && assume_usd_per_eval.is_none() {
        return bad("workflowEval.maxUsdPerDay needs assumeUsdPerEval");
    }
    WorkflowEvalConfig {
        auto,
        sample,
        max_per_day,
        max_usd_per_day,
        assume_usd_per_eval,
        error: None,
    }
}

#[derive(Clone, Debug)]
pub struct AccountMeta {
    pub usage_provider: String,
    pub instances: Vec<String>,
    pub usage_id: Option<String>,
    pub model_prefix: Option<String>,
}

#[derive(Clone, Debug)]
pub struct RouterConfig {
    pub file: String,
    pub version: Value,
    pub pool: Vec<Value>,
    pub instance_limits: HashMap<String, u32>,
    pub usage_state_path: Option<String>,
    /// Jev judge model; `None` uses the built-in default.
    pub decision_model: Option<String>,
    /// `"vercel"` sends the judgment through Vercel AI Gateway; else OpenRouter.
    pub decision_provider: Option<String>,
    /// Opt-in automatic evaluation of settled workflow tasks; default off.
    pub workflow_eval: WorkflowEvalConfig,
    /// Persistent user model restrictions; defaults preserve the Argon
    /// audit/review ban exactly when the generated JSON predates them.
    pub user_rules: UserRules,
    pub account: HashMap<String, String>,
    pub usage_id: HashMap<String, String>,
    /// Insertion-ordered, like `Object.entries` on the JSON object.
    pub meta: Vec<(String, AccountMeta)>,
}

pub fn router_config() -> anyhow::Result<RouterConfig> {
    if let Ok(file) = std::env::var("MODEL_ROUTE_ROUTER_JSON") {
        return load_router_json(&file);
    }
    registry_config(&agentdeck_core::Paths::from_env()?)
}

fn registry_config(paths: &agentdeck_core::Paths) -> anyhow::Result<RouterConfig> {
    let reg = agentdeck_core::Registry::load(paths)?;
    parse_router_config(
        agentdeck_core::router::router_json_value(&reg)?,
        "instances.toml",
    )
}

pub fn load_router_json(file: &str) -> anyhow::Result<RouterConfig> {
    let fail = |why| {
        anyhow::anyhow!(
            "model-route: router JSON unusable at {file}: {why}; regenerate it with `agentdeck router json`"
        )
    };
    let text = std::fs::read_to_string(file).map_err(|e| fail(e.to_string()))?;
    parse_router_config(
        serde_json::from_str(&text).map_err(|e| fail(e.to_string()))?,
        file,
    )
}

fn parse_router_config(data: Value, source: &str) -> anyhow::Result<RouterConfig> {
    let fail = |why: String| {
        anyhow::anyhow!(
            "model-route: router JSON unusable at {source}: {why}; regenerate it with `agentdeck router json`"
        )
    };
    let pool = data
        .get("pool")
        .and_then(Value::as_array)
        .ok_or_else(|| fail("expected {pool[], accounts{}}".into()))?
        .clone();
    let accounts = data
        .get("accounts")
        .and_then(Value::as_object)
        .ok_or_else(|| fail("expected {pool[], accounts{}}".into()))?
        .clone();
    for e in &pool {
        let ok = e.is_object()
            && e.get("providerInstanceId")
                .and_then(Value::as_str)
                .is_some()
            && e.get("model").and_then(Value::as_str).is_some()
            && e.get("options").is_none_or(|o| o.is_object());
        if !ok {
            return Err(fail("bad pool entry".into()));
        }
    }
    if data.get("usageStatePath").is_some_and(|v| !v.is_string()) {
        return Err(fail("bad usageStatePath".into()));
    }
    let mut instance_limits = HashMap::new();
    if let Some(limits) = data.get("instanceLimits") {
        for (id, limit) in limits
            .as_object()
            .ok_or_else(|| fail("bad instanceLimits".into()))?
        {
            let n = limit
                .as_u64()
                .filter(|n| *n > 0 && *n <= u32::MAX as u64)
                .ok_or_else(|| fail(format!("bad instanceLimits for {id}")))?;
            instance_limits.insert(id.clone(), n as u32);
        }
    }
    let mut account = HashMap::new();
    let mut usage_id = HashMap::new();
    let mut meta = Vec::new();
    for (name, m) in &accounts {
        let bad = || {
            fail(format!(
                "bad account {}",
                serde_json::to_string(name).unwrap_or_default()
            ))
        };
        let instances = m
            .get("instances")
            .and_then(Value::as_array)
            .filter(|a| !a.is_empty())
            .ok_or_else(bad)?;
        let usage_provider = m
            .get("usageProvider")
            .and_then(Value::as_str)
            .ok_or_else(bad)?
            .to_string();
        let prefix = match m.get("modelPrefix") {
            None => None,
            Some(Value::String(s)) => Some(s.clone()),
            Some(_) => {
                return Err(fail(format!(
                    "bad account {}",
                    serde_json::to_string(name).unwrap_or_default()
                )));
            }
        };
        let mut am = AccountMeta {
            usage_provider,
            instances: vec![],
            usage_id: None,
            model_prefix: prefix.clone(),
        };
        if let Some(prefix) = prefix {
            let _ = prefix;
            for inst in instances {
                let inst = inst.as_str().ok_or_else(bad)?;
                if !account.contains_key(inst) {
                    return Err(fail(format!(
                        "prefix account {} lists unmapped instance {}",
                        serde_json::to_string(name).unwrap_or_default(),
                        serde_json::to_string(inst).unwrap_or_default()
                    )));
                }
            }
            am.instances = instances
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
        } else {
            for inst in instances {
                let inst = inst.as_str().ok_or_else(bad)?;
                if account.contains_key(inst) {
                    return Err(fail(format!(
                        "bad or duplicate instance {}",
                        serde_json::to_string(inst).unwrap_or_default()
                    )));
                }
                account.insert(inst.to_string(), name.clone());
                if let Some(uid) = m.get("usageId") {
                    let uid = uid.as_str().ok_or_else(bad)?;
                    usage_id.insert(inst.to_string(), uid.to_string());
                    am.usage_id = Some(uid.to_string());
                }
                am.instances.push(inst.to_string());
            }
        }
        meta.push((name.clone(), am));
    }
    Ok(RouterConfig {
        file: source.to_string(),
        version: data.get("version").cloned().unwrap_or(Value::from(1)),
        pool,
        instance_limits,
        usage_state_path: data
            .get("usageStatePath")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        decision_model: data
            .get("decisionModel")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        decision_provider: data
            .get("decisionProvider")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        workflow_eval: parse_workflow_eval(&data),
        user_rules: UserRules::from_json(data.get("userRules"))
            .map_err(|e| fail(format!("bad userRules: {e}")))?,
        account,
        usage_id,
        meta,
    })
}

pub fn account_of(cfg: &RouterConfig, instance: &str, model: &str) -> Option<String> {
    for (name, m) in &cfg.meta {
        if let Some(prefix) = &m.model_prefix
            && m.instances.iter().any(|i| i == instance)
            && model.starts_with(prefix)
        {
            return Some(name.clone());
        }
    }
    cfg.account.get(instance).cloned()
}

#[derive(Clone, Debug)]
pub struct UsageAccount {
    pub id: Option<String>,
    pub provider: String,
}

pub fn owns_usage(cfg: &RouterConfig, account: &UsageAccount, instance: &str, model: &str) -> bool {
    if let Some(uid) = cfg.usage_id.get(instance)
        && let Some(id) = &account.id
    {
        return id == uid;
    }
    account.provider == account_of(cfg, instance, model).unwrap_or_default()
}

pub fn enabled_favorites(favorites: Vec<Value>, settings: &Value) -> Vec<Value> {
    favorites
        .into_iter()
        .filter(|f| {
            let p = f
                .get("provider")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let inst = settings
                .get("providerInstances")
                .and_then(|v| v.get(p))
                .and_then(|v| v.get("enabled"))
                .and_then(Value::as_bool);
            let legacy = settings
                .get("providers")
                .and_then(|v| v.get(p))
                .and_then(|v| v.get("enabled"))
                .and_then(Value::as_bool);
            inst.or(legacy) != Some(false)
        })
        .collect()
}

#[derive(Clone, Debug)]
pub struct QuotaWindow {
    pub key: String,
    pub label: Option<String>,
    pub scope: Option<String>,
    pub remaining_percent: f64,
    pub duration_mins: f64,
    pub resets_at: Option<f64>,
    pub cost_factor: Option<f64>,
    pub pending: f64,
}

pub fn quota_windows(provider: &str, model: &str, windows: &[QuotaWindow]) -> Vec<QuotaWindow> {
    windows
        .iter()
        .filter(|w| {
            if provider == "antigravity" {
                let key = w.key.to_lowercase();
                if Regex::new(r"^gemini[-_].*argon").unwrap().is_match(&key) {
                    return Regex::new(r"(?i)argon").unwrap().is_match(model);
                }
                if Regex::new(r"^gemini[-_]").unwrap().is_match(&key) {
                    return Regex::new(r"(?i)gemini").unwrap().is_match(model)
                        && !Regex::new(r"(?i)argon").unwrap().is_match(model);
                }
                if Regex::new(r"^(3p|third[-_]party)[-_]")
                    .unwrap()
                    .is_match(&key)
                {
                    return !Regex::new(r"(?i)gemini").unwrap().is_match(model);
                }
            }
            match &w.scope {
                None => true,
                Some(s) => model.to_lowercase().contains(&s.to_lowercase()),
            }
        })
        .cloned()
        .collect()
}

#[derive(Clone, Debug)]
pub struct AgentCost {
    pub provider: String,
    pub model: Option<String>,
    pub weekly: [f64; 4],
    pub burn: f64,
}

#[derive(Clone, Debug)]
pub struct ReservedQuota {
    pub active_agents: usize,
    pub windows: Vec<QuotaWindow>,
    pub balances: Vec<f64>,
    pub pending_dollars: f64,
}

const USD_COST: [f64; 4] = [0.05, 0.3, 1.0, 3.0];

fn window_factor(w: &QuotaWindow) -> f64 {
    if w.duration_mins >= 10080.0 {
        1.0
    } else {
        w.cost_factor.unwrap_or(5.0)
    }
}

pub fn reserve_active_quota(
    cfg: &RouterConfig,
    account: &UsageSnapshot,
    agents: &[ActiveAgent],
    costs_of: &CostsOf<'_>,
) -> ReservedQuota {
    let active: Vec<AgentCost> = agents
        .iter()
        .filter(|a| {
            owns_usage(
                cfg,
                &UsageAccount {
                    id: account.id.clone(),
                    provider: account.provider.clone(),
                },
                &a.provider,
                a.model.as_deref().unwrap_or(""),
            )
        })
        .map(|a| {
            let (weekly, burn) = costs_of(&a.provider, a.model.as_deref());
            AgentCost {
                provider: a.provider.clone(),
                model: a.model.clone(),
                weekly,
                burn,
            }
        })
        .collect();
    let windows = account
        .windows
        .iter()
        .map(|w| {
            let pending = active
                .iter()
                .filter(|a| {
                    a.model.is_none()
                        || !quota_windows(
                            &a.provider,
                            a.model.as_deref().unwrap_or(""),
                            std::slice::from_ref(w),
                        )
                        .is_empty()
                })
                .map(|a| (0.1_f64).max(a.weekly[1] * window_factor(w)))
                .sum::<f64>();
            let mut w = w.clone();
            w.pending = pending;
            w.remaining_percent = (w.remaining_percent - pending).max(0.0);
            w
        })
        .collect();
    let pending_dollars: f64 = active.iter().map(|a| USD_COST[1] * a.burn).sum();
    let balances: Vec<f64> = account
        .balances
        .iter()
        .map(|b| (*b - pending_dollars).max(0.0))
        .collect();
    ReservedQuota {
        active_agents: active.len(),
        windows,
        balances,
        pending_dollars,
    }
}

#[derive(Clone, Debug)]
pub struct UsageSnapshot {
    pub id: Option<String>,
    pub provider: String,
    pub windows: Vec<QuotaWindow>,
    pub balances: Vec<f64>,
}

#[derive(Clone, Debug)]
pub struct ActiveAgent {
    pub id: String,
    pub provider: String,
    pub model: Option<String>,
}

fn time_left(w: &QuotaWindow, now: f64) -> Option<f64> {
    w.resets_at.and_then(|r| {
        if r.is_finite() && r > now && w.duration_mins > 0.0 {
            Some(((r - now) / (w.duration_mins * 60.0)).min(1.0))
        } else {
            None
        }
    })
}

fn headroom(w: &QuotaWindow, reserve_percent: f64, now: f64) -> f64 {
    (w.remaining_percent
        - if w.duration_mins >= 10080.0 {
            reserve_percent.min(time_left(w, now).unwrap_or(1.0) * 100.0)
        } else {
            0.0
        })
    .max(0.0)
}

pub fn budget_allowed(
    windows: &[QuotaWindow],
    balances: &[f64],
    weekly: &[f64; 4],
    burn: f64,
    reserve_percent: f64,
    now: f64,
) -> [bool; 4] {
    [0, 1, 2, 3].map(|size| {
        windows
            .iter()
            .all(|w| headroom(w, reserve_percent, now) > weekly[size] * window_factor(w))
            && balances.iter().all(|b| *b > USD_COST[size] * burn)
    })
}

// Full cost with the caller's reserve percent (headroom for the denominator).
pub fn budget_cost_full(
    windows: &[QuotaWindow],
    balances: &[f64],
    weekly: &[f64; 4],
    burn: f64,
    reserve_percent: f64,
    now: f64,
) -> Option<[f64; 4]> {
    if windows.is_empty() && balances.is_empty() {
        return None;
    }
    Some([0, 1, 2, 3].map(|size| {
        let mut m = f64::NEG_INFINITY;
        for w in windows {
            m = m.max(
                (0.1_f64).max(weekly[size] * window_factor(w))
                    / (0.001_f64).max(headroom(w, reserve_percent, now)),
            );
        }
        for b in balances {
            m = m.max(USD_COST[size] * burn / (0.001_f64).max(*b));
        }
        m
    }))
}

#[derive(Clone, Debug)]
pub struct QuotaWindowEstimate {
    pub key: String,
    pub label: Option<String>,
    pub remaining_percent: f64,
    pub resets_in_hours: Option<f64>,
}

#[derive(Clone, Debug)]
pub struct QuotaProvenance {
    /// "measured" (Codex session logs), "estimate" (plan table × burn), or
    /// "unknown" (no usable quota data). Informational only: never used for
    /// ranking, gating or validation.
    pub basis: &'static str,
    /// Human provenance: the estimate source string, or the unknown reason.
    pub detail: String,
    /// Measured session count; set only when basis is "measured".
    pub sample_sessions: Option<usize>,
    /// Rounded minutes since the quota snapshot; None when not reported.
    pub stale_minutes: Option<i64>,
    /// True when live agent metadata was unavailable (reserve may be low).
    pub load_unknown: bool,
    /// Applicable post-reserve windows (percent units); empty when none apply.
    pub windows: Vec<QuotaWindowEstimate>,
    /// Applicable post-reserve balances in USD.
    pub balances: Vec<f64>,
}

#[derive(Clone, Debug)]
pub struct Candidate {
    pub provider: String,
    pub model: String,
    pub profiled: bool,
    pub burn: Option<f64>,
    pub target: Value,
    pub reasoning_error: Option<String>,
    pub no_review: bool,
    pub policy_ban: Option<String>,
    pub fit: f64,
    pub quality: f64,
    pub confidence: f64,
    pub allowed: bool,
    pub task_cost: Option<f64>,
    pub quota_note: String,
    pub active_agents: usize,
    pub quota_provenance: QuotaProvenance,
}

#[derive(Clone, Debug)]
pub struct Ranked {
    pub candidate: Candidate,
    pub account: Option<String>,
    pub shared_quota: bool,
    pub shared_quota_penalty: f64,
    pub paid_credit_penalty: f64,
    pub cheap_model_bonus: f64,
    pub eligible: bool,
    pub score: f64,
}

pub fn rank_candidates(
    cfg: &RouterConfig,
    candidates: Vec<Candidate>,
    difficulty: i64,
    source: Option<&str>,
    weights: (f64, f64, f64),
) -> Vec<Ranked> {
    let (shared_w, paid_w, cheap_w) = weights;
    let account = source.and_then(|s| cfg.account.get(s).cloned());
    let scale = if difficulty > 1 { 0.5 } else { 1.0 };
    let pooled: Vec<Candidate> = candidates
        .iter()
        .map(|c| {
            let same: Vec<&Candidate> = candidates.iter().filter(|s| s.model == c.model).collect();
            let mean = |get: &dyn Fn(&Candidate) -> f64| {
                if same.len() > 1 && same.iter().all(|s| get(s).is_finite()) {
                    same.iter().map(|s| get(s)).sum::<f64>() / same.len() as f64
                } else {
                    get(c)
                }
            };
            let mut c = c.clone();
            c.quality = mean(&|x| x.quality);
            c.fit = mean(&|x| x.fit);
            c.confidence = mean(&|x| x.confidence);
            c
        })
        .collect();
    let mut out: Vec<Ranked> = pooled
        .into_iter()
        .map(|c| {
            let own = account_of(cfg, &c.provider, &c.model);
            let shared_quota = account.is_some() && own == account;
            let shared_quota_penalty = if shared_quota { shared_w * scale } else { 0.0 };
            let paid_credit_penalty = if own.as_deref() == Some("openrouter") {
                paid_w * scale
            } else {
                0.0
            };
            let cheap_model_bonus = cheap_w * (0.0_f64).max(1.0 - c.burn.unwrap_or(1.0)) * scale;
            // Unknown-profile models are eligible on the same terms: the
            // catalog gate already fails closed (a model absent from the
            // saved T3 catalog gets a reasoning error), Jev judges them on
            // the honest "unprofiled; capability unknown" criterion, and
            // quota stays conservative (max burn) via model_costs.
            let eligible = c.allowed
                && c.reasoning_error.is_none()
                && c.policy_ban.is_none()
                && c.fit >= MIN_FIT;
            let score = c.quality - shared_quota_penalty - paid_credit_penalty + cheap_model_bonus;
            Ranked {
                account: own,
                shared_quota,
                shared_quota_penalty,
                paid_credit_penalty,
                cheap_model_bonus,
                eligible,
                score,
                candidate: c,
            }
        })
        .collect();
    out.sort_by(|a, b| {
        (b.eligible as u8)
            .cmp(&(a.eligible as u8))
            .then(
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
            .then(a.candidate.active_agents.cmp(&b.candidate.active_agents))
            .then(
                a.candidate
                    .task_cost
                    .unwrap_or(1.0)
                    .partial_cmp(&b.candidate.task_cost.unwrap_or(1.0))
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
            .then(
                b.candidate
                    .fit
                    .partial_cmp(&a.candidate.fit)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
    });
    out
}

pub fn valid_answer(a: &Value, require_probabilities: bool) -> bool {
    let unit = |x: &Value| x.as_f64().is_some_and(|v| (0.0..=1.0).contains(&v));
    let score = a.get("score").and_then(Value::as_f64);
    let conf = a.get("confidence");
    if score.is_none_or(|s| !(0.0..=3.0).contains(&s)) || conf.is_none_or(|c| !unit(c)) {
        return false;
    }
    if !require_probabilities && a.get("probabilities").is_none() {
        return true;
    }
    let p: Vec<bool> = ["0", "1", "2", "3"]
        .iter()
        .map(|k| {
            a.get("probabilities")
                .and_then(|p| p.get(*k))
                .is_some_and(unit)
        })
        .collect();
    if p.iter().any(|b| !b) {
        return false;
    }
    let sum: f64 = ["0", "1", "2", "3"]
        .iter()
        .map(|k| a["probabilities"][*k].as_f64().unwrap_or(0.0))
        .sum();
    (sum - 1.0).abs() <= 0.02
}

#[derive(Clone, Debug, PartialEq)]
pub struct Validation {
    pub allowed: bool,
    pub reason: String,
}

pub fn validate_target(ranking: &[Ranked], provider: &str, model: &str) -> Validation {
    let r = ranking
        .iter()
        .find(|c| c.candidate.provider == provider && c.candidate.model == model);
    let best = ranking.iter().find(|c| c.eligible);
    let deny = |reason: String| Validation {
        allowed: false,
        reason,
    };
    let Some(r) = r else {
        return deny("not an exact active favorite".into());
    };
    if let Some(ban) = &r.candidate.policy_ban {
        return deny(ban.clone());
    }
    if let Some(e) = &r.candidate.reasoning_error {
        return deny(e.clone());
    }
    if !r.candidate.allowed {
        return deny("not enough current quota for this task size".into());
    }
    // `!(fit >= MIN)` so NaN (unreachable: answers are validated) also denies.
    if r.candidate
        .fit
        .partial_cmp(&MIN_FIT)
        .is_none_or(|o| o == std::cmp::Ordering::Less)
    {
        return deny(format!("fit {:.3} below {MIN_FIT}", r.candidate.fit));
    }
    let best = match best {
        Some(b) => b,
        None => return deny("no adequate model with enough quota".into()),
    };
    if r.candidate.provider == best.candidate.provider && r.candidate.model == best.candidate.model
    {
        return Validation {
            allowed: true,
            reason: "selected winner".into(),
        };
    }
    let gap = best.score - r.score;
    if gap <= TARGET_MARGIN
        && r.candidate.task_cost.unwrap_or(1.0) > best.candidate.task_cost.unwrap_or(1.0) + 1e-9
    {
        return deny("near-equal requested model costs more than the selected winner".into());
    }
    if gap <= TARGET_MARGIN {
        Validation {
            allowed: true,
            reason: format!(
                "within {TARGET_MARGIN} of {}/{} (gap {:.3})",
                best.candidate.provider, best.candidate.model, gap
            ),
        }
    } else {
        deny(format!(
            "score {:.3} below {}/{}; margin is {TARGET_MARGIN}",
            gap, best.candidate.provider, best.candidate.model
        ))
    }
}

#[cfg(test)]
mod generated_config_tests {
    use super::*;

    #[test]
    fn routing_reads_live_pool_even_when_snapshot_is_stale() {
        let dir = std::env::temp_dir().join(format!("router-live-{}", std::process::id()));
        let paths = agentdeck_core::Paths::new(dir.join("config"), dir.join("state"));
        agentdeck_core::Registry::update(&paths, |reg| {
            reg.add(agentdeck_core::AddSpec {
                id: "a".into(),
                harness: agentdeck_core::Harness::Codex,
                display_name: None,
                home: "/fake/a".into(),
                binary: None,
                no_t3: true,
            })?;
            reg.add_pool(agentdeck_core::PoolEntry {
                instance: "a".into(),
                model: "m".into(),
                options: Default::default(),
                enabled: true,
            })?;
            Ok(())
        })
        .unwrap();
        let reg = agentdeck_core::Registry::load(&paths).unwrap();
        agentdeck_core::apply_changes(
            &agentdeck_core::router::plan_router_files(&paths, &reg).unwrap(),
        )
        .unwrap();
        let first = registry_config(&paths).unwrap();
        agentdeck_core::pool::set_pool_enabled(&paths, "a", "m", &Default::default(), false, false)
            .unwrap();
        agentdeck_core::registry::set_max_parallel(&paths, "a", Some(3), false).unwrap();
        let latest = registry_config(&paths).unwrap();
        assert!(first.instance_limits.is_empty());
        assert_eq!(latest.instance_limits["a"], 3);
        assert_eq!(first.pool[0]["enabled"], true);
        assert_eq!(latest.pool[0]["enabled"], false);
        assert_ne!(latest.version, first.version);
        let stale = load_router_json(
            agentdeck_core::router::router_json_path(&paths)
                .to_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(stale.pool[0]["enabled"], true);
        assert!(stale.instance_limits.is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn invalid_instance_limits_fail_closed() {
        for limit in [
            serde_json::json!(0),
            serde_json::json!(-1),
            serde_json::json!(1.5),
            serde_json::json!(4294967296u64),
            Value::Null,
            serde_json::json!("2"),
        ] {
            let cfg =
                serde_json::json!({"pool": [], "accounts": {}, "instanceLimits": {"codex": limit}});
            assert!(parse_router_config(cfg, "test").is_err());
        }
        assert!(
            parse_router_config(
                serde_json::json!({"pool": [], "accounts": {}, "instanceLimits": []}),
                "test"
            )
            .is_err()
        );
    }

    /// The real shape: a shared opencode instance with a prefix account.
    #[test]
    fn loader_accepts_what_the_generator_writes() {
        let reg = agentdeck_core::registry::Registry::from_toml(
            r#"
version = 1
[[instance]]
id = "opencode"
harness = "opencode"
display_name = "opencode"
home = "~/.local/share/opencode"
max_parallel = 4
[[pool]]
instance = "opencode"
model = "opencode-go/m"
[pool.options]
[[usage]]
id = "openrouter-main"
kind = "openrouter"
display = "OpenRouter"
api_key_env = "K"
[[usage]]
id = "opencode-go"
kind = "opencode-go"
display = "Go"
"#,
        )
        .unwrap();
        let dir = std::env::temp_dir().join(format!("router-gen-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("router.json");
        let value = agentdeck_core::router::router_json_value(&reg).unwrap();
        std::fs::write(&file, value.to_string()).unwrap();
        let config = load_router_json(file.to_str().unwrap()).unwrap();
        assert_eq!(config.meta.len(), 2);
        assert_eq!(config.instance_limits["opencode"], 4);
        // Generated defaults survive the round trip with their source.
        assert_eq!(
            config.user_rules,
            agentdeck_core::settings::UserRules::default()
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn workflow_eval_parses_with_safe_defaults_and_rejects_bad_limits() {
        let base = serde_json::json!({"pool": [], "accounts": {}});
        let cfg = |we: serde_json::Value| {
            let mut v = base.clone();
            v["workflowEval"] = we;
            parse_router_config(v, "test").unwrap().workflow_eval
        };
        let absent = parse_router_config(base.clone(), "test")
            .unwrap()
            .workflow_eval;
        assert!(!absent.auto && absent.sample == 1 && absent.error.is_none());
        let on = cfg(
            serde_json::json!({"auto": true, "sample": 8, "maxPerDay": 5,
            "maxUsdPerDay": 0.5, "assumeUsdPerEval": 0.01}),
        );
        assert!(on.auto && on.error.is_none());
        assert_eq!((on.sample, on.max_per_day), (8, 5));
        assert_eq!(
            (on.max_usd_per_day, on.assume_usd_per_eval),
            (Some(0.5), Some(0.01))
        );
        // Invalid configs disable auto with a reason instead of failing load.
        for (we, part) in [
            (serde_json::json!([]), "must be an object"),
            (serde_json::json!({"auto": "yes"}), "auto must be a boolean"),
            (serde_json::json!({"sample": 0}), "sample must be 1-1024"),
            (serde_json::json!({"sample": 1025}), "sample must be 1-1024"),
            (
                serde_json::json!({"maxPerDay": 0}),
                "maxPerDay must be a positive",
            ),
            (
                serde_json::json!({"maxUsdPerDay": 1.0}),
                "needs assumeUsdPerEval",
            ),
            (
                serde_json::json!({"maxUsdPerDay": -1.0}),
                "must be a number",
            ),
        ] {
            let c = cfg(we);
            assert!(!c.auto, "invalid config must not enable auto");
            assert!(
                c.error.as_deref().is_some_and(|e| e.contains(part)),
                "{:?}",
                c.error
            );
        }
    }
}

#[cfg(test)]
mod user_rules_tests {
    use super::*;

    fn ranked_with_ban(ban: Option<&str>) -> Vec<Ranked> {
        let candidate = Candidate {
            provider: "a".into(),
            model: "gemini-argon-x".into(),
            profiled: true,
            burn: Some(1.0),
            target: serde_json::json!({"providerInstanceId": "a", "model": "gemini-argon-x"}),
            reasoning_error: None,
            no_review: true,
            policy_ban: ban.map(str::to_string),
            fit: 0.9,
            quality: 0.8,
            confidence: 0.9,
            allowed: true,
            task_cost: Some(1.0),
            quota_note: String::new(),
            active_agents: 0,
            quota_provenance: QuotaProvenance {
                basis: "unknown",
                detail: String::new(),
                sample_sessions: None,
                stale_minutes: None,
                load_unknown: true,
                windows: vec![],
                balances: vec![],
            },
        };
        vec![Ranked {
            account: None,
            shared_quota: false,
            shared_quota_penalty: 0.0,
            paid_credit_penalty: 0.0,
            cheap_model_bonus: 0.0,
            eligible: ban.is_none(),
            score: 0.8,
            candidate,
        }]
    }

    #[test]
    fn default_matches_embedded_roster_ban() {
        let roster_bans: Vec<String> = families()
            .into_iter()
            .filter(|(_, f)| f.no_review)
            .map(|(pat, _)| pat)
            .collect();
        assert_eq!(
            UserRules::default().no_review_models,
            roster_bans,
            "default user rules must preserve the embedded ban exactly"
        );
        let rules = UserRules::default();
        assert!(rules.is_no_review("gemini-foo-argon"));
        assert!(!rules.is_no_review("gemini-2.5-pro"));
    }

    #[test]
    fn invalid_user_rules_fail_closed() {
        for v in [
            serde_json::json!({"noReviewModels": "x"}),
            serde_json::json!({"denyModels": [42]}),
            serde_json::json!({"noReviewModels": ["(["]}),
            serde_json::json!({"source": "someone"}),
            serde_json::json!("x"),
        ] {
            assert!(UserRules::from_json(Some(&v)).is_err());
            assert!(
                parse_router_config(
                    serde_json::json!({"pool": [], "accounts": {}, "userRules": v}),
                    "test"
                )
                .is_err()
            );
        }
        // Absent stays compatible: today's behavior byte-identical.
        let cfg =
            parse_router_config(serde_json::json!({"pool": [], "accounts": {}}), "test").unwrap();
        assert_eq!(cfg.user_rules, UserRules::default());
    }

    #[test]
    fn deny_wins_in_validation() {
        let denied = ranked_with_ban(Some("denied by user rule"));
        let v = validate_target(&denied, "a", "gemini-argon-x");
        assert!(!v.allowed);
        assert_eq!(v.reason, "denied by user rule");
        let clean = ranked_with_ban(None);
        assert!(validate_target(&clean, "a", "gemini-argon-x").allowed);
    }
}

#[cfg(test)]
mod unknown_models_tests {
    use super::*;

    fn candidate(profiled: bool, fit: f64, allowed: bool) -> Candidate {
        Candidate {
            provider: "a".into(),
            model: "some-new-model-9".into(),
            profiled,
            burn: if profiled { Some(1.0) } else { None },
            target: serde_json::json!({"providerInstanceId": "a", "model": "some-new-model-9"}),
            reasoning_error: None,
            no_review: false,
            policy_ban: None,
            fit,
            quality: 0.8,
            confidence: 0.9,
            allowed,
            task_cost: Some(1.0),
            quota_note: String::new(),
            active_agents: 0,
            quota_provenance: QuotaProvenance {
                basis: "estimate",
                detail: "Pro 5x estimate × burn 12".into(),
                sample_sessions: None,
                stale_minutes: None,
                load_unknown: false,
                windows: vec![],
                balances: vec![],
            },
        }
    }

    fn rank_one(c: Candidate) -> Vec<Ranked> {
        rank_candidates(
            &RouterConfig {
                file: String::new(),
                version: Value::Null,
                pool: vec![],
                instance_limits: std::collections::HashMap::new(),
                usage_state_path: None,
                decision_model: None,
                decision_provider: None,
                workflow_eval: WorkflowEvalConfig::default(),
                user_rules: UserRules::default(),
                account: std::collections::HashMap::new(),
                usage_id: std::collections::HashMap::new(),
                meta: vec![],
            },
            vec![c],
            1,
            None,
            (0.05, 0.0, 0.15),
        )
    }

    #[test]
    fn no_profile_is_eligible_when_judged_adequate() {
        let ranked = rank_one(candidate(false, 0.85, true));
        assert!(
            ranked[0].eligible,
            "unprofiled but adequate must be routable"
        );
        assert!(!ranked[0].candidate.profiled);
        assert!(ranked[0].candidate.burn.is_none());
        let v = validate_target(&ranked, "a", "some-new-model-9");
        assert!(v.allowed, "validation must not demand a roster profile");
    }

    #[test]
    fn low_fit_unprofiled_stays_out() {
        let ranked = rank_one(candidate(false, 0.5, true));
        assert!(!ranked[0].eligible);
        let v = validate_target(&ranked, "a", "some-new-model-9");
        assert!(!v.allowed);
        assert!(v.reason.contains("fit"), "{}", v.reason);
    }

    #[test]
    fn quota_denied_unprofiled_stays_out() {
        let ranked = rank_one(candidate(false, 0.9, false));
        assert!(!ranked[0].eligible);
        let v = validate_target(&ranked, "a", "some-new-model-9");
        assert!(!v.allowed);
        assert!(v.reason.contains("quota"), "{}", v.reason);
    }

    #[test]
    fn regex_ban_denies_unprofiled() {
        let mut c = candidate(false, 0.9, true);
        c.policy_ban = Some(NO_REVIEW_BAN.into());
        let ranked = rank_one(c);
        assert!(!ranked[0].eligible);
        let v = validate_target(&ranked, "a", "some-new-model-9");
        assert!(!v.allowed);
        assert_eq!(v.reason, NO_REVIEW_BAN);
    }

    #[test]
    fn reasoning_error_blocks_unknown_catalog_model() {
        let mut c = candidate(false, 0.9, true);
        c.reasoning_error =
            Some("refresh orchestrator_capabilities catalog: no model a/some-new-model-9".into());
        let ranked = rank_one(c);
        assert!(!ranked[0].eligible);
        let v = validate_target(&ranked, "a", "some-new-model-9");
        assert!(!v.allowed);
    }

    #[test]
    fn unknown_target_fails_closed() {
        let ranked = rank_one(candidate(false, 0.9, true));
        let v = validate_target(&ranked, "a", "no-such-model");
        assert!(!v.allowed);
        assert_eq!(v.reason, "not an exact active favorite");
    }
}

/// Ported from the legacy Node `policy.test.mjs` (deleted with the router
/// `.mjs` sources): the numeric edges of window matching, budgeting,
/// active-quota reserve, ranking and answer validation.
#[cfg(test)]
mod legacy_suite_edges {
    use super::*;

    fn w(
        key: &str,
        remaining: f64,
        mins: f64,
        resets: Option<f64>,
        scope: Option<&str>,
    ) -> QuotaWindow {
        QuotaWindow {
            key: key.into(),
            label: None,
            scope: scope.map(str::to_string),
            remaining_percent: remaining,
            duration_mins: mins,
            resets_at: resets,
            cost_factor: None,
            pending: 0.0,
        }
    }

    fn allowed(
        windows: &[QuotaWindow],
        weekly: [f64; 4],
        burn: f64,
        balances: &[f64],
        now: f64,
        size: usize,
    ) -> bool {
        budget_allowed(windows, balances, &weekly, burn, 5.0, now)[size]
    }

    fn cfg(accounts: &[(&str, &str)]) -> RouterConfig {
        RouterConfig {
            file: String::new(),
            version: Value::Null,
            pool: vec![],
            instance_limits: Default::default(),
            usage_state_path: None,
            decision_model: None,
            decision_provider: None,
            workflow_eval: WorkflowEvalConfig::default(),
            user_rules: UserRules::default(),
            account: accounts
                .iter()
                .map(|(i, a)| ((*i).into(), (*a).into()))
                .collect(),
            usage_id: Default::default(),
            meta: vec![],
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn cand(
        provider: &str,
        model: &str,
        fit: f64,
        quality: f64,
        allowed: bool,
        task_cost: f64,
        burn: Option<f64>,
        agents: usize,
    ) -> Candidate {
        Candidate {
            provider: provider.into(),
            model: model.into(),
            profiled: true,
            burn,
            target: serde_json::json!({"providerInstanceId": provider, "model": model}),
            reasoning_error: None,
            no_review: false,
            policy_ban: None,
            fit,
            quality,
            confidence: 0.9,
            allowed,
            task_cost: Some(task_cost),
            quota_note: String::new(),
            active_agents: agents,
            quota_provenance: QuotaProvenance {
                basis: "estimate",
                detail: String::new(),
                sample_sessions: None,
                stale_minutes: None,
                load_unknown: true,
                windows: vec![],
                balances: vec![],
            },
        }
    }

    #[test]
    fn scoped_and_argon_windows_match() {
        let claude = [
            w("weekly_all", 0.0, 10080.0, None, None),
            w("weekly_scoped:fable", 0.0, 10080.0, None, Some("Fable")),
        ];
        assert_eq!(
            quota_windows("claudeAgent", "claude-fable-5-1", &claude).len(),
            2
        );
        assert_eq!(
            quota_windows("claudeAgent", "claude-opus-5-5", &claude)
                .iter()
                .map(|x| x.key.clone())
                .collect::<Vec<_>>(),
            ["weekly_all"]
        );
        let agy = [
            w("3p-weekly", 0.0, 10080.0, None, None),
            w("gemini-weekly", 0.0, 10080.0, None, None),
            w("gemini-argon-weekly", 0.0, 10080.0, None, None),
        ];
        assert_eq!(
            quota_windows("antigravity", "gemini-3.8-flash-high", &agy)
                .iter()
                .map(|x| x.key.clone())
                .collect::<Vec<_>>(),
            ["gemini-weekly"]
        );
        assert_eq!(
            quota_windows("antigravity", "gemini-4-argon-high", &agy)
                .iter()
                .map(|x| x.key.clone())
                .collect::<Vec<_>>(),
            ["gemini-argon-weekly"]
        );
        assert_eq!(
            quota_windows("antigravity", "claude-opus-5-5", &agy)
                .iter()
                .map(|x| x.key.clone())
                .collect::<Vec<_>>(),
            ["3p-weekly"]
        );
    }

    #[test]
    fn weekly_thresholds_and_expired_snapshots() {
        let weekly = [0.5, 2.0, 6.0, 15.0];
        assert!(!allowed(
            &[w("w", 7.0, 10080.0, None, None)],
            weekly,
            1.0,
            &[],
            0.0,
            1
        ));
        assert!(allowed(
            &[w("w", 8.0, 10080.0, None, None)],
            weekly,
            1.0,
            &[],
            0.0,
            1
        ));
        assert!(allowed(
            &[w("w", 5.0, 300.0, None, None)],
            weekly,
            1.0,
            &[],
            0.0,
            0
        ));
        assert!(allowed(&[], weekly, 1.0, &[3.8], 0.0, 3));
        assert!(!allowed(&[], weekly, 3.0, &[3.8], 0.0, 3));
        assert_eq!(budget_cost_full(&[], &[], &weekly, 1.0, 5.0, 0.0), None);
        // Near reset the reserve is released; an expired snapshot implies no budget.
        let now = 1000.0;
        let timed = |remaining: f64, hours: f64| {
            [w("w", remaining, 10080.0, Some(now + hours * 3600.0), None)]
        };
        assert!(!allowed(&timed(5.0, 30.0), weekly, 1.0, &[], now, 1));
        assert!(allowed(&timed(5.0, 1.0), weekly, 1.0, &[], now, 1));
        assert!(!allowed(&timed(1.0, 1.0), weekly, 1.0, &[], now, 1));
        assert!(!allowed(&timed(5.0, 0.0), weekly, 1.0, &[], now, 1));
        assert!(!allowed(&timed(5.0, -1.0), weekly, 1.0, &[], now, 1));
    }

    #[test]
    fn active_quota_reserve_never_mutates_the_live_snapshot() {
        let costs = |_: &str, m: Option<&str>| {
            if m.is_some_and(|m| m.contains("fable")) {
                ([1.5, 6.0, 18.0, 45.0], 3.0)
            } else {
                ([0.5, 2.0, 6.0, 15.0], 1.0)
            }
        };
        let account = UsageSnapshot {
            id: None,
            provider: "claude".into(),
            windows: vec![
                w("weekly_all", 14.0, 10080.0, None, None),
                w("weekly_scoped:fable", 91.0, 10080.0, None, Some("Fable")),
            ],
            balances: vec![],
        };
        let agents = |model: Option<&str>| ActiveAgent {
            id: "a".into(),
            provider: "claudeAgent".into(),
            model: model.map(str::to_string),
        };
        let cfg = cfg(&[
            ("claudeAgent", "claude"),
            ("codex", "codex"),
            ("claudeAgent_openrouter", "openrouter"),
            ("opencode", "openrouter"),
        ]);
        let reserved = reserve_active_quota(
            &cfg,
            &account,
            &[
                agents(Some("claude-opus-5-5")),
                agents(Some("claude-fable-5-1")),
                ActiveAgent {
                    id: "b".into(),
                    provider: "codex".into(),
                    model: Some("gpt-6.1-sol".into()),
                },
            ],
            &costs,
        );
        assert_eq!(reserved.active_agents, 2);
        assert_eq!(reserved.windows[0].remaining_percent, 6.0);
        assert_eq!(reserved.windows[1].remaining_percent, 85.0);
        assert!(!allowed(
            &reserved.windows,
            [0.5, 2.0, 6.0, 15.0],
            1.0,
            &[],
            0.0,
            1
        ));
        assert_eq!(account.windows[0].remaining_percent, 14.0);
        let unknown = reserve_active_quota(
            &cfg,
            &account,
            &[agents(None)],
            &|_: &str, _: Option<&str>| ([1.5, 6.0, 18.0, 45.0], 3.0),
        );
        assert_eq!(unknown.windows[0].remaining_percent, 8.0);
        assert_eq!(unknown.windows[1].remaining_percent, 85.0);
        // Balances fund both agents' size-1 dollar cost.
        let money = reserve_active_quota(
            &cfg,
            &UsageSnapshot {
                id: None,
                provider: "openrouter".into(),
                windows: vec![],
                balances: vec![1.0],
            },
            &[
                ActiveAgent {
                    id: "o".into(),
                    provider: "claudeAgent_openrouter".into(),
                    model: Some("muse-spark".into()),
                },
                ActiveAgent {
                    id: "c".into(),
                    provider: "opencode".into(),
                    model: Some("muse-spark".into()),
                },
            ],
            &costs,
        );
        assert_eq!(money.active_agents, 2);
        assert!((money.balances[0] - 0.4).abs() < 1e-9);
    }

    #[test]
    fn shared_quota_ranking_spreads_to_the_idler() {
        let weights = (0.05, 0.0, 0.15);
        let cfg = cfg(&[
            ("claudeAgent", "claude"),
            ("codex", "codex"),
            ("codex_second", "codex"),
            ("antigravity", "antigravity"),
            ("claudeAgent_openrouter", "openrouter"),
        ]);
        // A quota-denied favorite cannot take the task even with a quality edge.
        let ranked = rank_candidates(
            &cfg,
            vec![
                cand(
                    "claudeAgent",
                    "claude-opus-5-5",
                    0.95,
                    1.0,
                    false,
                    0.1,
                    Some(1.0),
                    0,
                ),
                cand("codex", "gpt-6.1-sol", 0.95, 0.8, true, 0.1, Some(1.0), 0),
            ],
            3,
            Some("claudeAgent"),
            weights,
        );
        assert_eq!(
            ranked
                .iter()
                .find(|r| r.eligible)
                .unwrap()
                .candidate
                .provider,
            "codex"
        );
        // A small quality edge does not spend the orchestrator's shared quota.
        let ranked = rank_candidates(
            &cfg,
            vec![
                cand(
                    "claudeAgent",
                    "claude-opus-5-5",
                    0.95,
                    0.82,
                    true,
                    0.1,
                    Some(1.0),
                    0,
                ),
                cand("codex", "gpt-6.1-sol", 0.95, 0.8, true, 0.1, Some(1.0), 0),
            ],
            1,
            Some("claudeAgent"),
            weights,
        );
        assert_eq!(
            ranked
                .iter()
                .find(|r| r.eligible)
                .unwrap()
                .candidate
                .provider,
            "codex"
        );
        // One model on two accounts pools one verdict; parallel work goes idle-first.
        let spread = rank_candidates(
            &cfg,
            vec![
                cand("codex", "gpt-6.1-sol", 0.95, 0.86, true, 0.5, Some(1.0), 1),
                cand(
                    "codex_second",
                    "gpt-6.1-sol",
                    0.95,
                    0.84,
                    true,
                    0.6,
                    Some(1.0),
                    0,
                ),
                cand(
                    "antigravity",
                    "gemini-x",
                    0.95,
                    0.849,
                    true,
                    0.5,
                    Some(1.0),
                    0,
                ),
            ],
            3,
            Some("claudeAgent"),
            weights,
        );
        assert_eq!(spread[0].candidate.provider, "codex_second");
        assert_eq!(spread[0].candidate.quality, spread[1].candidate.quality);
        // Cheap-model bonus wins a small gap, never a whole Jev level; halved when hard.
        let cheap = |model: &str, burn: f64, quality: f64| {
            cand(
                "claude_third",
                model,
                0.95,
                quality,
                true,
                0.1,
                Some(burn),
                0,
            )
        };
        let pick = |a: Candidate, b: Candidate, difficulty: i64| {
            rank_candidates(&cfg, vec![a, b], difficulty, Some("codex"), weights)
                .into_iter()
                .find(|r| r.eligible)
                .unwrap()
                .candidate
                .model
        };
        assert_eq!(
            pick(
                cheap("claude-sonnet-5-5", 0.6, 0.75),
                cheap("claude-haiku-5-5", 0.2, 0.72),
                0
            ),
            "claude-haiku-5-5"
        );
        assert_eq!(
            pick(
                cheap("claude-sonnet-5-5", 0.6, 0.75),
                cheap("claude-haiku-5-5", 0.2, 0.72),
                2
            ),
            "claude-sonnet-5-5"
        );
        // Eligibility uses the raw fit, never a rounded one.
        let fit = |f: f64| {
            rank_candidates(
                &cfg,
                vec![cand("codex", "m", f, 0.8, true, 0.1, Some(1.0), 0)],
                1,
                None,
                weights,
            )[0]
            .eligible
        };
        assert!(!fit(MIN_FIT - 0.0004));
    }

    #[test]
    fn answers_must_be_bounded_and_coherent() {
        let answer = |score: f64, probs: Value, confidence: f64| serde_json::json!({"score": score, "confidence": confidence, "probabilities": probs});
        let p = || serde_json::json!({"0": 0.0, "1": 0.1, "2": 0.6, "3": 0.3});
        assert!(valid_answer(&answer(2.2, p(), 0.8), true));
        assert!(!valid_answer(
            &answer(
                9.0,
                serde_json::json!({"0": 0.0, "1": 0.0, "2": 1.0, "3": 1.0}),
                0.8
            ),
            true
        ));
        assert!(!valid_answer(&answer(-0.1, p(), 0.8), true));
        assert!(!valid_answer(&answer(2.0, p(), 1.2), true));
        assert!(!valid_answer(
            &answer(2.0, serde_json::json!({"1": 0.1, "2": 0.6, "3": 0.3}), 0.8),
            true
        ));
        assert!(!valid_answer(
            &answer(
                2.0,
                serde_json::json!({"0": 0.0, "1": 0.1, "2": 0.2, "3": 0.2}),
                0.8
            ),
            true
        ));
        assert!(!valid_answer(
            &answer(
                2.0,
                serde_json::json!({"0": -0.1, "1": 0.2, "2": 0.6, "3": 0.3}),
                0.8
            ),
            true
        ));
    }

    #[test]
    fn target_validation_reports_winner_gap_quota_fit() {
        let cfg = cfg(&[
            ("codex", "codex"),
            ("codex_second", "codex"),
            ("antigravity", "antigravity"),
            ("claudeAgent", "claude"),
            ("opencode", "openrouter"),
        ]);
        let ranked = rank_candidates(
            &cfg,
            vec![
                cand("codex", "sol", 0.95, 0.8, true, 0.1, Some(1.0), 0),
                cand("antigravity", "flash", 0.95, 0.79, true, 0.1, Some(1.0), 0),
                cand("claudeAgent", "opus", 0.95, 0.7, true, 0.1, Some(1.0), 0),
                cand("opencode", "muse", 0.95, 0.85, false, 0.1, Some(1.0), 0),
                cand("codex_second", "sol2", 0.6, 0.9, true, 0.1, Some(1.0), 0),
            ],
            1,
            Some("claudeAgent"),
            (0.05, 0.0, 0.15),
        );
        let v = validate_target(&ranked, "codex", "sol");
        assert!(v.allowed);
        assert_eq!(v.reason, "selected winner");
        assert!(validate_target(&ranked, "antigravity", "flash").allowed);
        assert!(!validate_target(&ranked, "claudeAgent", "opus").allowed);
        assert!(
            validate_target(&ranked, "opencode", "muse")
                .reason
                .contains("quota"),
            "{}",
            validate_target(&ranked, "opencode", "muse").reason
        );
        assert!(
            validate_target(&ranked, "codex_second", "sol2")
                .reason
                .contains("fit"),
            "{}",
            validate_target(&ranked, "codex_second", "sol2").reason
        );
        assert_eq!(
            validate_target(&ranked, "codex", "gpt-other").reason,
            "not an exact active favorite"
        );
    }
}
