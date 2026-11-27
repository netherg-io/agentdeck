//! Port of `route.mjs`: CLI parsing, Jev judging, quota gating and ranking.

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::load::{AgentLoad, read_active_agents, read_instance_failures};
use crate::policy::{
    Candidate, MIN_FIT, ModelCost, NO_REVIEW_BAN, QuotaProvenance, QuotaWindow,
    QuotaWindowEstimate, Ranked, RouterConfig, UsageAccount, UsageSnapshot, budget_allowed,
    budget_cost_full, enabled_favorites, family_of, hint_of_on, is_review_task, load_router_json,
    max_burn, owns_usage, parse_route_markers, quota_windows, rank_candidates,
    reserve_active_quota, router_config, routing_weights, valid_answer, validate_target,
};
use crate::reasoning::{read_live_catalog, reasoning_target, save_catalog};
use crate::{home_dir, js_num, parse_time_ms, round2, round3, runtime_dir, state_home};

const SIZES: [&str; 4] = [
    "a quick answer or a few-line edit",
    "a focused change or review of a few files",
    "a multi-file feature, refactor or review",
    "a long autonomous session over a large codebase",
];
const DIFFICULTY: [&str; 4] = [
    "mechanical work, lookup, summary, rename, formatting or a simple script",
    "ordinary bounded implementation, tests, exploration or review with clear requirements",
    "hard debugging, ambiguous architecture, security or specialist reasoning",
    "exceptionally difficult work requiring the strongest specialist",
];
const REVIEW: [&str; 4] = [
    "builds, fixes or changes something; includes acting on findings of an earlier review or audit",
    "mostly implementation, with some assessment of existing work along the way",
    "mostly assessing existing work (code, design, UI), with some other work",
    "only reviews, audits or evaluates existing work and reports findings without changing it",
];
const CAPABILITY: [&str; 4] = [
    "Cannot perform the requested task or input modality.",
    "Significant capability gaps; unreliable for the required task.",
    "Adequate capabilities for a good result with normal checks.",
    "Specific relevant strengths should produce a substantially better result on THIS task. A merely stronger model with no practical advantage does not qualify.",
];

const USAGE: &str = "usage: agentdeck route [--source <provider instance>] [--validate-target <provider instance> <model>] [--exclude <provider instance>]... [--review] [--effort medium|high|xhigh] \"<task brief>\" | - (brief on stdin) | --check | --catalog (orchestrator_capabilities structuredContent on stdin) | hook --harness <name> | claude-shell [args...] | t3-mcp-call TOOL FILE | workflow CMD ... | outcomes record|list|evaluate ...";

pub struct Env {
    pub home: std::path::PathBuf,
    pub runtime: std::path::PathBuf,
    pub state_home: std::path::PathBuf,
    pub router_json: Option<String>,
}

impl Env {
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            home: home_dir()?,
            runtime: runtime_dir()?,
            state_home: state_home()?,
            router_json: std::env::var("MODEL_ROUTE_ROUTER_JSON").ok(),
        })
    }
}

fn key_of(provider: &str, model: &str) -> String {
    format!("{provider}/{model}")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

// --- settings / usage state ------------------------------------------------

fn read_settings(home: &std::path::Path) -> anyhow::Result<Value> {
    let text = std::fs::read_to_string(home.join(".t3/userdata/settings.json"))?;
    Ok(serde_json::from_str(&text)?)
}

#[derive(Clone, Debug)]
struct UsageAccountFull {
    provider: String,
    id: Option<String>,
    plan: String,
    status: Option<String>,
    error: Option<String>,
    updated_at: f64,
    windows: Vec<QuotaWindow>,
    balances: Vec<f64>,
}

fn parse_window(v: &Value) -> QuotaWindow {
    QuotaWindow {
        key: v
            .get("key")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        label: v.get("label").and_then(Value::as_str).map(str::to_string),
        scope: v.get("scope").and_then(Value::as_str).map(str::to_string),
        remaining_percent: v
            .get("remaining_percent")
            .and_then(Value::as_f64)
            .unwrap_or(f64::NAN),
        duration_mins: v
            .get("duration_mins")
            .and_then(Value::as_f64)
            .unwrap_or(f64::NAN),
        resets_at: v.get("resets_at").and_then(Value::as_f64),
        cost_factor: v.get("costFactor").and_then(Value::as_f64),
        pending: 0.0,
    }
}

fn parse_usage_state(path: &std::path::Path) -> anyhow::Result<Vec<UsageAccountFull>> {
    let text = std::fs::read_to_string(path)?;
    let data: Value = serde_json::from_str(&text)?;
    Ok(data
        .get("accounts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|a| UsageAccountFull {
            provider: a
                .get("provider")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            id: a.get("id").and_then(Value::as_str).map(str::to_string),
            plan: a
                .get("plan")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            status: a.get("status").and_then(Value::as_str).map(str::to_string),
            error: a.get("error").and_then(Value::as_str).map(str::to_string),
            updated_at: a
                .get("updated_at")
                .and_then(Value::as_f64)
                .unwrap_or(f64::NAN),
            windows: a
                .get("windows")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(parse_window)
                .collect(),
            balances: a
                .get("balances")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(|b| match b.get("total") {
                    Some(Value::String(s)) => s.parse::<f64>().unwrap_or(f64::NAN),
                    Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
                    _ => f64::NAN,
                })
                .collect(),
        })
        .collect())
}

// --- Codex cost calibration (route.mjs codexCosts) --------------------------

const COST_VERSION: u64 = 8;
const RESET_JITTER: f64 = 600.0;
const QUIET_MS: f64 = 120_000.0;
const MIN_RATE_PERCENT: f64 = 10.0;
const MAX_STEP_PERCENT: f64 = 2.0;

#[derive(Default)]
struct Measured {
    costs: std::collections::HashMap<String, (usize, [f64; 4])>,
    short_factor: std::collections::HashMap<String, f64>,
}

fn codex_costs(env: &Env) -> Measured {
    let cache = env
        .runtime
        .join(format!("model-route-codex-costs-v{COST_VERSION}.json"));
    if let Ok(meta) = std::fs::metadata(&cache)
        && let Ok(age) = meta.modified().and_then(|m| {
            std::time::SystemTime::now()
                .duration_since(m)
                .map_err(|_| std::io::Error::other("future"))
        })
        && age < std::time::Duration::from_secs(6 * 3600)
        && let Ok(text) = std::fs::read_to_string(&cache)
        && let Ok(v) = serde_json::from_str::<Value>(&text)
        && v.get("version").and_then(Value::as_u64) == Some(COST_VERSION)
    {
        return measured_from_cache(&v);
    }
    let m = compute_codex_costs(env);
    let _ = std::fs::write(
        &cache,
        serde_json::to_string(&measured_to_json(&m)).unwrap_or_default(),
    );
    m
}

fn measured_from_cache(v: &Value) -> Measured {
    let mut m = Measured::default();
    if let Some(costs) = v.get("costs").and_then(Value::as_object) {
        for (k, c) in costs {
            let n = c.get("n").and_then(Value::as_u64).unwrap_or(0) as usize;
            let pct = c.get("pct").and_then(Value::as_array).map(|a| {
                let mut p = [0.0; 4];
                for (i, x) in a.iter().take(4).enumerate() {
                    p[i] = x.as_f64().unwrap_or(0.0);
                }
                p
            });
            if let Some(pct) = pct {
                m.costs.insert(k.clone(), (n, pct));
            }
        }
    }
    if let Some(sf) = v.get("shortFactor").and_then(Value::as_object) {
        for (k, f) in sf {
            if let Some(f) = f.as_f64() {
                m.short_factor.insert(k.clone(), f);
            }
        }
    }
    m
}

fn measured_to_json(m: &Measured) -> Value {
    let mut costs = serde_json::Map::new();
    let mut keys: Vec<_> = m.costs.keys().collect();
    keys.sort();
    for k in keys {
        let (n, pct) = &m.costs[k];
        costs.insert(k.clone(), serde_json::json!({"n": n, "pct": pct}));
    }
    let mut sf = serde_json::Map::new();
    let mut skeys: Vec<_> = m.short_factor.keys().collect();
    skeys.sort();
    for k in skeys {
        sf.insert(k.clone(), Value::from(m.short_factor[k]));
    }
    serde_json::json!({"version": COST_VERSION, "costs": costs, "shortFactor": sf})
}

struct CodexEvent {
    t: f64,
    file: String,
    model: String,
    plan: String,
    used: f64,
    resets: f64,
    tokens: f64,
}

/// `~/.codex/sessions/YYYY/MM/DD/*.jsonl`, like the Node router's `*/*/*/*.jsonl`.
fn codex_session_files(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut dirs = vec![root.to_path_buf()];
    for _ in 0..3 {
        dirs = dirs
            .iter()
            .flat_map(|d| std::fs::read_dir(d).into_iter().flatten().flatten())
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .map(|e| e.path())
            .collect();
    }
    dirs.iter()
        .flat_map(|d| std::fs::read_dir(d).into_iter().flatten().flatten())
        .filter(|e| {
            e.path().extension().and_then(|x| x.to_str()) == Some("jsonl")
                && e.file_type().is_ok_and(|t| t.is_file())
        })
        .map(|e| e.path())
        .collect()
}

fn compute_codex_costs(env: &Env) -> Measured {
    let root = env.home.join(".codex/sessions");
    let mut events: Vec<CodexEvent> = vec![];
    let mut five_hour: std::collections::HashMap<String, (f64, f64)> =
        std::collections::HashMap::new();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0);
    for file in codex_session_files(&root) {
        if std::fs::metadata(&file).is_ok_and(|m| {
            m.modified().is_ok_and(|t| {
                now_ms
                    - t.duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as f64)
                        .unwrap_or(now_ms)
                    > 21.0 * 86400e3
            })
        }) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        let file_s = file.to_string_lossy().into_owned();
        let mut model: Option<String> = None;
        let mut total = 0.0;
        let mut short: Option<(f64, f64, f64, f64)> = None; // week, hour5, resets, hour5Resets
        for line in text.lines() {
            if !line.contains("\"turn_context\"") && !line.contains("\"token_count\"") {
                continue;
            }
            let Ok(o) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if o.get("type").and_then(Value::as_str) == Some("turn_context") {
                if let Some(m) = o
                    .get("payload")
                    .and_then(|p| p.get("model"))
                    .and_then(Value::as_str)
                {
                    model = Some(m.to_string());
                }
                continue;
            }
            let rl = o.get("payload").and_then(|p| p.get("rate_limits"));
            let week = rl
                .and_then(|r| r.get("primary"))
                .filter(|w| w.get("window_minutes").and_then(Value::as_u64) == Some(10080))
                .or_else(|| {
                    rl.and_then(|r| r.get("secondary"))
                        .filter(|w| w.get("window_minutes").and_then(Value::as_u64) == Some(10080))
                });
            let hour5 = rl
                .and_then(|r| r.get("primary"))
                .filter(|w| w.get("window_minutes").and_then(Value::as_u64) == Some(300))
                .or_else(|| {
                    rl.and_then(|r| r.get("secondary"))
                        .filter(|w| w.get("window_minutes").and_then(Value::as_u64) == Some(300))
                });
            let plan_type = rl
                .and_then(|r| r.get("plan_type"))
                .and_then(Value::as_str)
                .map(str::to_string);
            if let (Some(week), Some(hour5), Some(plan)) = (week, hour5, plan_type) {
                let wu = week
                    .get("used_percent")
                    .and_then(Value::as_f64)
                    .unwrap_or(f64::NAN);
                let hu = hour5
                    .get("used_percent")
                    .and_then(Value::as_f64)
                    .unwrap_or(f64::NAN);
                let wr = week
                    .get("resets_at")
                    .and_then(Value::as_f64)
                    .unwrap_or(f64::NAN);
                let hr = hour5
                    .get("resets_at")
                    .and_then(Value::as_f64)
                    .unwrap_or(f64::NAN);
                if let Some((sw, sh, sr, shr)) = short {
                    let dw = wu - sw;
                    let dh = hu - sh;
                    if sr == wr
                        && (shr - hr).abs() <= RESET_JITTER
                        && dw >= 0.0
                        && dh >= 0.0
                        && dw <= MAX_STEP_PERCENT
                        && dh <= 5.0 * MAX_STEP_PERCENT
                    {
                        let e = five_hour.entry(plan.to_lowercase()).or_insert((0.0, 0.0));
                        e.0 += dw;
                        e.1 += dh;
                    }
                }
                short = Some((wu, hu, wr, hr));
            }
            let t = o
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(parse_time_ms)
                .unwrap_or(f64::NAN);
            let last = o
                .get("payload")
                .and_then(|p| p.get("info"))
                .and_then(|i| i.get("last_token_usage"))
                .and_then(|u| u.get("total_tokens"))
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            let mut tokens = last;
            if let Some(cum) = o
                .get("payload")
                .and_then(|p| p.get("info"))
                .and_then(|i| i.get("total_token_usage"))
                .and_then(|u| u.get("total_tokens"))
                .and_then(Value::as_f64)
            {
                tokens = (cum - total).max(0.0);
                total = cum;
            }
            let is_token_count = o
                .get("payload")
                .and_then(|p| p.get("type"))
                .and_then(Value::as_str)
                == Some("token_count");
            let week2 = rl
                .and_then(|r| r.get("primary"))
                .filter(|w| w.get("window_minutes").and_then(Value::as_u64) == Some(10080))
                .or_else(|| {
                    rl.and_then(|r| r.get("secondary"))
                        .filter(|w| w.get("window_minutes").and_then(Value::as_u64) == Some(10080))
                });
            if is_token_count
                && let (Some(week), Some(model), Some(plan)) = (
                    week2,
                    model.clone(),
                    rl.and_then(|r| r.get("plan_type")).and_then(Value::as_str),
                )
                && let (Some(used), Some(resets)) = (
                    week.get("used_percent").and_then(Value::as_f64),
                    week.get("resets_at").and_then(Value::as_f64),
                )
                && t.is_finite()
                && used.is_finite()
                && resets.is_finite()
            {
                events.push(CodexEvent {
                    t,
                    file: file_s.clone(),
                    model,
                    plan: plan.to_lowercase(),
                    used,
                    resets,
                    tokens,
                });
            }
        }
    }
    events.sort_by(|a, b| a.t.partial_cmp(&b.t).unwrap_or(std::cmp::Ordering::Equal));
    // Group into streams by plan + reset epoch (600s jitter tolerance).
    let mut windows: Vec<(String, f64, Vec<usize>)> = vec![];
    for (i, e) in events.iter().enumerate() {
        let pos = windows
            .iter()
            .position(|w| w.0 == e.plan && (w.1 - e.resets).abs() <= RESET_JITTER);
        match pos {
            Some(p) => {
                windows[p].1 = e.resets;
                windows[p].2.push(i);
            }
            None => windows.push((e.plan.clone(), e.resets, vec![i])),
        }
    }
    let mut rates: std::collections::HashMap<String, (f64, f64)> = std::collections::HashMap::new();
    let mut sessions: std::collections::HashMap<String, (std::collections::HashSet<String>, f64)> =
        std::collections::HashMap::new();
    for (_, _, idx) in &windows {
        let mut foreign_t = f64::NEG_INFINITY;
        for (k, &ei) in idx.iter().enumerate() {
            let e = &events[ei];
            if k > 0 && events[idx[k - 1]].file != e.file {
                foreign_t = events[idx[k - 1]].t;
            }
            let s = sessions
                .entry(e.file.clone())
                .or_insert_with(|| (std::collections::HashSet::new(), 0.0));
            s.0.insert(format!("{}|{}", e.plan, e.model.to_lowercase()));
            s.1 += e.tokens;
            if k > 0 {
                let prev = &events[idx[k - 1]];
                if prev.file == e.file
                    && prev.model == e.model
                    && prev.t - foreign_t > QUIET_MS
                    && e.used >= prev.used
                    && e.used - prev.used <= MAX_STEP_PERCENT
                {
                    let r = rates
                        .entry(format!("{}|{}", e.plan, e.model.to_lowercase()))
                        .or_insert((0.0, 0.0));
                    r.0 += e.used - prev.used;
                    r.1 += e.tokens;
                }
            }
        }
    }
    for (_, _, idx) in &windows {
        let mut models = std::collections::HashSet::new();
        for &ei in idx {
            models.insert(events[ei].model.clone());
        }
        let mut hi = events[idx[0]].used;
        let (mut used, mut tokens) = (0.0, 0.0);
        for &ei in &idx[1..] {
            let e = &events[ei];
            if e.used < hi - MAX_STEP_PERCENT {
                used = f64::NAN;
                break;
            }
            let d = (e.used - hi).max(0.0);
            if d <= MAX_STEP_PERCENT {
                used += d;
                tokens += e.tokens;
            }
            hi = hi.max(e.used);
        }
        if models.len() != 1 || !used.is_finite() {
            continue;
        }
        let key = format!(
            "{}|{}",
            events[idx[0]].plan,
            models.into_iter().next().unwrap_or_default().to_lowercase()
        );
        let r = rates.entry(key).or_insert((0.0, 0.0));
        r.0 += used;
        r.1 += tokens;
    }
    // Overwrite with whole-stream rates where a stream is clean and large enough.
    {
        let mut stream_rates: std::collections::HashMap<String, (f64, f64)> =
            std::collections::HashMap::new();
        for (_, _, idx) in &windows {
            let mut models = std::collections::HashSet::new();
            for &ei in idx {
                models.insert(events[ei].model.clone());
            }
            let mut hi = events[idx[0]].used;
            let (mut used, mut tokens) = (0.0, 0.0);
            for &ei in &idx[1..] {
                let e = &events[ei];
                if e.used < hi - MAX_STEP_PERCENT {
                    used = f64::NAN;
                    break;
                }
                let d = (e.used - hi).max(0.0);
                if d <= MAX_STEP_PERCENT {
                    used += d;
                    tokens += e.tokens;
                }
                hi = hi.max(e.used);
            }
            if models.len() != 1 || !used.is_finite() {
                continue;
            }
            let key = format!(
                "{}|{}",
                events[idx[0]].plan,
                models.into_iter().next().unwrap_or_default().to_lowercase()
            );
            let r = stream_rates.entry(key).or_insert((0.0, 0.0));
            r.0 += used;
            r.1 += tokens;
        }
        for (key, r) in stream_rates {
            if r.0 >= MIN_RATE_PERCENT && r.1 > 0.0 {
                rates.insert(key, r);
            }
        }
    }
    let mut groups: std::collections::HashMap<String, Vec<f64>> = std::collections::HashMap::new();
    for (keys, tokens) in sessions.values() {
        if keys.len() == 1 {
            groups
                .entry(keys.iter().next().cloned().unwrap_or_default())
                .or_default()
                .push(*tokens);
        }
    }
    let mut out = Measured::default();
    for (key, mut v) in groups {
        let Some(r) = rates.get(&key) else { continue };
        if v.len() < 10 || r.0 < MIN_RATE_PERCENT || r.1 == 0.0 {
            continue;
        }
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let pct =
            [0.25, 0.5, 0.75, 0.9].map(|q| round2(v[(v.len() as f64 * q) as usize] * r.0 / r.1));
        out.costs.insert(key, (v.len(), pct));
    }
    for (plan, f) in five_hour {
        if f.0 >= MIN_RATE_PERCENT {
            out.short_factor.insert(plan, round2(f.1 / f.0));
        }
    }
    out
}

// --- quota -----------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct LiveQuotaHealth {
    updated_at: f64,
    min_remaining: f64,
    max_remaining: f64,
}

const QUOTA_RESET_SETTLE_SECS: f64 = 120.0;
const QUOTA_RESET_REPEAT_SETTLE_SECS: f64 = 15.0 * 60.0;
const QUOTA_RESET_FLOOR_PCT: f64 = 10.0;
const QUOTA_RESET_HEALTHY_PCT: f64 = 50.0;

struct QuotaResult {
    burn: Option<f64>,
    weekly: Option<[f64; 4]>,
    basis: Option<String>,
    estimate_basis: &'static str,
    estimate_detail: String,
    sample_sessions: Option<usize>,
    stale_minutes: Option<i64>,
    load_unknown: bool,
    estimate_windows: Vec<QuotaWindowEstimate>,
    estimate_balances: Vec<f64>,
    allowed: [bool; 4],
    cost: Option<[f64; 4]>,
    note: String,
    active_agents: usize,
    live_health: Option<LiveQuotaHealth>,
}

impl QuotaResult {
    /// True when a fresh, live `status == "ok"` quota snapshot recorded after a
    /// quota failure proves the provider suddenly reset the model's quota.
    /// Repeated consecutive failures require a longer settle window so a lagging
    /// provider quota API cannot cause a tight retry loop.
    fn refilled_since(&self, block: crate::load::QuotaBlock, now: f64) -> bool {
        let Some(health) = self.live_health else {
            return false;
        };
        let settle = if block.count >= 2 {
            QUOTA_RESET_REPEAT_SETTLE_SECS
        } else {
            QUOTA_RESET_SETTLE_SECS
        };
        health.updated_at <= now
            && health.updated_at >= block.failed_at + settle
            && health.min_remaining >= QUOTA_RESET_FLOOR_PCT
            && health.max_remaining >= QUOTA_RESET_HEALTHY_PCT
    }
}

fn unknown_quota(err: &str, load_unknown: bool, stale_minutes: Option<i64>) -> QuotaResult {
    QuotaResult {
        burn: None,
        weekly: None,
        basis: None,
        estimate_basis: "unknown",
        estimate_detail: err.to_string(),
        sample_sessions: None,
        stale_minutes,
        load_unknown,
        estimate_windows: vec![],
        estimate_balances: vec![],
        allowed: [true; 4],
        cost: None,
        note: format!("quota unknown ({err})"),
        active_agents: 0,
        live_health: None,
    }
}

/// Rounded minutes since the quota snapshot; None when never reported.
fn stale_minutes(updated_at: f64, now: f64) -> Option<i64> {
    updated_at
        .is_finite()
        .then(|| ((now - updated_at) / 60.0).round() as i64)
}

fn model_costs(
    measured: &Measured,
    acc: &UsageAccountFull,
    provider: &str,
    model: &str,
) -> (ModelCost, String, Option<usize>) {
    let normalized = if ["opus", "sonnet", "haiku", "fable"].contains(&model) {
        format!("claude-{model}")
    } else {
        model.to_string()
    };
    let fam = family_of(&normalized);
    // Two size-2 Sonnet 5.5 high-thinking reviews on separate Google AI Pro accounts
    // (2026-11-20) used 31.9/33.8% weekly and 62.0/67.3% in the third-party 5h window.
    // Round up to 34.5% weekly (1.5 × 23); extrapolate other sizes with the plan curve.
    let antigravity_sonnet = acc.provider == "antigravity"
        && acc.plan.eq_ignore_ascii_case("Google AI Pro")
        && normalized.to_ascii_lowercase().starts_with("claude-sonnet");
    let burn = if antigravity_sonnet {
        23.0
    } else {
        fam.as_ref().map(|f| f.burn).unwrap_or_else(max_burn)
    };
    let plan_key = if provider == "codex" {
        crate::policy::roster()["codexPlanTypes"]
            .get(&acc.plan)
            .and_then(Value::as_str)
            .map(str::to_string)
    } else {
        None
    };
    let m = plan_key
        .as_deref()
        .and_then(|p| measured.costs.get(&format!("{p}|{}", model.to_lowercase())));
    let plans = crate::policy::roster()
        .get("plans")
        .and_then(|p| p.get(&acc.provider))
        .and_then(Value::as_object);
    let plan_cost = plans
        .and_then(|ps| {
            ps.iter()
                .find(|(k, _)| k.to_lowercase() == acc.plan.to_lowercase())
        })
        .and_then(|(_, v)| v.as_array())
        .map(|a| {
            let mut w = [0.0; 4];
            for (i, x) in a.iter().take(4).enumerate() {
                w[i] = x.as_f64().unwrap_or(0.0);
            }
            w
        });
    let star = crate::policy::roster()
        .get("plans")
        .and_then(|p| p.get("*"))
        .and_then(Value::as_array)
        .map(|a| {
            let mut w = [0.0; 4];
            for (i, x) in a.iter().take(4).enumerate() {
                w[i] = x.as_f64().unwrap_or(0.0);
            }
            w
        })
        .unwrap_or([2.0, 8.0, 25.0, 60.0]);
    let (weekly, basis, sample) = match (m, plan_cost) {
        (Some((n, pct)), _) => (
            *pct,
            format!("measured over {n} sessions on {}", acc.plan),
            Some(*n),
        ),
        (None, Some(c)) => (
            c.map(|x| x * burn),
            if antigravity_sonnet {
                format!("{} Sonnet estimate × burn {burn} (3p 5h ×2)", acc.plan)
            } else {
                format!("{} estimate × burn {burn}", acc.plan)
            },
            None,
        ),
        (None, None) => (
            star.map(|x| x * burn),
            format!("unknown plan \"{}\", conservative estimate", acc.plan),
            None,
        ),
    };
    (
        ModelCost {
            weekly,
            burn,
            third_party_short_factor: antigravity_sonnet.then_some(2.0),
        },
        basis,
        sample,
    )
}

