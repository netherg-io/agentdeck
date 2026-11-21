//! `agentdeck mcp-server`: stdio MCP server (plugin `mcp-server`) so a model
//! manages skills, the shared instructions and drift for every harness at
//! once instead of patching only its own, and keeps per-T3-thread working
//! state that survives provider switches. Newline-delimited JSON-RPC 2.0.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use agentdeck_coordinator::checkpoint::{
    CheckpointInput, CheckpointJudge, CheckpointVerdict, HeuristicCheckpointJudge,
    HttpCheckpointJudge, record_outcome,
};
use agentdeck_coordinator::hygiene::check_disk_ok;
use agentdeck_coordinator::pool::{PoolSelection, select_coordinator};
use agentdeck_coordinator::t3_control::observe_thread_sqlite;
use agentdeck_core::shared::InstructionChoice;
use agentdeck_core::{Change, Paths, Registry, backlog, manage, plugins, sync, thread_state};
use serde_json::{Value, json};

use crate::cmd_coordinator;

const PROTOCOL: &str = "2025-06-18";

fn tools() -> Value {
    let project = json!({
        "type": "string",
        "description": "Absolute path of a repository for a project-only skill (<project>/.agents/skills, linked into .claude/skills). Omit for every harness."
    });
    let project_name = json!({
        "type": "string",
        "description": "Backlog project slug (a-z, 0-9, '-'), e.g. blockfield"
    });
    let thread = json!({
        "type": "string",
        "description": "T3 thread id. Omit to use this session's thread (T3CODE_THREAD_ID)."
    });
    json!([
        {
            "name": "skills_list",
            "description": "Every skill the harnesses see: name, origin (own, installed, plugin, project, harness, system), description, whether the model may invoke it, per-harness problems, and conflicts (shadowed copies, a shared skill shadowing a Codex system skill, a harness keeping its own copy, a project skill overriding a global one).",
            "inputSchema": {"type": "object", "properties": {"project": project}}
        },
        {
            "name": "skill_install",
            "description": "Install a skill for every harness (or one project). Source: a local dir, owner/repo, owner/repo/sub/dir or a github.com URL; pass `skill` when the source holds several. Refuses names that would shadow a Codex system or plugin skill, and an existing skill unless replace is true.",
            "inputSchema": {"type": "object", "required": ["source"], "properties": {
                "source": {"type": "string"},
                "skill": {"type": "string", "description": "Skill name to pick from a multi-skill source"},
                "project": project,
                "replace": {"type": "boolean"}
            }}
        },
        {
            "name": "skill_remove",
            "description": "Remove a shared or project skill from every harness (backed up first). Plugin, harness-managed and system skills are refused with how to remove them.",
            "inputSchema": {"type": "object", "required": ["name"], "properties": {"name": {"type": "string"}, "project": project}}
        },
        {
            "name": "skill_invocation",
            "description": "Let the model invoke a skill on its own (model=true) or only when the user calls it (model=false). Sets Claude's disable-model-invocation and Codex's implicit-invocation policy.",
            "inputSchema": {"type": "object", "required": ["name", "model"], "properties": {"name": {"type": "string"}, "model": {"type": "boolean"}, "project": project}}
        },
        {
            "name": "instructions_read",
            "description": "The shared global instructions every harness links to, with agentdeck's managed blocks shown as one-line notes, and the link state of each harness file.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "instructions_edit",
            "description": "Edit the shared global instructions for every harness: replace the exact, unique `old` text with `new` (empty `new` deletes it), or with `old` empty append `new` as new rules. Managed agentdeck blocks cannot be edited. Returns the diff.",
            "inputSchema": {"type": "object", "required": ["old", "new"], "properties": {"old": {"type": "string"}, "new": {"type": "string"}}}
        },
        {
            "name": "drift_list",
            "description": "Drift between harnesses and agentdeck's shared state: additive items (safe to apply) and decisions (need a choice).",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "drift_apply",
            "description": "Apply every additive drift item (relink skills and instructions, restore agentdeck blocks). Decisions stay.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "state_read",
            "description": "Saved working state of this T3 thread (or the thread it was forked from): the snapshot (goal, scope, done, next), the owner's verbatim decisions, run phase/mandate, Owner Inbox and Assumptions. Read it first after a handoff, compaction, limit pause or \"continue\". Returns `state_stale` (true when the snapshot lags checklist movement) and `checklist_updates_since_state`.",
            "inputSchema": {"type": "object", "properties": {"thread": thread}}
        },
        {
            "name": "state_write",
            "description": "Save this T3 thread's working state. `state` replaces the snapshot (first line = the goal; cite issues and files instead of copying them; max 16 KiB) and resets freshness tracking. `decision` appends one owner decision or consent verbatim with a timestamp; decisions are never removed. `phase` sets the run phase (intake, running, sleeping_for_quota, completed), `mandate` the structured Run Mandate, `sleep_until` an RFC3339 deadline (null clears it), `record_checkpoint` records a Checkpoint (refused while `state_stale`).",
            "inputSchema": {"type": "object", "properties": {
                "state": {"type": "string"},
                "decision": {"type": "string"},
                "phase": {"type": "string", "enum": ["intake", "running", "sleeping_for_quota", "completed"]},
                "mandate": {"type": "object", "description": "Structured Run Mandate (project, allowed_hosts, allowed_apps, allow_releases, fix_forward_max, review_rounds_max, notes)"},
                "sleep_until": {"type": ["string", "null"], "description": "RFC3339 UTC deadline while sleeping_for_quota; null clears it"},
                "record_checkpoint": {"type": "boolean"},
                "thread": thread
            }}
        },
        {
            "name": "checklist_update",
            "description": "The owner's progress checklist for this T3 thread, shown in agentdeck's GUI and applet. Upsert items by `id` (a new id needs `title`; status defaults to todo) and drop `remove` ids. Status: todo, running, review, done, blocked, held. `note` is one line on what runs or blocks now; `links` cite PRs, issues and backlog items. `project` binds the thread to a backlog project. Update it whenever an item changes state.",
            "inputSchema": {"type": "object", "properties": {
                "items": {"type": "array", "items": {"type": "object", "required": ["id"], "properties": {
                    "id": {"type": "string"},
                    "title": {"type": "string"},
                    "status": {"type": "string", "enum": ["todo", "running", "review", "done", "blocked", "held"]},
                    "note": {"type": "string"},
                    "links": {"type": "array", "items": {"type": "string"}}
                }}},
                "remove": {"type": "array", "items": {"type": "string"}},
                "project": project_name,
                "thread": thread
            }}
        },
        {
            "name": "owner_inbox_add",
            "description": "Park one branch in the Owner Inbox: work that physically needs the owner (credentials, manual external actions) or exceeds the Run Mandate. Only that branch parks; the rest of the backlog continues. `action` is the ready command, script path or link for the owner.",
            "inputSchema": {"type": "object", "required": ["title", "reason", "action"], "properties": {
                "title": {"type": "string"},
                "reason": {"type": "string"},
                "action": {"type": "string"},
                "id": {"type": "string", "description": "Caller-provided slug; generated as inbox-N when omitted"},
                "blocked_items": {"type": "array", "items": {"type": "string"}},
                "thread": thread
            }}
        },
        {
            "name": "owner_inbox_resolve",
            "description": "Mark one Owner Inbox item resolved (the owner handled it or it lapsed).",
            "inputSchema": {"type": "object", "required": ["id"], "properties": {
                "id": {"type": "string"},
                "thread": thread
            }}
        },
        {
            "name": "owner_inbox_list",
            "description": "List this thread's Owner Inbox items, open ones first. Pass `include_resolved: true` to also see resolved items.",
            "inputSchema": {"type": "object", "properties": {
                "include_resolved": {"type": "boolean"},
                "thread": thread
            }}
        },
        {
            "name": "assumption_add",
            "description": "Record one reversible autonomous decision (product or technical detail) with a revert hint, so the owner can review or override it in the Run Summary.",
            "inputSchema": {"type": "object", "required": ["decision", "rationale", "revert_hint"], "properties": {
                "decision": {"type": "string"},
                "rationale": {"type": "string"},
                "revert_hint": {"type": "string"},
                "id": {"type": "string", "description": "Caller-provided slug; generated as asm-N when omitted"},
                "thread": thread
            }}
        },
        {
            "name": "assumption_list",
            "description": "List this thread's recorded Assumptions.",
            "inputSchema": {"type": "object", "properties": {"thread": thread}}
        },
        {
            "name": "coordinator_next_target",
            "description": "Select the best Coordinator Pool target from live T3 usage caches (fullest 5h window while guarding weekly reserve), or return sleeping_for_quota with wake_at when all entries are exhausted, plus whether proactive rotation is recommended.",
            "inputSchema": {"type": "object", "properties": {
                "current_instance": {"type": "string", "description": "Current T3 provider instance id (defaults to the thread's observed instance)"},
                "thread": thread
            }}
        },
        {
            "name": "coordinator_checkpoint_check",
            "description": "Check whether this Coordinator thread is at a clean Checkpoint boundary. Refuses immediately if `state_stale` is true; otherwise checks soft thresholds / milestone / active children / hazards and returns { should_checkpoint, state_stale, confidence, reason, recommended_target }.",
            "inputSchema": {"type": "object", "properties": {
                "milestone_just_completed": {"type": "boolean"},
                "run_count_since_checkpoint": {"type": "integer"},
                "item_count_since_checkpoint": {"type": "integer"},
                "active_children": {"type": "integer"},
                "recent_events_tail": {"type": "array", "items": {"type": "string"}},
                "current_instance": {"type": "string"},
                "thread": thread
            }}
        },
        {
            "name": "coordinator_hygiene_check",
            "description": "Check free disk space on the workspace partition against [coordinator] min_free_disk_gb and return { ok, free_gb, min_gb }.",
            "inputSchema": {"type": "object", "properties": {
                "path": {"type": "string", "description": "Directory to inspect; defaults to the server's current working directory"}
            }}
        },
        {
            "name": "backlog_list",
            "description": "A project's coordinator backlog (findings, follow-ups, deferred steps, owner decisions to act on). Without `id`: one line per item; with `id`: the full item with history. Status filter: open (default), closed or all; `label` filters by label such as next-release or partial.",
            "inputSchema": {"type": "object", "required": ["project"], "properties": {
                "project": project_name,
                "id": {"type": "integer"},
                "status": {"type": "string", "enum": ["open", "closed", "all"]},
                "label": {"type": "string"}
            }}
        },
        {
            "name": "backlog_add",
            "description": "Add a backlog item at once for anything unfinished: a finding, follow-up, deferred step or verbatim owner decision to act on. Returns its id; cite it as `backlog:<project>#<id>`.",
            "inputSchema": {"type": "object", "required": ["project", "title"], "properties": {
                "project": project_name,
                "title": {"type": "string"},
                "body": {"type": "string", "description": "Report, evidence paths, repo@sha"},
                "close_when": {"type": "string", "description": "Closing criterion"},
                "labels": {"type": "array", "items": {"type": "string"}},
                "links": {"type": "array", "items": {"type": "string"}}
            }}
        },
        {
            "name": "backlog_update",
            "description": "Change a backlog item: fields replace, `note` appends a dated history line, `close` ({pr, sha}) closes it once the fix is merged with green CI, `reject` (reason) closes it without a fix, `done` (evidence) closes an item whose work has no PR (prod config, a rotation, a check), `reopen` opens it again.",
            "inputSchema": {"type": "object", "required": ["project", "id"], "properties": {
                "project": project_name,
                "id": {"type": "integer"},
                "title": {"type": "string"},
                "body": {"type": "string"},
                "close_when": {"type": "string"},
                "labels": {"type": "array", "items": {"type": "string"}},
                "links": {"type": "array", "items": {"type": "string"}},
                "note": {"type": "string"},
                "close": {"type": "object", "required": ["pr", "sha"], "properties": {"pr": {"type": "string"}, "sha": {"type": "string"}}},
                "reject": {"type": "string", "description": "Close without a fix: the reason (obsolete, duplicate, declined by the owner)"},
                "done": {"type": "string", "description": "Close work that has no PR: what was done and how it was verified"},
                "reopen": {"type": "boolean"}
            }}
        },
        {
            "name": "drift_resolve",
            "description": "Resolve one instruction-file decision from drift_list: `shared` makes the file a link to the shared instructions (its local text is backed up), `local` makes the file's text the shared instructions.",
            "inputSchema": {"type": "object", "required": ["path", "choice"], "properties": {
                "path": {"type": "string"},
                "choice": {"type": "string", "enum": ["shared", "local"]}
            }}
        }
    ])
}

