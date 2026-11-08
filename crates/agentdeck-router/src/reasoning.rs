//! Port of `reasoning.mjs`: the model option catalog and effort mapping.

use serde_json::Value;

use crate::policy::family_of;

pub fn catalog_file(state_home: &std::path::Path) -> std::path::PathBuf {
    state_home.join("model-route/catalog.json")
}

const IDS: [&str; 4] = ["reasoningEffort", "effort", "variant", "thinking"];
const LEVELS: [&str; 4] = ["low", "medium", "high", "xhigh"];
const RANK: [&str; 7] = ["none", "minimal", "low", "medium", "high", "xhigh", "max"];

fn min_effort(model: &str) -> Option<String> {
    family_of(model).and_then(|f| f.min_effort)
}

fn valid_descriptor(d: &Value) -> bool {
    if d.get("id").and_then(Value::as_str).is_none() {
        return false;
    }
    match d.get("type").and_then(Value::as_str) {
        Some("boolean") => true,
        Some("select") => d.get("options").and_then(Value::as_array).is_some_and(|o| {
            o.iter()
                .all(|x| x.get("id").and_then(Value::as_str).is_some())
        }),
        _ => false,
    }
}

fn normalize_descriptor(d: Value) -> Value {
    if d.get("type").is_none()
        && let Some(values) = d.get("values").and_then(Value::as_array)
    {
        if values.iter().all(|v| v.is_boolean()) {
            return serde_json::json!({"id": d.get("id"), "type": "boolean"});
        }
        let default = d.get("default");
        return serde_json::json!({
            "id": d.get("id"),
            "type": "select",
            "options": values.iter().map(|v| {
                let s = match v {
                    Value::Bool(b) => b.to_string(),
                    Value::Number(n) => n.to_string(),
                    Value::String(s) => s.clone(),
                    _ => v.to_string(),
                };
                serde_json::json!({"id": s, "isDefault": Some(v) == default})
            }).collect::<Vec<_>>(),
        });
    }
    d
}

fn normalize_option_value(id: &str, v: &Value) -> String {
    match (id, v) {
        (_, Value::String(s)) => s.clone(),
        (_, Value::Bool(b)) => b.to_string(),
        (_, Value::Number(n)) => n.to_string(),
        _ => v.to_string(),
    }
}