fn quota(
    cfg: &RouterConfig,
    measured: &Measured,
    usage: &[UsageAccountFull],
    load: &AgentLoad,
    provider: &str,
    model: &str,
    now: f64,
) -> QuotaResult {
    let found = usage.iter().find(|a| {
        owns_usage(
            cfg,
            &UsageAccount {
                id: a.id.clone(),
                provider: a.provider.clone(),
            },
            provider,
            model,
        )
    });
    let Some(acc) = found else {
        return unknown_quota("no ai-usage account", !load.known, None);
    };
    let mut acc = acc.clone();
    if acc.provider == "antigravity" {
        crate::policy::cap_antigravity_short_windows(&mut acc.windows);
    }
    // Capture raw live window health before synthesizing resets for elapsed periods
    // so only an actual fresh provider reading can clear a prior quota failure.
    let live_health = if acc.status.as_deref() == Some("ok")
        && acc.updated_at.is_finite()
        && acc.updated_at <= now
    {
        let raw_windows = quota_windows(&acc.provider, model, &acc.windows);
        if !raw_windows.is_empty() && raw_windows.iter().all(|w| w.remaining_percent.is_finite()) {
            let min_remaining = raw_windows
                .iter()
                .map(|w| w.remaining_percent)
                .fold(f64::INFINITY, f64::min);
            let max_remaining = raw_windows
                .iter()
                .map(|w| w.remaining_percent)
                .fold(f64::NEG_INFINITY, f64::max);
            Some(LiveQuotaHealth {
                updated_at: acc.updated_at,
                min_remaining,
                max_remaining,
            })
        } else {
            None
        }
    } else {
        None
    };
    // A window whose reset already passed is a stale reading of a finished
    // period (e.g. the poller is in backoff): treat it as refilled.
    for w in acc.windows.iter_mut() {
        if w.resets_at.is_some_and(|r| r.is_finite() && r <= now) {
            w.remaining_percent = 100.0;
            w.resets_at = None;
        }
    }
    let plan_key = if acc.provider == "codex" {
        crate::policy::roster()["codexPlanTypes"]
            .get(&acc.plan)
            .and_then(Value::as_str)
            .map(str::to_string)
    } else {
        None
    };
    if let Some(pk) = plan_key
        && let Some(factor) = measured.short_factor.get(&pk)
    {
        for w in acc.windows.iter_mut() {
            if w.duration_mins == 300.0 {
                w.cost_factor = Some(*factor);
            }
        }
    }
    if acc.windows.is_empty() && acc.balances.is_empty() {
        return unknown_quota(
            &acc.error.clone().unwrap_or_else(|| {
                acc.status
                    .clone()
                    .unwrap_or_else(|| "no ai-usage account".into())
            }),
            !load.known,
            stale_minutes(acc.updated_at, now),
        );
    }
    let (model_cost, basis, sample) = model_costs(measured, &acc, provider, model);
    let (_, _, _, reserve_default) = routing_weights();
    let reserve_percent = if family_of(model).is_some_and(|f| f.spend_to_zero) {
        0.0
    } else {
        reserve_default
    };
    let reserved = reserve_active_quota(
        cfg,
        &UsageSnapshot {
            id: acc.id.clone(),
            provider: acc.provider.clone(),
            windows: acc.windows.clone(),
            balances: acc.balances.clone(),
        },
        &load.agents,
        &|p, m| model_costs(measured, &acc, p, m.unwrap_or("")).0,
    );
    let windows = quota_windows(&acc.provider, model, &reserved.windows);
    let allowed = budget_allowed(
        &windows,
        &reserved.balances,
        &model_cost,
        reserve_percent,
        now,
    );
    let cost = budget_cost_full(
        &windows,
        &reserved.balances,
        &model_cost,
        reserve_percent,
        now,
    );
    let stale_min = ((now - acc.updated_at) / 60.0).round() as i64;
    let mut parts: Vec<String> = windows
        .iter()
        .map(|w| {
            let mut s = format!(
                "{} {}% left",
                w.label.as_deref().unwrap_or("undefined"),
                w.remaining_percent.round() as i64
            );
            if w.pending != 0.0 {
                s += &format!(" after ~{:.1}% active-work reserve", w.pending);
            }
            if let Some(r) = w.resets_at.filter(|r| r.is_finite()) {
                s += &format!("; resets in {:.1}h", (0.0_f64).max(r - now) / 3600.0);
            }
            s
        })
        .collect();
    for b in &reserved.balances {
        let mut s = format!("${b:.2} left");
        if reserved.pending_dollars != 0.0 {
            s += &format!(
                " after ~${:.2} active-work reserve",
                reserved.pending_dollars
            );
        }
        parts.push(s);
    }
    let mut note = parts.join(", ");
    note += &format!(
        " ({} active agents{})",
        reserved.active_agents,
        if load.known { "" } else { "; count unknown" }
    );
    if reserve_percent != 0.0 && windows.iter().any(|w| w.duration_mins >= 10080.0) {
        note += &format!(" (up to {reserve_percent}% weekly reserve)");
    }
    if acc.status.as_deref() != Some("ok") {
        note += &format!(
            " (cached quota; {})",
            acc.error
                .clone()
                .unwrap_or_else(|| acc.status.clone().unwrap_or_default())
        );
    }
    if acc.updated_at > now {
        note += " (quota timestamp in future)";
    }
    if stale_min > 30 {
        note += &format!(" (ai-usage stale {stale_min} min)");
    }
    QuotaResult {
        burn: Some(model_cost.burn),
        weekly: if windows.is_empty() {
            None
        } else {
            Some(model_cost.weekly)
        },
        basis: if windows.is_empty() {
            None
        } else {
            Some(basis.clone())
        },
        estimate_basis: if sample.is_some() {
            "measured"
        } else {
            "estimate"
        },
        estimate_detail: basis,
        sample_sessions: sample,
        stale_minutes: stale_minutes(acc.updated_at, now),
        load_unknown: !load.known,
        estimate_windows: windows
            .iter()
            .map(|w| QuotaWindowEstimate {
                key: w.key.clone(),
                label: w.label.clone(),
                remaining_percent: w.remaining_percent,
                resets_in_hours: w.resets_at.filter(|r| r.is_finite()).map(|r| {
                    // Same display granularity as the `quota` note (0.1h).
                    (0.0_f64.max(r - now) / 360.0).round() / 10.0
                }),
            })
            .collect(),
        estimate_balances: reserved.balances.clone(),
        allowed,
        cost,
        note,
        active_agents: reserved.active_agents,
        live_health,
    }
}

/// Structured quota provenance for one ranking entry. Additive only: the
/// legacy `quota` note, `taskCost` and all ranking/gating stay unchanged.
/// Windows carry percents, balances USD; anything unavailable is null/empty,
/// never zero-filled.
fn quota_estimate_json(c: &Candidate) -> Value {
    let p = &c.quota_provenance;
    serde_json::json!({
        "basis": p.basis,
        "detail": p.detail,
        "sampleSessions": p.sample_sessions.map(|n| Value::from(n as u64)).unwrap_or(Value::Null),
        "staleMinutes": p.stale_minutes.map(Value::from).unwrap_or(Value::Null),
        "loadUnknown": p.load_unknown,
        "windows": p.windows.iter().map(|w| serde_json::json!({
            "key": w.key,
            "label": w.label.clone().map(Value::from).unwrap_or(Value::Null),
            "unit": "percent",
            "remainingPercent": js_num(round2(w.remaining_percent)),
            "resetsInHours": w.resets_in_hours.map(js_num).unwrap_or(Value::Null),
        })).collect::<Vec<_>>(),
        "balancesUsd": p.balances.iter().map(|b| js_num(round2(*b))).collect::<Vec<_>>(),
    })
}

// --- Jev ---------------------------------------------------------------------

/// Reads a key from `env` or `~/.config/model-route/<file>`.
fn decision_key(env: &str, file: &str) -> anyhow::Result<String> {
    let key_file = home_dir()?.join(".config/model-route").join(file);
    std::env::var(env)
        .ok()
        .filter(|k| !k.is_empty())
        .or_else(|| {
            std::fs::read_to_string(&key_file)
                .ok()
                .map(|k| k.trim().to_string())
                .filter(|k| !k.is_empty())
        })
        .ok_or_else(|| {
            JevHttpError {
                trigger: agentdeck_core::judge::FallbackTrigger::None,
                is_language_model: false,
                batch_kind: crate::jev_queue::BatchErrorKind::ChannelWide,
                message: format!(
                    "decision key not found: set {env} or write {}",
                    key_file.display()
                ),
            }
            .into()
        })
}

/// OpenRouter's question map as Vercel's OpenAI-shaped `/v1/decisions` body:
/// an ordered question list whose criteria become score levels 0-3.
fn decisions_body(request: &Value) -> Value {
    let questions: Vec<Value> = request
        .get("questions")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .map(|(name, q)| {
            let levels: Vec<Value> = q
                .get("criteria")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .enumerate()
                .map(|(i, c)| serde_json::json!({"label": i.to_string(), "description": c}))
                .collect();
            serde_json::json!({"type": "score", "name": name, "instructions": q.get("instructions"), "levels": levels})
        })
        .collect();
    serde_json::json!({
        "model": request.get("model"),
        "input": serde_json::json!({"state": request.get("state")}).to_string(),
        "questions": questions,
    })
}

/// Vercel's answer list back to OpenRouter's `{name: {score, confidence, probabilities}}`;
/// refusals are dropped, so the completeness check rejects them.
fn decisions_answers(response: &Value) -> Value {
    let mut out = serde_json::Map::new();
    for a in response
        .get("answers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(name) = a.get("name").and_then(Value::as_str) else {
            continue;
        };
        if a.get("type").and_then(Value::as_str) != Some("score") {
            continue;
        }
        let mut entry = serde_json::Map::new();
        entry.insert(
            "score".into(),
            a.get("score").cloned().unwrap_or(Value::Null),
        );
        if let Some(c) = a.get("confidence") {
            entry.insert("confidence".into(), c.clone());
        }
        if let Some(ps) = a.get("probabilities").and_then(Value::as_array) {
            let probs: serde_json::Map<String, Value> = ps
                .iter()
                .filter_map(|p| {
                    Some((
                        p.get("value")?.as_i64()?.to_string(),
                        p.get("probability")?.clone(),
                    ))
                })
                .collect();
            entry.insert("probabilities".into(), Value::Object(probs));
        }
        out.insert(name.to_string(), Value::Object(entry));
    }
    Value::Object(out)
}

/// The same questions asked as one JSON chat completion, for a Vercel
/// language model (the gateway's decisions endpoint takes decision models only).
fn gateway_body(request: &Value) -> Value {
    let system = "You answer routing questions for an AI agent router. Reply with one JSON object and nothing else. \
For every key of `questions` return {\"score\": n, \"confidence\": c, \"probabilities\": {\"0\": p0, \"1\": p1, \"2\": p2, \"3\": p3}}: \
n is the index (0-3) of the best-matching entry in that question's `criteria`, c in [0,1] is your confidence, \
and the probabilities over the four criteria sum to 1. Follow each question's `instructions`; `state` is the context.";
    let user = serde_json::json!({
        "state": request.get("state"),
        "questions": request.get("questions"),
    });
    serde_json::json!({
        "model": request.get("model"),
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user.to_string()},
        ],
        "response_format": {"type": "json_object"},
        "temperature": 0,
    })
}

/// Answers from a chat completion's message content; tolerates a ```json fence.
fn gateway_answers(response: &Value) -> anyhow::Result<Value> {
    let content = response
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("gateway response has no message content"))?;
    let json = content
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    serde_json::from_str(json).map_err(|e| anyhow::anyhow!("gateway answer is not JSON: {e}"))
}

const OPENROUTER_BASE: &str = "https://openrouter.ai";
const VERCEL_BASE: &str = "https://ai-gateway.vercel.sh";
const JEV_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Debug)]
struct JevHttpError {
    trigger: agentdeck_core::judge::FallbackTrigger,
    is_language_model: bool,
    batch_kind: crate::jev_queue::BatchErrorKind,
    message: String,
}

impl std::fmt::Display for JevHttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for JevHttpError {}

fn classify_jev_error(err: &anyhow::Error) -> agentdeck_core::judge::FallbackTrigger {
    if let Some(http) = err.downcast_ref::<JevHttpError>() {
        return http.trigger;
    }
    if let Some(rl) = err.downcast_ref::<crate::jev_queue::RateLimited>() {
        return agentdeck_core::judge::FallbackTrigger::RateLimited {
            retry_after: rl.retry_after,
        };
    }
    agentdeck_core::judge::FallbackTrigger::None
}

fn classify_jev_batch_error(err: &anyhow::Error) -> crate::jev_queue::BatchErrorKind {
    if let Some(http) = err.downcast_ref::<JevHttpError>() {
        return http.batch_kind;
    }
    if err
        .downcast_ref::<crate::jev_queue::RateLimited>()
        .is_some()
    {
        return crate::jev_queue::BatchErrorKind::ChannelWide;
    }
    crate::jev_queue::BatchErrorKind::RequestSpecific
}

/// Conservatively classify an HTTP 400 response body as a channel-level failure
/// (wrong/unknown model, unsupported API parameter, auth/billing failure) that
/// will fail every request on this endpoint regardless of task content.
/// Never leaks body text into errors or logs.
fn is_channel_wide_400_body(body: &str) -> bool {
    let haystack = match serde_json::from_str::<Value>(body) {
        Ok(v) => {
            let mut parts = Vec::new();
            for ptr in [
                "/error/code",
                "/error/type",
                "/error/message",
                "/error",
                "/code",
                "/message",
            ] {
                if let Some(s) = v.pointer(ptr).and_then(Value::as_str) {
                    parts.push(s);
                }
            }
            parts.join(" ")
        }
        Err(_) => body.to_string(),
    }
    .to_ascii_lowercase();

    // Context-length, payload-size, and moderation errors can succeed once tasks
    // are split one-by-one, so never short-circuit the whole batch on them.
    const REQUEST_SPECIFIC_HINTS: &[&str] = &[
        "context length",
        "context_length",
        "maximum context",
        "max_tokens",
        "too many tokens",
        "token limit",
        "payload too large",
        "content policy",
        "content_policy",
        "moderation",
    ];
    if REQUEST_SPECIFIC_HINTS.iter().any(|h| haystack.contains(h)) {
        return false;
    }

    const CHANNEL_WIDE_400_HINTS: &[&str] = &[
        "model_not_found",
        "not a valid model",
        "model not found",
        "model does not exist",
        "unknown model",
        "invalid model",
        "unsupported model",
        "model is not supported",
        "no endpoints found",
        "not a decision model",
        "unknown_parameter",
        "unknown parameter",
        "unsupported_parameter",
        "unsupported parameter",
        "unrecognized request argument",
        "invalid parameter",
        "additional properties are not allowed",
        "response_format",
        "invalid_api_key",
        "invalid api key",
        "insufficient_quota",
        "insufficient credits",
    ];
    CHANNEL_WIDE_400_HINTS.iter().any(|h| haystack.contains(h))
        || (haystack.contains("model")
            && (haystack.contains("does not exist") || haystack.contains("not found")))
}

#[derive(Clone, Debug)]
struct JevBases {
    openrouter: String,
    vercel: String,
}

impl Default for JevBases {
    fn default() -> Self {
        Self {
            openrouter: OPENROUTER_BASE.to_string(),
            vercel: VERCEL_BASE.to_string(),
        }
    }
}

fn is_loopback_url(url: &str) -> bool {
    url.starts_with("http://127.0.0.1")
        || url.starts_with("http://[::1]")
        || url.starts_with("http://localhost")
}

async fn post_json(
    url: &str,
    key: &str,
    body: String,
    deadline: std::time::Instant,
    check_language_model: bool,
) -> anyhow::Result<Value> {
    let Some(timeout) = agentdeck_core::judge::request_timeout(JEV_REQUEST_TIMEOUT, deadline)
    else {
        return Err(JevHttpError {
            trigger: agentdeck_core::judge::FallbackTrigger::Transient,
            is_language_model: false,
            batch_kind: crate::jev_queue::BatchErrorKind::LeaderTimeout,
            message: "jev_error_timeout".into(),
        }
        .into());
    };
    let mut builder = reqwest::Client::builder();
    if is_loopback_url(url) {
        builder = builder.no_proxy();
    }
    let client = builder
        .build()
        .map_err(|_| anyhow::anyhow!("jev_error_client"))?;
    let res = client
        .post(url)
        .timeout(timeout)
        .header("Authorization", format!("Bearer {key}"))
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|e| JevHttpError {
            trigger: agentdeck_core::judge::FallbackTrigger::Transient,
            is_language_model: false,
            batch_kind: if e.is_timeout() {
                crate::jev_queue::BatchErrorKind::LeaderTimeout
            } else {
                crate::jev_queue::BatchErrorKind::ChannelWide
            },
            message: if e.is_timeout() {
                "jev_error_timeout".into()
            } else {
                "jev_error_network".into()
            },
        })?;
    if !res.status().is_success() {
        let status = res.status().as_u16();
        // Decide 429 on status + Retry-After before reading the body, and never
        // let a 429 body trigger the Vercel language-model chat retry.
        if status == 429 {
            let retry_after = agentdeck_core::judge::parse_retry_after(
                res.headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok()),
            );
            return Err(JevHttpError {
                trigger: agentdeck_core::judge::classify_http_status(429, Some(retry_after)),
                is_language_model: false,
                batch_kind: crate::jev_queue::BatchErrorKind::ChannelWide,
                message: "jev_http_status_429".into(),
            }
            .into());
        }
        // Only inspect non-429 4xx body for the Vercel language-model sentinel or
        // conservative channel-wide 400 classification; never leak body text.
        let body_text = if (400..500).contains(&status) && (check_language_model || status == 400) {
            res.text().await.unwrap_or_default()
        } else {
            String::new()
        };
        let is_language_model = check_language_model
            && (400..500).contains(&status)
            && body_text.contains("is a language model");
        let channel_wide = matches!(status, 401 | 403 | 404 | 405)
            || (500..=599).contains(&status)
            || (status == 400 && !is_language_model && is_channel_wide_400_body(&body_text));
        return Err(JevHttpError {
            trigger: agentdeck_core::judge::classify_http_status(status, None),
            is_language_model,
            batch_kind: if channel_wide {
                crate::jev_queue::BatchErrorKind::ChannelWide
            } else {
                crate::jev_queue::BatchErrorKind::RequestSpecific
            },
            message: format!("jev_http_status_{status}"),
        }
        .into());
    }
    let bytes = res.bytes().await.map_err(|e| JevHttpError {
        trigger: agentdeck_core::judge::FallbackTrigger::Transient,
        is_language_model: false,
        batch_kind: if e.is_timeout() {
            crate::jev_queue::BatchErrorKind::LeaderTimeout
        } else {
            crate::jev_queue::BatchErrorKind::ChannelWide
        },
        message: if e.is_timeout() {
            "jev_error_timeout".into()
        } else {
            "jev_error_network".into()
        },
    })?;
    serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("Jev returned invalid JSON"))
}

async fn jev_fetch_endpoint(
    endpoint: &agentdeck_core::judge::JudgeEndpoint,
    request: &Value,
    request_body: &str,
    key: Option<&str>,
    deadline: std::time::Instant,
    bases: &JevBases,
    language_models: &agentdeck_core::judge::LanguageModelCache,
) -> anyhow::Result<(Value, Option<f64>)> {
    if bases.openrouter == OPENROUTER_BASE
        && bases.vercel == VERCEL_BASE
        && let Ok(mock) = std::env::var("AGENTDECK_ROUTE_MOCK_JEV")
    {
        let text = std::fs::read_to_string(mock)?;
        let v: Value = serde_json::from_str(&text)?;
        let raw_answers = v.get("answers").cloned().unwrap_or(Value::Null);
        let answers = if let (Some(req_qs), Some(ans_map)) = (
            request.get("questions").and_then(Value::as_object),
            raw_answers.as_object(),
        ) && request.pointer("/state/tasks").is_some()
        {
            let mut out = serde_json::Map::new();
            for qk in req_qs.keys() {
                if let Some(val) = ans_map.get(qk) {
                    out.insert(qk.clone(), val.clone());
                } else if let Some((_, base)) = qk.split_once("__")
                    && let Some(val) = ans_map.get(base)
                {
                    out.insert(qk.clone(), val.clone());
                }
            }
            Value::Object(out)
        } else {
            raw_answers
        };
        return Ok((
            answers,
            v.get("usage")
                .and_then(|u| u.get("cost"))
                .and_then(Value::as_f64),
        ));
    }
    // Rewrite only the outbound model when falling back so the caller's primary
    // request (and Jev cache key) stay untouched.
    let rewritten = (request.get("model").and_then(Value::as_str) != Some(endpoint.model.as_str()))
        .then(|| {
            let mut r = request.clone();
            if let Some(o) = r.as_object_mut() {
                o.insert("model".into(), Value::from(endpoint.model.as_str()));
            }
            r
        });
    let eff_request = rewritten.as_ref().unwrap_or(request);

    if endpoint.provider == agentdeck_core::settings::PROVIDER_VERCEL {
        let key = match key {
            Some(k) => k.to_string(),
            None => decision_key("AI_GATEWAY_API_KEY", "vercel.key")?,
        };
        if !language_models.is_language_model(endpoint) {
            let decisions_url = format!("{}/v1/decisions", bases.vercel.trim_end_matches('/'));
            match post_json(
                &decisions_url,
                &key,
                decisions_body(eff_request).to_string(),
                deadline,
                true,
            )
            .await
            {
                Ok(v) => {
                    let cost = v
                        .pointer("/provider_metadata/gateway/cost")
                        .and_then(|c| c.as_f64().or_else(|| c.as_str()?.parse().ok()));
                    return Ok((decisions_answers(&v), cost));
                }
                Err(e)
                    if e.downcast_ref::<JevHttpError>()
                        .is_some_and(|he| he.is_language_model) =>
                {
                    language_models.record_language_model_logged(endpoint);
                }
                Err(e) => return Err(e),
            }
        }
        let chat_url = format!("{}/v1/chat/completions", bases.vercel.trim_end_matches('/'));
        let v = post_json(
            &chat_url,
            &key,
            gateway_body(eff_request).to_string(),
            deadline,
            false,
        )
        .await?;
        // shortcut: the chat completion's cost is not read, so Jev cost reports as unknown.
        return Ok((gateway_answers(&v)?, None));
    }
    let key = match key {
        Some(k) => k.to_string(),
        None => decision_key("JEV_OPENROUTER_API_KEY", "openrouter.key")?,
    };
    let eff_body = match &rewritten {
        Some(r) => stringify_like_node(r),
        None => request_body.to_string(),
    };
    let url = format!(
        "{}/api/alpha/decisions",
        bases.openrouter.trim_end_matches('/')
    );
    let v = post_json(&url, &key, eff_body, deadline, false).await?;
    Ok((
        v.get("answers").cloned().unwrap_or(Value::Null),
        v.get("usage")
            .and_then(|u| u.get("cost"))
            .and_then(Value::as_f64),
    ))
}

