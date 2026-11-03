//! Agentdeck copy of the model-route Node router: the `router.json`
//! generator (pool + account maps), hook registration and the instruction
//! marker block. The router itself runs from this binary (`agentdeck route`,
//! `agentdeck route hook`), so no `.mjs` files are extracted at runtime: the
//! assets stay in the repo as parity-test fixtures.
//!
//! Everything lands in `<state_dir>/router/` (just `router.json`). The old
//! standalone copy in `~/.general/scripts/model-route` is never touched.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::changes::{Change, backup_dest, plan_write_file};
use crate::paths::Paths;
use crate::plugins::apply_marker_block;
use crate::pool::PoolValue;
use crate::registry::Registry;
use crate::usage::UsageKind;

pub const ROUTER_VERSION_MAJOR: u32 = 1;
/// The daemon dual-writes the old ai-usage state path, so the router keeps
/// reading it by default.
pub const USAGE_STATE_PATH: &str = "~/.local/state/ai-usage/state.json";
/// Marker name for the instruction block; the plugin name stays `router`.
pub const ROUTER_MARKER: &str = "model-route";

/// Runtime files extracted next to `router.json`. The ported router
/// (`route`, `hook`) runs from the binary, so nothing is extracted any more:
/// every `.mjs` asset stays in the repo only as the parity-test fixture.
/// `router.json` is generated data and is kept.
const FILES: &[(&str, &[u8])] = &[];

/// Previously extracted files: enable wrote them, disable/upgrade removes
/// the ones still matching the embedded bytes.
const LEGACY_FILES: &[(&str, &[u8])] = &[
    ("route.mjs", include_bytes!("../assets/router/route.mjs")),
    ("policy.mjs", include_bytes!("../assets/router/policy.mjs")),
    ("hook.mjs", include_bytes!("../assets/router/hook.mjs")),
    (
        "reasoning.mjs",
        include_bytes!("../assets/router/reasoning.mjs"),
    ),
    ("load.mjs", include_bytes!("../assets/router/load.mjs")),
    (
        "workflow.mjs",
        include_bytes!("../assets/router/workflow.mjs"),
    ),
    (
        "t3-mcp-call.mjs",
        include_bytes!("../assets/router/t3-mcp-call.mjs"),
    ),
    (
        "claude-shell-mcp.mjs",
        include_bytes!("../assets/router/claude-shell-mcp.mjs"),
    ),
    (
        "roster.json",
        include_bytes!("../assets/router/roster.json"),
    ),
];

pub const CLAUDE_MATCHER: &str = "^(?:Agent|Workflow)$|.*delegate_task$";
pub const CODEX_MATCHER: &str = "^(?:collaboration\\.?)?spawn_agent$|^Agent$|.*delegate_task$";
pub const AGY_MATCHER: &str = "^invoke_subagent$|^start_subagent$|^call_mcp_tool$|.*delegate_task$";

pub fn router_dir(paths: &Paths) -> PathBuf {
    paths.state_dir.join("router")
}

pub fn router_json_path(paths: &Paths) -> PathBuf {
    router_dir(paths).join("router.json")
}

/// Command string registered in harness hooks (hooks fail closed on error).
/// The hook runs from this binary, so no Node is needed at runtime.
pub fn hook_command(exe: &Path, harness: &str) -> String {
    format!("{} route hook --harness {harness}", exe.display())
}

pub fn claude_settings(home: &Path) -> PathBuf {
    home.join(".claude/settings.json")
}

pub fn codex_hooks(home: &Path) -> PathBuf {
    home.join(".codex/hooks.json")
}

pub fn agy_hooks(home: &Path) -> PathBuf {
    home.join(".gemini/config/plugins/model-route/hooks.json")
}

pub fn agy_config(home: &Path) -> PathBuf {
    home.join(".gemini/config/config.json")
}

pub fn opencode_plugin(home: &Path) -> PathBuf {
    home.join(".config/opencode/plugin/model-route.ts")
}

/// Account key from a usage id: `claude-main` -> `claude`,
/// `codex-second` -> `codex_second`.
fn account_key(id: &str) -> String {
    id.strip_suffix("-main").unwrap_or(id).replace('-', "_")
}

