//! Port of `route.mjs`: CLI parsing, Jev judging, quota gating and ranking.

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::load::{AgentLoad, read_active_agents};
use crate::policy::{
    Candidate, MIN_FIT, NO_REVIEW_BAN, QuotaProvenance, QuotaWindow, QuotaWindowEstimate, Ranked,
    RouterConfig, UsageAccount, UsageSnapshot, budget_allowed, budget_cost_full, enabled_favorites,
    family_of, hint_of, is_review_task, load_router_json, max_burn, owns_usage, quota_windows,
    rank_candidates, reserve_active_quota, router_config, routing_weights, valid_answer,
    validate_target,
};
use crate::reasoning::{read_catalog, reasoning_target, save_catalog};
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
const CAPABILITY: [&str; 4] = [
    "Cannot perform the requested task or input modality.",
    "Significant capability gaps; unreliable for the required task.",
    "Adequate capabilities for a good result with normal checks.",
    "Specific relevant strengths should produce a substantially better result on THIS task. A merely stronger model with no practical advantage does not qualify.",
];

const USAGE: &str = "usage: agentdeck route [--source <provider instance>] [--validate-target <provider instance> <model>] \"<task brief>\" | - (brief on stdin) | --check | --catalog (orchestrator_capabilities structuredContent on stdin) | hook --harness <name> | claude-shell [args...] | t3-mcp-call TOOL FILE | workflow CMD ... | outcomes record|list|evaluate ...";

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
            runtime: runtime_dir(),
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

const COST_VERSION: u64 = 7;
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