fn primary_endpoint(cfg: &RouterConfig) -> anyhow::Result<agentdeck_core::judge::JudgeEndpoint> {
    let provider = cfg
        .decision_provider
        .clone()
        .unwrap_or_else(|| agentdeck_core::settings::PROVIDER_OPENROUTER.to_string());
    if provider == agentdeck_core::settings::PROVIDER_VERCEL && cfg.decision_model.is_none() {
        return Err(anyhow::anyhow!(
            "decision provider vercel needs [router] decision_model"
        ));
    }
    let model = cfg
        .decision_model
        .clone()
        .unwrap_or_else(|| agentdeck_core::settings::DEFAULT_DECISION_MODEL.to_string());
    Ok(agentdeck_core::judge::JudgeEndpoint::new(provider, model))
}

/// One live decision through the configured provider (explicit only).
///
/// Builds a one-question Jev decision over caller-supplied `state` (task +
/// evidence, already size-checked) and returns the raw `answers`, the reported
/// judge cost (`None` when the provider reports none), and the judge identity
/// (`provider` label, `model`). Reuses the routing decision provider, model,
/// shared `[judge_fallback]`, cross-process 429 blocks, and keys.
/// Honours `AGENTDECK_ROUTE_MOCK_JEV` like routing.
pub async fn judge_evaluation(
    state: Value,
    questions: Value,
) -> anyhow::Result<(Value, Option<f64>, String, String)> {
    let env = Env::from_env()?;
    let cfg = load_router_config(&env)?;
    let primary = primary_endpoint(&cfg)?;
    let fallback = cfg.judge_fallback.endpoint();
    let request =
        serde_json::json!({"model": &primary.model, "state": state, "questions": questions});
    let body = stringify_like_node(&request);
    let language_models = agentdeck_core::judge::LanguageModelCache::for_runtime_dir(&env.runtime);
    if std::env::var_os("AGENTDECK_ROUTE_MOCK_JEV").is_some() {
        let (answers, cost) = jev_fetch_endpoint(
            &primary,
            &request,
            &body,
            None,
            std::time::Instant::now() + JEV_REQUEST_TIMEOUT,
            &JevBases::default(),
            &language_models,
        )
        .await?;
        return Ok((answers, cost, primary.provider, primary.model));
    }
    let blocks = agentdeck_core::judge::BlockStore::for_runtime_dir(&env.runtime);
    let bases = JevBases::default();
    let ((answers, cost), channel) = agentdeck_core::judge::dispatch_with_deadline(
        &blocks,
        &primary,
        fallback.as_ref(),
        agentdeck_core::judge::DispatchBudget::new(
            crate::jev_queue::BUDGET,
            crate::jev_queue::FALLBACK_RESERVE,
        ),
        std::time::Instant::now,
        std::time::SystemTime::now,
        tokio::time::sleep,
        |ep, attempt_deadline| {
            let ep = ep.clone();
            let req = request.clone();
            let body = body.clone();
            let bases = bases.clone();
            let language_models = language_models.clone();
            async move {
                jev_fetch_endpoint(
                    &ep,
                    &req,
                    &body,
                    None,
                    attempt_deadline,
                    &bases,
                    &language_models,
                )
                .await
            }
        },
        classify_jev_error,
        |ep, ra, rem| {
            anyhow::anyhow!(
                "{}",
                agentdeck_core::judge::format_rate_limit_exceeded(ep, ra, rem)
            )
        },
    )
    .await?;
    let used = match (channel, fallback) {
        (agentdeck_core::judge::JudgeChannel::Fallback, Some(fb)) => fb,
        _ => primary,
    };
    Ok((answers, cost, used.provider, used.model))
}

/// One live decision on a fixed small task, bypassing the cache and fallback:
/// checks that the specific provider, model and key work while honouring and
/// updating the shared cross-process 429 block store. `key` overrides the
/// stored key. Returns (size score, difficulty score, cost in USD when reported).
pub async fn probe_decision(
    provider: Option<&str>,
    model: &str,
    key: Option<&str>,
) -> anyhow::Result<(f64, f64, Option<f64>)> {
    let runtime_base = agentdeck_core::judge::default_runtime_base()?;
    let blocks = agentdeck_core::judge::BlockStore::for_runtime_dir(&runtime_base);
    let language_models = agentdeck_core::judge::LanguageModelCache::for_runtime_dir(&runtime_base);
    probe_decision_with_bases(
        provider,
        model,
        key,
        &blocks,
        &language_models,
        &JevBases::default(),
    )
    .await
}

async fn probe_decision_with_bases(
    provider: Option<&str>,
    model: &str,
    key: Option<&str>,
    blocks: &agentdeck_core::judge::BlockStore,
    language_models: &agentdeck_core::judge::LanguageModelCache,
    bases: &JevBases,
) -> anyhow::Result<(f64, f64, Option<f64>)> {
    let endpoint = agentdeck_core::judge::JudgeEndpoint::new(
        provider.unwrap_or(agentdeck_core::settings::PROVIDER_OPENROUTER),
        model,
    );
    let request = serde_json::json!({
        "model": model,
        "state": {"task": "Rename one local variable in a 20-line Python script.", "models": {}},
        "questions": {
            "size": {"type": "score", "instructions": "How much work will the actual delegated task take?", "criteria": SIZES},
            "difficulty": {"type": "score", "instructions": "How difficult is the actual task?", "criteria": DIFFICULTY},
        },
    });
    let body = stringify_like_node(&request);
    let deadline = std::time::Instant::now() + JEV_REQUEST_TIMEOUT;
    let step = agentdeck_core::judge::dispatch_once(
        blocks,
        &endpoint,
        None,
        deadline,
        std::time::Duration::ZERO,
        None,
        std::time::Instant::now,
        std::time::SystemTime::now,
        |ep, attempt_deadline| {
            let ep = ep.clone();
            let req = request.clone();
            let body = body.clone();
            let bases = bases.clone();
            let language_models = language_models.clone();
            let key_owned = key.map(str::to_string);
            async move {
                jev_fetch_endpoint(
                    &ep,
                    &req,
                    &body,
                    key_owned.as_deref(),
                    attempt_deadline,
                    &bases,
                    &language_models,
                )
                .await
            }
        },
        classify_jev_error,
        |ep, ra, _rem| {
            anyhow::anyhow!(
                "{}",
                agentdeck_core::judge::format_blocked_until(ep, ra, std::time::SystemTime::now())
            )
        },
    )
    .await;
    let (answers, cost) = match step {
        agentdeck_core::judge::DispatchStep::Done(v, _) => v,
        agentdeck_core::judge::DispatchStep::Wait(rem) => {
            return Err(anyhow::anyhow!(
                "{}",
                agentdeck_core::judge::format_blocked_until(
                    &endpoint,
                    rem,
                    std::time::SystemTime::now(),
                )
            ));
        }
        agentdeck_core::judge::DispatchStep::Fail(e) => return Err(e),
    };
    let score = |k: &str| {
        answers
            .get(k)
            .filter(|a| valid_answer(a, false))
            .and_then(|a| a.get("score"))
            .and_then(Value::as_f64)
            .ok_or_else(|| {
                anyhow::anyhow!("the model answered, but {k} is missing or out of range")
            })
    };
    Ok((score("size")?, score("difficulty")?, cost))
}

// --- CLI ---------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatedTarget {
    pub provider: String,
    pub model: String,
}

/// Total deadline for one `route_batch` call (fits a 60s 429 block when no
/// fallback is configured, or falls back immediately when configured).
pub const BATCH_BUDGET: std::time::Duration = std::time::Duration::from_secs(120);

#[derive(Clone, Debug)]
pub struct BatchOptions {
    pub harness: Option<String>,
    pub source: Option<String>,
    pub budget: std::time::Duration,
}

impl Default for BatchOptions {
    fn default() -> Self {
        Self {
            harness: None,
            source: None,
            budget: BATCH_BUDGET,
        }
    }
}

fn usage_state_path(env: &Env, cfg: &RouterConfig) -> std::path::PathBuf {
    cfg.usage_state_path
        .clone()
        .map(|p| {
            p.strip_prefix("~/")
                .map(|rest| env.home.join(rest))
                .unwrap_or_else(|| std::path::PathBuf::from(p))
        })
        .unwrap_or_else(|| env.home.join(".local/state/ai-usage/state.json"))
}

/// (provider instance, model, capability profile) per enabled favorite.
type Favorites = Vec<(String, String, Option<String>)>;