fn home() -> anyhow::Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("HOME is not set"))
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

fn project_arg(args: &Value) -> anyhow::Result<Option<PathBuf>> {
    match str_arg(args, "project") {
        None => Ok(None),
        Some(p) => {
            let p = PathBuf::from(p);
            if !p.is_absolute() || !p.is_dir() {
                anyhow::bail!("project must be an existing absolute dir");
            }
            Ok(Some(p))
        }
    }
}

const COORDINATOR_TOOLS: [&str; 12] = [
    "checklist_update",
    "backlog_list",
    "backlog_add",
    "backlog_update",
    "owner_inbox_add",
    "owner_inbox_resolve",
    "owner_inbox_list",
    "assumption_add",
    "assumption_list",
    "coordinator_next_target",
    "coordinator_checkpoint_check",
    "coordinator_hygiene_check",
];

fn coordinator_enabled(paths: &Paths) -> bool {
    Registry::load(paths).is_ok_and(|r| plugins::plugin_enabled(&r, plugins::PLUGIN_COORDINATOR))
}

fn session_thread() -> Option<String> {
    std::env::var(thread_state::THREAD_ENV)
        .ok()
        .filter(|s| !s.is_empty())
}

fn thread_arg(args: &Value) -> anyhow::Result<String> {
    str_arg(args, "thread")
        .map(str::to_string)
        .or_else(session_thread)
        .ok_or_else(|| {
            anyhow::anyhow!("no T3 thread: pass `thread` (this session has no T3CODE_THREAD_ID)")
        })
}