/// Build the router JSON from `[[pool]]` + `[[usage]]`.
///
/// Pool entries map 1:1. Each usage entry with a linked instance becomes one
/// account (`usageId` for the per-login claude/codex kinds). Instance-less
/// entries take the still-unclaimed pool instances (unambiguous with a single
/// such entry); the shared opencode-go account only matches models under
/// `opencode-go/` on the already-mapped `opencode` instance.
pub fn router_json_value(reg: &Registry) -> anyhow::Result<Value> {
    let pool: Vec<Value> = reg
        .pool
        .iter()
        .map(|e| {
            let mut o = Map::new();
            o.insert(
                "providerInstanceId".into(),
                Value::String(e.instance.clone()),
            );
            o.insert("model".into(), Value::String(e.model.clone()));
            o.insert("enabled".into(), Value::Bool(e.enabled));
            if !e.options.is_empty() {
                o.insert(
                    "options".into(),
                    Value::Object(
                        e.options
                            .iter()
                            .map(|(k, v)| {
                                (
                                    k.clone(),
                                    match v {
                                        PoolValue::Bool(b) => Value::Bool(*b),
                                        PoolValue::String(s) => Value::String(s.clone()),
                                    },
                                )
                            })
                            .collect(),
                    ),
                );
            }
            Value::Object(o)
        })
        .collect();
    let mut mapped: BTreeSet<String> = BTreeSet::new();
    let mut accounts: BTreeMap<String, Value> = BTreeMap::new();
    for u in &reg.usage {
        if let Some(inst) = &u.instance {
            let mut o = Map::new();
            o.insert(
                "instances".into(),
                Value::Array(vec![Value::String(inst.clone())]),
            );
            o.insert(
                "usageProvider".into(),
                Value::String(u.kind.as_str().to_string()),
            );
            if matches!(u.kind, UsageKind::Claude | UsageKind::Codex) {
                o.insert("usageId".into(), Value::String(u.id.clone()));
            }
            if matches!(u.kind, UsageKind::OpencodeGo) {
                o.insert("modelPrefix".into(), Value::from("opencode-go/"));
            }
            if mapped.contains(inst) {
                anyhow::bail!(
                    "usage {:?} shares instance {inst:?} with another account",
                    u.id
                );
            }
            mapped.insert(inst.clone());
            accounts.insert(account_key(&u.id), Value::Object(o));
        }
    }
    let mut uncovered: Vec<String> = reg
        .pool
        .iter()
        .map(|e| e.instance.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|i| !mapped.contains(i))
        .collect();
    // shortcut: standalone keys share every unclaimed instance; with several
    // such keys the map would double-claim, so bail and ask for instance links.
    for u in reg
        .usage
        .iter()
        .filter(|u| u.instance.is_none() && u.kind != UsageKind::OpencodeGo)
    {
        let take: Vec<String> = if matches!(u.kind, UsageKind::Antigravity) {
            uncovered
                .iter()
                .filter(|i| i.as_str() == "antigravity")
                .cloned()
                .collect()
        } else {
            uncovered.clone()
        };
        if take.is_empty() {
            continue;
        }
        let mut o = Map::new();
        o.insert(
            "instances".into(),
            Value::Array(take.iter().map(|s| Value::String(s.clone())).collect()),
        );
        o.insert(
            "usageProvider".into(),
            Value::String(u.kind.as_str().to_string()),
        );
        uncovered.retain(|i| !take.contains(i));
        for i in &take {
            mapped.insert(i.clone());
        }
        accounts.insert(account_key(&u.id), Value::Object(o));
    }
    if mapped.contains("opencode")
        && reg
            .usage
            .iter()
            .any(|u| u.instance.is_none() && u.kind == UsageKind::OpencodeGo)
    {
        let mut o = Map::new();
        o.insert(
            "instances".into(),
            Value::Array(vec![Value::String("opencode".to_string())]),
        );
        o.insert("usageProvider".into(), Value::from("opencode-go"));
        o.insert("modelPrefix".into(), Value::from("opencode-go/"));
        accounts.insert("opencode-go".into(), Value::Object(o));
    }
    let limits: BTreeMap<String, u32> = reg
        .instances
        .iter()
        .filter_map(|i| i.max_parallel.map(|limit| (i.id.clone(), limit)))
        .collect();
    let version = content_version(&pool, &accounts, &limits);
    let mut root = Map::new();
    root.insert("version".into(), Value::from(version));
    root.insert("pool".into(), Value::Array(pool));
    if !limits.is_empty() {
        root.insert("instanceLimits".into(), serde_json::to_value(limits)?);
    }
    // A prefix account refines an instance another account maps, and the router
    // validates in file order: prefix accounts go last.
    let (prefixed, plain): (Vec<_>, Vec<_>) = accounts
        .into_iter()
        .partition(|(_, v)| v.get("modelPrefix").is_some());
    root.insert(
        "accounts".into(),
        Value::Object(plain.into_iter().chain(prefixed).collect()),
    );
    root.insert(
        "usageStatePath".into(),
        Value::String(USAGE_STATE_PATH.to_string()),
    );
    let settings = crate::settings::read(reg);
    if let Some(provider) = settings.decision_provider {
        root.insert("decisionProvider".into(), Value::String(provider));
    }
    if let Some(model) = settings.decision_model {
        root.insert("decisionModel".into(), Value::String(model));
    }
    // Visible to the GUI and route output; invalid rules fail closed here so
    // a bad edit never ships a router.json that silently drops the ban.
    // Excluded from `content_version` on purpose: rules change eligibility,
    // not the Jev questions, so no cache bust is needed.
    let rules = crate::settings::UserRules::from_registry(reg)?;
    root.insert("userRules".into(), rules.to_json());
    Ok(Value::Object(root))
}