fn resolve_favorites(
    cfg: &RouterConfig,
    settings: &Value,
    state_home: &std::path::Path,
) -> anyhow::Result<(Favorites, Vec<String>)> {
    let configured: Vec<Value> = enabled_favorites(
        cfg.pool
            .iter()
            .filter(|e| e.get("enabled").and_then(Value::as_bool) != Some(false))
            .map(|e| {
                let mut m = serde_json::Map::new();
                m.insert(
                    "provider".into(),
                    e.get("providerInstanceId").cloned().unwrap_or(Value::Null),
                );
                m.insert(
                    "model".into(),
                    e.get("model").cloned().unwrap_or(Value::Null),
                );
                if let Some(o) = e.get("options") {
                    m.insert("options".into(), o.clone());
                }
                Value::Object(m)
            })
            .collect(),
        settings,
    );
    let opencode_models: Vec<String> = if configured
        .iter()
        .any(|f| f.get("provider").and_then(Value::as_str) == Some("opencode"))
    {
        let catalog = read_live_catalog(state_home)?;
        catalog
            .get("providers")
            .and_then(Value::as_array)
            .and_then(|ps| {
                ps.iter().find(|p| {
                    p.get("providerInstanceId").and_then(Value::as_str) == Some("opencode")
                })
            })
            .and_then(|p| p.get("models"))
            .and_then(Value::as_array)
            .map(|ms| {
                ms.iter()
                    .filter_map(|m| m.get("id").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    } else {
        vec![]
    };
    let favorites: Vec<(String, String, Option<String>)> = configured
        .into_iter()
        .filter(|f| {
            f.get("provider").and_then(Value::as_str) != Some("opencode")
                || opencode_models.contains(
                    &f.get("model")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                )
        })
        .map(|f| {
            let provider = f
                .get("provider")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let model = f
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let profile = family_of(&model).and_then(|x| x.profile);
            (provider, model, profile)
        })
        .collect();
    let unprofiled: Vec<String> = favorites
        .iter()
        .filter(|(_, _, p)| p.is_none())
        .map(|(pr, m, _)| format!("{pr}/{m}"))
        .collect();
    Ok((favorites, unprofiled))
}

fn known_brief_hints() -> Vec<String> {
    let brief_hint = crate::policy::roster_brief_hint();
    let mut v: Vec<String> = crate::policy::roster()["families"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|f| {
            let fh = f
                .get("briefHint")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let parts: Vec<&str> = [brief_hint.as_str(), fh]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect();
            if parts.is_empty() {
                None
            } else {
                Some(parts.join("\n\n"))
            }
        })
        .collect();
    v.push(brief_hint.clone());
    if let Some(a) = crate::policy::roster()["antigravityBriefHint"].as_str() {
        v.push(format!("{brief_hint}\n\n{a}"));
    }
    // Dedupe (first wins), drop empties, longest first.
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<String> = v
        .into_iter()
        .filter(|s| !s.is_empty() && seen.insert(s.clone()))
        .collect();
    out.sort_by_key(|s| std::cmp::Reverse(s.len()));
    out
}

fn normalize_task(
    raw: &str,
    known_hints: &[String],
    exclude: &mut Vec<String>,
) -> (String, crate::policy::RouteMarkers) {
    let mut task = raw.trim().to_string();
    if let Some(h) = known_hints
        .iter()
        .find(|h| task.ends_with(h.as_str()) && task.len() > h.len())
    {
        task = task[..task.len() - h.len()].trim().to_string();
    }
    let markers = parse_route_markers(&task);
    for x in &markers.exclude {
        if !exclude.iter().any(|e| e == x) {
            exclude.push(x.clone());
        }
    }
    task = markers.cleaned_task().trim().to_string();
    (task, markers)
}

fn build_criteria(favorites: &[(String, String, Option<String>)]) -> Vec<(String, String)> {
    favorites
        .iter()
        .map(|(p, m, prof)| {
            (
                key_of(p, m),
                format!(
                    "{m}: {}",
                    prof.as_deref().unwrap_or("unprofiled; capability unknown")
                ),
            )
        })
        .collect()
}

/// Build the Jev request with Node's exact key order.
fn build_jev_request(
    primary_model: &str,
    task: &str,
    criteria: &[(String, String)],
) -> (Value, String) {
    let mut questions = serde_json::Map::new();
    questions.insert("size".into(), serde_json::json!({"type": "score", "instructions": "How much work will the actual delegated task take? Scope is not difficulty. Do not assume a large task from a long prompt, repository name or safeguards.", "criteria": SIZES}));
    questions.insert("difficulty".into(), serde_json::json!({"type": "score", "instructions": "How difficult is the actual task, independently of its volume? Many files, a large repository, long inputs or a request to use Sonnet/Opus do not by themselves make work difficult.", "criteria": DIFFICULTY}));
    questions.insert("review".into(), serde_json::json!({"type": "score", "instructions": "Is the delegated task itself a review or audit of existing work? Mentioning a review, or fixing what a review found, does not make the task a review.", "criteria": REVIEW}));
    for (k, _) in criteria {
        questions.insert(
            k.clone(),
            serde_json::json!({"type": "score",
                "instructions": format!("Using the capability profile in context.models.{k}, rate expected result quality for context.task independently of other models. Distinguish adequate capability from a substantial practical quality advantage for this task. Routine file search, summaries and bounded edits do not require a frontier leader; a missing specialty or short track record alone is not a capability gap. All candidates have file and shell tools. Ignore the native tool's model field and prices."),
                "criteria": CAPABILITY}),
        );
    }
    let mut state = serde_json::Map::new();
    state.insert("task".into(), Value::from(task));
    let mut models = serde_json::Map::new();
    for (k, c) in criteria {
        models.insert(k.clone(), Value::from(c.clone()));
    }
    state.insert("models".into(), Value::Object(models));
    let mut request = serde_json::Map::new();
    request.insert("model".into(), Value::from(primary_model));
    request.insert("state".into(), Value::Object(state));
    request.insert("questions".into(), Value::Object(questions));
    let request_value = Value::Object(request);
    let body_json = stringify_like_node(&request_value);
    (request_value, body_json)
}

fn jev_cache_key(
    source: Option<&str>,
    cfg_version: &Value,
    primary: &agentdeck_core::judge::JudgeEndpoint,
    fallback: Option<&agentdeck_core::judge::JudgeEndpoint>,
    request_value: &Value,
) -> String {
    let eff_fallback = fallback
        .filter(|f| *f != primary)
        .map(|f| (&f.provider, &f.model));
    stringify_like_node(&serde_json::json!([
        source,
        cfg_version,
        (&primary.provider, &primary.model),
        eff_fallback,
        request_value,
    ]))
}

fn answers_complete(a: &Value, criteria: &[(String, String)]) -> bool {
    let size_ok = a.get("size").is_some_and(|s| valid_answer(s, false));
    let diff_ok = a.get("difficulty").is_some_and(|d| valid_answer(d, false));
    size_ok
        && diff_ok
        && criteria
            .iter()
            .all(|(k, _)| a.get(k).is_some_and(|x| valid_answer(x, true)))
}

struct RoutingSnapshot {
    now: f64,
    usage: Vec<UsageAccountFull>,
    load: AgentLoad,
    current_cfg: RouterConfig,
    pending: std::collections::HashMap<String, usize>,
    capacity_error: Option<String>,
    catalog: Value,
    failures: std::collections::HashMap<String, crate::load::InstanceFailures>,
}

fn read_routing_snapshot(
    env: &Env,
    usage_path: &std::path::Path,
    fallback_now: f64,
) -> anyhow::Result<RoutingSnapshot> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(fallback_now);
    let usage = parse_usage_state(usage_path)
        .map_err(|e| anyhow::anyhow!("cannot read usage state {}: {e}", usage_path.display()))?;
    let mut load = read_active_agents(&env.home.join(".t3/userdata/statev2.sqlite"));
    let current_cfg = load_router_config(env)?;
    let mut pending = std::collections::HashMap::new();
    let mut capacity_error = None;
    if !current_cfg.instance_limits.is_empty() {
        match crate::admission::snapshot(&env.home, &env.state_home) {
            Ok((fresh, reservations)) => {
                load = fresh;
                pending = reservations;
            }
            Err(e) => capacity_error = Some(format!("cannot check instance limit: {e}")),
        }
    }
    let catalog = read_live_catalog(&env.state_home)?;
    let failures = read_instance_failures(&env.home.join(".t3/userdata/statev2.sqlite"), now);
    Ok(RoutingSnapshot {
        now,
        usage,
        load,
        current_cfg,
        pending,
        capacity_error,
        catalog,
        failures,
    })
}

#[allow(clippy::too_many_arguments)]
fn build_route_output(
    cfg: &RouterConfig,
    measured: &Measured,
    snapshot: &RoutingSnapshot,
    favorites: &[(String, String, Option<String>)],
    unprofiled: &[String],
    source: Option<&str>,
    source_warning: Option<&str>,
    task: &str,
    requested: Option<&ValidatedTarget>,
    exclude: &[String],
    force_review: bool,
    effort: Option<i64>,
    answers: &Value,
    cost: Option<f64>,
    jev_hit: bool,
    jev_channel: agentdeck_core::judge::JudgeChannel,
) -> Value {
    let jev_cache = if jev_hit { "hit" } else { "miss" };
    let now = snapshot.now;
    let current_cfg = &snapshot.current_cfg;
    let load = &snapshot.load;
    let pending = &snapshot.pending;

    let size = (answers
        .get("size")
        .and_then(|s| s.get("score"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0)
        .round() as i64)
        .clamp(0, 3);
    let difficulty = (answers
        .get("difficulty")
        .and_then(|s| s.get("score"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0)
        .round() as i64)
        .clamp(0, 3);
    // Explicit --review (e.g. delegate_task role=review), else Jev's review
    // judgment, with keyword fallback only when Jev gave no valid answer.
    let is_review = force_review
        || match answers.get("review").filter(|a| valid_answer(a, false)) {
            Some(a) => {
                a.get("score")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0)
                    .round()
                    >= 2.0
            }
            None => is_review_task(task),
        };
    let mut candidates: Vec<Candidate> = vec![];
    for (provider, model, profile) in favorites {
        let a = answers
            .get(key_of(provider, model).as_str())
            .cloned()
            .unwrap_or(Value::Null);
        let (target, reasoning_error) = match reasoning_target(
            &snapshot.catalog,
            provider,
            model,
            effort.unwrap_or(difficulty),
        ) {
            Ok(mut t) => {
                crate::reasoning::with_worker_agent(&snapshot.catalog, &mut t);
                (t, None)
            }
            Err(e) => (
                serde_json::json!({"providerInstanceId": provider, "model": model}),
                Some(e.to_string()),
            ),
        };
        let fam = family_of(model);
        // User rules (defaults preserve the embedded Argon ban): deny wins,
        // then the audit/review ban, then the instance capacity ban.
        let user_rules = &current_cfg.user_rules;
        let no_review = user_rules.is_no_review(model);
        let mut policy_ban = if user_rules.is_denied(model) {
            Some("denied by user rule".to_string())
        } else if no_review && is_review {
            Some(NO_REVIEW_BAN.to_string())
        } else {
            None
        };
        if let Some(limit) = current_cfg.instance_limits.get(provider) {
            policy_ban = snapshot
                .capacity_error
                .clone()
                .or_else(|| {
                    crate::admission::reason(
                        load,
                        pending.get(provider).copied().unwrap_or_default(),
                        provider,
                        *limit,
                    )
                })
                .or(policy_ban);
        }
        // Exclude every candidate on an excluded instance's account (author
        // instance on review or a limit-hit account on workflow retry).
        let account_of = |i: &str| {
            current_cfg
                .account
                .get(i)
                .cloned()
                .unwrap_or_else(|| i.to_string())
        };
        if exclude
            .iter()
            .any(|x| account_of(x) == account_of(provider))
        {
            policy_ban = Some("excluded for this route".to_string());
        }
        let q = quota(cfg, measured, &snapshot.usage, load, provider, model, now);
        let failure = snapshot.failures.get(provider);
        if let Some(block) = failure.and_then(|f| f.quota_block_for(model))
            && !q.refilled_since(block, now)
        {
            policy_ban = policy_ban.or_else(|| {
                Some(format!(
                    "recent quota failure; retry after {} seconds",
                    (block.until - now).ceil() as u64
                ))
            });
        }
        if let Some(until) = failure.and_then(|f| f.cooldown_until) {
            policy_ban = policy_ban.or_else(|| {
                Some(format!(
                    "recent provider errors; retry after {} seconds",
                    (until - now).ceil() as u64
                ))
            });
        }
        let probs = a.get("probabilities");
        // Jev judges every favorite, including ones without a roster family,
        // on the honest "unprofiled; capability unknown" criterion above; a
        // low score still gates them out via MIN_FIT.
        let fit = probs
            .and_then(|p| p.get("2"))
            .and_then(Value::as_f64)
            .unwrap_or(0.0)
            + probs
                .and_then(|p| p.get("3"))
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
        let spend_zero = fam.as_ref().is_some_and(|f| f.spend_to_zero);
        candidates.push(Candidate {
            provider: provider.clone(),
            model: model.clone(),
            profiled: profile.is_some(),
            burn: fam.as_ref().map(|f| q.burn.unwrap_or(f.burn)),
            target,
            reasoning_error,
            no_review,
            policy_ban,
            fit,
            failure_penalty: failure.map_or(0.0, |f| f.penalty),
            quality: a.get("score").and_then(Value::as_f64).unwrap_or(0.0)
                / (CAPABILITY.len() - 1) as f64,
            confidence: a.get("confidence").and_then(Value::as_f64).unwrap_or(0.0),
            allowed: q.allowed[size as usize],
            task_cost: if spend_zero {
                Some(0.0)
            } else {
                q.cost.map(|c| c[size as usize])
            },
            quota_note: q.note,
            active_agents: q.active_agents,
            quota_provenance: QuotaProvenance {
                basis: q.estimate_basis,
                detail: q.estimate_detail.clone(),
                sample_sessions: q.sample_sessions,
                stale_minutes: q.stale_minutes,
                load_unknown: q.load_unknown,
                windows: q.estimate_windows.clone(),
                balances: q.estimate_balances.clone(),
            },
        });
    }
    let (shared_w, paid_w, cheap_w, _) = routing_weights();
    let ranking = rank_candidates(
        cfg,
        candidates,
        difficulty,
        source,
        (shared_w, paid_w, if is_review { 0.0 } else { cheap_w }),
    );
    let best = ranking.iter().find(|r| r.eligible);
    let blocked: Vec<String> = ranking
        .iter()
        .filter(|r| {
            (!r.candidate.allowed || r.candidate.policy_ban.is_some()) && r.candidate.fit >= MIN_FIT
        })
        .map(|r| format!("{}/{}", r.candidate.provider, r.candidate.model))
        .collect();
    let selected_conf = best.map(|b| b.candidate.confidence).unwrap_or(0.0);
    let conf_size = answers
        .get("size")
        .and_then(|s| s.get("confidence"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let conf_diff = answers
        .get("difficulty")
        .and_then(|s| s.get("confidence"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let jev_confidence = conf_size.min(conf_diff).min(selected_conf);

    let round_entry = |r: &Ranked| {
        let c = &r.candidate;
        let mut m = serde_json::Map::new();
        let num = |v: f64| js_num(round3(v));
        m.insert("provider".into(), Value::from(c.provider.clone()));
        m.insert("model".into(), Value::from(c.model.clone()));
        m.insert("profiled".into(), Value::from(c.profiled));
        if let Some(b) = c.burn {
            m.insert("burn".into(), num(b));
        }
        m.insert("target".into(), c.target.clone());
        if let Some(e) = &c.reasoning_error {
            m.insert("reasoningError".into(), Value::from(e.clone()));
        }
        if c.no_review {
            m.insert("noReview".into(), Value::from(true));
        }
        if let Some(b) = &c.policy_ban {
            m.insert("policyBan".into(), Value::from(b.clone()));
        }
        m.insert("fit".into(), num(c.fit));
        m.insert("quality".into(), num(c.quality));
        if c.failure_penalty > 0.0 {
            m.insert("failurePenalty".into(), num(c.failure_penalty));
        }
        m.insert("confidence".into(), num(c.confidence));
        m.insert("allowed".into(), Value::from(c.allowed));
        m.insert(
            "taskCost".into(),
            c.task_cost
                .map(|t| js_num(round3(t)))
                .unwrap_or(Value::Null),
        );
        m.insert("quota".into(), Value::from(c.quota_note.clone()));
        m.insert("quotaEstimate".into(), quota_estimate_json(c));
        m.insert("activeAgents".into(), Value::from(c.active_agents as u64));
        let limit = current_cfg.instance_limits.get(&c.provider);
        // Absent max_parallel means no cap: say so, since null would read as "unknown".
        m.insert("unlimitedParallel".into(), Value::from(limit.is_none()));
        if let Some(limit) = limit {
            m.insert("maxParallel".into(), Value::from(*limit));
            m.insert(
                "instanceActiveAgents".into(),
                Value::from(
                    load.agents
                        .iter()
                        .filter(|a| a.provider == c.provider)
                        .count() as u64,
                ),
            );
            m.insert(
                "pendingLaunches".into(),
                Value::from(pending.get(&c.provider).copied().unwrap_or_default() as u64),
            );
        }
        m.insert(
            "account".into(),
            r.account.clone().map(Value::from).unwrap_or(Value::Null),
        );
        m.insert("sharedQuota".into(), Value::from(r.shared_quota));
        m.insert("sharedQuotaPenalty".into(), num(r.shared_quota_penalty));
        m.insert("paidCreditPenalty".into(), num(r.paid_credit_penalty));
        m.insert("cheapModelBonus".into(), num(r.cheap_model_bonus));
        m.insert("eligible".into(), Value::from(r.eligible));
        m.insert("score".into(), num(r.score));
        Value::Object(m)
    };
    let brief = |r: &Ranked| {
        serde_json::json!({
            "provider": r.candidate.provider,
            "model": r.candidate.model,
            "quality": js_num(round3(r.candidate.quality)),
            "score": js_num(round3(r.score)),
            "sharedQuotaPenalty": js_num(round3(r.shared_quota_penalty)),
            "paidCreditPenalty": js_num(round3(r.paid_credit_penalty)),
            "cheapModelBonus": js_num(round3(r.cheap_model_bonus)),
        })
    };

    let mut out = serde_json::Map::new();
    out.insert(
        "target".into(),
        best.map(|b| b.candidate.target.clone())
            .unwrap_or(Value::Null),
    );
    if let Some(b) = best.and_then(|x| hint_of_on(&x.candidate.provider, &x.candidate.model)) {
        out.insert("briefHint".into(), Value::from(b));
    }
    out.insert(
        "source".into(),
        source.map(Value::from).unwrap_or(Value::Null),
    );
    out.insert(
        "sourceAccount".into(),
        source
            .and_then(|s| cfg.account.get(s))
            .cloned()
            .map(Value::String)
            .unwrap_or(Value::Null),
    );
    if let Some(w) = source_warning {
        out.insert("sourceWarning".into(), Value::from(w));
    }
    out.insert(
        "reason".into(),
        Value::from(if best.is_none() && !current_cfg.instance_limits.is_empty() {
            "no adequate model with enough quota and an available instance slot"
        } else if best.is_none() {
            "no adequate model with enough quota"
        } else {
            "highest estimated quality with enough quota, after shared-quota and paid-credit penalties and cheap-model bonus"
        }),
    );
    out.insert(
        "size".into(),
        Value::from(format!("{size}: {}", SIZES[size as usize])),
    );
    out.insert(
        "difficulty".into(),
        Value::from(format!("{difficulty}: {}", DIFFICULTY[difficulty as usize])),
    );
    out.insert("jevConfidence".into(), js_num(round3(jev_confidence)));
    out.insert(
        "confidence".into(),
        serde_json::json!({
            "size": js_num(round3(conf_size)),
            "difficulty": js_num(round3(conf_diff)),
            "selected": js_num(round3(selected_conf)),
        }),
    );
    if jev_confidence < 0.4 {
        out.insert(
            "confidenceWarning".into(),
            serde_json::json!({
                "message": "Jev confidence below 0.4: compare expected quality and quota penalties of the top two eligible candidates",
                "topTwo": ranking.iter().filter(|r| r.eligible).take(2).map(brief).collect::<Vec<_>>(),
            }),
        );
    }
    if let Some(req) = requested {
        if let Some(h) = hint_of_on(&req.provider, &req.model) {
            out.insert("requestedBriefHint".into(), Value::from(h));
        }
        let req_target = ranking
            .iter()
            .find(|r| r.candidate.provider == req.provider && r.candidate.model == req.model)
            .map(|r| r.candidate.target.clone())
            .unwrap_or_else(
                || serde_json::json!({"providerInstanceId": req.provider, "model": req.model}),
            );
        out.insert("requestedTarget".into(), req_target);
        out.insert(
            "requestedNoReview".into(),
            Value::from(current_cfg.user_rules.is_no_review(&req.model)),
        );
        let validation = if current_cfg.user_rules.is_denied(&req.model) {
            serde_json::json!({"allowed": false, "reason": "denied by user rule"})
        } else if current_cfg.user_rules.is_no_review(&req.model) && is_review {
            serde_json::json!({"allowed": false, "reason": NO_REVIEW_BAN})
        } else {
            let v = validate_target(&ranking, &req.provider, &req.model);
            serde_json::json!({"allowed": v.allowed, "reason": v.reason})
        };
        out.insert("validation".into(), validation);
    }
    out.insert(
        "coefficients".into(),
        crate::policy::roster()
            .get("routing")
            .cloned()
            .unwrap_or(Value::Null),
    );
    // Explicit rules + source for the GUI and for route-output readers.
    out.insert("userRules".into(), current_cfg.user_rules.to_json());
    out.insert(
        "blocked".into(),
        Value::Array(blocked.iter().cloned().map(Value::String).collect()),
    );
    out.insert(
        "ranking".into(),
        Value::from(ranking.iter().map(round_entry).collect::<Vec<_>>()),
    );
    out.insert(
        "unprofiled".into(),
        Value::Array(unprofiled.iter().cloned().map(Value::String).collect()),
    );
    let mut load_json = serde_json::Map::new();
    load_json.insert("known".into(), Value::from(load.known));
    load_json.insert(
        "agents".into(),
        Value::from(
            load.agents
                .iter()
                .map(|a| serde_json::json!({"id": a.id, "provider": a.provider, "model": a.model}))
                .collect::<Vec<_>>(),
        ),
    );
    if let Some(n) = &load.note {
        load_json.insert("note".into(), Value::from(n.clone()));
    }
    out.insert("activeLoad".into(), Value::Object(load_json));
    if let Some(c) = cost {
        out.insert("cost".into(), js_num(c));
    }
    out.insert("jevCache".into(), Value::from(jev_cache));
    out.insert("jevChannel".into(), Value::from(jev_channel.as_str()));
    Value::Object(out)
}

/// Pre-route a wave of planned launches in as few Jev calls as possible (up to
/// `jev_queue::MAX_TASKS` per call), warming the Jev cache so later hook
/// invocations hit the cache. Returns one JSON object per input launch in order:
/// either the exact `agentdeck route` JSON or `{"error": "..."}`.
pub async fn route_batch(
    env: &Env,
    launches: &[Value],
    opts: BatchOptions,
) -> anyhow::Result<Vec<Value>> {
    let own_env = crate::hook::current_environ();
    let ancestors = crate::hook::process_ancestors_full();
    route_batch_with_bases(
        env,
        launches,
        opts,
        &JevBases::default(),
        None,
        &own_env,
        &ancestors,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn route_batch_with_bases(
    env: &Env,
    launches: &[Value],
    opts: BatchOptions,
    bases: &JevBases,
    key_override: Option<&str>,
    own_env: &[String],
    ancestors: &[crate::hook::ProcessAncestor],
) -> anyhow::Result<Vec<Value>> {
    if launches.is_empty() {
        return Ok(Vec::new());
    }
    let settings =
        read_settings(&env.home).map_err(|e| anyhow::anyhow!("cannot read T3 settings: {e}"))?;
    let harness = opts.harness.as_deref().unwrap_or_default();
    let source = match crate::hook::resolve_source(
        &env.home,
        own_env,
        ancestors,
        crate::hook::SourceQuery {
            harness: opts.harness.as_deref(),
            explicit_source: opts.source.as_deref(),
            thread_id: None,
            from_hook: false,
        },
    ) {
        Ok(Some(s)) if !s.is_empty() => Some(s),
        Ok(_) => {
            let msg = "cannot determine routing source unambiguously; pass source or run with T3CODE_THREAD_ID";
            return Ok(vec![serde_json::json!({"error": msg}); launches.len()]);
        }
        Err(e) => {
            let msg = format!("cannot resolve routing source: {e}");
            return Ok(vec![serde_json::json!({"error": msg}); launches.len()]);
        }
    };
    let cfg = load_router_config(env)?;
    let source_warning = match &source {
        Some(s) if !cfg.account.contains_key(s) => Some(format!(
            "unknown source instance \"{s}\": no shared-quota protection"
        )),
        _ => None,
    };
    let usage_path = usage_state_path(env, &cfg);
    let now_before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let (favorites, unprofiled) = resolve_favorites(&cfg, &settings, &env.state_home)?;
    let criteria = build_criteria(&favorites);
    let primary = primary_endpoint(&cfg)?;
    let fallback = cfg.judge_fallback.endpoint();
    let known_hints = known_brief_hints();
    let complete = |a: &Value| answers_complete(a, &criteria);

    enum BatchSlot {
        Error(String),
        Cached {
            task: String,
            requested: Option<ValidatedTarget>,
            exclude: Vec<String>,
            is_review: bool,
            effort: Option<i64>,
            answers: Value,
            channel: agentdeck_core::judge::JudgeChannel,
        },
        Pending {
            task: String,
            requested: Option<ValidatedTarget>,
            exclude: Vec<String>,
            is_review: bool,
            effort: Option<i64>,
            fetch_idx: usize,
            duplicate: bool,
        },
    }

    let mut slots: Vec<BatchSlot> = Vec::with_capacity(launches.len());
    let mut to_fetch: Vec<(String, Value, String)> = Vec::new();
    let mut hash_to_fetch: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();

    for item in launches {
        let Some(obj) = item.as_object() else {
            slots.push(BatchSlot::Error(
                "each launch must be an object with tool and input".into(),
            ));
            continue;
        };
        let Some(tool) = obj
            .get("tool")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            slots.push(BatchSlot::Error("launch requires non-empty tool".into()));
            continue;
        };
        let Some(input) = obj.get("input").filter(|v| v.is_object()) else {
            slots.push(BatchSlot::Error("launch input must be an object".into()));
            continue;
        };
        let effort = match obj.get("effort") {
            None | Some(Value::Null) => None,
            Some(v) => {
                match v
                    .as_str()
                    .and_then(|l| ["medium", "high", "xhigh"].iter().position(|x| *x == l))
                {
                    Some(level) => Some(level as i64),
                    None => {
                        slots.push(BatchSlot::Error(
                            "effort must be medium, high, or xhigh".into(),
                        ));
                        continue;
                    }
                }
            }
        };
        let parsed = match crate::hook::parse_launch(tool, input, harness) {
            Ok(p) => p,
            Err(e) => {
                slots.push(BatchSlot::Error(format!("{e}")));
                continue;
            }
        };
        match parsed.kind {
            crate::hook::LaunchKind::Other => {
                slots.push(BatchSlot::Error(format!(
                    "unsupported launch tool {:?}",
                    parsed.tool_name
                )));
                continue;
            }
            crate::hook::LaunchKind::Workflow => {
                slots.push(BatchSlot::Error(
                    "Workflow cannot be pre-routed; route each child launch separately".into(),
                ));
                continue;
            }
            crate::hook::LaunchKind::Delegate => {
                if parsed.brief.trim().is_empty() {
                    slots.push(BatchSlot::Error(
                        "delegate_task requires a non-empty task".into(),
                    ));
                    continue;
                }
                if parsed.target_malformed {
                    slots.push(BatchSlot::Error(
                        "delegate_task target requires non-empty providerInstanceId and model"
                            .into(),
                    ));
                    continue;
                }
            }
            crate::hook::LaunchKind::Native => {}
        }

        let mut exclude = parsed.markers.exclude.clone();
        let (task, _) = normalize_task(&parsed.brief, &known_hints, &mut exclude);
        if task.is_empty() {
            slots.push(BatchSlot::Error("empty task brief".into()));
            continue;
        }

        let (request_value, body_json) = build_jev_request(&primary.model, &task, &criteria);
        let hash_input = jev_cache_key(
            source.as_deref(),
            &cfg.version,
            &primary,
            fallback.as_ref(),
            &request_value,
        );
        if let Some((answers, channel)) = read_jev_cache(&env.runtime, &hash_input, &complete) {
            slots.push(BatchSlot::Cached {
                task,
                requested: parsed.target,
                exclude,
                is_review: parsed.is_review,
                effort,
                answers,
                channel,
            });
            continue;
        }
        let (fetch_idx, duplicate) = match hash_to_fetch.get(&hash_input).copied() {
            Some(idx) => (idx, true),
            None => {
                let idx = to_fetch.len();
                to_fetch.push((hash_input.clone(), request_value, body_json));
                hash_to_fetch.insert(hash_input, idx);
                (idx, false)
            }
        };
        slots.push(BatchSlot::Pending {
            task,
            requested: parsed.target,
            exclude,
            is_review: parsed.is_review,
            effort,
            fetch_idx,
            duplicate,
        });
    }

    let mut fetched: Vec<anyhow::Result<crate::jev_queue::QueueAnswer>> = Vec::new();
    if !to_fetch.is_empty() {
        let blocks = agentdeck_core::judge::BlockStore::for_runtime_dir(&env.runtime);
        let language_models =
            agentdeck_core::judge::LanguageModelCache::for_runtime_dir(&env.runtime);
        let queue_items: Vec<(Value, String)> = to_fetch
            .iter()
            .map(|(_, req, body)| (req.clone(), body.clone()))
            .collect();
        let bases_owned = bases.clone();
        let key_owned = key_override.map(str::to_string);
        let raw_outcomes = crate::jev_queue::ask_many_with_deadline(
            &env.runtime.join("model-route-jev-queue"),
            &queue_items,
            opts.budget,
            &blocks,
            &primary,
            fallback.as_ref(),
            move |endpoint, request, body, attempt_deadline| {
                let bases = bases_owned.clone();
                let key_owned = key_owned.clone();
                let language_models = language_models.clone();
                async move {
                    jev_fetch_endpoint(
                        &endpoint,
                        &request,
                        &body,
                        key_owned.as_deref(),
                        attempt_deadline,
                        &bases,
                        &language_models,
                    )
                    .await
                }
            },
            classify_jev_error,
            classify_jev_batch_error,
        )
        .await;
        for (idx, outcome) in raw_outcomes.into_iter().enumerate() {
            match outcome {
                Ok(queued) if complete(&queued.answers) => {
                    write_jev_cache(
                        &env.runtime,
                        &to_fetch[idx].0,
                        &queued.answers,
                        queued.channel,
                    );
                    fetched.push(Ok(queued));
                }
                Ok(_) => fetched.push(Err(anyhow::anyhow!(
                    "Jev returned incomplete or out-of-range routing answers"
                ))),
                Err(e) => fetched.push(Err(e)),
            }
        }
    }

    let needs_snapshot = slots
        .iter()
        .any(|s| matches!(s, BatchSlot::Cached { .. } | BatchSlot::Pending { .. }));
    if !needs_snapshot {
        return Ok(slots
            .into_iter()
            .map(|s| match s {
                BatchSlot::Error(msg) => serde_json::json!({"error": msg}),
                _ => unreachable!(),
            })
            .collect());
    }
    let snapshot = read_routing_snapshot(env, &usage_path, now_before)?;
    let measured = codex_costs(env);

    let mut out = Vec::with_capacity(slots.len());
    for slot in slots {
        match slot {
            BatchSlot::Error(msg) => out.push(serde_json::json!({"error": msg})),
            BatchSlot::Cached {
                task,
                requested,
                exclude,
                is_review,
                effort,
                answers,
                channel,
            } => {
                out.push(build_route_output(
                    &cfg,
                    &measured,
                    &snapshot,
                    &favorites,
                    &unprofiled,
                    source.as_deref(),
                    source_warning.as_deref(),
                    &task,
                    requested.as_ref(),
                    &exclude,
                    is_review,
                    effort,
                    &answers,
                    Some(0.0),
                    true,
                    channel,
                ));
            }
            BatchSlot::Pending {
                task,
                requested,
                exclude,
                is_review,
                effort,
                fetch_idx,
                duplicate,
            } => match &fetched[fetch_idx] {
                Ok(queued) => {
                    let (hit, cost) = if duplicate {
                        (true, Some(0.0))
                    } else {
                        (false, queued.cost)
                    };
                    out.push(build_route_output(
                        &cfg,
                        &measured,
                        &snapshot,
                        &favorites,
                        &unprofiled,
                        source.as_deref(),
                        source_warning.as_deref(),
                        &task,
                        requested.as_ref(),
                        &exclude,
                        is_review,
                        effort,
                        &queued.answers,
                        cost,
                        hit,
                        queued.channel,
                    ));
                }
                Err(e) => out.push(serde_json::json!({"error": format!("{e:#}")})),
            },
        }
    }
    Ok(out)
}

pub async fn run(argv: Vec<String>) -> i32 {
    match run_inner(argv).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

async fn run_inner(argv: Vec<String>) -> anyhow::Result<i32> {
    match argv.first().map(String::as_str) {
        Some("hook") => return Ok(crate::hook::run(&argv[1..])),
        Some("claude-shell") => return Ok(crate::shell::run(&argv[1..])),
        Some("t3-mcp-call") => return Ok(crate::mcp_call::run(&argv[1..]).await),
        Some("workflow") => return Ok(crate::workflow::run(&argv[1..]).await),
        Some("outcomes") => return Ok(crate::outcomes::run(&argv[1..]).await),
        _ => {}
    }
    let env = Env::from_env()?;
    if argv.len() == 1 && argv[0] == "--catalog" {
        let mut stdin = String::new();
        use std::io::Read;
        std::io::stdin().read_to_string(&mut stdin)?;
        let data: Value = serde_json::from_str(&stdin).map_err(|_| {
            anyhow::anyhow!(
                "expected orchestrator_capabilities structuredContent with model option descriptors"
            )
        })?;
        match save_catalog(&env.state_home, data) {
            Ok(()) => {
                println!("model option catalog updated");
                return Ok(0);
            }
            Err(e) => {
                eprintln!("{e}");
                return Ok(1);
            }
        }
    }
    let mut args = argv.as_slice();
    let mut source: Option<String> = None;
    let mut requested: Option<ValidatedTarget> = None;
    let mut exclude: Vec<String> = Vec::new();
    let mut force_review = false;
    let mut effort: Option<i64> = None;
    while args.first().is_some_and(|a| a.starts_with("--")) && args[0] != "--check" {
        let flag = args[0].clone();
        args = &args[1..];
        if flag == "--" {
            break;
        }
        if flag == "--source" && !args.is_empty() {
            source = Some(args[0].clone());
            args = &args[1..];
        } else if flag == "--validate-target" && args.len() >= 2 {
            requested = Some(ValidatedTarget {
                provider: args[0].clone(),
                model: args[1].clone(),
            });
            args = &args[2..];
        } else if flag == "--exclude" && !args.is_empty() {
            exclude.push(args[0].clone());
            args = &args[1..];
        } else if flag == "--review" {
            force_review = true;
        } else if flag == "--effort"
            && let Some(level) = args
                .first()
                .and_then(|l| ["medium", "high", "xhigh"].iter().position(|x| x == l))
        {
            // Same scale as `reasoning_target`: difficulty 0 medium, 1 high, 2 xhigh.
            effort = Some(level as i64);
            args = &args[1..];
        } else {
            eprintln!("{USAGE}");
            return Ok(2);
        }
    }
    let settings =
        read_settings(&env.home).map_err(|e| anyhow::anyhow!("cannot read T3 settings: {e}"))?;
    if source.is_none() {
        let own_env = crate::hook::current_environ();
        let ancestors = crate::hook::process_ancestors_full();
        source = crate::hook::resolve_source(
            &env.home,
            &own_env,
            &ancestors,
            crate::hook::SourceQuery::default(),
        )
        .ok()
        .flatten();
    }
    let cfg = match load_router_config(&env) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return Ok(1);
        }
    };
    let source_warning = match &source {
        Some(s) if !cfg.account.contains_key(s) => Some(format!(
            "unknown source instance \"{s}\": no shared-quota protection"
        )),
        _ => None,
    };
    let usage_path = usage_state_path(&env, &cfg);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let usage = parse_usage_state(&usage_path)
        .map_err(|e| anyhow::anyhow!("cannot read usage state {}: {e}", usage_path.display()))?;
    let load = read_active_agents(&env.home.join(".t3/userdata/statev2.sqlite"));
    let measured = codex_costs(&env);

    let (favorites, unprofiled) = resolve_favorites(&cfg, &settings, &env.state_home)?;

    if args.first().is_some_and(|a| a == "--check") {
        println!(
            "source: {}",
            source
                .as_deref()
                .unwrap_or("unknown (use --source to preserve orchestrator quota)")
        );
        // Source is visible so the GUI/operator can tell a default ban from
        // an explicit user override.
        println!(
            "user rules (source: {}): no-review: {}; denied: {}",
            cfg.user_rules.source.as_str(),
            if cfg.user_rules.no_review_models.is_empty() {
                "(none)".to_string()
            } else {
                cfg.user_rules.no_review_models.join(", ")
            },
            if cfg.user_rules.deny_models.is_empty() {
                "(none)".to_string()
            } else {
                cfg.user_rules.deny_models.join(", ")
            }
        );
        if !load.known {
            println!("live T3 agent metadata unavailable");
        }
        for (provider, model, profile) in &favorites {
            let q = quota(&cfg, &measured, &usage, &load, provider, model, now);
            print!(
                "{}{}  {provider}/{model}\n    left: {}",
                if profile.is_some() {
                    "ok"
                } else {
                    "UNPROFILED"
                },
                if cfg.user_rules.is_denied(model) {
                    " DENIED by user rule"
                } else {
                    ""
                },
                q.note
            );
            if let (Some(w), Some(b)) = (q.weekly, q.basis) {
                print!(
                    "\n    weekly % per size 0..3: {} ({b})",
                    w.iter()
                        .map(|c| format_js(round2(*c)))
                        .collect::<Vec<_>>()
                        .join(" / ")
                );
            }
            let allowed: Vec<String> = [0, 1, 2, 3]
                .iter()
                .filter(|s| q.allowed[**s])
                .map(|s| s.to_string())
                .collect();
            println!(
                "\n    allowed sizes: {}",
                if allowed.is_empty() {
                    "none".to_string()
                } else {
                    allowed.join(",")
                }
            );
        }
        // UNPROFILED is an informational flag (conservative max-burn quota,
        // profiled:false in JSON): unknown-profile favorites route on their
        // Jev judgment like any other, so --check reports and exits 0.
        return Ok(0);
    }

    let raw_task = if args.len() == 1 && args[0] == "-" {
        let mut s = String::new();
        use std::io::Read;
        std::io::stdin().read_to_string(&mut s)?;
        s
    } else {
        args.join(" ")
    };
    let known_hints = known_brief_hints();
    let (task, _) = normalize_task(&raw_task, &known_hints, &mut exclude);
    if task.is_empty() {
        eprintln!("{USAGE}");
        return Ok(2);
    }

    let criteria = build_criteria(&favorites);
    let primary = primary_endpoint(&cfg)?;
    let fallback = cfg.judge_fallback.endpoint();
    let (request_value, body_json) = build_jev_request(&primary.model, &task, &criteria);
    let hash_input = jev_cache_key(
        source.as_deref(),
        &cfg.version,
        &primary,
        fallback.as_ref(),
        &request_value,
    );
    let complete = |a: &Value| answers_complete(a, &criteria);
    let blocks = agentdeck_core::judge::BlockStore::for_runtime_dir(&env.runtime);
    let (answers, cost, jev_hit, jev_channel) = fetch_jev_cached(
        &env.runtime,
        &hash_input,
        &request_value,
        &body_json,
        &primary,
        fallback.as_ref(),
        crate::jev_queue::BUDGET,
        &blocks,
        &JevBases::default(),
        None,
        &complete,
    )
    .await?;
    // Re-read quota/load snapshots after Jev before ranking.
    let snapshot = read_routing_snapshot(&env, &usage_path, now)?;
    let out = build_route_output(
        &cfg,
        &measured,
        &snapshot,
        &favorites,
        &unprofiled,
        source.as_deref(),
        source_warning.as_deref(),
        &task,
        requested.as_ref(),
        &exclude,
        force_review,
        effort,
        &answers,
        cost,
        jev_hit,
        jev_channel,
    );
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(0)
}

fn jev_cache_path(runtime_dir: &std::path::Path, hash_input: &str) -> std::path::PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(hash_input.as_bytes());
    runtime_dir
        .join("model-route-jev-cache")
        .join(format!("{:x}.json", hasher.finalize()))
}

fn read_jev_cache(
    runtime_dir: &std::path::Path,
    hash_input: &str,
    complete: &dyn Fn(&Value) -> bool,
) -> Option<(Value, agentdeck_core::judge::JudgeChannel)> {
    let cache_dir = runtime_dir.join("model-route-jev-cache");
    let cache_file = jev_cache_path(runtime_dir, hash_input);
    let fresh = |t: f64| {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as f64)
            .unwrap_or(0.0);
        now_ms - t >= 0.0 && now_ms - t < 10.0 * 60e3
    };
    let text = match agentdeck_core::judge::read_private_entry(&cache_dir, &cache_file) {
        Ok(Some(t)) => t,
        Ok(None) => return None,
        Err(e) => {
            eprintln!(
                "agentdeck: refusing untrusted Jev cache entry {}: {e}",
                cache_file.display()
            );
            return None;
        }
    };
    let entry: Value = serde_json::from_str(&text).ok()?;
    if !entry.get("t").and_then(Value::as_f64).is_some_and(fresh) {
        return None;
    }
    let answers = entry.get("answers").filter(|a| complete(a)).cloned()?;
    let channel = match entry.get("channel").and_then(Value::as_str) {
        Some("fallback") => agentdeck_core::judge::JudgeChannel::Fallback,
        _ => agentdeck_core::judge::JudgeChannel::Primary,
    };
    Some((answers, channel))
}

fn write_jev_cache(
    runtime_dir: &std::path::Path,
    hash_input: &str,
    answers: &Value,
    channel: agentdeck_core::judge::JudgeChannel,
) {
    let cache_dir = runtime_dir.join("model-route-jev-cache");
    let cache_file = jev_cache_path(runtime_dir, hash_input);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0);
    let _ = (|| {
        agentdeck_core::judge::create_private_dir(&cache_dir)?;
        for f in std::fs::read_dir(&cache_dir)? {
            let f = f?;
            let name = f.file_name().to_string_lossy().into_owned();
            if name.len() == 69
                && name.ends_with(".json")
                && name[..64]
                    .chars()
                    .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
            {
                let stale = std::fs::metadata(f.path())
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .map(|t| {
                        std::time::SystemTime::now()
                            .duration_since(t)
                            .map(|d| d.as_millis() as f64)
                            .unwrap_or(f64::NAN)
                    });
                if stale.is_some_and(|age| !(0.0..10.0 * 60e3).contains(&age)) {
                    let _ = std::fs::remove_file(f.path());
                }
            }
        }
        let payload = serde_json::to_string(&serde_json::json!({
            "t": now_ms,
            "answers": answers,
            "channel": channel.as_str(),
        }))?;
        agentdeck_core::judge::write_atomic(&cache_file, &payload)?;
        Ok::<(), anyhow::Error>(())
    })();
}

/// Lookup or fetch a Jev routing judgment through the shared queue, storing
/// answers (including fallback answers) under the primary request's cache key.
#[allow(clippy::too_many_arguments)]
async fn fetch_jev_cached(
    runtime_dir: &std::path::Path,
    hash_input: &str,
    request_value: &Value,
    body_json: &str,
    primary: &agentdeck_core::judge::JudgeEndpoint,
    fallback: Option<&agentdeck_core::judge::JudgeEndpoint>,
    budget: std::time::Duration,
    blocks: &agentdeck_core::judge::BlockStore,
    bases: &JevBases,
    key_override: Option<&str>,
    complete: &dyn Fn(&Value) -> bool,
) -> anyhow::Result<(
    Value,
    Option<f64>,
    bool,
    agentdeck_core::judge::JudgeChannel,
)> {
    if let Some((answers, channel)) = read_jev_cache(runtime_dir, hash_input, complete) {
        // A hit reuses the cached judgment; like the Node router it reports zero
        // additional Jev cost for the reused answer.
        return Ok((answers, Some(0.0), true, channel));
    }

    let bases = bases.clone();
    let key_owned = key_override.map(str::to_string);
    let language_models = agentdeck_core::judge::LanguageModelCache::for_runtime_dir(runtime_dir);
    let queued = crate::jev_queue::ask_with_deadline(
        &runtime_dir.join("model-route-jev-queue"),
        request_value,
        body_json,
        budget,
        blocks,
        primary,
        fallback,
        move |endpoint, request, body, attempt_deadline| {
            let bases = bases.clone();
            let key_owned = key_owned.clone();
            let language_models = language_models.clone();
            async move {
                jev_fetch_endpoint(
                    &endpoint,
                    &request,
                    &body,
                    key_owned.as_deref(),
                    attempt_deadline,
                    &bases,
                    &language_models,
                )
                .await
            }
        },
        classify_jev_error,
        classify_jev_batch_error,
    )
    .await
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    if !complete(&queued.answers) {
        return Err(anyhow::anyhow!(
            "Jev returned incomplete or out-of-range routing answers"
        ));
    }
    write_jev_cache(runtime_dir, hash_input, &queued.answers, queued.channel);
    Ok((queued.answers, queued.cost, false, queued.channel))
}

/// Serialize exactly like `JSON.stringify` for the Jev body and the cache-key
/// hash: same key order and separators, plus `\u2028`/`\u2029` escaping which
/// serde omits but V8 emits.
pub(crate) fn stringify_like_node(v: &Value) -> String {
    serde_json::to_string(v)
        .unwrap_or_default()
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

fn load_router_config(env: &Env) -> anyhow::Result<RouterConfig> {
    if let Some(f) = &env.router_json {
        return load_router_json(f);
    }
    router_config()
}

/// `+c.toFixed(2)` joined for `--check`.
fn format_js(v: f64) -> String {
    let r = round2(v);
    if r.fract() == 0.0 && r.is_finite() && r.abs() < 9e15 {
        format!("{}", r as i64)
    } else {
        format!("{r}")
    }
}

#[cfg(test)]
mod gateway_tests {
    use super::*;

    #[test]
    fn gateway_round_trip_shapes() {
        let req = serde_json::json!({"model": "openai/gpt-x", "state": {"task": "t"}, "questions": {"size": {}}});
        let body = gateway_body(&req);
        assert_eq!(body["model"], "openai/gpt-x");
        assert!(
            body["messages"][1]["content"]
                .as_str()
                .unwrap()
                .contains("\"task\":\"t\"")
        );
        let resp = serde_json::json!({"choices": [{"message": {"content": "```json\n{\"size\": {\"score\": 1, \"confidence\": 0.8}}\n```"}}]});
        let a = gateway_answers(&resp).unwrap();
        assert!(valid_answer(&a["size"], false));
        assert!(gateway_answers(&serde_json::json!({})).is_err());
    }

    #[test]
    fn decisions_round_trip_shapes() {
        let req = serde_json::json!({"model": "perplexity/pplx-decider-v1.2", "state": {"task": "t"},
            "questions": {"size": {"type": "score", "instructions": "i", "criteria": SIZES}}});
        let body = decisions_body(&req);
        assert_eq!(body["questions"][0]["name"], "size");
        assert_eq!(body["questions"][0]["levels"][3]["label"], "3");
        assert!(body["input"].as_str().unwrap().contains("\"task\":\"t\""));
        let resp = serde_json::json!({"answers": [
            {"type": "score", "name": "size", "score": 0.12, "confidence": 0.92, "probabilities": [
                {"value": 0, "probability": 0.88}, {"value": 1, "probability": 0.11},
                {"value": 2, "probability": 0.005}, {"value": 3, "probability": 0.005}]},
            {"type": "refusal", "name": "difficulty"}]});
        let a = decisions_answers(&resp);
        assert!(valid_answer(&a["size"], true));
        assert!(a.get("difficulty").is_none());
    }

    async fn mock_http_server(
        responses: Vec<(&'static str, &'static str, String)>,
    ) -> (String, tokio::task::JoinHandle<Vec<(String, Value)>>) {
        use std::io::{BufRead, BufReader, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::task::spawn_blocking(move || {
            let mut seen = Vec::new();
            for (status, extra_headers, body) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(3)))
                    .unwrap();
                let mut reader = BufReader::new(&mut stream);
                let mut first = String::new();
                reader.read_line(&mut first).unwrap();
                let mut size = 0usize;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        size = value.trim().parse().unwrap();
                    }
                }
                let mut req_body = vec![0; size];
                reader.read_exact(&mut req_body).unwrap();
                let parsed: Value = serde_json::from_slice(&req_body).unwrap_or(Value::Null);
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
                seen.push((first, parsed));
            }
            seen
        });
        (base, task)
    }

    #[tokio::test]
    async fn cache_stores_fallback_answer_under_primary_key_and_honours_triggers() {
        let rt = std::env::temp_dir().join(format!(
            "agentdeck-route-fallback-cache-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&rt);
        std::fs::create_dir_all(&rt).unwrap();

        let ok_or = serde_json::json!({
            "answers": {
                "size": {"score": 1, "confidence": 0.9},
                "difficulty": {"score": 1, "confidence": 0.9},
            },
            "usage": {"cost": 0.002}
        })
        .to_string();
        // Primary (Vercel): first 429 Retry-After: 60 (triggers block + fallback),
        // then on a separate model: 503 (fallback), 401 (no fallback), invalid JSON (no fallback).
        let (vercel_base, vercel_srv) = mock_http_server(vec![
            (
                "429 Too Many Requests",
                "Retry-After: 60\r\n",
                r#"{"error":"rate limit"}"#.into(),
            ),
            ("503 Service Unavailable", "", "{}".into()),
            ("401 Unauthorized", "", "{}".into()),
            ("200 OK", "", "not-json".into()),
        ])
        .await;
        let (or_base, or_srv) = mock_http_server(vec![
            ("200 OK", "", ok_or.clone()),
            ("200 OK", "", ok_or.clone()),
            ("200 OK", "", ok_or),
        ])
        .await;

        let bases = JevBases {
            openrouter: or_base,
            vercel: vercel_base,
        };
        let blocks = agentdeck_core::judge::BlockStore::new(
            rt.join(agentdeck_core::judge::JUDGE_BLOCKS_DIR_NAME),
        );
        let primary = agentdeck_core::judge::JudgeEndpoint::new(
            agentdeck_core::settings::PROVIDER_VERCEL,
            "perplexity/pplx-decider-v1.2",
        );
        let fallback = agentdeck_core::judge::JudgeEndpoint::new(
            agentdeck_core::settings::PROVIDER_OPENROUTER,
            "~typesafe/jev-latest",
        );
        let req = serde_json::json!({
            "model": primary.model,
            "state": {"task": "fix bug", "models": {}},
            "questions": {
                "size": {"type": "score", "instructions": "size?", "criteria": SIZES},
                "difficulty": {"type": "score", "instructions": "diff?", "criteria": DIFFICULTY},
            }
        });
        let body = stringify_like_node(&req);
        let hash_input = stringify_like_node(&serde_json::json!([Value::Null, 1, req]));
        let complete = |a: &Value| {
            a.get("size").is_some_and(|s| valid_answer(s, false))
                && a.get("difficulty").is_some_and(|d| valid_answer(d, false))
        };

        // Call 1: primary 429 -> answered by fallback, cached under primary request's key.
        let (a1, cost1, hit1, ch1) = fetch_jev_cached(
            &rt,
            &hash_input,
            &req,
            &body,
            &primary,
            Some(&fallback),
            std::time::Duration::from_secs(10),
            &blocks,
            &bases,
            Some("test-key"),
            &complete,
        )
        .await
        .unwrap();
        assert!(!hit1);
        assert_eq!(ch1, agentdeck_core::judge::JudgeChannel::Fallback);
        assert_eq!(cost1, Some(0.002));
        assert_eq!(a1["size"]["score"], 1);

        // Call 2 (same primary key, even with NO fallback configured!): hits cache immediately!
        let (a2, cost2, hit2, ch2) = fetch_jev_cached(
            &rt,
            &hash_input,
            &req,
            &body,
            &primary,
            None,
            std::time::Duration::from_secs(10),
            &blocks,
            &bases,
            Some("test-key"),
            &complete,
        )
        .await
        .unwrap();
        assert!(hit2, "fallback answer must hit cache under primary key");
        assert_eq!(ch2, agentdeck_core::judge::JudgeChannel::Fallback);
        assert_eq!(cost2, Some(0.0));
        assert_eq!(a2, a1);

        // Call 3 (different task, same blocked primary, new BlockStore = second process):
        // skips blocked primary and hits fallback directly.
        let blocks2 = agentdeck_core::judge::BlockStore::new(
            rt.join(agentdeck_core::judge::JUDGE_BLOCKS_DIR_NAME),
        );
        let (_, _, hit3, ch3) = fetch_jev_cached(
            &rt,
            "different-task-hash",
            &req,
            &body,
            &primary,
            Some(&fallback),
            std::time::Duration::from_secs(10),
            &blocks2,
            &bases,
            Some("test-key"),
            &complete,
        )
        .await
        .unwrap();
        assert!(!hit3);
        assert_eq!(ch3, agentdeck_core::judge::JudgeChannel::Fallback);

        // Call 4..6 on an unblocked primary model: 503 -> fallback, 401 -> err, parse -> err.
        let unblocked = agentdeck_core::judge::JudgeEndpoint::new(
            agentdeck_core::settings::PROVIDER_VERCEL,
            "perplexity/other-model",
        );
        let (_, _, _, ch4) = fetch_jev_cached(
            &rt,
            "hash-503",
            &req,
            &body,
            &unblocked,
            Some(&fallback),
            std::time::Duration::from_secs(10),
            &blocks,
            &bases,
            Some("test-key"),
            &complete,
        )
        .await
        .unwrap();
        assert_eq!(ch4, agentdeck_core::judge::JudgeChannel::Fallback);

        let err401 = fetch_jev_cached(
            &rt,
            "hash-401",
            &req,
            &body,
            &unblocked,
            Some(&fallback),
            std::time::Duration::from_secs(10),
            &blocks,
            &bases,
            Some("test-key"),
            &complete,
        )
        .await
        .unwrap_err();
        assert!(err401.to_string().contains("401"), "{err401}");

        let err_parse = fetch_jev_cached(
            &rt,
            "hash-parse",
            &req,
            &body,
            &unblocked,
            Some(&fallback),
            std::time::Duration::from_secs(10),
            &blocks,
            &bases,
            Some("test-key"),
            &complete,
        )
        .await
        .unwrap_err();
        assert!(
            err_parse.to_string().contains("invalid JSON"),
            "{err_parse}"
        );

        let vercel_seen = vercel_srv.await.unwrap();
        assert_eq!(vercel_seen.len(), 4);
        let or_seen = or_srv.await.unwrap();
        assert_eq!(or_seen.len(), 3);
        assert_eq!(or_seen[0].1["model"], "~typesafe/jev-latest");
        let _ = std::fs::remove_dir_all(&rt);
    }

    fn setup_batch_env(tag: &str) -> (std::path::PathBuf, Env) {
        let dir = std::env::temp_dir().join(format!(
            "agentdeck-route-batch-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let home = dir.join("home");
        let runtime = dir.join("rt");
        let state_home = home.join(".local/state");
        for p in [
            home.join(".t3/userdata"),
            state_home.join("ai-usage"),
            state_home.join("model-route"),
            runtime.clone(),
        ] {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(home.join(".t3/userdata/settings.json"), "{}").unwrap();
        let catalog = serde_json::json!({
            "providers": [
                {
                    "providerInstanceId": "codex",
                    "driverKind": "codex",
                    "models": [
                        {
                            "id": "gpt-6.1-sol",
                            "options": [
                                {
                                    "id": "reasoningEffort",
                                    "type": "select",
                                    "options": [
                                        {"id": "medium"},
                                        {"id": "high"},
                                        {"id": "xhigh"}
                                    ]
                                }
                            ]
                        }
                    ]
                },
                {
                    "providerInstanceId": "claudeAgent",
                    "driverKind": "claudeAgent",
                    "models": [
                        {
                            "id": "claude-opus-5-5",
                            "options": [
                                {
                                    "id": "effort",
                                    "type": "select",
                                    "options": [
                                        {"id": "medium"},
                                        {"id": "high"},
                                        {"id": "xhigh"}
                                    ]
                                }
                            ]
                        }
                    ]
                }
            ]
        });
        std::fs::write(
            state_home.join("model-route/catalog.json"),
            catalog.to_string(),
        )
        .unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64() as i64;
        let usage = serde_json::json!({"accounts": [
            {"provider": "codex", "id": "codex-main", "plan": "Pro 5x", "status": "ok", "updated_at": now, "balances": [],
             "windows": [
                {"key": "weekly", "label": "Week", "remaining_percent": 90, "duration_mins": 10080, "resets_at": now + 5 * 86400},
                {"key": "5h", "label": "5h", "remaining_percent": 90, "duration_mins": 300, "resets_at": now + 3 * 3600}
             ]},
            {"provider": "claude", "id": "claude-main", "plan": "Max 5x", "status": "ok", "updated_at": now, "balances": [],
             "windows": [
                {"key": "weekly_all", "label": "Week", "remaining_percent": 80, "duration_mins": 10080, "resets_at": now + 5 * 86400},
                {"key": "session", "label": "5h", "remaining_percent": 80, "duration_mins": 300, "resets_at": now + 3 * 3600}
             ]}
        ]});
        std::fs::write(state_home.join("ai-usage/state.json"), usage.to_string()).unwrap();
        let router_json = dir.join("router.json");
        let cfg = serde_json::json!({
            "version": 1,
            "decisionProvider": "openrouter",
            "decisionModel": "~typesafe/jev-latest",
            "pool": [
                {"providerInstanceId": "codex", "model": "gpt-6.1-sol", "enabled": true},
                {"providerInstanceId": "claudeAgent", "model": "claude-opus-5-5", "enabled": true}
            ],
            "accounts": {
                "codex": {"instances": ["codex"], "usageProvider": "codex", "usageId": "codex-main"},
                "claude": {"instances": ["claudeAgent"], "usageProvider": "claude", "usageId": "claude-main"}
            }
        });
        std::fs::write(&router_json, cfg.to_string()).unwrap();
        let env = Env {
            home,
            runtime,
            state_home,
            router_json: Some(router_json.to_string_lossy().into_owned()),
        };
        (dir, env)
    }

    async fn dynamic_openrouter_server(
        expected_calls: usize,
    ) -> (String, tokio::task::JoinHandle<Vec<Value>>) {
        use std::io::{BufRead, BufReader, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let handle = tokio::task::spawn_blocking(move || {
            let mut seen = Vec::new();
            for _ in 0..expected_calls {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(&mut stream);
                let mut first = String::new();
                reader.read_line(&mut first).unwrap();
                let mut size = 0usize;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        size = value.trim().parse().unwrap();
                    }
                }
                let mut req_body = vec![0; size];
                reader.read_exact(&mut req_body).unwrap();
                let parsed: Value = serde_json::from_slice(&req_body).unwrap();
                // Encode task index (0..3) into `size.score` so tests can verify
                // per-launch ordering matches the input launches.
                let mut answers = serde_json::Map::new();
                for qk in parsed["questions"].as_object().unwrap().keys() {
                    let (task_text, base_q) = if let Some((prefix, rest)) = qk.split_once("__") {
                        (
                            parsed["state"]["tasks"][prefix]
                                .as_str()
                                .unwrap_or_default(),
                            rest,
                        )
                    } else {
                        (
                            parsed["state"]["task"].as_str().unwrap_or_default(),
                            qk.as_str(),
                        )
                    };
                    let idx_mod = task_text
                        .split('#')
                        .nth(1)
                        .and_then(|s| s.split_whitespace().next())
                        .and_then(|n| n.parse::<i64>().ok())
                        .unwrap_or(1)
                        % 4;
                    let entry = match base_q {
                        "size" => serde_json::json!({"score": idx_mod, "confidence": 0.9}),
                        "difficulty" => serde_json::json!({"score": 1, "confidence": 0.9}),
                        "review" => serde_json::json!({"score": 0, "confidence": 0.95}),
                        "codex_gpt_6_1_sol" => serde_json::json!({
                            "score": 2.8, "confidence": 0.9,
                            "probabilities": {"0": 0.0, "1": 0.05, "2": 0.15, "3": 0.8}
                        }),
                        _ => serde_json::json!({
                            "score": 2.2, "confidence": 0.85,
                            "probabilities": {"0": 0.0, "1": 0.1, "2": 0.7, "3": 0.2}
                        }),
                    };
                    answers.insert(qk.clone(), entry);
                }
                let body =
                    serde_json::json!({"answers": answers, "usage": {"cost": 0.01}}).to_string();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
                seen.push(parsed);
            }
            seen
        });
        (base, handle)
    }

    #[tokio::test]
    async fn route_batch_3_launches_uses_1_request_and_preserves_order_and_warms_cache() {
        let (dir, env) = setup_batch_env("three");
        let (or_base, srv) = dynamic_openrouter_server(1).await;
        let bases = JevBases {
            openrouter: or_base,
            vercel: VERCEL_BASE.into(),
        };
        let launches = vec![
            serde_json::json!({
                "tool": "mcp__t3-code__delegate_task",
                "input": {"task": "Implement unit #0 parser"}
            }),
            serde_json::json!({
                "tool": "Agent",
                "input": {"prompt": "Implement unit #1 formatter"}
            }),
            serde_json::json!({
                "tool": "mcp__t3-code__delegate_task",
                "input": {
                    "task": "Implement unit #2 tests",
                    "target": {"providerInstanceId": "codex", "model": "gpt-6.1-sol"}
                }
            }),
        ];
        let results = route_batch_with_bases(
            &env,
            &launches,
            BatchOptions {
                harness: Some("claude".into()),
                source: None,
                budget: BATCH_BUDGET,
            },
            &bases,
            Some("test-key"),
            &[],
            &[],
        )
        .await
        .unwrap();
        let seen = srv.await.unwrap();
        assert_eq!(
            seen.len(),
            1,
            "3 launches must send exactly 1 judge request"
        );
        assert_eq!(seen[0]["state"]["tasks"].as_object().unwrap().len(), 3);
        assert_eq!(results.len(), 3);
        for (i, r) in results.iter().enumerate() {
            assert!(r.get("error").is_none(), "unexpected error at {i}: {r}");
            assert!(
                r["size"].as_str().unwrap().starts_with(&format!("{i}:")),
                "order mismatch at {i}: {}",
                r["size"]
            );
            assert_eq!(r["source"], "claudeAgent");
            assert_eq!(r["jevCache"], "miss");
            assert_eq!(r["jevChannel"], "primary");
            assert_eq!(r["target"]["providerInstanceId"], "codex");
        }
        assert!(results[2].get("validation").is_some());

        // Re-running the same launches must hit the cache with 0 judge requests.
        let dead_bases = JevBases {
            openrouter: "http://127.0.0.1:1".into(),
            vercel: "http://127.0.0.1:1".into(),
        };
        let cached = route_batch_with_bases(
            &env,
            &launches,
            BatchOptions {
                harness: Some("claude".into()),
                source: None,
                budget: BATCH_BUDGET,
            },
            &dead_bases,
            Some("test-key"),
            &[],
            &[],
        )
        .await
        .unwrap();
        assert_eq!(cached.len(), 3);
        for r in &cached {
            assert_eq!(r["jevCache"], "hit");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn route_batch_10_launches_uses_2_requests_and_skips_invalid_and_user_choice() {
        let (dir, env) = setup_batch_env("ten");
        let (or_base, srv) = dynamic_openrouter_server(2).await;
        let bases = JevBases {
            openrouter: or_base,
            vercel: VERCEL_BASE.into(),
        };
        // 10 real launches + 1 invalid + 1 [route:user-choice] = 12 entries,
        // only 10 need judge calls -> 2 judge requests (8 + 2).
        let mut launches: Vec<Value> = (0..10)
            .map(|i| {
                serde_json::json!({
                    "tool": "mcp__t3-code__delegate_task",
                    "input": {"task": format!("Task #{i} for wave")}
                })
            })
            .collect();
        launches.insert(
            4,
            serde_json::json!({
                "tool": "Bash",
                "input": {"command": "ls"}
            }),
        );
        launches.insert(
            7,
            serde_json::json!({
                "tool": "mcp__t3-code__delegate_task",
                "input": {
                    "task": "[route:user-choice] Audit auth with chosen model",
                    "target": {"providerInstanceId": "claudeAgent", "model": "claude-opus-5-5"}
                }
            }),
        );

        let results = route_batch_with_bases(
            &env,
            &launches,
            BatchOptions {
                harness: Some("claude".into()),
                source: None,
                budget: BATCH_BUDGET,
            },
            &bases,
            Some("test-key"),
            &[],
            &[],
        )
        .await
        .unwrap();
        let seen = srv.await.unwrap();
        assert_eq!(seen.len(), 2, "11 judged launches must use 2 requests");
        assert_eq!(seen[0]["state"]["tasks"].as_object().unwrap().len(), 8);
        assert_eq!(seen[1]["state"]["tasks"].as_object().unwrap().len(), 3);
        assert_eq!(results.len(), 12);
        assert!(
            results[4].get("error").is_some(),
            "invalid launch must yield {{error}}: {}",
            results[4]
        );
        assert_eq!(results[7]["jevCache"], "miss");
        assert!(results[7].get("validation").is_some());
        for (idx, r) in results.iter().enumerate() {
            if idx != 4 {
                assert!(r.get("error").is_none(), "unexpected error at {idx}: {r}");
                assert_eq!(r["jevCache"], "miss");
                assert_eq!(r["target"]["providerInstanceId"], "codex");
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn route_batch_returns_per_launch_error_when_source_ambiguous_or_unknown() {
        let (dir, env) = setup_batch_env("ambig-src");
        // Configure two codex instances so harness="codex" without CODEX_HOME or T3CODE_THREAD_ID is ambiguous.
        std::fs::write(
            env.home.join(".t3/userdata/settings.json"),
            serde_json::json!({
                "providerInstances": {
                    "codex": {"driver": "codex", "config": {}},
                    "codex_b": {"driver": "codex", "config": {"shadowHomePath": "/tmp/codex-b"}}
                }
            })
            .to_string(),
        )
        .unwrap();
        let dead_bases = JevBases {
            openrouter: "http://127.0.0.1:1".into(),
            vercel: "http://127.0.0.1:1".into(),
        };
        let launches = vec![serde_json::json!({
            "tool": "mcp__t3-code__delegate_task",
            "input": {"task": "Refactor auth"}
        })];
        // Explicit empty own_env and ancestors: independent of whether `cargo test` runs under Codex.
        let res = route_batch_with_bases(
            &env,
            &launches,
            BatchOptions {
                harness: Some("codex".into()),
                source: None,
                budget: BATCH_BUDGET,
            },
            &dead_bases,
            Some("test-key"),
            &[],
            &[],
        )
        .await
        .unwrap();
        assert_eq!(res.len(), 1);
        let err = res[0]["error"]
            .as_str()
            .expect("ambiguous source must return per-launch error");
        assert!(
            err.to_lowercase().contains("source"),
            "error must explain missing/ambiguous source, not network failure: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn http_errors_never_leak_provider_body_in_route_or_batch() {
        let (dir, env) = setup_batch_env("no-body-leak");
        let (or_base, or_srv) = mock_http_server(vec![(
            "401 Unauthorized",
            "",
            r#"{"error":"echoed SECRET_PROXY_REFLECTION_999"}"#.into(),
        )])
        .await;
        let bases = JevBases {
            openrouter: or_base,
            vercel: VERCEL_BASE.into(),
        };
        let launches = vec![serde_json::json!({
            "tool": "mcp__t3-code__delegate_task",
            "input": {"task": "Test body redaction"}
        })];
        let results = route_batch_with_bases(
            &env,
            &launches,
            BatchOptions {
                harness: Some("claude".into()),
                source: Some("claudeAgent".into()),
                budget: BATCH_BUDGET,
            },
            &bases,
            Some("test-key"),
            &[],
            &[],
        )
        .await
        .unwrap();
        or_srv.await.unwrap();
        let err = results[0]["error"].as_str().expect("must error on 401");
        assert!(
            !err.contains("SECRET_PROXY_REFLECTION_999"),
            "provider HTTP body must never leak into error: {err}"
        );
        assert!(
            err.contains("jev_http_status_401"),
            "error must return fixed status code: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn route_body_abort_falls_back_and_429_language_model_does_not_retry_chat() {
        use std::io::{BufRead, BufReader, Read, Write};
        let rt =
            std::env::temp_dir().join(format!("agentdeck-route-body-429-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&rt);
        std::fs::create_dir_all(&rt).unwrap();

        let vercel_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let vercel_base = format!("http://{}", vercel_listener.local_addr().unwrap());
        let vercel_paths = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let vp = vercel_paths.clone();
        let vercel_srv = std::thread::spawn(move || {
            for idx in 0..2 {
                let (mut stream, _) = vercel_listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                    .unwrap();
                let mut reader = BufReader::new(&mut stream);
                let mut first = String::new();
                reader.read_line(&mut first).unwrap();
                let mut size = 0usize;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((n, v)) = line.split_once(':')
                        && n.eq_ignore_ascii_case("content-length")
                    {
                        size = v.trim().parse().unwrap();
                    }
                }
                let mut buf = vec![0; size];
                reader.read_exact(&mut buf).unwrap();
                vp.lock().unwrap().push(first);
                if idx == 0 {
                    // Truncated 200 body -> body-read error -> transient -> fallback.
                    let _ = stream.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 500\r\nConnection: close\r\n\r\n{\"answers\":",
                    );
                } else {
                    // 429 with "is a language model" body must NOT trigger /v1/chat/completions.
                    let body = r#"{"error":"model is a language model"}"#;
                    let _ = write!(
                        stream,
                        "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 60\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                }
            }
        });

        let ok_or = serde_json::json!({
            "answers": {
                "size": {"score": 1, "confidence": 0.9},
                "difficulty": {"score": 1, "confidence": 0.9},
            },
            "usage": {"cost": 0.002}
        })
        .to_string();
        let or_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let or_base = format!("http://{}", or_listener.local_addr().unwrap());
        let or_srv = std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = or_listener.accept().unwrap();
                let mut reader = BufReader::new(&mut stream);
                let mut first = String::new();
                reader.read_line(&mut first).unwrap();
                let mut size = 0usize;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((n, v)) = line.split_once(':')
                        && n.eq_ignore_ascii_case("content-length")
                    {
                        size = v.trim().parse().unwrap();
                    }
                }
                let mut buf = vec![0; size];
                reader.read_exact(&mut buf).unwrap();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{ok_or}",
                    ok_or.len()
                )
                .unwrap();
            }
        });

        let bases = JevBases {
            openrouter: or_base,
            vercel: vercel_base,
        };
        let blocks = agentdeck_core::judge::BlockStore::new(rt.join("blocks"));
        let primary = agentdeck_core::judge::JudgeEndpoint::new(
            agentdeck_core::settings::PROVIDER_VERCEL,
            "perplexity/pplx-decider-v1.2",
        );
        let fallback = agentdeck_core::judge::JudgeEndpoint::new(
            agentdeck_core::settings::PROVIDER_OPENROUTER,
            "~typesafe/jev-latest",
        );
        let req = serde_json::json!({
            "model": primary.model,
            "state": {"task": "fix bug", "models": {}},
            "questions": {
                "size": {"type": "score", "instructions": "size?", "criteria": SIZES},
                "difficulty": {"type": "score", "instructions": "diff?", "criteria": DIFFICULTY},
            }
        });
        let body = stringify_like_node(&req);
        let complete = |a: &Value| {
            a.get("size").is_some_and(|s| valid_answer(s, false))
                && a.get("difficulty").is_some_and(|d| valid_answer(d, false))
        };

        let (_, _, _, ch1) = fetch_jev_cached(
            &rt,
            "h-trunc",
            &req,
            &body,
            &primary,
            Some(&fallback),
            std::time::Duration::from_secs(10),
            &blocks,
            &bases,
            Some("test-key"),
            &complete,
        )
        .await
        .unwrap();
        assert_eq!(ch1, agentdeck_core::judge::JudgeChannel::Fallback);

        let (_, _, _, ch2) = fetch_jev_cached(
            &rt,
            "h-429-lm",
            &req,
            &body,
            &primary,
            Some(&fallback),
            std::time::Duration::from_secs(10),
            &blocks,
            &bases,
            Some("test-key"),
            &complete,
        )
        .await
        .unwrap();
        assert_eq!(ch2, agentdeck_core::judge::JudgeChannel::Fallback);

        vercel_srv.join().unwrap();
        or_srv.join().unwrap();
        let seen = vercel_paths.lock().unwrap();
        assert_eq!(
            seen.len(),
            2,
            "must not retry /v1/chat/completions on 429: {seen:?}"
        );
        assert!(seen.iter().all(|p| p.contains("/v1/decisions")));
        assert!(blocks.remaining(&primary).is_some());
        let _ = std::fs::remove_dir_all(&rt);
    }

    #[tokio::test]
    async fn vercel_language_model_retry_and_fallback_share_single_deadline() {
        use std::io::{BufRead, BufReader, Read, Write};
        let rt =
            std::env::temp_dir().join(format!("agentdeck-route-deadline-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&rt);
        std::fs::create_dir_all(&rt).unwrap();

        // Hanging Vercel server (stalls 2s) + fast OpenRouter fallback with 800ms overall deadline.
        let vercel_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let vercel_base = format!("http://{}", vercel_listener.local_addr().unwrap());
        let _vercel_srv = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = vercel_listener.accept() {
                let mut reader = BufReader::new(&mut stream);
                let mut line = String::new();
                let _ = reader.read_line(&mut line);
                std::thread::sleep(std::time::Duration::from_secs(2));
            }
        });
        let ok_or = serde_json::json!({
            "answers": {
                "size": {"score": 1, "confidence": 0.9},
                "difficulty": {"score": 1, "confidence": 0.9},
            },
            "usage": {"cost": 0.002}
        })
        .to_string();
        let or_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let or_base = format!("http://{}", or_listener.local_addr().unwrap());
        let or_srv = std::thread::spawn(move || {
            let (mut stream, _) = or_listener.accept().unwrap();
            let mut reader = BufReader::new(&mut stream);
            let mut first = String::new();
            reader.read_line(&mut first).unwrap();
            let mut size = 0usize;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some((n, v)) = line.split_once(':')
                    && n.eq_ignore_ascii_case("content-length")
                {
                    size = v.trim().parse().unwrap();
                }
            }
            let mut buf = vec![0; size];
            reader.read_exact(&mut buf).unwrap();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{ok_or}",
                ok_or.len()
            )
            .unwrap();
        });

        let bases = JevBases {
            openrouter: or_base,
            vercel: vercel_base,
        };
        let blocks = agentdeck_core::judge::BlockStore::new(rt.join("blocks"));
        let primary = agentdeck_core::judge::JudgeEndpoint::new(
            agentdeck_core::settings::PROVIDER_VERCEL,
            "perplexity/pplx-decider-v1.2",
        );
        let fallback = agentdeck_core::judge::JudgeEndpoint::new(
            agentdeck_core::settings::PROVIDER_OPENROUTER,
            "~typesafe/jev-latest",
        );
        let req = serde_json::json!({
            "model": primary.model,
            "state": {"task": "fix bug", "models": {}},
            "questions": {
                "size": {"type": "score", "instructions": "size?", "criteria": SIZES},
                "difficulty": {"type": "score", "instructions": "diff?", "criteria": DIFFICULTY},
            }
        });
        let body = stringify_like_node(&req);
        let complete = |a: &Value| {
            a.get("size").is_some_and(|s| valid_answer(s, false))
                && a.get("difficulty").is_some_and(|d| valid_answer(d, false))
        };

        let started = std::time::Instant::now();
        let (_, _, _, ch) = fetch_jev_cached(
            &rt,
            "h-deadline",
            &req,
            &body,
            &primary,
            Some(&fallback),
            std::time::Duration::from_millis(800),
            &blocks,
            &bases,
            Some("test-key"),
            &complete,
        )
        .await
        .unwrap();
        let elapsed = started.elapsed();
        assert_eq!(ch, agentdeck_core::judge::JudgeChannel::Fallback);
        assert!(
            elapsed < std::time::Duration::from_millis(1200),
            "fallback must succeed within single 800ms deadline, took {elapsed:?}"
        );
        or_srv.join().unwrap();

        // Also check Vercel /decisions (450ms delay) -> /chat/completions (hangs) respects single 800ms deadline.
        let v2_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let v2_base = format!("http://{}", v2_listener.local_addr().unwrap());
        let _v2_srv = std::thread::spawn(move || {
            for idx in 0..2 {
                let Ok((mut stream, _)) = v2_listener.accept() else {
                    break;
                };
                let mut reader = BufReader::new(&mut stream);
                let mut first = String::new();
                let _ = reader.read_line(&mut first);
                let mut size = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some((n, v)) = line.split_once(':')
                        && n.eq_ignore_ascii_case("content-length")
                    {
                        size = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut buf = vec![0; size];
                let _ = reader.read_exact(&mut buf);
                if idx == 0 {
                    std::thread::sleep(std::time::Duration::from_millis(450));
                    let b = r#"{"error":"model is a language model"}"#;
                    let _ = write!(
                        stream,
                        "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{b}",
                        b.len()
                    );
                } else {
                    std::thread::sleep(std::time::Duration::from_secs(2));
                }
            }
        });
        let bases2 = JevBases {
            openrouter: "http://127.0.0.1:1".into(),
            vercel: v2_base,
        };
        let started2 = std::time::Instant::now();
        let res2 = fetch_jev_cached(
            &rt,
            "h-vercel-chat-deadline",
            &req,
            &body,
            &primary,
            None,
            std::time::Duration::from_millis(800),
            &blocks,
            &bases2,
            Some("test-key"),
            &complete,
        )
        .await;
        let elapsed2 = started2.elapsed();
        assert!(res2.is_err());
        assert!(
            elapsed2 < std::time::Duration::from_millis(1100),
            "Vercel /decisions + /chat/completions must share one 800ms deadline, took {elapsed2:?}"
        );
        let _ = std::fs::remove_dir_all(&rt);
    }

    #[tokio::test]
    async fn route_batch_user_choice_warms_cache_and_fails_when_judge_down() {
        let (dir, env) = setup_batch_env("user-choice-warm");
        let launch = serde_json::json!({
            "tool": "mcp__t3-code__delegate_task",
            "input": {
                "task": "[route:user-choice] Audit auth with chosen model",
                "target": {"providerInstanceId": "claudeAgent", "model": "claude-opus-5-5"}
            }
        });

        // 1. With judge down, route_batch must NOT falsely claim allowed: true.
        let dead_bases = JevBases {
            openrouter: "http://127.0.0.1:1".into(),
            vercel: "http://127.0.0.1:1".into(),
        };
        let down = route_batch_with_bases(
            &env,
            std::slice::from_ref(&launch),
            BatchOptions {
                harness: Some("claude".into()),
                source: Some("claudeAgent".into()),
                budget: std::time::Duration::from_secs(5),
            },
            &dead_bases,
            Some("test-key"),
            &[],
            &[],
        )
        .await
        .unwrap();
        assert!(
            down[0].get("error").is_some(),
            "[route:user-choice] must fail when judge is down, not fabricate allowed:true: {}",
            down[0]
        );

        // 2. With judge up, route_batch calls judge, returns normal route output, and warms cache.
        let (or_base, srv) = dynamic_openrouter_server(1).await;
        let bases = JevBases {
            openrouter: or_base,
            vercel: VERCEL_BASE.into(),
        };
        let warmed = route_batch_with_bases(
            &env,
            std::slice::from_ref(&launch),
            BatchOptions {
                harness: Some("claude".into()),
                source: Some("claudeAgent".into()),
                budget: BATCH_BUDGET,
            },
            &bases,
            Some("test-key"),
            &[],
            &[],
        )
        .await
        .unwrap();
        assert_eq!(srv.await.unwrap().len(), 1);
        assert_eq!(warmed[0]["jevCache"], "miss", "{}", warmed[0]);
        assert!(warmed[0].get("ranking").is_some(), "{}", warmed[0]);

        // 3. Re-running with judge down now hits the warmed cache!
        let hit = route_batch_with_bases(
            &env,
            std::slice::from_ref(&launch),
            BatchOptions {
                harness: Some("claude".into()),
                source: Some("claudeAgent".into()),
                budget: std::time::Duration::from_secs(5),
            },
            &dead_bases,
            Some("test-key"),
            &[],
            &[],
        )
        .await
        .unwrap();
        assert_eq!(hit[0]["jevCache"], "hit", "{}", hit[0]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn read_jev_cache_refuses_loose_permissions_and_symlinks() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let rt =
            std::env::temp_dir().join(format!("agentdeck-jev-cache-perms-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&rt);
        std::fs::create_dir_all(&rt).unwrap();

        let answers = serde_json::json!({
            "size": {"score": 1, "confidence": 0.9},
            "difficulty": {"score": 1, "confidence": 0.9},
        });
        let complete = |a: &Value| {
            a.get("size").is_some_and(|s| valid_answer(s, false))
                && a.get("difficulty").is_some_and(|d| valid_answer(d, false))
        };
        let key = "cache-perms-test-key";
        write_jev_cache(
            &rt,
            key,
            &answers,
            agentdeck_core::judge::JudgeChannel::Primary,
        );
        assert!(read_jev_cache(&rt, key, &complete).is_some());

        let cache_dir = rt.join("model-route-jev-cache");
        let cache_file = jev_cache_path(&rt, key);

        // 1. Loose file permissions (0644) must be refused.
        std::fs::set_permissions(&cache_file, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            read_jev_cache(&rt, key, &complete).is_none(),
            "world/group-readable Jev cache file must be refused"
        );
        std::fs::set_permissions(&cache_file, std::fs::Permissions::from_mode(0o600)).unwrap();

        // 2. Symlinked cache file must be refused.
        let decoy = cache_dir.join("decoy.json");
        std::fs::copy(&cache_file, &decoy).unwrap();
        std::fs::set_permissions(&decoy, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::remove_file(&cache_file).unwrap();
        symlink(&decoy, &cache_file).unwrap();
        assert!(
            read_jev_cache(&rt, key, &complete).is_none(),
            "symlinked Jev cache file must be refused"
        );
        std::fs::remove_file(&cache_file).unwrap();
        std::fs::rename(&decoy, &cache_file).unwrap();

        // 3. Loose directory permissions (0755) must be refused.
        std::fs::set_permissions(&cache_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            read_jev_cache(&rt, key, &complete).is_none(),
            "world/group-accessible Jev cache dir must be refused"
        );

        let _ = std::fs::remove_dir_all(&rt);
    }

    #[tokio::test]
    async fn jev_cache_invalidates_when_primary_provider_or_fallback_changes() {
        let (dir, env) = setup_batch_env("cache-key-channels");
        let router_json = std::path::PathBuf::from(env.router_json.as_ref().unwrap());

        // Start with openrouter primary + vercel fallback.
        let mut cfg: Value =
            serde_json::from_str(&std::fs::read_to_string(&router_json).unwrap()).unwrap();
        cfg["judgeFallback"] = serde_json::json!({
            "provider": "vercel",
            "model": "perplexity/pplx-decider-v1.2"
        });
        std::fs::write(&router_json, cfg.to_string()).unwrap();

        let (or_base, or_srv) = {
            use std::io::{BufRead, BufReader, Read, Write};
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let handle = std::thread::spawn(move || {
                let mut seen = 0usize;
                for _ in 0..2 {
                    let (mut stream, _) = listener.accept().unwrap();
                    let mut reader = BufReader::new(&mut stream);
                    let mut first = String::new();
                    reader.read_line(&mut first).unwrap();
                    let mut size = 0usize;
                    loop {
                        let mut line = String::new();
                        reader.read_line(&mut line).unwrap();
                        if line == "\r\n" {
                            break;
                        }
                        if let Some((n, v)) = line.split_once(':')
                            && n.eq_ignore_ascii_case("content-length")
                        {
                            size = v.trim().parse().unwrap();
                        }
                    }
                    let mut buf = vec![0; size];
                    reader.read_exact(&mut buf).unwrap();
                    let body = serde_json::json!({
                        "answers": {
                            "size": {"score": 1, "confidence": 0.9},
                            "difficulty": {"score": 1, "confidence": 0.9},
                            "review": {"score": 0, "confidence": 0.95},
                            "codex_gpt_6_1_sol": {
                                "score": 2.8, "confidence": 0.9,
                                "probabilities": {"0": 0.0, "1": 0.05, "2": 0.15, "3": 0.8}
                            },
                            "claudeagent_claude_opus_5_5": {
                                "score": 2.2, "confidence": 0.85,
                                "probabilities": {"0": 0.0, "1": 0.1, "2": 0.7, "3": 0.2}
                            }
                        },
                        "usage": {"cost": 0.01}
                    })
                    .to_string();
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .unwrap();
                    seen += 1;
                }
                seen
            });
            (base, handle)
        };
        let bases = JevBases {
            openrouter: or_base,
            vercel: VERCEL_BASE.into(),
        };
        let launches = vec![serde_json::json!({
            "tool": "mcp__t3-code__delegate_task",
            "input": {"task": "Implement unit #0 channel cache test"}
        })];

        // 1. Initial call warms cache under (openrouter/~typesafe/jev-latest, vercel/pplx).
        let r1 = route_batch_with_bases(
            &env,
            &launches,
            BatchOptions {
                harness: Some("claude".into()),
                source: Some("claudeAgent".into()),
                budget: BATCH_BUDGET,
            },
            &bases,
            Some("test-key"),
            &[],
            &[],
        )
        .await
        .unwrap();
        assert_eq!(r1[0]["jevCache"], "miss");

        // 2. Remove [judge_fallback]: must MISS cache and re-query judge!
        cfg.as_object_mut().unwrap().remove("judgeFallback");
        std::fs::write(&router_json, cfg.to_string()).unwrap();
        let r2 = route_batch_with_bases(
            &env,
            &launches,
            BatchOptions {
                harness: Some("claude".into()),
                source: Some("claudeAgent".into()),
                budget: BATCH_BUDGET,
            },
            &bases,
            Some("test-key"),
            &[],
            &[],
        )
        .await
        .unwrap();
        assert_eq!(
            r2[0]["jevCache"], "miss",
            "removing [judge_fallback] must invalidate the Jev cache key"
        );
        assert_eq!(or_srv.join().unwrap(), 2);

        // 3. Switch primary provider from openrouter -> vercel (keeping exact same model string!):
        // must also miss cache instead of reusing openrouter's cached entry.
        cfg["decisionProvider"] = serde_json::json!("vercel");
        std::fs::write(&router_json, cfg.to_string()).unwrap();
        let ok_vercel = serde_json::json!({
            "answers": [
                {"type": "score", "name": "size", "score": 1, "confidence": 0.9},
                {"type": "score", "name": "difficulty", "score": 1, "confidence": 0.9},
                {"type": "score", "name": "review", "score": 0, "confidence": 0.95},
                {"type": "score", "name": "codex_gpt_6_1_sol", "score": 2.8, "confidence": 0.9,
                 "probabilities": [
                     {"value": 0, "probability": 0.0},
                     {"value": 1, "probability": 0.05},
                     {"value": 2, "probability": 0.15},
                     {"value": 3, "probability": 0.8}
                 ]},
                {"type": "score", "name": "claudeagent_claude_opus_5_5", "score": 2.2, "confidence": 0.85,
                 "probabilities": [
                     {"value": 0, "probability": 0.0},
                     {"value": 1, "probability": 0.1},
                     {"value": 2, "probability": 0.7},
                     {"value": 3, "probability": 0.2}
                 ]}
            ]
        })
        .to_string();
        let (vercel_base, vercel_srv) = mock_http_server(vec![("200 OK", "", ok_vercel)]).await;
        let bases_v = JevBases {
            openrouter: "http://127.0.0.1:1".into(),
            vercel: vercel_base,
        };
        let r3 = route_batch_with_bases(
            &env,
            &launches,
            BatchOptions {
                harness: Some("claude".into()),
                source: Some("claudeAgent".into()),
                budget: BATCH_BUDGET,
            },
            &bases_v,
            Some("test-key"),
            &[],
            &[],
        )
        .await
        .unwrap();
        assert_eq!(
            r3[0]["jevCache"], "miss",
            "changing primary decisionProvider must invalidate the Jev cache key"
        );
        assert_eq!(vercel_srv.await.unwrap().len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn vercel_language_model_cached_skips_decisions_on_subsequent_calls() {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::sync::atomic::{AtomicBool, Ordering};
        let rt =
            std::env::temp_dir().join(format!("agentdeck-route-lm-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&rt);
        std::fs::create_dir_all(&rt).unwrap();

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let vercel_base = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let stop_bg = stop.clone();
        let paths = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let paths_bg = paths.clone();
        let srv = std::thread::spawn(move || {
            let chat_ok = serde_json::json!({
                "choices": [{
                    "message": {
                        "content": serde_json::json!({
                            "size": {"score": 1, "confidence": 0.9},
                            "difficulty": {"score": 1, "confidence": 0.9}
                        }).to_string()
                    }
                }]
            })
            .to_string();
            let lm_400 = r#"{"error":"openai/gpt-4o-mini is a language model"}"#;
            while !stop_bg.load(Ordering::SeqCst) {
                let (mut stream, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                        continue;
                    }
                    Err(_) => break,
                };
                stream.set_nonblocking(false).unwrap();
                let mut reader = BufReader::new(&mut stream);
                let mut first = String::new();
                reader.read_line(&mut first).unwrap();
                let mut size = 0usize;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((n, v)) = line.split_once(':')
                        && n.eq_ignore_ascii_case("content-length")
                    {
                        size = v.trim().parse().unwrap();
                    }
                }
                let mut buf = vec![0; size];
                reader.read_exact(&mut buf).unwrap();
                let (status, body) = if first.contains("/v1/decisions") {
                    ("400 Bad Request", lm_400)
                } else {
                    ("200 OK", chat_ok.as_str())
                };
                paths_bg.lock().unwrap().push(first);
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });

        let bases = JevBases {
            openrouter: "http://127.0.0.1:1".into(),
            vercel: vercel_base,
        };
        let blocks = agentdeck_core::judge::BlockStore::for_runtime_dir(&rt);
        let primary = agentdeck_core::judge::JudgeEndpoint::new(
            agentdeck_core::settings::PROVIDER_VERCEL,
            "openai/gpt-4o-mini",
        );
        let req1 = serde_json::json!({
            "model": primary.model,
            "state": {"task": "first task", "models": {}},
            "questions": {
                "size": {"type": "score", "instructions": "size?", "criteria": SIZES},
                "difficulty": {"type": "score", "instructions": "diff?", "criteria": DIFFICULTY},
            }
        });
        let req2 = serde_json::json!({
            "model": primary.model,
            "state": {"task": "second distinct task", "models": {}},
            "questions": {
                "size": {"type": "score", "instructions": "size?", "criteria": SIZES},
                "difficulty": {"type": "score", "instructions": "diff?", "criteria": DIFFICULTY},
            }
        });
        let complete = |a: &Value| {
            a.get("size").is_some_and(|s| valid_answer(s, false))
                && a.get("difficulty").is_some_and(|d| valid_answer(d, false))
        };

        fetch_jev_cached(
            &rt,
            "lm-cache-key-1",
            &req1,
            &stringify_like_node(&req1),
            &primary,
            None,
            std::time::Duration::from_secs(10),
            &blocks,
            &bases,
            Some("test-key"),
            &complete,
        )
        .await
        .unwrap();

        fetch_jev_cached(
            &rt,
            "lm-cache-key-2",
            &req2,
            &stringify_like_node(&req2),
            &primary,
            None,
            std::time::Duration::from_secs(10),
            &blocks,
            &bases,
            Some("test-key"),
            &complete,
        )
        .await
        .unwrap();

        stop.store(true, Ordering::SeqCst);
        srv.join().unwrap();

        let seen = paths.lock().unwrap();
        assert_eq!(
            seen.len(),
            3,
            "second call must go straight to /v1/chat/completions without hitting /v1/decisions: {seen:?}"
        );
        assert!(seen[0].contains("/v1/decisions"));
        assert!(seen[1].contains("/v1/chat/completions"));
        assert!(seen[2].contains("/v1/chat/completions"));
        let _ = std::fs::remove_dir_all(&rt);
    }

    #[tokio::test]
    async fn channel_wide_400_fails_batch_in_one_send_without_leaking_body_and_ambiguous_400_caps_sends()
     {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::sync::atomic::{AtomicBool, Ordering};

        fn spawn_400_server(
            responder: impl Fn(&Value) -> (&'static str, String) + Send + 'static,
        ) -> (
            String,
            std::sync::Arc<AtomicBool>,
            std::sync::Arc<std::sync::Mutex<usize>>,
            std::thread::JoinHandle<()>,
        ) {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            listener.set_nonblocking(true).unwrap();
            let stop = std::sync::Arc::new(AtomicBool::new(false));
            let stop_bg = stop.clone();
            let hits = std::sync::Arc::new(std::sync::Mutex::new(0usize));
            let hits_bg = hits.clone();
            let handle = std::thread::spawn(move || {
                while !stop_bg.load(Ordering::SeqCst) {
                    let (mut stream, _) = match listener.accept() {
                        Ok(pair) => pair,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(std::time::Duration::from_millis(5));
                            continue;
                        }
                        Err(_) => break,
                    };
                    stream.set_nonblocking(false).unwrap();
                    let mut reader = BufReader::new(&mut stream);
                    let mut first = String::new();
                    reader.read_line(&mut first).unwrap();
                    let mut size = 0usize;
                    loop {
                        let mut line = String::new();
                        reader.read_line(&mut line).unwrap();
                        if line == "\r\n" {
                            break;
                        }
                        if let Some((n, v)) = line.split_once(':')
                            && n.eq_ignore_ascii_case("content-length")
                        {
                            size = v.trim().parse().unwrap();
                        }
                    }
                    let mut req_body = vec![0; size];
                    reader.read_exact(&mut req_body).unwrap();
                    let parsed: Value = serde_json::from_slice(&req_body).unwrap_or(Value::Null);
                    *hits_bg.lock().unwrap() += 1;
                    let (status, body) = responder(&parsed);
                    write!(
                        stream,
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .unwrap();
                }
            });
            (base, stop, hits, handle)
        }

        let make_launches = |prefix: &str| -> Vec<Value> {
            (0..8)
                .map(|i| {
                    serde_json::json!({
                        "tool": "mcp__t3-code__delegate_task",
                        "input": {"task": format!("{prefix} unit #{i}")}
                    })
                })
                .collect()
        };

        // 1. Unknown model 400 -> 1 send total, all 8 fail with jev_http_status_400, no body leak.
        let (dir1, env1) = setup_batch_env("400-unknown-model");
        let (base1, stop1, hits1, srv1) = spawn_400_server(|_| {
            (
                "400 Bad Request",
                r#"{"error":{"message":"bogus/model is not a valid model ID SECRET_BODY_XYZ","code":"model_not_found"}}"#.into(),
            )
        });
        let res1 = route_batch_with_bases(
            &env1,
            &make_launches("unknown-model"),
            BatchOptions {
                harness: Some("claude".into()),
                source: Some("claudeAgent".into()),
                budget: std::time::Duration::from_secs(10),
            },
            &JevBases {
                openrouter: base1,
                vercel: "http://127.0.0.1:1".into(),
            },
            Some("test-key"),
            &[],
            &[],
        )
        .await
        .unwrap();
        stop1.store(true, Ordering::SeqCst);
        srv1.join().unwrap();
        assert_eq!(
            *hits1.lock().unwrap(),
            1,
            "unknown-model 400 must fail the whole batch of 8 in 1 send"
        );
        for r in &res1 {
            let err = r["error"].as_str().expect("must fail");
            assert_eq!(err, "jev_http_status_400");
            assert!(!err.contains("SECRET_BODY_XYZ"));
        }
        let _ = std::fs::remove_dir_all(&dir1);

        // 2. Invalid API parameter 400 -> 1 send total, no body leak.
        let (dir2, env2) = setup_batch_env("400-invalid-param");
        let (base2, stop2, hits2, srv2) = spawn_400_server(|_| {
            (
                "400 Bad Request",
                r#"{"error":{"message":"Unknown parameter: 'extra_foo' SECRET_PARAM_XYZ"}}"#.into(),
            )
        });
        let res2 = route_batch_with_bases(
            &env2,
            &make_launches("invalid-param"),
            BatchOptions {
                harness: Some("claude".into()),
                source: Some("claudeAgent".into()),
                budget: std::time::Duration::from_secs(10),
            },
            &JevBases {
                openrouter: base2,
                vercel: "http://127.0.0.1:1".into(),
            },
            Some("test-key"),
            &[],
            &[],
        )
        .await
        .unwrap();
        stop2.store(true, Ordering::SeqCst);
        srv2.join().unwrap();
        assert_eq!(
            *hits2.lock().unwrap(),
            1,
            "invalid-parameter 400 must fail the whole batch of 8 in 1 send"
        );
        for r in &res2 {
            let err = r["error"].as_str().expect("must fail");
            assert_eq!(err, "jev_http_status_400");
            assert!(!err.contains("SECRET_PARAM_XYZ"));
        }
        let _ = std::fs::remove_dir_all(&dir2);

        // 3. Ambiguous 400 -> falls back one-by-one capped at batch_size + 1 (9 sends, NOT 15).
        let (dir3, env3) = setup_batch_env("400-ambiguous");
        let (base3, stop3, hits3, srv3) = spawn_400_server(|_| {
            (
                "400 Bad Request",
                r#"{"error":"bad request SECRET_AMBIG_XYZ"}"#.into(),
            )
        });
        let res3 = route_batch_with_bases(
            &env3,
            &make_launches("ambig"),
            BatchOptions {
                harness: Some("claude".into()),
                source: Some("claudeAgent".into()),
                budget: std::time::Duration::from_secs(10),
            },
            &JevBases {
                openrouter: base3,
                vercel: "http://127.0.0.1:1".into(),
            },
            Some("test-key"),
            &[],
            &[],
        )
        .await
        .unwrap();
        stop3.store(true, Ordering::SeqCst);
        srv3.join().unwrap();
        assert_eq!(
            *hits3.lock().unwrap(),
            9,
            "ambiguous 400 on 8 tasks must make exactly batch_size + 1 = 9 sends"
        );
        for r in &res3 {
            let err = r["error"].as_str().expect("must fail");
            assert_eq!(err, "jev_http_status_400");
            assert!(!err.contains("SECRET_AMBIG_XYZ"));
        }
        let _ = std::fs::remove_dir_all(&dir3);

        // 4. Context-length 400 on combined batch (recovers one-by-one in 9 sends, all 8 succeed).
        let (dir4, env4) = setup_batch_env("400-context-len");
        let (base4, stop4, hits4, srv4) = spawn_400_server(|req| {
            if req.pointer("/state/tasks").is_some() {
                return (
                    "400 Bad Request",
                    r#"{"error":{"message":"maximum context length exceeded"}}"#.into(),
                );
            }
            let body = serde_json::json!({
                "answers": {
                    "size": {"score": 1, "confidence": 0.9},
                    "difficulty": {"score": 1, "confidence": 0.9},
                    "review": {"score": 0, "confidence": 0.95},
                    "codex_gpt_6_1_sol": {
                        "score": 2.8, "confidence": 0.9,
                        "probabilities": {"0": 0.0, "1": 0.05, "2": 0.15, "3": 0.8}
                    },
                    "claudeagent_claude_opus_5_5": {
                        "score": 2.2, "confidence": 0.85,
                        "probabilities": {"0": 0.0, "1": 0.1, "2": 0.7, "3": 0.2}
                    }
                },
                "usage": {"cost": 0.01}
            })
            .to_string();
            ("200 OK", body)
        });
        let res4 = route_batch_with_bases(
            &env4,
            &make_launches("ctx-len"),
            BatchOptions {
                harness: Some("claude".into()),
                source: Some("claudeAgent".into()),
                budget: std::time::Duration::from_secs(10),
            },
            &JevBases {
                openrouter: base4,
                vercel: "http://127.0.0.1:1".into(),
            },
            Some("test-key"),
            &[],
            &[],
        )
        .await
        .unwrap();
        stop4.store(true, Ordering::SeqCst);
        srv4.join().unwrap();
        assert_eq!(
            *hits4.lock().unwrap(),
            9,
            "context-length 400 must recover all 8 tasks one-by-one in 9 sends"
        );
        for (i, r) in res4.iter().enumerate() {
            assert!(
                r.get("error").is_none(),
                "task {i} should succeed after one-by-one fallback: {r}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir4);
    }
}

#[cfg(test)]
mod quota_estimate_tests {
    use super::*;
    use crate::policy::RouterConfig;

    const NOW: f64 = 1_760_000_000.0;

    fn test_cfg() -> RouterConfig {
        RouterConfig {
            file: String::new(),
            version: Value::Null,
            pool: vec![],
            instance_limits: std::collections::HashMap::new(),
            usage_state_path: None,
            decision_model: None,
            decision_provider: None,
            judge_fallback: agentdeck_core::settings::JudgeFallbackSettings::default(),
            workflow_eval: crate::policy::WorkflowEvalConfig::default(),
            user_rules: agentdeck_core::settings::UserRules::default(),
            account: [("codex".to_string(), "codex".to_string())]
                .into_iter()
                .collect(),
            usage_id: std::collections::HashMap::new(),
            meta: vec![],
        }
    }

    fn load_known() -> AgentLoad {
        AgentLoad {
            known: true,
            agents: vec![],
            note: None,
        }
    }

    fn codex_acc(plan: &str, updated_secs_ago: f64) -> UsageAccountFull {
        UsageAccountFull {
            provider: "codex".into(),
            id: Some("codex-main".into()),
            plan: plan.into(),
            status: Some("ok".into()),
            error: None,
            updated_at: NOW - updated_secs_ago,
            windows: vec![QuotaWindow {
                key: "weekly".into(),
                label: Some("Week".into()),
                scope: None,
                remaining_percent: 80.0,
                duration_mins: 10080.0,
                resets_at: Some(NOW + 3.0 * 86400.0),
                cost_factor: None,
                pending: 0.0,
            }],
            balances: vec![],
        }
    }

    fn measured_for(key: &str, n: usize) -> Measured {
        let mut m = Measured::default();
        m.costs.insert(key.into(), (n, [0.4, 1.6, 5.0, 12.0]));
        m
    }

    /// Ported from the legacy Node `costs.test.mjs` (deleted with the router
    /// `.mjs` sources): session-log calibration deduplicates repeats, ignores
    /// stale snapshot jumps and excludes mixed-plan sessions.
    #[test]
    fn session_log_calibration_ignores_repeats_jumps_and_mixed_plans() {
        let home = std::env::temp_dir().join(format!(
            "codex-costs-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        // Real Codex layout: sessions/YYYY/MM/DD/<file>.jsonl.
        let sessions = home.join(".codex/sessions/2026/10/01");
        std::fs::create_dir_all(&sessions).unwrap();
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as f64)
            .unwrap_or(0.0);
        let start = now_ms - 3.0 * 86400e3;
        let resets = (now_ms / 1000.0).round() as i64 + 3 * 86400;
        // ISO-8601 timestamps; only spacing and ordering matter.
        fn iso(ms: f64) -> String {
            let secs = (ms / 1000.0) as i64;
            let millis = (ms % 1000.0).rem_euclid(1000.0) as i64;
            // Days since epoch to civil date (Hinnant's algorithm).
            let z = secs.div_euclid(86400) + 719468;
            let era = z.div_euclid(146097);
            let doe = z.rem_euclid(146097);
            let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
            let y = yoe + era * 400;
            let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
            let mp = (5 * doy + 2) / 153;
            let d = doy - (153 * mp + 2) / 5 + 1;
            let m = if mp < 10 { mp + 3 } else { mp - 9 };
            let y = if m <= 2 { y + 1 } else { y };
            let tod = secs.rem_euclid(86400);
            format!(
                "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}Z",
                tod / 3600,
                (tod % 3600) / 60,
                tod % 60
            )
        }
        let mut logs: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        let mut totals: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
        let mut event = |file: &str,
                         minute: f64,
                         model: &str,
                         plan: &str,
                         used: f64,
                         tokens: f64,
                         repeat: bool| {
            let total = totals.entry(file.to_string()).or_insert(0.0);
            if !repeat {
                *total += tokens;
            }
            let lines = logs.entry(file.to_string()).or_default();
            lines.push(
                serde_json::json!({"timestamp": iso(start + minute * 60e3), "type": "turn_context",
                    "payload": {"model": model}})
                .to_string(),
            );
            lines.push(
                serde_json::json!({"timestamp": iso(start + minute * 60e3), "type": "event_msg",
                    "payload": {"type": "token_count",
                        "info": {"last_token_usage": {"total_tokens": tokens},
                            "total_token_usage": {"total_tokens": *total}},
                        "rate_limits": {"primary": {"used_percent": used, "window_minutes": 10080,
                            "resets_at": resets}, "secondary": null, "plan_type": plan}}})
                .to_string(),
            );
        };
        // Pro 5x gpt-x: 1 point per 1000 tokens; repeats log no new tokens.
        for i in 0..10 {
            for k in 0..3 {
                event(
                    &format!("x{i}"),
                    (i * 10 + k) as f64,
                    "gpt-x",
                    "prolite",
                    (10 + i * 3 + k) as f64,
                    1000.0,
                    false,
                );
            }
            event(
                &format!("x{i}"),
                i as f64 * 10.0 + 2.5,
                "gpt-x",
                "prolite",
                (12 + i * 3) as f64,
                1000.0,
                true,
            );
        }
        // A stale snapshot jump inside a session is not consumption.
        event("x0", 3.0, "gpt-x", "prolite", 60.0, 0.0, false);
        event("x0", 4.0, "gpt-x", "prolite", 12.0, 0.0, false);
        // A session mixing both accounts cannot be one account's sample.
        event("mix", 150.0, "gpt-x", "prolite", 45.0, 1000.0, false);
        event("mix", 151.0, "gpt-x", "plus", 1.0, 1000.0, false);
        for (name, lines) in &logs {
            std::fs::write(sessions.join(format!("{name}.jsonl")), lines.join("\n")).unwrap();
        }
        let env = Env {
            home: home.clone(),
            runtime: home.join("rt"),
            state_home: home.join("state"),
            router_json: None,
        };
        let m = compute_codex_costs(&env);
        let (n, pct) = m.costs.get("prolite|gpt-x").expect("measured costs");
        assert_eq!(*n, 10);
        assert_eq!(*pct, [3.0, 3.0, 3.0, 3.0]);
        assert!(!m.costs.contains_key("plus|gpt-x"));
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn near_empty_short_window_rejects_size_two() {
        let mut cfg = test_cfg();
        cfg.account
            .insert("antigravity".into(), "antigravity".into());
        let mut acc = codex_acc("Google AI Pro", 0.0);
        acc.provider = "antigravity".into();
        acc.id = None;
        acc.windows[0].key = "gemini-5h".into();
        acc.windows[0].duration_mins = 300.0;
        acc.windows[0].remaining_percent = 100.0 - 99.6;
        let q = quota(
            &cfg,
            &Measured::default(),
            &[acc],
            &load_known(),
            "antigravity",
            "gemini-4-hydrogen-high",
            NOW,
        );
        assert!(
            !q.allowed[2],
            "a nearly exhausted short window cannot fund another launch"
        );
    }

    #[test]
    fn antigravity_sonnet_uses_calibrated_admission_and_reservations() {
        let instance = "antigravity_4";
        let model = "claude-sonnet-5-5";
        let mut cfg = test_cfg();
        cfg.account.insert(instance.into(), "antigravity".into());
        cfg.account.insert("claudeAgent".into(), "claude".into());
        let measured = Measured::default();
        let mut acc = codex_acc("Google AI Pro", 0.0);
        acc.id = None;
        acc.provider = "antigravity".into();
        acc.windows[0].key = "3p-weekly".into();
        acc.windows[0].scope = Some("Third-party models".into());
        acc.windows.push(QuotaWindow {
            key: "3p-5h".into(),
            duration_mins: 300.0,
            ..acc.windows[0].clone()
        });
        acc.windows.push(QuotaWindow {
            key: "gemini-weekly".into(),
            scope: None,
            remaining_percent: 0.0,
            ..acc.windows[0].clone()
        });
        let mut load = load_known();
        let q = quota(&cfg, &measured, &[acc.clone()], &load, instance, model, NOW);
        assert_eq!(q.weekly.unwrap().map(round2), [4.6, 13.8, 34.5, 92.0]);
        assert_eq!(q.burn, Some(23.0));
        assert_eq!(q.estimate_basis, "estimate");
        assert_eq!(q.allowed, [true, true, true, false]);
        assert_eq!(q.estimate_windows.len(), 2);
        load.agents.push(crate::policy::ActiveAgent {
            id: "active-sonnet".into(),
            provider: instance.into(),
            model: Some(model.into()),
        });
        let q = quota(&cfg, &measured, &[acc.clone()], &load, instance, model, NOW);
        assert_eq!(q.allowed, [true, true, false, false]);
        assert_eq!(round2(q.estimate_windows[0].remaining_percent), 66.2);
        assert_eq!(round2(q.estimate_windows[1].remaining_percent), 52.4);

        let q = quota(
            &cfg,
            &measured,
            &[acc.clone()],
            &load,
            instance,
            "claude-opus-5-5",
            NOW,
        );
        assert_eq!(q.weekly.unwrap(), [0.2, 0.6, 1.5, 4.0]);
        assert_eq!(round2(q.estimate_windows[1].remaining_percent), 52.4);

        acc.provider = "claude".into();
        acc.plan = "Pro".into();
        acc.windows.pop();
        acc.windows[0].key = "weekly_all".into();
        acc.windows[0].scope = None;
        acc.windows[1].key = "session".into();
        acc.windows[1].scope = None;
        let q = quota(&cfg, &measured, &[acc], &load, "claudeAgent", model, NOW);
        assert_eq!(q.weekly.unwrap().map(round2), [0.24, 0.96, 3.0, 7.2]);
        assert_eq!(q.allowed, [true; 4]);
    }

    #[test]
    fn measured_basis_reports_sample_count() {
        let cfg = test_cfg();
        // "Pro 5x" maps via codexPlanTypes to raw plan "prolite".
        let m = measured_for("prolite|gpt-6.1-sol", 12);
        let acc = codex_acc("Pro 5x", 60.0);
        let (cost, basis, sample) = model_costs(&m, &acc, "codex", "gpt-6.1-sol");
        assert_eq!(cost.weekly, [0.4, 1.6, 5.0, 12.0]);
        assert_eq!(sample, Some(12));
        assert!(basis.contains("measured over 12 sessions"), "{basis}");
        let q = quota(&cfg, &m, &[acc], &load_known(), "codex", "gpt-6.1-sol", NOW);
        assert_eq!(q.estimate_basis, "measured");
        assert_eq!(q.sample_sessions, Some(12));
        assert_eq!(q.stale_minutes, Some(1));
        assert!(!q.load_unknown);
        assert_eq!(q.estimate_windows.len(), 1);
        assert_eq!(q.estimate_windows[0].key, "weekly");
    }

    #[test]
    fn plan_fallback_is_estimate_without_sample() {
        let cfg = test_cfg();
        let m = Measured::default(); // no session data at all
        let acc = codex_acc("Pro 5x", 60.0);
        let (_, basis, sample) = model_costs(&m, &acc, "codex", "gpt-6.1-sol");
        assert_eq!(sample, None);
        assert!(basis.contains("estimate"), "{basis}");
        let q = quota(&cfg, &m, &[acc], &load_known(), "codex", "gpt-6.1-sol", NOW);
        assert_eq!(q.estimate_basis, "estimate");
        assert_eq!(q.sample_sessions, None);
        // Ranking inputs are untouched: gated by quota, cost present.
        assert!(q.cost.is_some());
    }

    #[test]
    fn unknown_plan_is_conservative_estimate() {
        let m = Measured::default();
        let acc = codex_acc("NoSuchPlan", 60.0);
        let (_, basis, sample) = model_costs(&m, &acc, "codex", "gpt-6.1-sol");
        assert_eq!(sample, None);
        assert!(basis.contains("conservative estimate"), "{basis}");
    }

    #[test]
    fn unknown_model_quota_is_conservative() {
        let cfg = test_cfg();
        let m = Measured::default();
        let acc = codex_acc("Pro 5x", 60.0);
        // No roster family: quota reserves the highest profiled burn and says
        // so, instead of faking a cheap observed cost.
        let (cost, basis, sample) = model_costs(&m, &acc, "codex", "some-new-model-9");
        let burn = cost.burn;
        assert_eq!(burn, crate::policy::max_burn());
        assert_eq!(sample, None);
        assert!(basis.contains("estimate"), "{basis}");
        assert!(basis.contains(&burn.to_string()), "{basis}");
        let q = quota(
            &cfg,
            &m,
            &[acc],
            &load_known(),
            "codex",
            "some-new-model-9",
            NOW,
        );
        assert_eq!(q.estimate_basis, "estimate");
        assert!(q.cost.is_some(), "conservative cost still gates task sizes");
    }

    #[test]
    fn unknown_quota_reports_unknown_basis() {
        let cfg = test_cfg();
        let m = Measured::default();
        let load = AgentLoad {
            known: false,
            agents: vec![],
            note: Some("metadata unavailable".into()),
        };
        let q = quota(&cfg, &m, &[], &load, "codex", "gpt-6.1-sol", NOW);
        assert_eq!(q.estimate_basis, "unknown");
        assert_eq!(q.estimate_detail, "no ai-usage account");
        assert_eq!(q.sample_sessions, None);
        assert_eq!(q.stale_minutes, None);
        assert!(q.load_unknown);
        assert!(q.estimate_windows.is_empty());
        assert!(q.estimate_balances.is_empty());
        assert_eq!(q.allowed, [true; 4]);
        assert!(q.note.contains("quota unknown"));
    }

    #[test]
    fn estimate_json_shapes_windows_and_nulls() {
        let cfg = test_cfg();
        let m = Measured::default();
        let acc = codex_acc("Pro 5x", 45.0 * 60.0);
        let q = quota(&cfg, &m, &[acc], &load_known(), "codex", "gpt-6.1-sol", NOW);
        let provenance = QuotaProvenance {
            basis: q.estimate_basis,
            detail: q.estimate_detail.clone(),
            sample_sessions: q.sample_sessions,
            stale_minutes: q.stale_minutes,
            load_unknown: q.load_unknown,
            windows: q.estimate_windows.clone(),
            balances: q.estimate_balances.clone(),
        };
        let c = Candidate {
            provider: "codex".into(),
            model: "gpt-6.1-sol".into(),
            profiled: true,
            burn: Some(1.0),
            target: Value::Null,
            reasoning_error: None,
            no_review: false,
            policy_ban: None,
            fit: 0.9,
            failure_penalty: 0.0,
            quality: 0.8,
            confidence: 0.9,
            allowed: true,
            task_cost: None,
            quota_note: q.note.clone(),
            active_agents: 0,
            quota_provenance: provenance,
        };
        let v = quota_estimate_json(&c);
        assert_eq!(v["basis"], "estimate");
        assert!(v["detail"].as_str().unwrap().contains("estimate"));
        assert!(v["sampleSessions"].is_null());
        assert_eq!(v["staleMinutes"], 45);
        assert_eq!(v["loadUnknown"], false);
        assert_eq!(v["windows"][0]["unit"], "percent");
        assert_eq!(v["windows"][0]["remainingPercent"], 80);
        assert_eq!(v["windows"][0]["resetsInHours"], 72);
        assert!(v["balancesUsd"].as_array().unwrap().is_empty());

        let u = unknown_quota("no ai-usage account", true, None);
        let uc = Candidate {
            provider: "codex".into(),
            model: "gpt-6.1-sol".into(),
            profiled: false,
            burn: None,
            target: Value::Null,
            reasoning_error: None,
            no_review: false,
            policy_ban: None,
            fit: 0.0,
            failure_penalty: 0.0,
            quality: 0.0,
            confidence: 0.0,
            allowed: true,
            task_cost: None,
            quota_note: u.note.clone(),
            active_agents: 0,
            quota_provenance: QuotaProvenance {
                basis: u.estimate_basis,
                detail: u.estimate_detail.clone(),
                sample_sessions: u.sample_sessions,
                stale_minutes: u.stale_minutes,
                load_unknown: u.load_unknown,
                windows: u.estimate_windows.clone(),
                balances: u.estimate_balances.clone(),
            },
        };
        let uv = quota_estimate_json(&uc);
        assert_eq!(uv["basis"], "unknown");
        assert!(uv["sampleSessions"].is_null());
        assert!(uv["staleMinutes"].is_null());
        assert_eq!(uv["loadUnknown"], true);
        assert!(uv["windows"].as_array().unwrap().is_empty());
    }
}

#[cfg(test)]
mod probe_tests {
    #[tokio::test]
    async fn probe_reads_mock_answers() {
        let dir = std::env::temp_dir().join(format!("agentdeck-probe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mock = dir.join("jev.json");
        std::fs::write(
            &mock,
            r#"{"answers":{"size":{"score":0,"confidence":0.9},"difficulty":{"score":0,"confidence":0.8}},"usage":{"cost":0.001}}"#,
        )
        .unwrap();
        let blocks = agentdeck_core::judge::BlockStore::new(dir.join("blocks"));
        let language_models =
            agentdeck_core::judge::LanguageModelCache::new(dir.join("language-models"));
        // SAFETY: test-only env override read by jev_fetch.
        unsafe { std::env::set_var("AGENTDECK_ROUTE_MOCK_JEV", &mock) };
        let r = super::probe_decision_with_bases(
            None,
            "m",
            Some("k"),
            &blocks,
            &language_models,
            &super::JevBases::default(),
        )
        .await;
        unsafe { std::env::remove_var("AGENTDECK_ROUTE_MOCK_JEV") };
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(r.unwrap(), (0.0, 0.0, Some(0.001)));
    }

    #[tokio::test]
    async fn probe_decision_records_429_block_and_honours_active_block() {
        use std::io::{BufRead, BufReader, Read, Write};
        let dir = std::env::temp_dir().join(format!("agentdeck-probe-429-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hits_bg = hits.clone();
        let srv = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            hits_bg.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut reader = BufReader::new(&mut stream);
            let mut first = String::new();
            reader.read_line(&mut first).unwrap();
            let mut size = 0usize;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some((n, v)) = line.split_once(':')
                    && n.eq_ignore_ascii_case("content-length")
                {
                    size = v.trim().parse().unwrap();
                }
            }
            let mut buf = vec![0; size];
            reader.read_exact(&mut buf).unwrap();
            let body = r#"{"error":"rate limited"}"#;
            write!(
                stream,
                "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 60\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });

        let blocks = agentdeck_core::judge::BlockStore::new(dir.join("blocks"));
        let language_models =
            agentdeck_core::judge::LanguageModelCache::new(dir.join("language-models"));
        let bases = super::JevBases {
            openrouter: base,
            vercel: "http://127.0.0.1:1".into(),
        };

        // 1. First probe hits 429 -> records block and returns blocked-until error.
        let err1 = super::probe_decision_with_bases(
            None,
            "probe-model",
            Some("k"),
            &blocks,
            &language_models,
            &bases,
        )
        .await
        .unwrap_err();
        assert!(
            err1.to_string().contains("blocked until"),
            "expected blocked-until error: {err1}"
        );
        let ep = agentdeck_core::judge::JudgeEndpoint::new(
            agentdeck_core::settings::PROVIDER_OPENROUTER,
            "probe-model",
        );
        assert!(
            blocks.remaining(&ep).is_some(),
            "probe 429 must persist cross-process block"
        );

        // 2. Second probe sees active block -> 0 HTTP requests, returns blocked-until error.
        let err2 = super::probe_decision_with_bases(
            None,
            "probe-model",
            Some("k"),
            &blocks,
            &language_models,
            &bases,
        )
        .await
        .unwrap_err();
        assert!(
            err2.to_string().contains("blocked until"),
            "expected blocked-until error on cached block: {err2}"
        );
        srv.join().unwrap();
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "second probe must not hit network while 429 block is active"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn router_and_core_block_stores_resolve_same_dir() {
        let rt = crate::runtime_dir().unwrap();
        assert_eq!(
            agentdeck_core::judge::BlockStore::default_runtime()
                .unwrap()
                .dir(),
            agentdeck_core::judge::BlockStore::for_runtime_dir(&rt).dir()
        );
    }
}

#[cfg(test)]
mod live_quota_shape_tests {
    use super::*;

    const NOW: f64 = 1_794_569_047.0;

    // Router accounts and usage state as generated on 2026-11-13, when
    // `agy-main` (account name `agy`) had its gemini-5h window exhausted.
    fn cfg() -> RouterConfig {
        crate::policy::parse_router_config(
            serde_json::json!({"pool": [], "accounts": {
                "agy": {"instances": ["antigravity"], "usageProvider": "antigravity"},
                "claude_second": {"instances": ["claude_second"], "usageProvider": "claude", "usageId": "claude-second"},
                "codex": {"instances": ["codex"], "usageProvider": "codex", "usageId": "codex-main"},
                "openrouter": {"instances": ["opencode"], "usageProvider": "openrouter"},
                "opencode-go": {"instances": ["opencode"], "usageProvider": "opencode-go", "modelPrefix": "opencode-go/"}
            }}),
            "test",
        )
        .unwrap()
    }

    fn usage(gemini_5h_resets: f64) -> Vec<UsageAccountFull> {
        let state = serde_json::json!({"accounts": [
            {"id": "agy-main", "provider": "antigravity", "status": "ok", "plan": "Google AI Pro",
             "updated_at": NOW - 40.0, "balances": [], "windows": [
                {"key": "gemini-5h", "label": "Gemini · 5h", "used_percent": 100.0, "remaining_percent": 0.0,
                 "duration_mins": 300, "resets_at": gemini_5h_resets, "scope": null},
                {"key": "gemini-weekly", "used_percent": 34.8, "remaining_percent": 65.2,
                 "duration_mins": 10080, "resets_at": NOW + 188_000.0, "scope": null},
                {"key": "3p-5h", "used_percent": 8.2, "remaining_percent": 91.8,
                 "duration_mins": 300, "resets_at": NOW + 14_000.0, "scope": "Сторонние модели"},
                {"key": "3p-weekly", "used_percent": 4.6, "remaining_percent": 95.4,
                 "duration_mins": 10080, "resets_at": NOW + 556_000.0, "scope": "Сторонние модели"},
                {"key": "gemini-argon-wtu-weekly", "used_percent": 44.2, "remaining_percent": 55.8,
                 "duration_mins": 10080, "resets_at": NOW + 351_000.0, "scope": "Gemini Argon"}]},
            {"id": "claude-second", "provider": "claude", "status": "ok", "plan": "Pro",
             "updated_at": NOW - 40.0, "balances": [], "windows": [
                {"key": "session", "used_percent": 0.0, "remaining_percent": 100.0, "duration_mins": 300, "resets_at": null},
                {"key": "weekly_all", "used_percent": 100.0, "remaining_percent": 0.0, "duration_mins": 10080, "resets_at": NOW + 124_000.0},
                {"key": "weekly_scoped:fable", "used_percent": 0.0, "remaining_percent": 100.0, "duration_mins": 10080,
                 "resets_at": NOW + 124_000.0, "scope": "Fable"}]}
        ]});
        let path = std::env::temp_dir().join(format!(
            "agentdeck-live-quota-{}-{}-{gemini_5h_resets}.json",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::write(&path, state.to_string()).unwrap();
        let parsed = parse_usage_state(&path).unwrap();
        std::fs::remove_file(&path).ok();
        parsed
    }

    fn allowed(usage: &[UsageAccountFull], provider: &str, model: &str) -> [bool; 4] {
        let load = AgentLoad {
            known: true,
            agents: vec![],
            note: None,
        };
        quota(
            &cfg(),
            &Measured::default(),
            usage,
            &load,
            provider,
            model,
            NOW,
        )
        .allowed
    }

    #[test]
    fn exhausted_antigravity_gemini_window_blocks_only_its_models() {
        let u = usage(NOW + 2_480.0);
        assert_eq!(
            allowed(&u, "antigravity", "gemini-4-hydrogen-high"),
            [false; 4]
        );
        assert_eq!(
            allowed(&u, "antigravity", "gemini-4-hydrogen-xhigh"),
            [false; 4]
        );
        assert_eq!(
            allowed(&u, "antigravity", "gemini-3.8-flash-high"),
            [false; 4]
        );
        // Argon and third-party models have their own windows.
        assert!(allowed(&u, "antigravity", "gemini-4-argon-low")[1]);
        assert!(allowed(&u, "antigravity", "claude-opus-5-5-high")[1]);
        let note = quota(
            &cfg(),
            &Measured::default(),
            &u,
            &AgentLoad {
                known: true,
                agents: vec![],
                note: None,
            },
            "antigravity",
            "gemini-4-hydrogen-high",
            NOW,
        )
        .note;
        assert!(!note.contains("no ai-usage account"), "{note}");
    }

    #[test]
    fn two_antigravity_accounts_use_their_own_quota() {
        use agentdeck_core::{AddSpec, Harness, Registry, UsageEntry, UsageKind};
        let mut reg = Registry::empty();
        for (inst, uid) in [
            ("antigravity", "agy-main"),
            ("antigravity_2", "antigravity-2"),
        ] {
            reg.add(AddSpec {
                id: inst.into(),
                harness: Harness::Agy,
                display_name: None,
                home: "~".into(),
                binary: None,
                no_t3: false,
            })
            .unwrap();
            reg.add_usage(UsageEntry {
                id: uid.into(),
                kind: UsageKind::Antigravity,
                instance: Some(inst.into()),
                display: String::new(),
                api_key_env: None,
                db_file: None,
                extra: Default::default(),
            })
            .unwrap();
        }
        let cfg = crate::policy::parse_router_config(
            agentdeck_core::router::router_json_value(&reg).unwrap(),
            "test",
        )
        .unwrap();
        let window = |remaining: f64| {
            serde_json::json!([{"key": "gemini-weekly", "used_percent": 100.0 - remaining,
                "remaining_percent": remaining, "duration_mins": 10080,
                "resets_at": NOW + 300_000.0, "scope": null}])
        };
        // The exhausted account is listed first: a provider-only match would
        // charge both instances to it.
        let state = serde_json::json!({"accounts": [
            {"id": "agy-main", "provider": "antigravity", "status": "ok", "plan": "Google AI Pro",
             "updated_at": NOW - 40.0, "balances": [], "windows": window(0.0)},
            {"id": "antigravity-2", "provider": "antigravity", "status": "ok", "plan": "Google AI Pro",
             "updated_at": NOW - 40.0, "balances": [], "windows": window(100.0)}
        ]});
        let path =
            std::env::temp_dir().join(format!("agentdeck-two-agy-{}.json", std::process::id()));
        std::fs::write(&path, state.to_string()).unwrap();
        let u = parse_usage_state(&path).unwrap();
        std::fs::remove_file(&path).ok();
        let load = AgentLoad {
            known: true,
            agents: vec![],
            note: None,
        };
        let q = |inst: &str| {
            quota(
                &cfg,
                &Measured::default(),
                &u,
                &load,
                inst,
                "gemini-4-hydrogen-high",
                NOW,
            )
            .allowed
        };
        assert_eq!(q("antigravity"), [false; 4]);
        assert!(q("antigravity_2")[1], "{:?}", q("antigravity_2"));
    }

    #[test]
    fn passed_reset_makes_the_window_usable_again() {
        let u = usage(NOW - 60.0);
        assert!(allowed(&u, "antigravity", "gemini-4-hydrogen-high")[1]);
    }

    #[test]
    fn claude_weekly_all_exhaustion_blocks_every_model_on_that_account() {
        let u = usage(NOW + 2_480.0);
        assert_eq!(allowed(&u, "claude_second", "claude-opus-5-5"), [false; 4]);
        assert_eq!(allowed(&u, "claude_second", "claude-fable-5-2"), [false; 4]);
    }

    #[test]
    fn opencode_go_monthly_43200_uses_weekly_factor_not_nan_short_factor() {
        let load = AgentLoad {
            known: true,
            agents: vec![],
            note: None,
        };
        let state_with_43200 = serde_json::json!({"accounts": [{
            "id": "opencode-go", "provider": "opencode-go", "status": "ok", "plan": "Go",
            "updated_at": NOW - 40.0, "balances": [], "windows": [
                {"key": "rolling", "label": "5 часов", "used_percent": 2.0, "remaining_percent": 98.0, "duration_mins": 300, "resets_at": NOW + 3_600.0},
                {"key": "weekly", "label": "Неделя", "used_percent": 10.0, "remaining_percent": 90.0, "duration_mins": 10080, "resets_at": NOW + 200_000.0},
                {"key": "monthly", "label": "Месяц", "used_percent": 95.0, "remaining_percent": 5.0, "duration_mins": 43200, "resets_at": NOW + 2_000_000.0}
            ]
        }]});
        let mut state_with_null = state_with_43200.clone();
        state_with_null["accounts"][0]["windows"][2]["duration_mins"] = Value::Null;

        let parse = |val: Value| {
            let p = std::env::temp_dir().join(format!(
                "agentdeck-router-b1-{}-{}.json",
                std::process::id(),
                val["accounts"][0]["windows"][2]["duration_mins"]
            ));
            std::fs::write(&p, val.to_string()).unwrap();
            let u = parse_usage_state(&p).unwrap();
            std::fs::remove_file(&p).ok();
            u
        };

        let u_43200 = parse(state_with_43200);
        let u_null = parse(state_with_null);
        let q_43200 = quota(
            &cfg(),
            &Measured::default(),
            &u_43200,
            &load,
            "opencode",
            "opencode-go/glm-5",
            NOW,
        );
        let q_null = quota(
            &cfg(),
            &Measured::default(),
            &u_null,
            &load,
            "opencode",
            "opencode-go/glm-5",
            NOW,
        );
        // With 43200 mins (what old opencode_go.rs and T3 mapper both publish),
        // monthly gets factor 1.0; null/NaN would wrongly apply short-window factor 5.0.
        assert_eq!(q_43200.allowed, [true, true, true, true]);
        assert_eq!(q_null.allowed, [true, true, true, false]);
        assert!(q_43200.cost.unwrap()[3] < q_null.cost.unwrap()[3]);
    }

    #[test]
    fn antigravity_coarse_windows_reject_near_empty_quota() {
        let load = AgentLoad {
            known: true,
            agents: vec![],
            note: None,
        };
        let state = serde_json::json!({"accounts": [{
            "id": "agy-main", "provider": "antigravity", "status": "ok", "plan": "Google AI Pro",
            "updated_at": NOW - 40.0, "balances": [], "windows": [
                {"key": "claude-quota", "label": "Claude Quota", "used_percent": 20.0, "remaining_percent": 80.0, "duration_mins": null, "resets_at": null, "scope": null},
                {"key": "gemini-quota", "label": "Gemini Quota", "used_percent": 99.5, "remaining_percent": 0.5, "duration_mins": null, "resets_at": null, "scope": null},
                {"key": "gpt-quota", "label": "GPT Quota", "used_percent": 15.0, "remaining_percent": 85.0, "duration_mins": null, "resets_at": null, "scope": null}
            ]
        }]});
        let p = std::env::temp_dir().join(format!(
            "agentdeck-router-agy-coarse-{}.json",
            std::process::id()
        ));
        std::fs::write(&p, state.to_string()).unwrap();
        let u = parse_usage_state(&p).unwrap();
        std::fs::remove_file(&p).ok();

        // Missing durations use short-window admission; a fractional remainder cannot fund a launch.
        let q_gemini = quota(
            &cfg(),
            &Measured::default(),
            &u,
            &load,
            "antigravity",
            "gemini-4-hydrogen-high",
            NOW,
        );
        let q_3p = quota(
            &cfg(),
            &Measured::default(),
            &u,
            &load,
            "antigravity",
            "claude-opus-5-5-high",
            NOW,
        );
        let q_argon = quota(
            &cfg(),
            &Measured::default(),
            &u,
            &load,
            "antigravity",
            "gemini-4-argon-low",
            NOW,
        );
        assert_eq!(q_gemini.allowed, [false; 4]);
        assert_eq!(q_3p.allowed, [true, true, true, true]);
        assert_eq!(q_argon.allowed, [true, true, false, false]);
    }
}