fn codex_session_files(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = vec![];
    let Ok(l1) = std::fs::read_dir(root) else {
        return out;
    };
    for a in l1.flatten() {
        let Ok(l2) = std::fs::read_dir(a.path()) else {
            continue;
        };
        for b in l2.flatten() {
            let Ok(l3) = std::fs::read_dir(b.path()) else {
                continue;
            };
            for c in l3.flatten() {
                if c.path().extension().and_then(|e| e.to_str()) == Some("jsonl")
                    && c.file_type().is_ok_and(|t| t.is_file())
                {
                    out.push(c.path());
                }
            }
        }
    }
    out
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

struct QuotaResult {
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
}

fn unknown_quota(err: &str, load_unknown: bool, stale_minutes: Option<i64>) -> QuotaResult {
    QuotaResult {
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
    }
}

/// Rounded minutes since the quota snapshot; None when never reported.
fn stale_minutes(updated_at: f64, now: f64) -> Option<i64> {
    updated_at
        .is_finite()
        .then(|| ((now - updated_at) / 60.0).round() as i64)
}

fn model_costs(
    _cfg: &RouterConfig,
    measured: &Measured,
    acc: &UsageAccountFull,
    provider: &str,
    model: &str,
) -> ([f64; 4], f64, String, Option<usize>) {
    let normalized = if ["opus", "sonnet", "haiku", "fable"].contains(&model) {
        format!("claude-{model}")
    } else {
        model.to_string()
    };
    let fam = family_of(&normalized);
    let burn = fam.as_ref().map(|f| f.burn).unwrap_or_else(max_burn);
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
            format!("{} estimate × burn {burn}", acc.plan),
            None,
        ),
        (None, None) => (
            star.map(|x| x * burn),
            format!("unknown plan \"{}\", conservative estimate", acc.plan),
            None,
        ),
    };
    (weekly, burn, basis, sample)
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
    let (weekly, burn, basis, sample) = model_costs(cfg, measured, &acc, provider, model);
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
        &|p, m| {
            let (w, b, _, _) = model_costs(cfg, measured, &acc, p, m.unwrap_or(""));
            (w, b)
        },
    );
    let windows = quota_windows(provider, model, &reserved.windows);
    let allowed = budget_allowed(
        &windows,
        &reserved.balances,
        &weekly,
        burn,
        reserve_percent,
        now,
    );
    let cost = budget_cost_full(
        &windows,
        &reserved.balances,
        &weekly,
        burn,
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
        weekly: if windows.is_empty() {
            None
        } else {
            Some(weekly)
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

// --- source detection --------------------------------------------------------

fn proc_stat_ppid(pid: u32) -> Option<u32> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after = text.split(") ").nth(1)?;
    after.split(' ').nth(1)?.parse().ok()
}

fn detect_source(settings: &Value) -> Option<String> {
    let mut pid = proc_stat_ppid(std::process::id())?;
    for _ in 0..12 {
        if pid <= 1 {
            break;
        }
        let cmd = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
        let parts: Vec<&str> = cmd
            .split(|b| *b == 0)
            .map(|b| std::str::from_utf8(b).unwrap_or_default())
            .collect();
        let names: Vec<String> = parts
            .iter()
            .take(2)
            .map(|c| c.rsplit('/').next().unwrap_or_default().to_string())
            .collect();
        if names.iter().any(|n| n == "codex") {
            let env = std::fs::read(format!("/proc/{pid}/environ")).unwrap_or_default();
            let codex_home = env.split(|b| *b == 0).find_map(|e| {
                std::str::from_utf8(e)
                    .ok()
                    .and_then(|s| s.strip_prefix("CODEX_HOME=").map(str::to_string))
            });
            let found = settings
                .get("providerInstances")
                .and_then(Value::as_object)
                .map(|insts| {
                    insts
                        .iter()
                        .find(|(_, v)| {
                            codex_home.is_some()
                                && v.get("driver").and_then(Value::as_str) == Some("codex")
                                && v.get("config")
                                    .and_then(|c| c.get("shadowHomePath"))
                                    .and_then(Value::as_str)
                                    == codex_home.as_deref()
                        })
                        .map(|(k, _)| k.clone())
                })
                .unwrap_or(None);
            return Some(found.unwrap_or_else(|| "codex".into()));
        }
        if names.iter().any(|n| n == "claude") {
            let claude_dir = std::env::var("CLAUDE_CONFIG_DIR").ok();
            let found = settings
                .get("providerInstances")
                .and_then(Value::as_object)
                .and_then(|insts| {
                    insts
                        .iter()
                        .find(|(_, v)| {
                            v.get("driver").and_then(Value::as_str) == Some("claudeAgent")
                                && claude_dir.is_some()
                                && v.get("config")
                                    .and_then(|c| c.get("homePath"))
                                    .and_then(Value::as_str)
                                    == claude_dir.as_deref()
                        })
                        .map(|(k, _)| k.clone())
                });
            return Some(found.unwrap_or_else(|| {
                if claude_dir.is_some_and(|d| d.contains(".claude-openrouter")) {
                    "claudeAgent_openrouter".into()
                } else {
                    "claudeAgent".into()
                }
            }));
        }
        if names.iter().any(|n| n == "opencode") {
            return Some("opencode".into());
        }
        if names.iter().any(|n| n == "agy")
            || parts
                .first()
                .is_some_and(|c| c.contains("/.t3/tools/antigravity-acp/"))
        {
            return Some("antigravity".into());
        }
        pid = proc_stat_ppid(pid)?;
    }
    None
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
            anyhow::anyhow!(
                "decision key not found: set {env} or write {}",
                key_file.display()
            )
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

async fn post_json(url: &str, key: &str, body: String) -> anyhow::Result<Value> {
    let res = reqwest::Client::new()
        .post(url)
        .timeout(std::time::Duration::from_secs(30))
        .header("Authorization", format!("Bearer {key}"))
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("Jev request failed: {e}"))?;
    if !res.status().is_success() {
        let status = res.status().as_u16();
        let text = res.text().await.unwrap_or_default();
        let snippet: String = text.chars().take(300).collect();
        return Err(anyhow::anyhow!("Jev HTTP {status}: {snippet}"));
    }
    res.json()
        .await
        .map_err(|e| anyhow::anyhow!("Jev returned invalid JSON: {e}"))
}

async fn jev_fetch(
    provider: Option<&str>,
    request: &Value,
    request_body: &str,
    key: Option<&str>,
) -> anyhow::Result<(Value, Option<f64>)> {
    if let Ok(mock) = std::env::var("AGENTDECK_ROUTE_MOCK_JEV") {
        let text = std::fs::read_to_string(mock)?;
        let v: Value = serde_json::from_str(&text)?;
        return Ok((
            v.get("answers").cloned().unwrap_or(Value::Null),
            v.get("usage")
                .and_then(|u| u.get("cost"))
                .and_then(Value::as_f64),
        ));
    }
    if provider == Some(agentdeck_core::settings::PROVIDER_VERCEL) {
        let key = match key {
            Some(k) => k.to_string(),
            None => decision_key("AI_GATEWAY_API_KEY", "vercel.key")?,
        };
        match post_json(
            "https://ai-gateway.vercel.sh/v1/decisions",
            &key,
            decisions_body(request).to_string(),
        )
        .await
        {
            Ok(v) => {
                let cost = v
                    .pointer("/provider_metadata/gateway/cost")
                    .and_then(|c| c.as_f64().or_else(|| c.as_str()?.parse().ok()));
                return Ok((decisions_answers(&v), cost));
            }
            Err(e) if e.to_string().contains("is a language model") => {}
            Err(e) => return Err(e),
        }
        let v = post_json(
            "https://ai-gateway.vercel.sh/v1/chat/completions",
            &key,
            gateway_body(request).to_string(),
        )
        .await?;
        // shortcut: the chat completion's cost is not read, so Jev cost reports as unknown.
        return Ok((gateway_answers(&v)?, None));
    }
    let key = match key {
        Some(k) => k.to_string(),
        None => decision_key("JEV_OPENROUTER_API_KEY", "openrouter.key")?,
    };
    let v = post_json(
        "https://openrouter.ai/api/alpha/decisions",
        &key,
        request_body.to_string(),
    )
    .await?;
    Ok((
        v.get("answers").cloned().unwrap_or(Value::Null),
        v.get("usage")
            .and_then(|u| u.get("cost"))
            .and_then(Value::as_f64),
    ))
}

/// One live decision through the configured provider (explicit only).
///
/// Builds a one-question Jev decision over caller-supplied `state` (task +
/// evidence, already size-checked) and returns the raw `answers`, the reported
/// judge cost (`None` when the provider reports none), and the judge identity
/// (`provider` label, `model`). Reuses the routing decision provider, model and
/// keys; adds no model settings. Honours `AGENTDECK_ROUTE_MOCK_JEV` like routing.
pub async fn judge_evaluation(
    state: Value,
    questions: Value,
) -> anyhow::Result<(Value, Option<f64>, String, String)> {
    let env = Env::from_env()?;
    let cfg = load_router_config(&env)?;
    let provider = cfg
        .decision_provider
        .clone()
        .unwrap_or_else(|| agentdeck_core::settings::PROVIDER_OPENROUTER.to_string());
    let model = cfg
        .decision_model
        .clone()
        .unwrap_or_else(|| agentdeck_core::settings::DEFAULT_DECISION_MODEL.to_string());
    if provider == agentdeck_core::settings::PROVIDER_VERCEL && cfg.decision_model.is_none() {
        return Err(anyhow::anyhow!(
            "decision provider vercel needs [router] decision_model"
        ));
    }
    let request = serde_json::json!({"model": &model, "state": state, "questions": questions});
    let body = stringify_like_node(&request);
    let (answers, cost) =
        jev_fetch(cfg.decision_provider.as_deref(), &request, &body, None).await?;
    Ok((answers, cost, provider, model))
}

/// One live decision on a fixed small task, bypassing the cache: checks that
/// the provider, model and key work. `key` overrides the stored key.
/// Returns (size score, difficulty score, cost in USD when reported).
pub async fn probe_decision(
    provider: Option<&str>,
    model: &str,
    key: Option<&str>,
) -> anyhow::Result<(f64, f64, Option<f64>)> {
    let request = serde_json::json!({
        "model": model,
        "state": {"task": "Rename one local variable in a 20-line Python script.", "models": {}},
        "questions": {
            "size": {"type": "score", "instructions": "How much work will the actual delegated task take?", "criteria": SIZES},
            "difficulty": {"type": "score", "instructions": "How difficult is the actual task?", "criteria": DIFFICULTY},
        },
    });
    let body = stringify_like_node(&request);
    let (answers, cost) = jev_fetch(provider, &request, &body, key).await?;
    let score = |k: &str| {
        answers
            .get(k)
            .filter(|a| valid_answer(a, false))
            .and_then(|a| a.get("score"))
            .and_then(Value::as_f64)
            .ok_or_else(|| {
                anyhow::anyhow!("the model answered, but {k} is missing or out of range: {answers}")
            })
    };
    Ok((score("size")?, score("difficulty")?, cost))
}

// --- CLI ---------------------------------------------------------------------

pub struct ValidatedTarget {
    pub provider: String,
    pub model: String,
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
        Some("workflow") => return Ok(crate::workflow::run(&argv[1..])),
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
        } else {
            eprintln!("{USAGE}");
            return Ok(2);
        }
    }
    let settings =
        read_settings(&env.home).map_err(|e| anyhow::anyhow!("cannot read T3 settings: {e}"))?;
    if source.is_none() {
        source = detect_source(&settings);
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
    let usage_path = cfg
        .usage_state_path
        .clone()
        .map(|p| {
            p.strip_prefix("~/")
                .map(|rest| env.home.join(rest))
                .unwrap_or_else(|| std::path::PathBuf::from(p))
        })
        .unwrap_or_else(|| env.home.join(".local/state/ai-usage/state.json"));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let mut usage = parse_usage_state(&usage_path)
        .map_err(|e| anyhow::anyhow!("cannot read usage state {}: {e}", usage_path.display()))?;
    let mut load = read_active_agents(&env.home.join(".t3/userdata/statev2.sqlite"));
    let measured = codex_costs(&env);

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
        &settings,
    );
    let opencode_models: Vec<String> = if configured
        .iter()
        .any(|f| f.get("provider").and_then(Value::as_str) == Some("opencode"))
    {
        let catalog = read_catalog(&env.state_home).map_err(|e| anyhow::anyhow!("{e}"))?;
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
        return Ok(if unprofiled.is_empty() { 0 } else { 1 });
    }

    let mut task = if args.len() == 1 && args[0] == "-" {
        let mut s = String::new();
        use std::io::Read;
        std::io::stdin().read_to_string(&mut s)?;
        s
    } else {
        args.join(" ")
    }
    .trim()
    .to_string();
    let brief_hint = crate::policy::roster_brief_hint();
    let known_hints: Vec<String> = {
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
        // Dedupe (first wins), drop empties, longest first.
        let mut seen = std::collections::HashSet::new();
        let mut out: Vec<String> = v
            .into_iter()
            .filter(|s| !s.is_empty() && seen.insert(s.clone()))
            .collect();
        out.sort_by_key(|s| std::cmp::Reverse(s.len()));
        out
    };
    if let Some(h) = known_hints
        .iter()
        .find(|h| task.ends_with(h.as_str()) && task.len() > h.len())
    {
        task = task[..task.len() - h.len()].trim().to_string();
    }
    if task.is_empty() {
        eprintln!("{USAGE}");
        return Ok(2);
    }

    let criteria: Vec<(String, String)> = favorites
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
        .collect();
    // Build the Jev request with Node's exact key order.
    let mut questions = serde_json::Map::new();
    questions.insert("size".into(), serde_json::json!({"type": "score", "instructions": "How much work will the actual delegated task take? Scope is not difficulty. Do not assume a large task from a long prompt, repository name or safeguards.", "criteria": SIZES}));
    questions.insert("difficulty".into(), serde_json::json!({"type": "score", "instructions": "How difficult is the actual task, independently of its volume? Many files, a large repository, long inputs or a request to use Sonnet/Opus do not by themselves make work difficult.", "criteria": DIFFICULTY}));
    for (k, _) in &criteria {
        questions.insert(
            k.clone(),
            serde_json::json!({"type": "score",
                "instructions": format!("Using the capability profile in context.models.{k}, rate expected result quality for context.task independently of other models. Distinguish adequate capability from a substantial practical quality advantage for this task. Routine file search, summaries and bounded edits do not require a frontier leader; a missing specialty or short track record alone is not a capability gap. All candidates have file and shell tools. Ignore the native tool's model field and prices."),
                "criteria": CAPABILITY}),
        );
    }
    let mut state = serde_json::Map::new();
    state.insert("task".into(), Value::from(task.clone()));
    let mut models = serde_json::Map::new();
    for (k, c) in &criteria {
        models.insert(k.clone(), Value::from(c.clone()));
    }
    state.insert("models".into(), Value::Object(models));
    let mut request = serde_json::Map::new();
    if cfg.decision_provider.as_deref() == Some(agentdeck_core::settings::PROVIDER_VERCEL)
        && cfg.decision_model.is_none()
    {
        return Err(anyhow::anyhow!(
            "decision provider vercel needs [router] decision_model"
        ));
    }
    request.insert(
        "model".into(),
        Value::from(
            cfg.decision_model
                .as_deref()
                .unwrap_or(agentdeck_core::settings::DEFAULT_DECISION_MODEL),
        ),
    );
    request.insert("state".into(), Value::Object(state));
    request.insert("questions".into(), Value::Object(questions));
    let request_value = Value::Object(request);
    let body_json = stringify_like_node(&request_value);
    let hash_input = stringify_like_node(&serde_json::json!([
        source.as_deref(),
        cfg.version,
        request_value
    ]));

    let complete = |a: &Value| {
        let size_ok = a.get("size").is_some_and(|s| valid_answer(s, false));
        let diff_ok = a.get("difficulty").is_some_and(|d| valid_answer(d, false));
        size_ok
            && diff_ok
            && criteria
                .iter()
                .all(|(k, _)| a.get(k).is_some_and(|x| valid_answer(x, true)))
    };
    let cache_dir = env.runtime.join("model-route-jev-cache");
    let mut hasher = Sha256::new();
    hasher.update(hash_input.as_bytes());
    let cache_file = cache_dir.join(format!("{:x}.json", hasher.finalize()));
    let fresh = |t: f64| {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as f64)
            .unwrap_or(0.0);
        now_ms - t >= 0.0 && now_ms - t < 10.0 * 60e3
    };
    let mut cached: Option<Value> = None;
    if let Ok(text) = std::fs::read_to_string(&cache_file)
        && let Ok(entry) = serde_json::from_str::<Value>(&text)
        && entry.get("t").and_then(Value::as_f64).is_some_and(fresh)
        && complete(entry.get("answers").unwrap_or(&Value::Null))
    {
        cached = entry.get("answers").cloned();
    }
    let jev_hit = cached.is_some();
    // A hit reuses the cached judgment; like the Node router it reports zero
    // additional Jev cost for the reused answer.
    let (answers, cost) = match cached {
        Some(a) => (a, Some(0.0)),
        None => {
            let (answers, cost) = jev_fetch(
                cfg.decision_provider.as_deref(),
                &request_value,
                &body_json,
                None,
            )
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
            if !complete(&answers) {
                return Err(anyhow::anyhow!(
                    "Jev returned incomplete or out-of-range routing answers"
                ));
            }
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as f64)
                .unwrap_or(0.0);
            let _ = (|| {
                std::fs::create_dir_all(&cache_dir)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(
                        &cache_dir,
                        std::fs::Permissions::from_mode(0o700),
                    );
                }
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
                let tmp = cache_dir.join(format!(
                    "{}.{}.tmp",
                    cache_file
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("jev-cache"),
                    std::process::id()
                ));
                std::fs::write(
                    &tmp,
                    serde_json::to_string(&serde_json::json!({"t": now_ms, "answers": answers}))?,
                )?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
                }
                std::fs::rename(&tmp, &cache_file)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(
                        &cache_file,
                        std::fs::Permissions::from_mode(0o600),
                    );
                }
                Ok::<(), anyhow::Error>(())
            })();
            (answers, cost)
        }
    };
    let jev_cache = if jev_hit { "hit" } else { "miss" };
    // Re-read quota/load snapshots after Jev before ranking.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(now);
    usage = parse_usage_state(&usage_path)
        .map_err(|e| anyhow::anyhow!("cannot read usage state {}: {e}", usage_path.display()))?;
    load = read_active_agents(&env.home.join(".t3/userdata/statev2.sqlite"));
    let current_cfg = load_router_config(&env)?;
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
    let catalog = read_catalog(&env.state_home).map_err(|e| anyhow::anyhow!("{e}"))?;

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
    let mut candidates: Vec<Candidate> = vec![];
    for (provider, model, profile) in &favorites {
        let a = answers
            .get(key_of(provider, model).as_str())
            .cloned()
            .unwrap_or(Value::Null);
        let (target, reasoning_error) =
            match reasoning_target(&catalog, provider, model, difficulty) {
                Ok(t) => (t, None),
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
        } else if no_review && is_review_task(&task) {
            Some(NO_REVIEW_BAN.to_string())
        } else {
            None
        };
        if let Some(limit) = current_cfg.instance_limits.get(provider) {
            policy_ban = capacity_error
                .clone()
                .or_else(|| {
                    crate::admission::reason(
                        &load,
                        pending.get(provider).copied().unwrap_or_default(),
                        provider,
                        *limit,
                    )
                })
                .or(policy_ban);
        }
        let probs = a.get("probabilities");
        let fit = if profile.is_some() {
            probs
                .and_then(|p| p.get("2"))
                .and_then(Value::as_f64)
                .unwrap_or(0.0)
                + probs
                    .and_then(|p| p.get("3"))
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0)
        } else {
            0.0
        };
        let q = quota(&cfg, &measured, &usage, &load, provider, model, now);
        let spend_zero = fam.as_ref().is_some_and(|f| f.spend_to_zero);
        candidates.push(Candidate {
            provider: provider.clone(),
            model: model.clone(),
            profiled: profile.is_some(),
            burn: fam.as_ref().map(|f| f.burn),
            target,
            reasoning_error,
            no_review,
            policy_ban,
            fit,
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
        &cfg,
        candidates,
        difficulty,
        source.as_deref(),
        (shared_w, paid_w, cheap_w),
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
        if let Some(limit) = current_cfg.instance_limits.get(&c.provider) {
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
    if let Some(b) = best.and_then(|x| hint_of(&x.candidate.model)) {
        out.insert("briefHint".into(), Value::from(b));
    }
    out.insert(
        "source".into(),
        source.clone().map(Value::String).unwrap_or(Value::Null),
    );
    out.insert(
        "sourceAccount".into(),
        source
            .as_deref()
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
    if let Some(req) = &requested {
        if let Some(h) = hint_of(&req.model) {
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
        } else if current_cfg.user_rules.is_no_review(&req.model) && is_review_task(&task) {
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
    if let Some(n) = load.note {
        load_json.insert("note".into(), Value::from(n));
    }
    out.insert("activeLoad".into(), Value::Object(load_json));
    if let Some(c) = cost {
        out.insert("cost".into(), js_num(c));
    }
    out.insert("jevCache".into(), Value::from(jev_cache));
    println!("{}", serde_json::to_string_pretty(&Value::Object(out))?);
    Ok(0)
}

/// Serialize exactly like `JSON.stringify` for the Jev body and the cache-key
/// hash: same key order and separators, plus `\u2028`/`\u2029` escaping which
/// serde omits but V8 emits.
fn stringify_like_node(v: &Value) -> String {
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

    #[test]
    fn measured_basis_reports_sample_count() {
        let cfg = test_cfg();
        // "Pro 5x" maps via codexPlanTypes to raw plan "prolite".
        let m = measured_for("prolite|gpt-6.1-sol", 12);
        let acc = codex_acc("Pro 5x", 60.0);
        let (weekly, _burn, basis, sample) = model_costs(&cfg, &m, &acc, "codex", "gpt-6.1-sol");
        assert_eq!(weekly, [0.4, 1.6, 5.0, 12.0]);
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
        let (_w, _b, basis, sample) = model_costs(&cfg, &m, &acc, "codex", "gpt-6.1-sol");
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
        let cfg = test_cfg();
        let m = Measured::default();
        let acc = codex_acc("NoSuchPlan", 60.0);
        let (_w, _b, basis, sample) = model_costs(&cfg, &m, &acc, "codex", "gpt-6.1-sol");
        assert_eq!(sample, None);
        assert!(basis.contains("conservative estimate"), "{basis}");
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
        // SAFETY: test-only env override read by jev_fetch.
        unsafe { std::env::set_var("AGENTDECK_ROUTE_MOCK_JEV", &mock) };
        let r = super::probe_decision(None, "m", Some("k")).await;
        unsafe { std::env::remove_var("AGENTDECK_ROUTE_MOCK_JEV") };
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(r.unwrap(), (0.0, 0.0, Some(0.001)));
    }
}