/// Stable version that busts the Jev cache whenever pool/accounts change.
fn content_version(
    pool: &[Value],
    accounts: &BTreeMap<String, Value>,
    limits: &BTreeMap<String, u32>,
) -> u64 {
    let text = serde_json::to_string(
        &serde_json::json!({"pool": pool, "accounts": accounts, "instanceLimits": limits}),
    )
    .unwrap_or_default();
    let mut h: u64 = 0xcbf29ce484222325;
    for b in text.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Plan extracting the embedded files plus a fresh `router.json`. Pure plan:
/// callers skip apply on `--dry-run`.
pub fn plan_router_files(paths: &Paths, reg: &Registry) -> anyhow::Result<Vec<Change>> {
    let dir = router_dir(paths);
    let mut changes = Vec::new();
    for (name, bytes) in FILES {
        let path = dir.join(name);
        let old = std::fs::read(&path).ok();
        changes.extend(plan_write_file(path, old.as_deref(), bytes));
    }
    let text = serde_json::to_string_pretty(&router_json_value(reg)?)? + "\n";
    let path = router_json_path(paths);
    let old = std::fs::read(&path).ok();
    changes.extend(plan_write_file(path, old.as_deref(), text.as_bytes()));
    Ok(changes)
}

/// Plan removing previously extracted files (only ones matching the
/// embedded bytes). `router.json` is data and is kept.
pub fn plan_remove_router_files(paths: &Paths) -> Vec<Change> {
    let dir = router_dir(paths);
    let mut changes = Vec::new();
    for (name, bytes) in LEGACY_FILES {
        let path = dir.join(name);
        if let Ok(old) = std::fs::read(&path)
            && old.as_slice() == *bytes
        {
            changes.push(Change::Remove { path });
        }
    }
    changes
}

/// Instruction body: the Delegation routing section (routed through this
/// binary) plus the roster `briefHint` verbatim.
pub fn router_block(exe: &Path) -> String {
    let route = format!("{} route", exe.display());
    format!(
        "## Delegation routing (agentdeck router)\n\n\
        - Claude and Antigravity: the router delegates. Write the complete task to a file and run \
        `{route} dispatch FILE --title TITLE [--isolation worktree] [--role ROLE]`. It picks the model, appends any \
        `briefHint`, delegates and prints a dispatch ID; its watcher cancels and archives a child that hit a usage limit \
        and re-dispatches it on another account. When T3 reports a dispatched child ended, run `{route} dispatch --status ID`: \
        `running` means a replacement is already working, so end your turn; otherwise use its `status` and `summary`. \
        Direct `delegate_task` is denied, except for a model the user named: start that task with `[route:user-choice]`. \
        `status: no_model`: split the work or do it yourself.\n\
        - Codex and OpenCode: route every `delegate_task` target first: `{route} \"<goal, inputs, read-only or edits, expected size>\"` \
        (add `--source <provider instance>` when the source is unknown). Only exact active pool entries are candidates; \
        never restore removed entries under another id. Before routing, call T3 `orchestrator_capabilities` and pipe its \
        `structuredContent` JSON to `{route} --catalog`; refresh after provider/model changes. Pass the complete returned \
        `target` (including `options`) as the `delegate_task` target; append a returned `briefHint` verbatim. \
        `jevConfidence` < 0.4: compare quality and quota penalties of the top two eligible candidates. `target: null`: \
        split the work or do it yourself. A denied target: use the next `eligible` model in `ranking`.\n\
        - Never set `runtimeMode` unless asked.\n"
    )
}

/// Plan the `<!-- model-route:begin -->` block across `files` (same dedupe
/// and backup rules as other marker blocks, but under the router's own
/// marker name).
pub fn plan_router_marker(
    files: &[PathBuf],
    body: Option<&str>,
    state_dir: &Path,
) -> anyhow::Result<Vec<Change>> {
    let mut seen: Vec<PathBuf> = Vec::new();
    let mut changes: Vec<Change> = Vec::new();
    for file in files {
        let key = std::fs::canonicalize(file).unwrap_or_else(|_| file.clone());
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        let old = match std::fs::read(file) {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        let old_text = old
            .as_deref()
            .map(|b| {
                std::str::from_utf8(b)
                    .map_err(|_| anyhow::anyhow!("{} is not valid UTF-8", file.display()))
            })
            .transpose()?
            .unwrap_or("");
        let Some(new_text) = apply_marker_block(old_text, ROUTER_MARKER, body)? else {
            continue;
        };
        let backup = old.is_some().then(|| {
            backup_dest(
                state_dir,
                file.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("instructions"),
            )
        });
        changes.push(Change::WriteFile {
            path: file.clone(),
            contents: new_text.into_bytes(),
            old,
            backup,
        });
    }
    Ok(changes)
}

fn read_json(path: &Path) -> anyhow::Result<(Option<Vec<u8>>, Value)> {
    match std::fs::read(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((None, Value::Object(Map::new()))),
        Err(e) => Err(e.into()),
        Ok(bytes) => {
            let v: Value = serde_json::from_slice(&bytes)
                .map_err(|e| anyhow::anyhow!("{} is not valid JSON: {e}", path.display()))?;
            Ok((Some(bytes), v))
        }
    }
}

fn write_json_change(
    path: PathBuf,
    old: Option<Vec<u8>>,
    v: &Value,
    state_dir: &Path,
) -> Option<Change> {
    let text = serde_json::to_string_pretty(v).unwrap_or_default() + "\n";
    let new = text.into_bytes();
    if old.as_deref() == Some(new.as_slice()) {
        return None;
    }
    let backup = old.is_some().then(|| {
        backup_dest(
            state_dir,
            path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("config"),
        )
    });
    Some(Change::WriteFile {
        path,
        contents: new,
        old,
        backup,
    })
}

fn as_obj(v: &mut Value) -> Option<&mut Map<String, Value>> {
    v.as_object_mut()
}

fn object(v: &mut Value) -> anyhow::Result<&mut Map<String, Value>> {
    as_obj(v).ok_or_else(|| anyhow::anyhow!("expected a JSON object"))
}

fn pretool_array(parent: &mut Map<String, Value>) -> anyhow::Result<&mut Vec<Value>> {
    if !parent.contains_key("PreToolUse") {
        parent.insert("PreToolUse".into(), Value::Array(Vec::new()));
    }
    match parent.get_mut("PreToolUse") {
        Some(Value::Array(a)) => Ok(a),
        _ => anyhow::bail!("PreToolUse is not an array"),
    }
}

/// Insert our hook entry under `matcher`; returns true when changed.
fn insert_hook(arr: &mut Vec<Value>, matcher: &str, command: &str) -> bool {
    if !arr
        .iter()
        .any(|e| e.get("matcher").and_then(|m| m.as_str()) == Some(matcher))
    {
        arr.push(serde_json::json!({"matcher": matcher, "hooks": []}));
    }
    let entry = arr
        .iter_mut()
        .find(|e| e.get("matcher").and_then(|m| m.as_str()) == Some(matcher))
        .unwrap();
    let hooks = entry.get_mut("hooks").and_then(|h| h.as_array_mut());
    let Some(hooks) = hooks else { return false };
    if hooks
        .iter()
        .any(|h| h.get("command").and_then(|c| c.as_str()) == Some(command))
    {
        return false;
    }
    hooks.push(serde_json::json!({"type": "command", "command": command, "timeout": 60}));
    true
}

/// Remove our hook entry (exact command match, so standalone-copy hooks are
/// left alone); returns true when changed.
fn remove_hook(arr: &mut Vec<Value>, matcher: &str, command: &str) -> bool {
    let Some(pos) = arr
        .iter()
        .position(|e| e.get("matcher").and_then(|m| m.as_str()) == Some(matcher))
    else {
        return false;
    };
    let changed = arr[pos]
        .get_mut("hooks")
        .and_then(|h| h.as_array_mut())
        .map(|hooks| {
            let before = hooks.len();
            hooks.retain(|h| h.get("command").and_then(|c| c.as_str()) != Some(command));
            hooks.len() != before
        })
        .unwrap_or(false);
    if arr[pos]
        .get("hooks")
        .and_then(|h| h.as_array())
        .is_some_and(|h| h.is_empty())
    {
        arr.remove(pos);
    }
    changed
}

const OPENCODE_PLUGIN_TEMPLATE: &str = r#"import { Plugin } from "@opencode/plugin"
import { Error as ToolError } from "@opencode/plugin/promise/tool"
import { spawn } from "node:child_process"

// Same routing as the Claude/Codex/agy pre-tool hooks: each subagent call goes through
// the agentdeck binary, which may deny it with a ready delegate_task target.
const BINARY = "{BINARY}"
// T3 starts the shared `opencode serve --service` from its AppImage; servers started elsewhere are not routed.
const inT3 = /t3_code/i.test(process.env.APPIMAGE ?? "")

// Async on purpose: a sync child would stall the whole shared server while the router answers.
function runHook(payload: unknown): Promise<{ decision: string; reason?: string }> {
  return new Promise((resolve) => {
    const child = spawn(BINARY, ["route", "hook", "--harness", "opencode"], { timeout: 58_000 })
    let out = ""
    child.stdout.on("data", (d) => (out += d))
    const failed = { decision: "deny", reason: "model-route failed or timed out; delegation was blocked." }
    child.on("error", () => resolve(failed))
    child.stdin.on("error", () => resolve(failed))
    child.on("close", (code) => {
      if (code !== 0) return resolve(failed)
      try {
        const result = JSON.parse(out)
        resolve(result.decision === "allow" || result.decision === "deny" ? result : failed)
      } catch {
        resolve(failed)
      }
    })
    child.stdin.end(JSON.stringify(payload))
  })
}

export default Plugin.define({
  id: "model-route",
  setup: async (ctx) => {
    if (!inT3) return
    await ctx.tool.hook("execute.before", async (call) => {
      if (call.tool !== "subagent" && !/delegate_task$/.test(call.tool)) return
      const out = await runHook({ tool_name: call.tool === "subagent" ? "subagent" : "delegate_task", tool_input: call.input,
        session_id: call.sessionID, inT3: true })
      if (out.decision === "deny") throw new ToolError({ message: out.reason ?? "Denied by model-route" })
    })
  },
})
"#;

fn opencode_plugin_text(exe: &Path) -> String {
    OPENCODE_PLUGIN_TEMPLATE.replace("{BINARY}", &exe.display().to_string())
}

/// The pre-port plugin file (spawned the extracted `hook.mjs`): recognized
/// so enable upgrades it and disable removes it instead of bailing.
const OPENCODE_PLUGIN_TEMPLATE_LEGACY: &str = r#"import { Plugin } from "@opencode/plugin"
import { Error as ToolError } from "@opencode/plugin/promise/tool"
import { spawn } from "node:child_process"

// Same routing as the Claude/Codex/agy pre-tool hooks: each subagent call goes through
// the agentdeck router copy, which may deny it with a ready delegate_task target.
const HOOK = "{HOOK}"
// T3 starts the shared `opencode serve --service` from its AppImage; servers started elsewhere are not routed.
const inT3 = /t3_code/i.test(process.env.APPIMAGE ?? "")

// Async on purpose: a sync child would stall the whole shared server while the router answers.
function runHook(payload: unknown): Promise<{ decision: string; reason?: string }> {
  return new Promise((resolve) => {
    const child = spawn(HOOK, ["--harness", "opencode"], { timeout: 58_000 })
    let out = ""
    child.stdout.on("data", (d) => (out += d))
    const failed = { decision: "deny", reason: "model-route failed or timed out; delegation was blocked." }
    child.on("error", () => resolve(failed))
    child.stdin.on("error", () => resolve(failed))
    child.on("close", (code) => {
      if (code !== 0) return resolve(failed)
      try {
        const result = JSON.parse(out)
        resolve(result.decision === "allow" || result.decision === "deny" ? result : failed)
      } catch {
        resolve(failed)
      }
    })
    child.stdin.end(JSON.stringify(payload))
  })
}

export default Plugin.define({
  id: "model-route",
  setup: async (ctx) => {
    if (!inT3) return
    await ctx.tool.hook("execute.before", async (call) => {
      if (call.tool !== "subagent" && !/delegate_task$/.test(call.tool)) return
      const out = await runHook({ tool_name: call.tool === "subagent" ? "subagent" : "delegate_task", tool_input: call.input,
        session_id: call.sessionID, inT3: true })
      if (out.decision === "deny") throw new ToolError({ message: out.reason ?? "Denied by model-route" })
    })
  },
})
"#;

fn opencode_plugin_text_legacy(paths: &Paths) -> String {
    OPENCODE_PLUGIN_TEMPLATE_LEGACY.replace(
        "{HOOK}",
        &router_dir(paths).join("hook.mjs").display().to_string(),
    )
}

/// The pre-port hook command (extracted `hook.mjs`): enable replaces it and
/// disable removes it, so a stale entry never fails closed after the port.
/// The standalone `~/.general` copy lives elsewhere and stays untouched.
fn legacy_hook_command(paths: &Paths, harness: &str) -> String {
    format!(
        "{} --harness {harness}",
        router_dir(paths).join("hook.mjs").display()
    )
}

/// The hook of the standalone `~/.general/scripts/model-route`: enable replaces
/// it everywhere it is registered, including secondary Claude accounts.
fn standalone_hook_command(home: &Path, harness: &str) -> String {
    format!(
        "{} --harness {harness}",
        home.join(".general/scripts/model-route/hook.mjs").display()
    )
}

/// Claude settings files holding a router hook: the primary always, secondary
/// accounts (`~/.claude-*`) when they already carry one of ours.
fn claude_hook_targets(home: &Path, paths: &Paths, exe: &Path) -> Vec<PathBuf> {
    let mut targets = vec![claude_settings(home)];
    let commands = [
        standalone_hook_command(home, "claude"),
        legacy_hook_command(paths, "claude"),
        hook_command(exe, "claude"),
    ];
    if let Ok(entries) = std::fs::read_dir(home) {
        let mut dirs: Vec<PathBuf> = entries
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(".claude-"))
            .map(|e| e.path().join("settings.json"))
            .filter(|p| {
                std::fs::read_to_string(p)
                    .is_ok_and(|t| commands.iter().any(|c| t.contains(c.as_str())))
            })
            .collect();
        dirs.sort();
        targets.extend(dirs);
    }
    targets
}

/// Plan hook registration (files only; the old standalone copy stays
/// untouched). Fails closed: any malformed harness file aborts the plan.
/// `replace` backs up a foreign OpenCode plugin file instead of refusing.
pub fn plan_hooks_enable(
    home: &Path,
    paths: &Paths,
    exe: &Path,
    state_dir: &Path,
    replace: bool,
) -> anyhow::Result<Vec<Change>> {
    let mut changes = Vec::new();
    // Claude.
    for path in claude_hook_targets(home, paths, exe) {
        let (old, mut v) = read_json(&path)?;
        let changed = {
            let root = object(&mut v)?;
            let hooks = root
                .entry("hooks")
                .or_insert_with(|| Value::Object(Map::new()));
            let list = pretool_array(object(hooks)?)?;
            remove_hook(list, CLAUDE_MATCHER, &legacy_hook_command(paths, "claude"));
            remove_hook(
                list,
                CLAUDE_MATCHER,
                &standalone_hook_command(home, "claude"),
            );
            insert_hook(list, CLAUDE_MATCHER, &hook_command(exe, "claude"))
        };
        if changed {
            changes.extend(write_json_change(path, old, &v, state_dir));
        }
    }
    // Codex.
    {
        let path = codex_hooks(home);
        let (old, mut v) = read_json(&path)?;
        let changed = {
            let root = object(&mut v)?;
            let hooks = root
                .entry("hooks")
                .or_insert_with(|| Value::Object(Map::new()));
            let list = pretool_array(object(hooks)?)?;
            remove_hook(list, CODEX_MATCHER, &legacy_hook_command(paths, "codex"));
            remove_hook(list, CODEX_MATCHER, &standalone_hook_command(home, "codex"));
            insert_hook(list, CODEX_MATCHER, &hook_command(exe, "codex"))
        };
        if changed {
            changes.extend(write_json_change(path, old, &v, state_dir));
        }
    }
    // agy.
    {
        let path = agy_hooks(home);
        let (old, mut v) = read_json(&path)?;
        let changed = {
            let root = object(&mut v)?;
            let plugin = root
                .entry("model-route")
                .or_insert_with(|| Value::Object(Map::new()));
            let list = pretool_array(object(plugin)?)?;
            remove_hook(list, AGY_MATCHER, &legacy_hook_command(paths, "agy"));
            remove_hook(list, AGY_MATCHER, &standalone_hook_command(home, "agy"));
            insert_hook(list, AGY_MATCHER, &hook_command(exe, "agy"))
        };
        if changed {
            changes.extend(write_json_change(path, old, &v, state_dir));
        }
        let path = agy_config(home);
        let (old, mut v) = read_json(&path)?;
        let changed = {
            let root = object(&mut v)?;
            let plugins = root
                .entry("plugins")
                .or_insert_with(|| Value::Object(Map::new()));
            let entry = object(plugins)?
                .entry("model-route")
                .or_insert_with(|| Value::Object(Map::new()));
            let already = entry.get("enabled") == Some(&Value::Bool(true));
            if !already {
                object(entry)?.insert("enabled".into(), Value::Bool(true));
            }
            !already
        };
        if changed {
            changes.extend(write_json_change(path, old, &v, state_dir));
        }
    }
    // OpenCode plugin file (upgrades the pre-port file in place).
    {
        let path = opencode_plugin(home);
        let text = opencode_plugin_text(exe);
        let old = std::fs::read(&path).ok();
        if old.as_deref() != Some(text.as_bytes()) {
            let standalone = old
                .as_deref()
                .is_some_and(|b| String::from_utf8_lossy(b).contains("model-route/hook.mjs"));
            if old.is_some()
                && !replace
                && !standalone
                && old.as_deref() != Some(opencode_plugin_text_legacy(paths).as_bytes())
            {
                anyhow::bail!(
                    "{} exists and was not written by agentdeck; remove it by hand or retry with --replace",
                    path.display()
                );
            }
            changes.push(Change::WriteFile {
                backup: old
                    .is_some()
                    .then(|| crate::changes::backup_dest(state_dir, "model-route.ts")),
                path,
                contents: text.into_bytes(),
                old,
            });
        }
    }
    Ok(changes)
}

/// Plan hook removal, in the disable order (hooks first: they fail closed).
pub fn plan_hooks_disable(
    home: &Path,
    paths: &Paths,
    exe: &Path,
    state_dir: &Path,
) -> anyhow::Result<Vec<Change>> {
    let mut changes = Vec::new();
    let targets = claude_hook_targets(home, paths, exe)
        .into_iter()
        .map(|p| (p, CLAUDE_MATCHER, "claude"))
        .chain([(codex_hooks(home), CODEX_MATCHER, "codex")]);
    for (path, matcher, harness) in targets {
        let Ok((old, mut v)) = read_json(&path) else {
            continue;
        };
        let Some(old) = old else { continue };
        let changed = as_obj(&mut v)
            .and_then(|root| root.get_mut("hooks"))
            .and_then(as_obj)
            .and_then(|hooks| pretool_array(hooks).ok())
            .is_some_and(|list| {
                remove_hook(list, matcher, &hook_command(exe, harness))
                    | remove_hook(list, matcher, &legacy_hook_command(paths, harness))
            });
        if changed {
            changes.extend(write_json_change(path, Some(old), &v, state_dir));
        }
    }
    {
        let path = agy_hooks(home);
        if let Ok((Some(old), mut v)) = read_json(&path) {
            let changed = as_obj(&mut v)
                .and_then(|root| root.get_mut("model-route"))
                .and_then(as_obj)
                .and_then(|plugin| pretool_array(plugin).ok())
                .is_some_and(|list| {
                    remove_hook(list, AGY_MATCHER, &hook_command(exe, "agy"))
                        | remove_hook(list, AGY_MATCHER, &legacy_hook_command(paths, "agy"))
                });
            if changed {
                changes.extend(write_json_change(path, Some(old), &v, state_dir));
            }
        }
        let path = agy_config(home);
        if let Ok((Some(old), mut v)) = read_json(&path) {
            let changed = as_obj(&mut v)
                .and_then(|root| root.get_mut("plugins"))
                .and_then(as_obj)
                .is_some_and(|plugins| plugins.shift_remove("model-route").is_some());
            if changed {
                changes.extend(write_json_change(path, Some(old), &v, state_dir));
            }
        }
    }
    {
        let path = opencode_plugin(home);
        if let Ok(old) = std::fs::read(&path)
            && (old == opencode_plugin_text(exe).as_bytes()
                || old == opencode_plugin_text_legacy(paths).as_bytes())
        {
            changes.push(Change::Remove { path });
        }
    }
    Ok(changes)
}

/// Read-only hook presence for status output.
pub fn hooks_present(home: &Path, exe: &Path) -> [bool; 4] {
    fn has(path: &Path, pointer: &str, matcher: &str, command: &str) -> bool {
        let Ok(bytes) = std::fs::read(path) else {
            return false;
        };
        let Ok(v) = serde_json::from_slice::<Value>(&bytes) else {
            return false;
        };
        v.pointer(pointer)
            .and_then(|a| a.as_array())
            .is_some_and(|a| {
                a.iter().any(|e| {
                    e.get("matcher").and_then(|m| m.as_str()) == Some(matcher)
                        && e.get("hooks").and_then(|h| h.as_array()).is_some_and(|h| {
                            h.iter()
                                .any(|h| h.get("command").and_then(|c| c.as_str()) == Some(command))
                        })
                })
            })
    }
    [
        has(
            &claude_settings(home),
            "/hooks/PreToolUse",
            CLAUDE_MATCHER,
            &hook_command(exe, "claude"),
        ),
        has(
            &codex_hooks(home),
            "/hooks/PreToolUse",
            CODEX_MATCHER,
            &hook_command(exe, "codex"),
        ),
        has(
            &agy_hooks(home),
            "/model-route/PreToolUse",
            AGY_MATCHER,
            &hook_command(exe, "agy"),
        ),
        std::fs::read_to_string(opencode_plugin(home))
            .is_ok_and(|t| t == opencode_plugin_text(exe)),
    ]
}

/// The T3 Claude provider binary path: a symlink named `claude-t3-shell`
/// pointing at this binary dispatches to `route claude-shell` by argv[0].
/// `AGENTDECK_BIN_DIR` overrides the directory in tests, like the
/// agent-process launcher.
pub fn claude_t3_shell_link(home: &Path) -> PathBuf {
    crate::plugins::bin_dir(home).join("claude-t3-shell")
}

/// Plan the `claude-t3-shell` symlink. A foreign file/link at the path
/// refuses unless `replace` backs it up first (same rule as agent-process).
pub fn plan_shell_link(
    home: &Path,
    exe: &Path,
    replace: bool,
    state_dir: &Path,
) -> anyhow::Result<Vec<Change>> {
    let link = claude_t3_shell_link(home);
    match std::fs::symlink_metadata(&link) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(vec![Change::Symlink {
            link,
            target: exe.to_path_buf(),
        }]),
        Err(e) => Err(e.into()),
        Ok(_) if std::fs::read_link(&link).is_ok_and(|t| t == exe) => Ok(vec![]),
        Ok(_) if replace => Ok(vec![
            Change::BackupRemove {
                path: link.clone(),
                backup: backup_dest(state_dir, "claude-t3-shell"),
            },
            Change::Symlink {
                link,
                target: exe.to_path_buf(),
            },
        ]),
        Ok(_) => anyhow::bail!(
            "{} exists and is not ours; remove it by hand or retry with --replace",
            link.display()
        ),
    }
}