pub fn save_catalog(state_home: &std::path::Path, mut data: Value) -> anyhow::Result<()> {
    if data.get("structuredContent").is_some() {
        data = data
            .get("structuredContent")
            .cloned()
            .unwrap_or(Value::Null);
    }
    if data.get("providers").is_none()
        && let Some(content) = data.get("content").and_then(Value::as_array)
    {
        let text = content
            .iter()
            .find(|c| c.get("type").and_then(Value::as_str) == Some("text"))
            .and_then(|c| c.get("text").and_then(Value::as_str))
            .unwrap_or("{}");
        data = serde_json::from_str(text).unwrap_or(Value::Null);
    }
    if let Some(providers) = data.get_mut("providers").and_then(Value::as_array_mut) {
        for p in providers.iter_mut() {
            if let Some(models) = p.get_mut("models").and_then(Value::as_array_mut) {
                for m in models.iter_mut() {
                    if let Some(options) = m.get_mut("options").and_then(Value::as_array_mut) {
                        *options = options.drain(..).map(normalize_descriptor).collect();
                    }
                }
            }
        }
    }
    let providers = data
        .get("providers")
        .and_then(Value::as_array)
        .filter(|p| !p.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "expected orchestrator_capabilities structuredContent with model option descriptors"
            )
        })?;
    for p in providers {
        let ok = p
            .get("providerInstanceId")
            .and_then(Value::as_str)
            .is_some()
            && p.get("driverKind").and_then(Value::as_str).is_some()
            && p.get("models")
                .and_then(Value::as_array)
                .is_some_and(|models| {
                    models.iter().all(|m| {
                        m.get("id").and_then(Value::as_str).is_some()
                            && m.get("options").is_none_or(|o| {
                                o.as_array().is_some_and(|o| o.iter().all(valid_descriptor))
                            })
                    })
                });
        if !ok {
            return Err(anyhow::anyhow!(
                "expected orchestrator_capabilities structuredContent with model option descriptors"
            ));
        }
    }
    let file = catalog_file(state_home);
    if let Some(parent) = file.parent() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
            let _ = std::fs::DirBuilder::new().mode(0o700).create(parent);
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
        #[cfg(not(unix))]
        {
            let _ = std::fs::create_dir_all(parent);
        }
    }
    let out = serde_json::json!({
        "providers": providers.iter().map(|p| serde_json::json!({
            "providerInstanceId": p.get("providerInstanceId"),
            "driverKind": p.get("driverKind"),
            "models": p.get("models").and_then(Value::as_array).cloned().unwrap_or_default().iter().map(|m| serde_json::json!({
                "id": m.get("id"),
                "options": m.get("options").cloned().unwrap_or(Value::from(Vec::<Value>::new())),
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "updatedAt": chrono_now_iso(),
    });
    let tmp = file.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(&tmp, serde_json::to_string(&out)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, &file)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

// `new Date().toISOString()` without a date/time crate (UTC, millis).
fn chrono_now_iso() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() as i64;
    let ms = now.subsec_millis();
    let days = secs.div_euclid(86400);
    let sod = secs.rem_euclid(86400);
    let (mut y, mut m, mut d) = (1970, 1, 1);
    let mut dd = days;
    while dd >= 365 {
        let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
        let len = if leap { 366 } else { 365 };
        if dd < len {
            break;
        }
        dd -= len;
        y += 1;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    for (i, len) in [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ]
    .into_iter()
    .enumerate()
    {
        if dd < len {
            m = i as i64 + 1;
            d = dd + 1;
            break;
        }
        dd -= len;
    }
    format!(
        "{:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{ms:03}Z",
        y,
        sod / 3600,
        sod % 3600 / 60,
        sod % 60
    )
}

pub fn read_catalog(state_home: &std::path::Path) -> anyhow::Result<Value> {
    let err = || {
        anyhow::anyhow!(
            "refresh model options: pass orchestrator_capabilities structuredContent to agentdeck route --catalog on stdin"
        )
    };
    let text = std::fs::read_to_string(catalog_file(state_home)).map_err(|_| err())?;
    let data: Value = serde_json::from_str(&text).map_err(|_| err())?;
    if !data
        .get("providers")
        .and_then(Value::as_array)
        .is_some_and(|p| p.iter().all(|x| x.get("driverKind").is_some()))
    {
        return Err(err());
    }
    Ok(data)
}

fn find_model<'a>(catalog: &'a Value, provider: &str, model: &str) -> Option<&'a Value> {
    catalog
        .get("providers")?
        .as_array()?
        .iter()
        .find(|p| p.get("providerInstanceId").and_then(Value::as_str) == Some(provider))?
        .get("models")?
        .as_array()?
        .iter()
        .find(|m| m.get("id").and_then(Value::as_str) == Some(model))
}

pub fn reasoning_descriptor(
    catalog: &Value,
    provider: &str,
    model: &str,
) -> anyhow::Result<Option<Value>> {
    let entry = find_model(catalog, provider, model).ok_or_else(|| {
        anyhow::anyhow!("refresh orchestrator_capabilities catalog: no model {provider}/{model}")
    })?;
    let instance = catalog["providers"].as_array().and_then(|ps| {
        ps.iter()
            .find(|p| p.get("providerInstanceId").and_then(Value::as_str) == Some(provider))
    });
    let driver = instance
        .and_then(|p| p.get("driverKind"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let d = entry
        .get("options")
        .and_then(Value::as_array)
        .and_then(|opts| {
            opts.iter().find(|d| {
                d.get("id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| IDS.contains(&id))
            })
        })
        .cloned();
    if d.is_some() && driver == "claudeAgent" && !model.starts_with("claude-") {
        return Err(anyhow::anyhow!(
            "T3 custom Claude model {provider}/{model} cannot apply its advertised reasoning option; use another harness"
        ));
    }
    Ok(d)
}

pub fn reasoning_target(
    catalog: &Value,
    provider: &str,
    model: &str,
    difficulty: i64,
) -> anyhow::Result<Value> {
    let target = serde_json::json!({"providerInstanceId": provider, "model": model});
    let d = reasoning_descriptor(catalog, provider, model)?;
    let Some(d) = d else { return Ok(target) };
    // Delegates never run on low or an implicit default: difficulty 0 medium, 1 high, 2-3 xhigh.
    let mut difficulty: i64 = (difficulty.max(0) + 1).min(3);
    if d.get("type").and_then(Value::as_str) == Some("boolean") {
        let mut target = serde_json::Map::new();
        target.insert("providerInstanceId".into(), Value::from(provider));
        target.insert("model".into(), Value::from(model));
        let mut opts = serde_json::Map::new();
        opts.insert(
            d.get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            Value::from(true),
        );
        target.insert("options".into(), Value::Object(opts));
        return Ok(Value::Object(target));
    }
    let supported: Vec<String> = d
        .get("options")
        .and_then(Value::as_array)
        .map(|o| {
            o.iter()
                .filter_map(|x| x.get("id").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let floor = min_effort(model)
        .and_then(|m| LEVELS.iter().position(|l| *l == m).map(|i| i as i64))
        .unwrap_or(-1);
    if floor > difficulty {
        difficulty = floor;
    }
    // Highest supported level up to the wanted one but not below medium, else the next one above.
    let wanted = difficulty.clamp(1, 3) as usize;
    let mut value = LEVELS[1..=wanted]
        .iter()
        .rev()
        .chain(LEVELS[wanted + 1..].iter())
        .find(|v| supported.iter().any(|s| s == *v))
        .map(|v| v.to_string())
        .or_else(|| {
            LEVELS
                .iter()
                .find(|v| supported.iter().any(|s| s == *v))
                .map(|v| v.to_string())
        })
        .or_else(|| {
            d.get("options")
                .and_then(Value::as_array)
                .and_then(|o| {
                    o.iter().find(|x| {
                        x.get("id").and_then(Value::as_str).is_some_and(|id| {
                            id == d
                                .get("currentValue")
                                .and_then(Value::as_str)
                                .unwrap_or("\u{0}")
                        }) || x.get("isDefault").and_then(Value::as_bool) == Some(true)
                    })
                })
                .and_then(|x| x.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string)
        });
    if value
        .as_deref()
        .is_none_or(|v| ["ultracode", "ultrathink"].contains(&v))
    {
        value = None;
    }
    match value {
        Some(v) => {
            let mut target = serde_json::Map::new();
            target.insert("providerInstanceId".into(), Value::from(provider));
            target.insert("model".into(), Value::from(model));
            let mut opts = serde_json::Map::new();
            opts.insert(
                d.get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                Value::from(v),
            );
            target.insert("options".into(), Value::Object(opts));
            Ok(Value::Object(target))
        }
        None => Err(anyhow::anyhow!(
            "cannot map difficulty to {provider}/{model} {}; choose a model with ordinary effort levels",
            d.get("id").and_then(Value::as_str).unwrap_or("")
        )),
    }
}

/// Adds the worker agent to `target.options` when T3 advertises it for the
/// model (OpenCode lists agents after loading `~/.config/opencode/agent`).
pub fn with_worker_agent(catalog: &Value, target: &mut Value) {
    let name = agentdeck_core::router::WORKER_AGENT;
    let listed = find_model(
        catalog,
        target["providerInstanceId"].as_str().unwrap_or_default(),
        target["model"].as_str().unwrap_or_default(),
    )
    .and_then(|m| m.get("options")?.as_array())
    .is_some_and(|opts| {
        opts.iter().any(|d| {
            d.get("id").and_then(Value::as_str) == Some("agent")
                && d.get("options").and_then(Value::as_array).is_some_and(|o| {
                    o.iter()
                        .any(|x| x.get("id").and_then(Value::as_str) == Some(name))
                })
        })
    });
    if listed {
        target["options"]
            .as_object_mut()
            .map(|o| o.insert("agent".into(), Value::from(name)))
            .unwrap_or_else(|| {
                target["options"] = serde_json::json!({"agent": name});
                None
            });
    }
}

fn target_entries(target: &Value) -> anyhow::Result<Vec<(String, Value)>> {
    // Missing/null `options` is empty like the .mjs; a wrongly typed value
    // fails closed.
    match target.get("options") {
        Some(Value::Object(m)) => Ok(m.iter().map(|(k, v)| (k.clone(), v.clone())).collect()),
        Some(Value::Array(a)) => Ok(a
            .iter()
            .filter_map(|e| Some((e.get("id")?.as_str()?.to_string(), e.get("value")?.clone())))
            .collect()),
        // Models without a reasoning knob legitimately get no `options`.
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(_) => Err(anyhow::anyhow!(
            "target.options must be an object or an array of {{id, value}}"
        )),
    }
}

pub fn validate_reasoning(catalog: &Value, target: &Value) -> anyhow::Result<Option<String>> {
    let provider = target
        .get("providerInstanceId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let model = target
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default();
    // Throws (like the .mjs) when the model is not in the catalog: the hook
    // fails closed instead of reporting a reasoning denial.
    let d = reasoning_descriptor(catalog, provider, model)?;
    let entries = target_entries(target)?;
    if entries.iter().any(|(id, _)| id.is_empty()) || {
        let mut ids: Vec<&str> = entries.iter().map(|(id, _)| id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        ids.len() != entries.len()
    } {
        return Ok(Some("invalid or duplicate target option IDs".into()));
    }
    if entries.iter().any(|(id, _)| {
        IDS.contains(&id.as_str())
            && Some(id.as_str()) != d.as_ref().and_then(|x| x.get("id")).and_then(Value::as_str)
    }) {
        return Ok(Some("reasoning option belongs to another harness".into()));
    }
    let Some(d) = d else { return Ok(None) };
    let did = d.get("id").and_then(Value::as_str).unwrap_or_default();
    let e = entries.iter().find(|(id, _)| id == did);
    let Some((_, value)) = e else {
        return Ok(Some(format!(
            "explicit target.options.{did} is required; omitted options can lose reasoning across providers/models"
        )));
    };
    if d.get("type").and_then(Value::as_str) == Some("boolean") {
        if !value.is_boolean() {
            return Ok(Some(format!(
                "unsupported target.options.{did} for this model"
            )));
        }
    } else if value.as_str().is_none_or(|s| {
        !d.get("options").and_then(Value::as_array).is_some_and(|o| {
            o.iter()
                .any(|x| x.get("id").and_then(Value::as_str) == Some(s))
        })
    }) {
        return Ok(Some(format!(
            "unsupported target.options.{did} for this model"
        )));
    }
    let raw = value;
    let value = normalize_option_value("", value);
    if let Some(floor) = min_effort(model)
        && let (Some(pos), Some(min)) = (
            RANK.iter().position(|r| *r == value.as_str()),
            RANK.iter().position(|r| *r == floor),
        )
        && pos < min
    {
        return Ok(Some(format!(
            "target.options.{did} {value} is below this model family's minimum {floor}"
        )));
    }
    if d.get("type").and_then(Value::as_str) == Some("boolean") {
        if raw != &Value::Bool(true) {
            return Ok(Some(format!("target.options.{did} must be true")));
        }
    } else if d.get("options").and_then(Value::as_array).is_some_and(|o| {
        o.iter().any(|x| {
            x.get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| LEVELS[1..].contains(&id))
        })
    }) && let (Some(pos), Some(min)) = (
        RANK.iter().position(|r| Some(*r) == raw.as_str()),
        RANK.iter().position(|r| *r == "medium"),
    ) && pos < min
    {
        return Ok(Some(format!(
            "target.options.{did} {} is below the router minimum medium",
            raw.as_str().unwrap_or_default()
        )));
    }
    Ok(None)
}

/// Port of `repairReasoning`: keep unrelated options, restore the catalog
/// knob from the explicit value when it validates, else from `recommended`
/// (the router's own target). Throws like the .mjs when the model is unknown
/// or `recommended` has no usable knob, so the hook fails closed.
pub fn repair_reasoning(
    catalog: &Value,
    target: &Value,
    recommended: &Value,
) -> anyhow::Result<Value> {
    let provider = target
        .get("providerInstanceId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let model = target
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let d = reasoning_descriptor(catalog, provider, model)?;
    let entries = target_entries(target)?;
    let unrelated: Vec<(String, Value)> = entries
        .iter()
        .filter(|(id, _)| !IDS.contains(&id.as_str()))
        .cloned()
        .collect();
    let array = target.get("options").is_some_and(|o| o.is_array());
    let mut out = target
        .as_object()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("invalid target"))?;
    let fixed = |pairs: Vec<(String, Value)>| {
        if array {
            Value::Array(
                pairs
                    .into_iter()
                    .map(|(id, value)| serde_json::json!({"id": id, "value": value}))
                    .collect(),
            )
        } else {
            Value::Object(pairs.into_iter().collect())
        }
    };
    let Some(d) = d else {
        out.insert("options".into(), fixed(unrelated));
        return Ok(Value::Object(out));
    };
    let did = d.get("id").and_then(Value::as_str).unwrap_or_default();
    let explicit: Vec<&(String, Value)> = entries.iter().filter(|(id, _)| id == did).collect();
    let value = if explicit.len() == 1 {
        let mut probe = out.clone();
        probe.insert(
            "options".into(),
            Value::Object(
                [(did.to_string(), explicit[0].1.clone())]
                    .into_iter()
                    .collect(),
            ),
        );
        if validate_reasoning(catalog, &Value::Object(probe))?.is_none() {
            explicit[0].1.clone()
        } else {
            recommended
                .get("options")
                .and_then(|o| o.get(did))
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no usable reasoning option"))?
        }
    } else {
        recommended
            .get("options")
            .and_then(|o| o.get(did))
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no usable reasoning option"))?
    };
    let mut corrected = unrelated;
    corrected.push((did.to_string(), value));
    out.insert("options".into(), fixed(corrected));
    Ok(Value::Object(out))
}

#[cfg(test)]
mod catalog_tests {
    use super::*;

    fn catalog() -> Value {
        serde_json::json!({"providers": [{
            "providerInstanceId": "codex",
            "driverKind": "codex",
            "models": [
                {"id": "some-new-model-9", "options": [
                    {"id": "reasoningEffort", "type": "select", "options": [
                        {"id": "low"}, {"id": "medium", "isDefault": true}, {"id": "high"}]}]},
                {"id": "plain-model", "options": []},
            ],
        }]})
    }

    #[test]
    fn absent_model_fails_closed() {
        assert!(reasoning_target(&catalog(), "codex", "no-such-model", 1).is_err());
    }

    #[test]
    fn catalog_listed_model_without_family_gets_target() {
        // No roster family, but an exact catalog entry: routable, no invented profile.
        let t = reasoning_target(&catalog(), "codex", "some-new-model-9", 1).unwrap();
        assert_eq!(t["model"], "some-new-model-9");
        assert_eq!(t["options"]["reasoningEffort"], "high");
        let easy = reasoning_target(&catalog(), "codex", "some-new-model-9", 0).unwrap();
        assert_eq!(easy["options"]["reasoningEffort"], "medium");
        let hard = reasoning_target(&catalog(), "codex", "some-new-model-9", 3).unwrap();
        // xhigh is not offered: the highest supported level below it.
        assert_eq!(hard["options"]["reasoningEffort"], "high");
        let at = |v: &str| {
            serde_json::json!({"providerInstanceId": "codex",
            "model": "some-new-model-9", "options": {"reasoningEffort": v}})
        };
        assert!(
            validate_reasoning(&catalog(), &at("low"))
                .unwrap()
                .is_some()
        );
        assert_eq!(validate_reasoning(&catalog(), &at("medium")).unwrap(), None);
        assert_eq!(validate_reasoning(&catalog(), &at("high")).unwrap(), None);
        let bare = reasoning_target(&catalog(), "codex", "plain-model", 2).unwrap();
        assert_eq!(bare["model"], "plain-model");
        assert!(bare.get("options").is_none());
    }
}

#[cfg(test)]
mod worker_agent_tests {
    use super::*;

    fn catalog(agents: &[&str]) -> Value {
        let agent = serde_json::json!({"id": "agent", "type": "select",
            "options": agents.iter().map(|a| serde_json::json!({"id": a})).collect::<Vec<_>>()});
        serde_json::json!({"providers": [{"providerInstanceId": "opencode", "models": [
            {"id": "m", "options": [agent]}]}]})
    }

    #[test]
    fn worker_agent_is_added_only_when_listed() {
        let base = serde_json::json!({"providerInstanceId": "opencode", "model": "m",
            "options": {"variant": "high"}});
        let mut t = base.clone();
        with_worker_agent(&catalog(&["build"]), &mut t);
        assert_eq!(t, base);
        with_worker_agent(&catalog(&["build", "agentdeck-worker"]), &mut t);
        assert_eq!(
            t["options"],
            serde_json::json!({"variant": "high", "agent": "agentdeck-worker"})
        );
        let mut bare = serde_json::json!({"providerInstanceId": "opencode", "model": "m"});
        with_worker_agent(&catalog(&["agentdeck-worker"]), &mut bare);
        assert_eq!(bare["options"]["agent"], "agentdeck-worker");
    }

    #[test]
    fn missing_options_are_empty_and_wrong_type_fails() {
        let catalog = serde_json::json!({"providers": [{"providerInstanceId": "antigravity",
            "models": [{"id": "gemini-4-argon-high", "options": []}]}]});
        let target = serde_json::json!({"providerInstanceId": "antigravity", "model": "gemini-4-argon-high"});
        assert_eq!(validate_reasoning(&catalog, &target).unwrap(), None);
        let mut null = target.clone();
        null["options"] = Value::Null;
        assert_eq!(validate_reasoning(&catalog, &null).unwrap(), None);
        let mut bad = target;
        bad["options"] = Value::from("high");
        assert!(validate_reasoning(&catalog, &bad).is_err());
    }
}

/// Ported from the legacy Node `reasoning.test.mjs` (deleted with the
/// router `.mjs` sources): effort mapping, family minimums, opaque
/// variants, validation/repair rules and catalog persistence.
#[cfg(test)]
mod legacy_suite_tests {
    use super::*;

    fn select(id: &str, values: &[&str]) -> Value {
        serde_json::json!({"id": id, "type": "select",
            "options": values.iter().map(|v| serde_json::json!({"id": v})).collect::<Vec<_>>()})
    }

    fn catalog() -> Value {
        serde_json::json!({"providers": [
            {"providerInstanceId": "codex", "driverKind": "codex", "models": [
                {"id": "sol", "options": [select("reasoningEffort", &["low", "medium", "high", "xhigh", "max", "ultra"])]},
                {"id": "luna", "options": [select("reasoningEffort", &["low", "medium", "high", "xhigh", "max"])]},
            ]},
            {"providerInstanceId": "codex_second", "driverKind": "codex", "models": [
                {"id": "sol", "options": [select("reasoningEffort", &["low", "medium", "high", "xhigh", "max", "ultra"])]},
            ]},
            {"providerInstanceId": "claudeAgent", "driverKind": "claudeAgent", "models": [
                {"id": "claude-opus", "options": [select("effort", &["low", "medium", "high", "xhigh", "max", "ultracode", "ultrathink"])]},
                {"id": "claude-old", "options": [select("effort", &["low", "medium", "high", "max"])]},
                {"id": "claude-haiku", "options": [{"id": "thinking", "type": "boolean"}]},
            ]},
            {"providerInstanceId": "opencode", "driverKind": "opencode", "models": [
                {"id": "ordinary", "options": [select("variant", &["low", "high", "xhigh"])]},
                {"id": "opaque", "options": [{"id": "variant", "type": "select",
                    "options": [{"id": "quick"}, {"id": "deep"}], "currentValue": "deep"}]},
                {"id": "unknown", "options": [select("variant", &["quick", "deep"])]},
                {"id": "none"},
                {"id": "other-only", "options": [select("serviceTier", &["default", "priority"])]},
            ]},
            {"providerInstanceId": "antigravity", "driverKind": "antigravity", "models": [
                {"id": "gemini-high"},
            ]},
            {"providerInstanceId": "custom-claude", "driverKind": "claudeAgent", "models": [
                {"id": "meta/muse", "options": [select("effort", &["low", "medium", "high", "xhigh", "max", "ultracode", "ultrathink"])]},
            ]},
        ]})
    }

    fn target(provider: &str, model: &str, options: Value) -> Value {
        serde_json::json!({"providerInstanceId": provider, "model": model, "options": options})
    }

    #[test]
    fn difficulty_maps_to_effort_without_low() {
        let c = catalog();
        // Delegates never run on low: difficulty 0 medium, 1 high, 2-3 xhigh.
        for (difficulty, value) in ["medium", "high", "xhigh", "xhigh"].iter().enumerate() {
            for provider in ["codex", "codex_second"] {
                assert_eq!(
                    reasoning_target(&c, provider, "sol", difficulty as i64).unwrap()["options"],
                    serde_json::json!({"reasoningEffort": value})
                );
            }
            for provider in ["claudeAgent", "claudeAgent_openrouter"] {
                // claudeAgent_openrouter is absent here; covered by the missing-model error below.
                if provider == "claudeAgent" {
                    assert_eq!(
                        reasoning_target(&c, provider, "claude-opus", difficulty as i64).unwrap()["options"],
                        serde_json::json!({"effort": value})
                    );
                }
            }
        }
        assert_eq!(
            reasoning_target(&c, "claudeAgent", "claude-old", 3).unwrap()["options"],
            serde_json::json!({"effort": "high"})
        );
        // Medium is not offered: the next supported level above it, never low.
        assert_eq!(
            reasoning_target(&c, "opencode", "ordinary", 0).unwrap()["options"],
            serde_json::json!({"variant": "high"})
        );
        assert_eq!(
            reasoning_target(&c, "opencode", "ordinary", 1).unwrap()["options"],
            serde_json::json!({"variant": "high"})
        );
        // Opaque variants use the advertised default; without one routing fails.
        assert_eq!(
            reasoning_target(&c, "opencode", "opaque", 2).unwrap()["options"],
            serde_json::json!({"variant": "deep"})
        );
        assert!(reasoning_target(&c, "opencode", "unknown", 2).is_err());
        assert_eq!(
            reasoning_target(&c, "opencode", "none", 2)
                .unwrap()
                .get("options"),
            None
        );
        assert_eq!(
            reasoning_target(&c, "antigravity", "gemini-high", 2).unwrap(),
            serde_json::json!({"providerInstanceId": "antigravity", "model": "gemini-high"})
        );
        assert!(reasoning_target(&c, "missing", "sol", 2).is_err());
        assert!(reasoning_target(&c, "custom-claude", "meta/muse", 2).is_err());
    }

    #[test]
    fn explicit_options_are_validated_and_repaired() {
        let c = catalog();
        let t = target("codex_second", "sol", serde_json::Value::Null);
        assert!(
            validate_reasoning(&c, &t)
                .unwrap()
                .unwrap()
                .contains("required")
        );
        assert!(
            validate_reasoning(
                &c,
                &target(
                    "codex_second",
                    "sol",
                    serde_json::json!({"reasoningEffort": "low"})
                )
            )
            .unwrap()
            .unwrap()
            .contains("minimum medium")
        );
        for value in ["medium", "ultra"] {
            assert_eq!(
                validate_reasoning(
                    &c,
                    &target(
                        "codex_second",
                        "sol",
                        serde_json::json!({"reasoningEffort": value, "serviceTier": "priority"})
                    )
                )
                .unwrap(),
                None
            );
        }
        assert!(
            validate_reasoning(
                &c,
                &target("codex_second", "sol", serde_json::json!({"effort": "high"}))
            )
            .unwrap()
            .unwrap()
            .contains("another harness")
        );
        for value in [
            serde_json::json!(""),
            serde_json::json!(false),
            serde_json::json!(2),
            serde_json::json!("invented"),
        ] {
            assert!(
                validate_reasoning(
                    &c,
                    &target(
                        "codex_second",
                        "sol",
                        serde_json::json!({"reasoningEffort": value})
                    )
                )
                .unwrap()
                .is_some()
            );
        }
        assert!(
            validate_reasoning(
                &c,
                &target(
                    "codex",
                    "luna",
                    serde_json::json!({"reasoningEffort": "ultra"})
                )
            )
            .unwrap()
            .is_some()
        );
        // Repair keeps unrelated options and replaces only the reasoning knob.
        let recommended = reasoning_target(&c, "codex_second", "sol", 1).unwrap();
        let partial = target(
            "codex_second",
            "sol",
            serde_json::json!({"serviceTier": "priority"}),
        );
        assert_eq!(
            repair_reasoning(&c, &partial, &recommended).unwrap()["options"],
            serde_json::json!({"serviceTier": "priority", "reasoningEffort": "high"})
        );
        assert_eq!(
            partial["options"],
            serde_json::json!({"serviceTier": "priority"})
        );
    }

    #[test]
    fn boolean_thinking_must_stay_true() {
        let c = catalog();
        assert_eq!(
            reasoning_target(&c, "claudeAgent", "claude-haiku", 0).unwrap()["options"],
            serde_json::json!({"thinking": true})
        );
        assert!(
            validate_reasoning(
                &c,
                &target(
                    "claudeAgent",
                    "claude-haiku",
                    serde_json::json!({"thinking": false})
                )
            )
            .unwrap()
            .unwrap()
            .contains("must be true")
        );
        assert_eq!(
            validate_reasoning(
                &c,
                &target(
                    "claudeAgent",
                    "claude-haiku",
                    serde_json::json!({"thinking": true})
                )
            )
            .unwrap(),
            None
        );
    }
}
