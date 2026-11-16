//! `agentdeck mcp-server`: stdio MCP server (plugin `mcp-server`) so a model
//! manages skills, the shared instructions and drift for every harness at
//! once instead of patching only its own, and keeps per-T3-thread working
//! state that survives provider switches. Newline-delimited JSON-RPC 2.0.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use agentdeck_core::shared::InstructionChoice;
use agentdeck_core::{Change, Paths, Registry, backlog, manage, plugins, sync, thread_state};
use serde_json::{Value, json};

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
            "description": "Saved working state of this T3 thread (or the thread it was forked from): the snapshot (goal, scope, done, next) and the owner's verbatim decisions. Read it first after a handoff, compaction, limit pause or \"continue\".",
            "inputSchema": {"type": "object", "properties": {"thread": thread}}
        },
        {
            "name": "state_write",
            "description": "Save this T3 thread's working state. `state` replaces the snapshot (first line = the goal; cite issues and files instead of copying them; max 16 KiB). `decision` appends one owner decision or consent verbatim with a timestamp; decisions are never removed.",
            "inputSchema": {"type": "object", "properties": {
                "state": {"type": "string"},
                "decision": {"type": "string"},
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
            "description": "Change a backlog item: fields replace, `note` appends a dated history line, `close` ({pr, sha}) closes it once the fix is merged with green CI, `reject` (reason) closes it without a fix, `reopen` opens it again.",
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

const COORDINATOR_TOOLS: [&str; 4] = [
    "checklist_update",
    "backlog_list",
    "backlog_add",
    "backlog_update",
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

fn call(paths: &Paths, name: &str, args: &Value) -> anyhow::Result<Value> {
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
                Some(f) => Ok(serde_json::to_value(f)?),
                None => Ok(json!({ "thread": id, "state": null })),
            }
        }
        "state_write" => {
            let id = thread_arg(args)?;
            let t3 = agentdeck_core::t3::t3_root().ok();
            let s = thread_state::write(
                paths,
                t3.as_deref(),
                &id,
                args.get("state").and_then(Value::as_str),
                str_arg(args, "decision"),
            )?;
            Ok(json!({ "thread": id, "updated": s.updated, "decisions": s.decisions.len() }))
        }
        "checklist_update" | "backlog_list" | "backlog_add" | "backlog_update"
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

fn handle(paths: &Paths, msg: &Value) -> Option<Value> {
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
                call(paths, name, &args)
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

pub fn run(paths: &Paths) -> anyhow::Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(msg) => handle(paths, &msg),
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