/// Plan `claude-t3-shell` removal: only our own symlink goes; a foreign
/// file is left alone.
pub fn plan_remove_shell_link(home: &Path, exe: &Path) -> Vec<Change> {
    let link = claude_t3_shell_link(home);
    if std::fs::read_link(&link).is_ok_and(|t| t == exe) {
        vec![Change::Remove { path: link }]
    } else {
        vec![]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{AddSpec, Harness};
    use crate::usage::{UsageEntry, UsageKind};

    pub(crate) fn test_paths(tag: &str) -> (Paths, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "agentdeck-router-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        (Paths::new(dir.join("config"), dir.join("state")), dir)
    }

    fn spec(id: &str) -> AddSpec {
        AddSpec {
            id: id.to_string(),
            harness: Harness::Claude,
            display_name: None,
            home: format!("/fake/{id}"),
            binary: None,
            no_t3: false,
        }
    }

    fn usage(id: &str, kind: UsageKind, instance: Option<&str>) -> UsageEntry {
        let mut e = UsageEntry {
            id: id.to_string(),
            kind,
            instance: instance.map(str::to_string),
            display: String::new(),
            config_dir: None,
            codex_home: None,
            token_file: None,
            api_key_env: None,
            db_file: None,
            extra: Default::default(),
        };
        match kind {
            UsageKind::Claude => e.config_dir = Some("~/.claude".into()),
            UsageKind::Codex => e.codex_home = Some("~/.codex".into()),
            UsageKind::Deepseek | UsageKind::Openrouter | UsageKind::Vercel => {
                e.api_key_env = Some("AGENTDECK_TEST_KEY".into());
            }
            UsageKind::Antigravity | UsageKind::OpencodeGo => {}
        }
        e
    }

    #[test]
    fn refresh_writes_files_and_valid_json() {
        let (paths, _g) = test_paths("refresh");
        Registry::update(&paths, |r| {
            r.add(spec("codex"))?;
            r.add(spec("opencode"))?;
            r.add_pool(crate::pool::PoolEntry {
                instance: "codex".into(),
                model: "gpt-6.1-sol".into(),
                options: Default::default(),
                enabled: true,
            })?;
            r.add_pool(crate::pool::PoolEntry {
                instance: "opencode".into(),
                model: "opencode-go/gpt-5".into(),
                options: Default::default(),
                enabled: true,
            })?;
            r.add_usage(usage("codex-main", UsageKind::Codex, Some("codex")))?;
            r.add_usage(usage("openrouter-main", UsageKind::Openrouter, None))?;
            r.add_usage(usage("opencode-go", UsageKind::OpencodeGo, None))?;
            Ok(())
        })
        .unwrap();
        let reg = Registry::load(&paths).unwrap();
        let changes = plan_router_files(&paths, &reg).unwrap();
        // Only router.json; the .mjs assets stay in the repo as fixtures.
        assert_eq!(changes.len(), 1);
        crate::changes::apply_changes(&changes).unwrap();
        let text = std::fs::read_to_string(router_json_path(&paths)).unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        assert!(v["version"].is_number());
        assert_eq!(v["pool"].as_array().unwrap().len(), 2);
        assert_eq!(v["pool"][0]["providerInstanceId"], "codex");
        // Defaults are explicit so the GUI can show the active ban + source.
        assert_eq!(v["userRules"]["source"], "default");
        assert_eq!(v["userRules"]["noReviewModels"][0], "gemini-.*argon");
        assert!(v["userRules"]["denyModels"].as_array().unwrap().is_empty());
        let acc = &v["accounts"];
        assert_eq!(acc["codex"]["usageId"], "codex-main");
        assert_eq!(acc["codex"]["instances"][0], "codex");
        assert_eq!(acc["openrouter"]["usageProvider"], "openrouter");
        assert_eq!(acc["opencode-go"]["modelPrefix"], "opencode-go/");
        // Refresh is idempotent.
        let reg = Registry::load(&paths).unwrap();
        assert!(plan_router_files(&paths, &reg).unwrap().is_empty());
    }

    #[test]
    fn user_rules_flow_into_generated_json() {
        let (paths, _g) = test_paths("rules");
        Registry::update(&paths, |r| r.add(spec("a"))).unwrap();
        let reg = Registry::load(&paths).unwrap();
        assert_eq!(
            router_json_value(&reg).unwrap()["userRules"]["source"],
            "default"
        );
        // Explicit write (even of the defaults) marks the source as user.
        crate::settings::write_user_rules(&paths, &crate::settings::UserRules::default()).unwrap();
        let reg = Registry::load(&paths).unwrap();
        let v = router_json_value(&reg).unwrap();
        assert_eq!(v["userRules"]["source"], "user");
        assert_eq!(v["userRules"]["noReviewModels"][0], "gemini-.*argon");
        // Invalid rules fail generation closed, never a silently unguarded file.
        Registry::update(&paths, |r| {
            use toml::Value;
            let router = r
                .extra
                .entry("router".to_string())
                .or_insert(Value::Table(Default::default()));
            let t = router.as_table_mut().unwrap();
            let mut rules = toml::map::Map::new();
            rules.insert("no_review_models".into(), Value::String("oops".into()));
            t.insert("rules".into(), Value::Table(rules));
            Ok(())
        })
        .unwrap();
        let reg = Registry::load(&paths).unwrap();
        assert!(router_json_value(&reg).is_err());
    }

    #[test]
    fn plan_writes_nothing_by_itself() {
        let (paths, _g) = test_paths("dry");
        Registry::update(&paths, |r| r.add(spec("a"))).unwrap();
        let reg = Registry::load(&paths).unwrap();
        let changes = plan_router_files(&paths, &reg).unwrap();
        assert!(!changes.is_empty());
        assert!(!router_json_path(&paths).exists());
    }

    #[test]
    fn enable_replaces_standalone_hooks_everywhere() {
        let (paths, home) = test_paths("standalone");
        let exe = home.join("agentdeck");
        let standalone = standalone_hook_command(&home, "claude");
        let settings = serde_json::json!({"hooks": {"PreToolUse": [
            {"matcher": CLAUDE_MATCHER, "hooks": [{"type": "command", "command": standalone}]}
        ]}});
        for dir in [".claude", ".claude-second"] {
            std::fs::create_dir_all(home.join(dir)).unwrap();
            std::fs::write(home.join(dir).join("settings.json"), settings.to_string()).unwrap();
        }
        let plugin = opencode_plugin(&home);
        std::fs::create_dir_all(plugin.parent().unwrap()).unwrap();
        std::fs::write(&plugin, "// spawns model-route/hook.mjs\n").unwrap();

        let changes = plan_hooks_enable(&home, &paths, &exe, &paths.state_dir, false).unwrap();
        crate::changes::apply_changes(&changes).unwrap();

        for dir in [".claude", ".claude-second"] {
            let text = std::fs::read_to_string(home.join(dir).join("settings.json")).unwrap();
            assert!(!text.contains("model-route/hook.mjs"), "{dir}: {text}");
            assert!(
                text.contains(&hook_command(&exe, "claude")),
                "{dir}: {text}"
            );
        }
        assert_eq!(
            std::fs::read_to_string(&plugin).unwrap(),
            opencode_plugin_text(&exe)
        );

        let changes = plan_hooks_disable(&home, &paths, &exe, &paths.state_dir).unwrap();
        crate::changes::apply_changes(&changes).unwrap();
        let text = std::fs::read_to_string(home.join(".claude-second/settings.json")).unwrap();
        assert!(!text.contains(&hook_command(&exe, "claude")), "{text}");
    }
}