fn describe(changes: &[Change]) -> Vec<String> {
    changes.iter().map(Change::describe).collect()
}

fn skill_dir(
    paths: &Paths,
    home: &Path,
    name: &str,
    project: Option<&Path>,
) -> anyhow::Result<PathBuf> {
    let candidates: Vec<PathBuf> = match project {
        Some(p) => vec![manage::project_skills_dir(p).join(name)],
        None => agentdeck_core::shared::configured_sources(paths, home)
            .into_iter()
            .rev()
            .map(|d| d.join(name))
            .collect(),
    };
    candidates
        .into_iter()
        .find(|d| d.join("SKILL.md").is_file() && !d.is_symlink())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no editable skill named {name} (plugin and harness skills are managed elsewhere)"
            )
        })
}

async fn call(paths: &Paths, name: &str, args: &Value) -> anyhow::Result<Value> {
    let home = home()?;
    match name {
        "skills_list" => {
            let project = project_arg(args)?;
            let (skills, conflicts) = manage::list_skills(paths, &home, project.as_deref());
            Ok(json!({ "skills": skills, "conflicts": conflicts }))
        }
        "skill_install" => {
            let source =
                str_arg(args, "source").ok_or_else(|| anyhow::anyhow!("source is required"))?;
            let project = project_arg(args)?;
            let root = manage::fetch(source, &paths.state_dir)?;
            let found = manage::find_skills(&root);
            let pick = str_arg(args, "skill");
            let src = match (pick, found.as_slice()) {
                (Some(n), _) => found
                    .iter()
                    .find(|d| d.file_name().is_some_and(|f| f == n))
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("{source} has no skill {n}"))?,
                (None, [one]) => one.clone(),
                (None, []) => anyhow::bail!("{source} has no SKILL.md"),
                (None, many) => anyhow::bail!(
                    "{source} holds several skills, pass `skill`: {}",
                    many.iter()
                        .filter_map(|d| d.file_name().map(|f| f.to_string_lossy().into_owned()))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            };
            let replace = args
                .get("replace")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let (skill, changes) =
                manage::plan_install(paths, &home, &src, source, project.as_deref(), replace)?;
            match &project {
                Some(_) => agentdeck_core::apply_changes(&changes)?,
                None => manage::apply_skills(paths, &home, &changes)?,
            }
            Ok(json!({ "installed": skill, "changes": describe(&changes) }))
        }
        "skill_remove" => {
            let skill = str_arg(args, "name").ok_or_else(|| anyhow::anyhow!("name is required"))?;
            let project = project_arg(args)?;
            let changes = manage::plan_remove(paths, &home, skill, project.as_deref())?;
            match &project {
                Some(_) => agentdeck_core::apply_changes(&changes)?,
                None => manage::apply_skills(paths, &home, &changes)?,
            }
            Ok(json!({ "removed": skill, "changes": describe(&changes) }))
        }
        "skill_invocation" => {
            let skill = str_arg(args, "name").ok_or_else(|| anyhow::anyhow!("name is required"))?;
            let model = args
                .get("model")
                .and_then(Value::as_bool)
                .ok_or_else(|| anyhow::anyhow!("model must be true or false"))?;
            let project = project_arg(args)?;
            let dir = skill_dir(paths, &home, skill, project.as_deref())?;
            let changes = manage::plan_invocation(&dir, model)?;
            match &project {
                Some(_) => agentdeck_core::apply_changes(&changes)?,
                // Antigravity gets copies, so they must be refreshed too.
                None => manage::apply_skills(paths, &home, &changes)?,
            }
            Ok(json!({ "skill": skill, "model_invocable": model, "changes": describe(&changes) }))
        }
        "instructions_read" => Ok(serde_json::to_value(manage::read_instructions(&home)?)?),
        "instructions_edit" => {
            let old = args.get("old").and_then(Value::as_str).unwrap_or("");
            let new = args.get("new").and_then(Value::as_str).unwrap_or("");
            let changes = manage::plan_instructions_edit(&home, &paths.state_dir, old, new)?;
            agentdeck_core::apply_changes(&changes)?;
            Ok(json!({ "changes": describe(&changes) }))
        }
        "drift_list" => {
            let t3 = agentdeck_core::t3::t3_root().ok();
            Ok(json!({ "items": sync::gather(paths, &home, t3.as_deref())? }))
        }
        "drift_apply" => {
            let t3 = agentdeck_core::t3::t3_root().ok();
            let changes = sync::apply_additive(paths, &home, t3.as_deref())?;
            Ok(json!({ "changes": describe(&changes) }))
        }
        "drift_resolve" => {
            let path = str_arg(args, "path").ok_or_else(|| anyhow::anyhow!("path is required"))?;
            let choice = match str_arg(args, "choice") {
                Some("shared") => InstructionChoice::UseShared,
                Some("local") => InstructionChoice::UseLocal,
                _ => anyhow::bail!("choice must be shared or local"),
            };
            let plan = agentdeck_core::shared::plan_instruction_resolution(
                &home,
                &paths.state_dir,
                Path::new(path),
                choice,
            )?;
            agentdeck_core::shared::apply_instruction_resolution(&home, &plan)?;
            Ok(json!({ "diff": plan.diff, "changes": describe(&plan.changes) }))
        }
        "state_read" => {
            let id = thread_arg(args)?;
            let t3 = agentdeck_core::t3::t3_root().ok();
            match thread_state::read(paths, t3.as_deref(), &id)? {
                Some(f) => {
                    let mut v = serde_json::to_value(f)?;
                    v["state_stale"] = json!(v["checklist_updates_since_state"]
                        .as_u64()
                        .unwrap_or(0)
                        >= 5
                        || (v["state"].as_str().unwrap_or("").trim().is_empty()
                            && !v["checklist"].as_array().is_none_or(Vec::is_empty)));
                    Ok(v)
                }
                None => Ok(json!({ "thread": id, "state": null, "state_stale": false, "checklist_updates_since_state": 0 })),
            }
        }
        "state_write" => {
            let id = thread_arg(args)?;
            let t3 = agentdeck_core::t3::t3_root().ok();
            let t3r = t3.as_deref();
            let has_snapshot = args.get("state").and_then(Value::as_str).is_some()
                || args.get("decision").and_then(Value::as_str).is_some_and(|s| !s.is_empty());
            let phase: Option<thread_state::RunPhase> = match args.get("phase") {
                None | Some(Value::Null) => None,
                Some(v) => Some(serde_json::from_value(v.clone())?),
            };
            let mandate: Option<thread_state::RunMandate> = match args.get("mandate") {
                None | Some(Value::Null) => None,
                Some(v) => Some(serde_json::from_value(v.clone())?),
            };
            let sleep_until: Option<Option<String>> = match args.get("sleep_until") {
                None => None,
                Some(Value::Null) => Some(None),
                Some(v) => match v.as_str() {
                    Some("") => Some(None),
                    Some(s) => Some(Some(s.to_string())),
                    None => anyhow::bail!("sleep_until must be an RFC3339 string or null"),
                },
            };
            let record_checkpoint = args
                .get("record_checkpoint")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let meta_requested =
                phase.is_some() || mandate.is_some() || sleep_until.is_some() || record_checkpoint;
            if !has_snapshot && !meta_requested {
                anyhow::bail!("pass state, decision, phase, mandate, sleep_until or record_checkpoint");
            }
            let mut current = if has_snapshot {
                thread_state::write(
                    paths,
                    t3r,
                    &id,
                    args.get("state").and_then(Value::as_str),
                    str_arg(args, "decision"),
                )?
            } else {
                thread_state::read(paths, t3r, &id)?.map(|f| f.data).unwrap_or_default()
            };
            if meta_requested {
                current = thread_state::set_run_meta(
                    paths,
                    t3r,
                    &id,
                    phase,
                    mandate,
                    sleep_until,
                    record_checkpoint,
                )?;
            }
            let stale = current.state_stale();
            let mut out = json!({
                "thread": id,
                "updated": current.updated,
                "decisions": current.decisions.len(),
                "phase": current.phase,
                "state_stale": stale,
                "checklist_updates_since_state": current.checklist_updates_since_state,
                "checkpoint_count": current.checkpoint_count,
            });
            if stale {
                out["warning"] = json!(
                    "state is stale: rewrite the snapshot before the next Checkpoint"
                );
            }
            Ok(out)
        }
        "checklist_update"
        | "backlog_list"
        | "backlog_add"
        | "backlog_update"
        | "owner_inbox_add"
        | "owner_inbox_resolve"
        | "owner_inbox_list"
        | "assumption_add"
        | "assumption_list"
        | "coordinator_next_target"
        | "coordinator_checkpoint_check"
        | "coordinator_hygiene_check"
            if !coordinator_enabled(paths) =>
        {
            anyhow::bail!("the agentdeck coordinator plugin is disabled")
        }
        "checklist_update" => {
            let id = thread_arg(args)?;
            let t3 = agentdeck_core::t3::t3_root().ok();
            let items: Vec<thread_state::ItemPatch> =
                serde_json::from_value(args.get("items").cloned().unwrap_or(json!([])))?;
            let remove: Vec<String> =
                serde_json::from_value(args.get("remove").cloned().unwrap_or(json!([])))?;
            let (s, report) = thread_state::update_checklist(
                paths,
                t3.as_deref(),
                &id,
                str_arg(args, "project"),
                &items,
                &remove,
            )?;
            if let Some(r) = report {
                crate::sync_watch::notify_send("Координатор".into(), r.text());
            }
            let (done, total) = s.progress();
            Ok(json!({ "thread": id, "done": done, "total": total, "updated": s.updated }))
        }
        "backlog_list" => {
            let project =
                str_arg(args, "project").ok_or_else(|| anyhow::anyhow!("project is required"))?;
            let b = backlog::load(paths, project)?;
            if let Some(n) = args.get("id").and_then(Value::as_u64) {
                let e = b.items.into_iter().find(|e| e.id == n);
                return e
                    .map(|e| Ok(serde_json::to_value(e)?))
                    .unwrap_or_else(|| anyhow::bail!("{project} has no backlog item {n}"));
            }
            let status = str_arg(args, "status").unwrap_or("open");
            let label = str_arg(args, "label");
            let rows: Vec<Value> = b
                .items
                .iter()
                .filter(|e| status == "all" || e.status == status)
                .filter(|e| label.is_none_or(|l| e.labels.iter().any(|x| x == l)))
                .map(|e| json!({ "id": e.id, "title": e.title, "status": e.status, "labels": e.labels, "links": e.links, "updated": e.updated }))
                .collect();
            Ok(json!({ "project": project, "items": rows }))
        }
        "backlog_add" => {
            let project =
                str_arg(args, "project").ok_or_else(|| anyhow::anyhow!("project is required"))?;
            let new: backlog::NewEntry = serde_json::from_value(args.clone())?;
            let e = backlog::add(paths, project, new)?;
            Ok(json!({ "id": e.id, "ref": format!("backlog:{project}#{}", e.id) }))
        }
        "backlog_update" => {
            let project =
                str_arg(args, "project").ok_or_else(|| anyhow::anyhow!("project is required"))?;
            let id = args
                .get("id")
                .and_then(Value::as_u64)
                .ok_or_else(|| anyhow::anyhow!("id is required"))?;
            let patch: backlog::Patch = serde_json::from_value(args.clone())?;
            let e = backlog::update(paths, project, id, patch)?;
            Ok(json!({ "id": e.id, "status": e.status, "updated": e.updated }))
        }
        "owner_inbox_add" => {
            let id = thread_arg(args)?;
            let t3 = agentdeck_core::t3::t3_root().ok();
            let blocked: Vec<String> =
                serde_json::from_value(args.get("blocked_items").cloned().unwrap_or(json!([])))?;
            let item = thread_state::inbox_add(
                paths,
                t3.as_deref(),
                &id,
                thread_state::NewInboxItem {
                    id: args
                        .get("id")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    title: str_arg(args, "title")
                        .ok_or_else(|| anyhow::anyhow!("title is required"))?
                        .to_string(),
                    reason: str_arg(args, "reason")
                        .ok_or_else(|| anyhow::anyhow!("reason is required"))?
                        .to_string(),
                    action: str_arg(args, "action")
                        .ok_or_else(|| anyhow::anyhow!("action is required"))?
                        .to_string(),
                    blocked_items: blocked,
                },
            )?;
            Ok(serde_json::to_value(item)?)
        }
        "owner_inbox_resolve" => {
            let thread = thread_arg(args)?;
            let t3 = agentdeck_core::t3::t3_root().ok();
            let item_id =
                str_arg(args, "id").ok_or_else(|| anyhow::anyhow!("id is required"))?;
            let item = thread_state::inbox_resolve(paths, t3.as_deref(), &thread, item_id)?;
            Ok(serde_json::to_value(item)?)
        }
        "owner_inbox_list" => {
            let thread = thread_arg(args)?;
            let t3 = agentdeck_core::t3::t3_root().ok();
            let found = thread_state::read(paths, t3.as_deref(), &thread)?;
            let include_resolved = args
                .get("include_resolved")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let items: Vec<Value> = found
                .map(|f| f.data.owner_inbox)
                .unwrap_or_default()
                .into_iter()
                .filter(|i| include_resolved || !i.resolved)
                .map(|i| serde_json::to_value(i).unwrap_or(Value::Null))
                .collect();
            Ok(json!({ "thread": thread, "items": items }))
        }
        "assumption_add" => {
            let thread = thread_arg(args)?;
            let t3 = agentdeck_core::t3::t3_root().ok();
            let item = thread_state::assumption_add(
                paths,
                t3.as_deref(),
                &thread,
                thread_state::NewAssumption {
                    id: args
                        .get("id")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    decision: str_arg(args, "decision")
                        .ok_or_else(|| anyhow::anyhow!("decision is required"))?
                        .to_string(),
                    rationale: str_arg(args, "rationale")
                        .ok_or_else(|| anyhow::anyhow!("rationale is required"))?
                        .to_string(),
                    revert_hint: str_arg(args, "revert_hint")
                        .ok_or_else(|| anyhow::anyhow!("revert_hint is required"))?
                        .to_string(),
                },
            )?;
            Ok(serde_json::to_value(item)?)
        }
        "assumption_list" => {
            let thread = thread_arg(args)?;
            let t3 = agentdeck_core::t3::t3_root().ok();
            let items = thread_state::read(paths, t3.as_deref(), &thread)?
                .map(|f| f.data.assumptions)
                .unwrap_or_default();
            Ok(json!({ "thread": thread, "items": items }))
        }
        "coordinator_next_target" => {
            let reg = Registry::load(paths)?;
            let cfg = cmd_coordinator::coordinator_config(&reg)?;
            let t3_root = agentdeck_core::t3::t3_root().ok();
            let current_instance = str_arg(args, "current_instance")
                .map(str::to_string)
                .or_else(|| {
                    let tid = str_arg(args, "thread")
                        .map(str::to_string)
                        .or_else(session_thread)?;
                    let root = t3_root.as_deref()?;
                    observe_thread_sqlite(root, &tid)
                        .ok()
                        .flatten()
                        .and_then(|o| o.provider_instance_id)
                });
            let quotas = cmd_coordinator::collect_pool_quotas(&cfg, t3_root.as_deref());
            let now = cmd_coordinator::now_epoch_secs();
            let selection = select_coordinator(&cfg, &quotas, now, current_instance.as_deref());
            let proactive = cmd_coordinator::proactive_rotate_recommended(
                &cfg,
                &quotas,
                current_instance.as_deref(),
            );
            match selection {
                PoolSelection::Selected {
                    entry,
                    five_hour_pct,
                    weekly_pct,
                    reason,
                } => Ok(json!({
                    "status": "selected",
                    "target": entry,
                    "five_hour_remaining_pct": five_hour_pct,
                    "weekly_remaining_pct": weekly_pct,
                    "proactive_rotate": proactive,
                    "reason": reason,
                })),
                PoolSelection::SleepForQuota {
                    wake_at_epoch_secs,
                    earliest_reset_epoch_secs,
                    next_candidate,
                    reason,
                } => Ok(json!({
                    "status": "sleeping_for_quota",
                    "wake_at": cmd_coordinator::format_utc_rfc3339(wake_at_epoch_secs),
                    "wake_at_epoch_secs": wake_at_epoch_secs,
                    "earliest_reset_epoch_secs": earliest_reset_epoch_secs,
                    "next_candidate": next_candidate,
                    "proactive_rotate": false,
                    "reason": reason,
                })),
                PoolSelection::NoPoolConfigured => Ok(json!({
                    "status": "no_pool_configured",
                    "proactive_rotate": false,
                    "reason": "no Coordinator Pool entries configured in [coordinator]",
                })),
            }
        }
        "coordinator_checkpoint_check" => {
            let thread = thread_arg(args)?;
            let reg = Registry::load(paths)?;
            let cfg = cmd_coordinator::coordinator_config(&reg)?;
            let t3_root = agentdeck_core::t3::t3_root().ok();
            let found = thread_state::read(paths, t3_root.as_deref(), &thread)?;
            let state_stale = found.as_ref().is_some_and(|f| f.data.state_stale());
            let observed = t3_root
                .as_deref()
                .and_then(|root| observe_thread_sqlite(root, &thread).ok().flatten());
            let current_instance = str_arg(args, "current_instance")
                .map(str::to_string)
                .or_else(|| {
                    observed
                        .as_ref()
                        .and_then(|o| o.provider_instance_id.clone())
                });
            let quotas = cmd_coordinator::collect_pool_quotas(&cfg, t3_root.as_deref());
            let recommended_target = match select_coordinator(
                &cfg,
                &quotas,
                cmd_coordinator::now_epoch_secs(),
                current_instance.as_deref(),
            ) {
                PoolSelection::Selected { entry, .. } => Some(entry),
                _ => None,
            };
            if state_stale {
                return Ok(json!({
                    "should_checkpoint": false,
                    "state_stale": true,
                    "confidence": 1.0,
                    "reason": "state snapshot is stale; rewrite state before checkpoint",
                    "recommended_target": recommended_target,
                }));
            }
            let recent_events_tail: Vec<String> = match args.get("recent_events_tail") {
                Some(v) => serde_json::from_value(v.clone())?,
                None => observed
                    .as_ref()
                    .filter(|o| !o.last_message_text.is_empty())
                    .map(|o| vec![o.last_message_text.clone()])
                    .unwrap_or_default(),
            };
            let input = CheckpointInput {
                run_count_since_checkpoint: args
                    .get("run_count_since_checkpoint")
                    .and_then(Value::as_u64)
                    .map(|v| v as u32)
                    .unwrap_or_else(|| observed.as_ref().map(|o| o.run_count as u32).unwrap_or(0)),
                item_count_since_checkpoint: args
                    .get("item_count_since_checkpoint")
                    .and_then(Value::as_u64)
                    .map(|v| v as u32)
                    .unwrap_or_else(|| {
                        observed.as_ref().map(|o| o.item_count as u32).unwrap_or(0)
                    }),
                milestone_just_completed: args
                    .get("milestone_just_completed")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                state_stale,
                active_children: args
                    .get("active_children")
                    .and_then(Value::as_u64)
                    .map(|v| v as usize)
                    .unwrap_or_else(|| {
                        observed
                            .as_ref()
                            .map(|o| o.pending_background_tasks as usize)
                            .unwrap_or(0)
                    }),
                recent_events_tail,
            };
            let verdict = match HttpCheckpointJudge::from_env(&cfg) {
                Ok(http) => match http.judge(&input, &cfg).await {
                    Ok(v) => v,
                    Err(e) => CheckpointVerdict::refuse(&format!("checkpoint judge error: {e}")),
                },
                Err(_) => HeuristicCheckpointJudge.judge(&input, &cfg).await?,
            };
            let _ = record_outcome(&paths.state_dir, &input, &verdict);
            let should_checkpoint =
                verdict.should_checkpoint && verdict.confidence >= cfg.checkpoint_confidence_threshold;
            Ok(json!({
                "should_checkpoint": should_checkpoint,
                "state_stale": state_stale,
                "confidence": verdict.confidence,
                "reason": verdict.reason,
                "recommended_target": recommended_target,
            }))
        }
        "coordinator_hygiene_check" => {
            let reg = Registry::load(paths)?;
            let cfg = cmd_coordinator::coordinator_config(&reg)?;
            let dir = match str_arg(args, "path") {
                Some(p) => PathBuf::from(p),
                None => std::env::current_dir()?,
            };
            let check = check_disk_ok(&dir, cfg.min_free_disk_gb);
            Ok(json!({
                "ok": check.ok,
                "free_gb": check.free_gb,
                "min_gb": check.min_gb,
                "path": dir,
            }))
        }
        other => anyhow::bail!("unknown tool {other}"),
    }
}

/// Sent once per session start, so a new provider session after a T3
/// handoff learns the goal even when the handoff summary dropped it.
fn init_instructions(paths: &Paths) -> String {
    let enabled = Registry::load(paths)
        .is_ok_and(|r| plugins::plugin_enabled(&r, plugins::PLUGIN_MCP_SERVER));
    let found = session_thread().filter(|_| enabled).and_then(|id| {
        let t3 = agentdeck_core::t3::t3_root().ok();
        thread_state::read(paths, t3.as_deref(), &id).ok().flatten()
    });
    thread_state::instructions(found.as_ref())
}

async fn handle(paths: &Paths, msg: &Value) -> Option<Value> {
    let id = msg.get("id").cloned()?; // notifications get no reply
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let result = match method {
        "initialize" => json!({
            "protocolVersion": msg.pointer("/params/protocolVersion").and_then(Value::as_str).unwrap_or(PROTOCOL),
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "agentdeck", "version": env!("CARGO_PKG_VERSION") },
            "instructions": init_instructions(paths),
        }),
        "ping" => json!({}),
        "tools/list" => {
            let on = coordinator_enabled(paths);
            let list: Vec<Value> = tools()
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|t| on || !COORDINATOR_TOOLS.contains(&t["name"].as_str().unwrap_or("")))
                .collect();
            json!({ "tools": list })
        }
        "tools/call" => {
            let name = msg
                .pointer("/params/name")
                .and_then(Value::as_str)
                .unwrap_or("");
            let args = msg
                .pointer("/params/arguments")
                .cloned()
                .unwrap_or(json!({}));
            // A disabled plugin must not keep acting through a stale registration.
            let enabled = Registry::load(paths)
                .is_ok_and(|r| plugins::plugin_enabled(&r, plugins::PLUGIN_MCP_SERVER));
            let out = if enabled {
                call(paths, name, &args).await
            } else {
                Err(anyhow::anyhow!(
                    "the agentdeck mcp-server plugin is disabled"
                ))
            };
            match out {
                Ok(v) => {
                    json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&v).unwrap_or_default() }] })
                }
                Err(e) => {
                    json!({ "content": [{ "type": "text", "text": format!("{e:#}") }], "isError": true })
                }
            }
        }
        _ => {
            return Some(json!({
                "jsonrpc": "2.0", "id": id,
                "error": { "code": -32601, "message": format!("method not found: {method}") }
            }));
        }
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

pub async fn run(paths: &Paths) -> anyhow::Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(msg) => handle(paths, &msg).await,
            Err(e) => Some(json!({
                "jsonrpc": "2.0", "id": null,
                "error": { "code": -32700, "message": format!("parse error: {e}") }
            })),
        };
        if let Some(reply) = reply {
            writeln!(stdout, "{reply}")?;
            stdout.flush()?;
        }
    }
    Ok(())
}
