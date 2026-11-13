//! Drift page: per-instance-group drift with Apply / Import / Fix / Re-enable.
//!
//! Reads are read-only; every mutating action first shows a preview built
//! from the [`agentdeck_core::Change`] list (or the import candidate list)
//! and only runs after an explicit confirm. Long work runs off the UI thread
//! via `Task::perform`; the page refreshes after any action and on the tick.

use std::path::{Path, PathBuf};

use agentdeck_core::{Harness, Paths, Registry, claude, codex, shared, sync, t3};
use i18n_embed_fl::fl;
use iced::widget::column;
use iced::{Element, Task};

use crate::i18n::loader;

/// Explicit roots for every read/write (tests pass temp dirs, never env).
#[derive(Debug, Clone)]
pub struct Ctx {
    pub paths: Paths,
    pub home: PathBuf,
    pub t3_root: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Apply,
    Import,
    Fix,
    Reenable,
}

#[derive(Debug, Clone)]
pub struct Action {
    pub kind: Kind,
    /// Item key encoding the target (`settings:<k>`, `mcpserver:<s>`,
    /// `fix:<path>`, `t3import`, …).
    pub key: String,
    /// Per-server Import action; the button label resolves in `view` so
    /// snapshots stay language-independent.
    pub server: Option<String>,
}

/// Language-independent note appended to an item's text in `view`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Note {
    NotInRegistry,
}

/// Language-independent item body, resolved to a localized line in `view`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextKey {
    NoDrift,
    T3Unavailable,
}

#[derive(Debug, Clone, Default)]
pub struct Item {
    pub key: String,
    /// Core `describe()` text (English, like CLI output); notes, keyed lines
    /// and button labels resolve in `view`.
    pub text: String,
    pub text_key: Option<TextKey>,
    pub actions: Vec<Action>,
    /// Codex real entry: the Fix confirm names this path.
    pub decision_path: Option<String>,
    pub note: Option<Note>,
}

#[derive(Debug, Clone, Default)]
pub struct Group {
    /// Registry id (`t3` for the providerInstances group).
    pub id: String,
    /// Literal title (`Claude <id>`); `None` resolves to the T3 group title
    /// in `view` so snapshots stay language-independent.
    pub title: Option<String>,
    pub harness: String,
    pub items: Vec<Item>,
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub groups: Vec<Group>,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Pending {
    pub group: String,
    pub key: String,
    pub kind: Kind,
    pub preview: Vec<String>,
    /// Fix confirm names this path (resolved to a label in `view`).
    pub confirm_path: Option<String>,
    pub instruction: Option<shared::InstructionResolution>,
}

#[derive(Debug, Clone, Default)]
pub struct State {
    pub snapshot: Snapshot,
    pub pending: Option<Pending>,
    pub busy: Option<String>,
    pub notices: Vec<String>,
    /// Codex groups fixed on a T3 instance: Re-enable is offered for these.
    pub reenable: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum Message {
    SnapshotLoaded(Snapshot),
    Act {
        group: String,
        key: String,
        kind: Kind,
    },
    PreviewReady(Box<Pending>),
    Confirm,
    Cancel,
    Done {
        notice: String,
    },
    ReenableDone {
        group: String,
        report: Vec<String>,
    },
}

fn expand(value: &str, home: &Path) -> PathBuf {
    if value == "~" {
        home.to_path_buf()
    } else if let Some(rest) = value.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(value)
    }
}

fn codex_main(reg: &Registry, home: &Path) -> PathBuf {
    let dflt = home.join(".codex");
    reg.instances
        .iter()
        .find(|i| i.harness == Harness::Codex && expand(&i.home, home) == dflt)
        .map(|_| dflt.clone())
        .unwrap_or(dflt)
}

fn apply_button() -> Action {
    Action {
        kind: Kind::Apply,
        key: String::new(),
        server: None,
    }
}

/// Servers present in the secondary `mcpServers` that differ from primary's.
fn importable_servers(
    primary: &Option<serde_json::Value>,
    secondary: &Option<serde_json::Value>,
) -> Vec<String> {
    let get = |v: &Option<serde_json::Value>| {
        v.as_ref()
            .and_then(|v| v.as_object())
            .map(|m| m.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default()
    };
    let sec = get(secondary);
    let mut out: Vec<String> = sec
        .iter()
        .filter(|s| {
            primary.as_ref().and_then(|p| p.get(*s)) != secondary.as_ref().and_then(|s2| s2.get(*s))
        })
        .cloned()
        .collect();
    out.sort();
    out
}

fn claude_group(inst: &agentdeck_core::Instance, ctx: &Ctx) -> Group {
    let home = expand(&inst.home, &ctx.home);
    let primary = ctx.home.join(".claude");
    let primary_json = ctx.home.join(".claude.json");
    let mut items = Vec::new();
    match claude::check(&primary, &home, &primary_json) {
        Err(e) => items.push(Item {
            key: "error".into(),
            text: format!("check failed: {e:#}"),
            ..Item::default()
        }),
        Ok(drift) => {
            for d in &drift {
                match d {
                    claude::Drift::SettingsKey { key, .. } => items.push(Item {
                        key: format!("settings:{key}"),
                        text: d.describe(),
                        actions: vec![
                            Action {
                                kind: Kind::Apply,
                                key: format!("settings:{key}"),
                                server: None,
                            },
                            Action {
                                kind: Kind::Import,
                                key: format!("settings:{key}"),
                                server: None,
                            },
                        ],
                        ..Item::default()
                    }),
                    claude::Drift::McpServers { primary, secondary } => {
                        let mut actions = vec![Action {
                            kind: Kind::Apply,
                            key: "mcp".into(),
                            server: None,
                        }];
                        for s in importable_servers(primary, secondary) {
                            actions.push(Action {
                                kind: Kind::Import,
                                key: format!("mcpserver:{s}"),
                                server: Some(s),
                            });
                        }
                        items.push(Item {
                            key: "mcp".into(),
                            text: d.describe(),
                            actions,
                            ..Item::default()
                        });
                    }
                    _ => items.push(Item {
                        key: format!("item:{}", items.len()),
                        text: d.describe(),
                        actions: match d {
                            claude::Drift::PrimaryConfigMissing { .. } => vec![],
                            _ => vec![apply_button()],
                        },
                        ..Item::default()
                    }),
                }
            }
        }
    }
    if items.is_empty() {
        items.push(Item {
            key: "clean".into(),
            text_key: Some(TextKey::NoDrift),
            ..Item::default()
        });
    }
    Group {
        id: inst.id.clone(),
        title: Some(format!("Claude {}", inst.id)),
        harness: "claude".into(),
        items,
    }
}

fn codex_group(inst: &agentdeck_core::Instance, ctx: &Ctx, main: &Path) -> Group {
    let shadow = expand(&inst.home, &ctx.home);
    let mut items = Vec::new();
    match codex::check(main, &shadow) {
        Err(e) => items.push(Item {
            key: "error".into(),
            text: format!("check failed: {e:#}"),
            ..Item::default()
        }),
        Ok(drift) => {
            for d in &drift {
                match d {
                    codex::Drift::MissingLink { link, .. }
                    | codex::Drift::WrongTarget { link, .. } => items.push(Item {
                        key: format!("link:{}", link.display()),
                        text: d.describe(),
                        actions: vec![apply_button()],
                        ..Item::default()
                    }),
                    codex::Drift::RealEntry { path, .. } => items.push(Item {
                        key: format!("fix:{}", path.display()),
                        text: d.describe(),
                        actions: vec![Action {
                            kind: Kind::Fix,
                            key: format!("fix:{}", path.display()),
                            server: None,
                        }],
                        decision_path: Some(path.display().to_string()),
                        ..Item::default()
                    }),
                    // Extras are left alone, private links need a fresh login
                    // (their describe() text already says so), the rest is
                    // informational.
                    _ => items.push(Item {
                        key: format!("item:{}", items.len()),
                        text: d.describe(),
                        ..Item::default()
                    }),
                }
            }
        }
    }
    if items.is_empty() {
        items.push(Item {
            key: "clean".into(),
            text_key: Some(TextKey::NoDrift),
            ..Item::default()
        });
    }
    Group {
        id: inst.id.clone(),
        title: Some(format!("Codex {}", inst.id)),
        harness: "codex".into(),
        items,
    }
}

/// Read-only snapshot behind the page. Never fails: gaps become text.
pub fn snapshot(ctx: &Ctx) -> Snapshot {
    let reg = match Registry::load(&ctx.paths) {
        Ok(reg) => reg,
        Err(e) => {
            return Snapshot {
                error: Some(format!("registry: {e:#}")),
                ..Snapshot::default()
            };
        }
    };
    let mut groups = vec![Group {
        id: sync::INSTRUCTIONS_GROUP_ID.into(),
        harness: "instructions".into(),
        items: sync::instruction_items(&ctx.home)
            .into_iter()
            .map(|item| {
                let Some(path) = item.path else {
                    return Item {
                        text: item.detail,
                        ..Default::default()
                    };
                };
                let key = path.to_string_lossy().into_owned();
                let mut actions = Vec::new();
                if shared::instruction_source(&ctx.home).is_file() {
                    actions.push(Action {
                        kind: Kind::Apply,
                        key: key.clone(),
                        server: None,
                    });
                }
                if path.is_file() {
                    actions.push(Action {
                        kind: Kind::Import,
                        key: key.clone(),
                        server: None,
                    });
                }
                Item {
                    key,
                    text: format!(
                        "~/{}",
                        path.strip_prefix(&ctx.home).unwrap_or(&path).display()
                    ),
                    actions,
                    ..Default::default()
                }
            })
            .collect(),
        ..Default::default()
    }];
    for inst in &reg.instances {
        if inst.harness == Harness::Claude {
            groups.push(claude_group(inst, ctx));
        }
    }
    let main = codex_main(&reg, &ctx.home);
    for inst in &reg.instances {
        if inst.harness == Harness::Codex && expand(&inst.home, &ctx.home) != main {
            groups.push(codex_group(inst, ctx, &main));
        }
    }
    Snapshot {
        groups,
        error: None,
    }
}

/// Task entry point: `Task::perform(|| load(ctx), Message::SnapshotLoaded)`.
pub fn load(ctx: Ctx) -> Message {
    Message::SnapshotLoaded(snapshot(&ctx))
}

fn harness_of(groups: &[Group], id: &str) -> String {
    groups
        .iter()
        .find(|g| g.id == id)
        .map(|g| g.harness.clone())
        .unwrap_or_default()
}

/// Dry-run the action behind an Apply/Import/Fix button: the preview shown
/// before the confirm. `key` is the [`Action`] key of the item.
pub fn preview(ctx: &Ctx, group: &str, key: &str, kind: Kind) -> Result<Pending, String> {
    let harness = harness_of(&snapshot(ctx).groups, group);
    match (harness.as_str(), kind, key) {
        ("instructions", Kind::Apply | Kind::Import, key) => {
            let choice = if kind == Kind::Apply {
                shared::InstructionChoice::UseShared
            } else {
                shared::InstructionChoice::UseLocal
            };
            let resolution = shared::plan_instruction_resolution(
                &ctx.home,
                &ctx.paths.state_dir,
                Path::new(key),
                choice,
            )
            .map_err(|error| format!("{error:#}"))?;
            Ok(Pending {
                group: group.into(),
                key: key.into(),
                kind,
                preview: resolution.diff.clone(),
                confirm_path: None,
                instruction: Some(resolution),
            })
        }
        ("claude", Kind::Apply, _) => {
            let reg = Registry::load(&ctx.paths).map_err(|e| format!("registry: {e:#}"))?;
            let inst = reg
                .get(group)
                .ok_or_else(|| format!("unknown instance {group:?}"))?;
            let changes = claude::plan_materialize(
                &ctx.home.join(".claude"),
                &expand(&inst.home, &ctx.home),
                &ctx.paths.state_dir,
                &ctx.home.join(".claude.json"),
            )
            .map_err(|e| format!("{e:#}"))?;
            Ok(Pending {
                group: group.into(),
                key: key.into(),
                kind,
                preview: changes.iter().map(claude::describe_change).collect(),
                confirm_path: None,
                instruction: None,
            })
        }
        ("claude", Kind::Import, key) => {
            let reg = Registry::load(&ctx.paths).map_err(|e| format!("registry: {e:#}"))?;
            let inst = reg
                .get(group)
                .ok_or_else(|| format!("unknown instance {group:?}"))?;
            let home = expand(&inst.home, &ctx.home);
            let change = if let Some(k) = key.strip_prefix("settings:") {
                claude::import_settings_key(
                    &ctx.home.join(".claude"),
                    &home,
                    &ctx.paths.state_dir,
                    k,
                )
                .map_err(|e| format!("{e:#}"))?
            } else if let Some(s) = key.strip_prefix("mcpserver:") {
                claude::import_mcp_server(
                    &ctx.home.join(".claude.json"),
                    &home,
                    &ctx.paths.state_dir,
                    s,
                )
                .map_err(|e| format!("{e:#}"))?
            } else {
                return Err(format!("nothing importable for {key:?}"));
            };
            Ok(Pending {
                group: group.into(),
                key: key.into(),
                kind,
                preview: change
                    .as_ref()
                    .map(claude::describe_change)
                    .into_iter()
                    .collect(),
                confirm_path: None,
                instruction: None,
            })
        }
        ("codex", Kind::Apply, _) => {
            let reg = Registry::load(&ctx.paths).map_err(|e| format!("registry: {e:#}"))?;
            let inst = reg
                .get(group)
                .ok_or_else(|| format!("unknown instance {group:?}"))?;
            let changes = codex::plan_materialize(
                &codex_main(&reg, &ctx.home),
                &expand(&inst.home, &ctx.home),
                &ctx.paths.state_dir,
            )
            .map_err(|e| format!("{e:#}"))?;
            Ok(Pending {
                group: group.into(),
                key: key.into(),
                kind,
                preview: changes.iter().map(|c| c.describe()).collect(),
                confirm_path: None,
                instruction: None,
            })
        }
        ("codex", Kind::Fix, key) => {
            let path = key
                .strip_prefix("fix:")
                .ok_or_else(|| format!("nothing fixable for {key:?}"))?;
            let reg = Registry::load(&ctx.paths).map_err(|e| format!("registry: {e:#}"))?;
            let inst = reg
                .get(group)
                .ok_or_else(|| format!("unknown instance {group:?}"))?;
            let plan = codex::fix(
                &codex_main(&reg, &ctx.home),
                &expand(&inst.home, &ctx.home),
                &ctx.paths.state_dir,
                true,
            )
            .map_err(|e| format!("{e:#}"))?;
            let mut preview: Vec<String> = plan.changes.iter().map(|c| c.describe()).collect();
            preview.extend(plan.deferred.iter().cloned());
            Ok(Pending {
                group: group.into(),
                key: key.into(),
                kind,
                preview,
                confirm_path: Some(path.to_string()),
                instruction: None,
            })
        }
        ("t3", Kind::Import, _) => {
            let root = ctx
                .t3_root
                .clone()
                .ok_or_else(|| "T3 unavailable".to_string())?;
            let found = t3::import_t3(&ctx.paths, &root, true).map_err(|e| format!("{e:#}"))?;
            Ok(Pending {
                group: group.into(),
                key: key.into(),
                kind,
                preview: found
                    .iter()
                    .map(|f| format!("{} {} {} t3=true", f.id, f.harness.as_str(), f.home))
                    .collect(),
                confirm_path: None,
                instruction: None,
            })
        }
        _ => Err("action not applicable here".to_string()),
    }
}

/// Run a confirmed preview. Returns the one-line notice for the page.
pub fn apply(ctx: &Ctx, pending: &Pending) -> String {
    let fail = |e: anyhow::Error| format!("failed: {e:#}");
    if let Some(resolution) = &pending.instruction {
        return match shared::apply_instruction_resolution(&ctx.home, resolution) {
            Ok(()) => fl!(loader(), "drift-instr-resolved"),
            Err(error) => fail(error),
        };
    }
    match (pending.kind, pending.key.as_str()) {
        (Kind::Apply, _) | (Kind::Import, _) | (Kind::Fix, _) => {
            let reg = match Registry::load(&ctx.paths) {
                Ok(r) => r,
                Err(e) => return fail(e),
            };
            let out: anyhow::Result<String> =
                (|| match harness_of(&snapshot(ctx).groups, &pending.group).as_str() {
                    "claude" => {
                        let inst = reg.get(&pending.group).ok_or_else(|| {
                            anyhow::anyhow!("unknown instance {:?}", pending.group)
                        })?;
                        let home = expand(&inst.home, &ctx.home);
                        match pending.kind {
                            Kind::Import if pending.key.starts_with("settings:") => {
                                let k = &pending.key["settings:".len()..];
                                match claude::import_settings_key(
                                    &ctx.home.join(".claude"),
                                    &home,
                                    &ctx.paths.state_dir,
                                    k,
                                )? {
                                    Some(c) => {
                                        agentdeck_core::apply_changes(std::slice::from_ref(&c))?;
                                        Ok(format!("imported {k:?} into primary"))
                                    }
                                    None => Ok("already in sync".into()),
                                }
                            }
                            Kind::Import => {
                                let s = pending.key.strip_prefix("mcpserver:").unwrap_or("");
                                match claude::import_mcp_server(
                                    &ctx.home.join(".claude.json"),
                                    &home,
                                    &ctx.paths.state_dir,
                                    s,
                                )? {
                                    Some(c) => {
                                        agentdeck_core::apply_changes(std::slice::from_ref(&c))?;
                                        Ok(format!("imported mcpServer {s:?} into primary"))
                                    }
                                    None => Ok("already in sync".into()),
                                }
                            }
                            _ => {
                                let changes = claude::plan_materialize(
                                    &ctx.home.join(".claude"),
                                    &home,
                                    &ctx.paths.state_dir,
                                    &ctx.home.join(".claude.json"),
                                )?;
                                agentdeck_core::apply_changes(&changes)?;
                                Ok(format!("synced {}", pending.group))
                            }
                        }
                    }
                    "codex" => {
                        let inst = reg.get(&pending.group).ok_or_else(|| {
                            anyhow::anyhow!("unknown instance {:?}", pending.group)
                        })?;
                        let shadow = expand(&inst.home, &ctx.home);
                        if pending.kind == Kind::Fix {
                            let plan = codex::fix(
                                &codex_main(&reg, &ctx.home),
                                &shadow,
                                &ctx.paths.state_dir,
                                true,
                            )?;
                            agentdeck_core::apply_changes(&plan.changes)?;
                            Ok(format!(
                                "fixed {} ({} deferred)",
                                pending.group,
                                plan.deferred.len()
                            ))
                        } else {
                            let changes = codex::plan_materialize(
                                &codex_main(&reg, &ctx.home),
                                &shadow,
                                &ctx.paths.state_dir,
                            )?;
                            agentdeck_core::apply_changes(&changes)?;
                            Ok(format!("synced {}", pending.group))
                        }
                    }
                    "t3" => {
                        let root = ctx
                            .t3_root
                            .clone()
                            .ok_or_else(|| anyhow::anyhow!("T3 unavailable"))?;
                        if pending.kind == Kind::Import {
                            let found = t3::import_t3(&ctx.paths, &root, false)?;
                            Ok(format!("imported {} instance(s)", found.len()))
                        } else {
                            let entries = t3::sync_t3(&ctx.paths, &root, false)?;
                            let n = entries.iter().filter(|e| e.action != "unchanged").count();
                            Ok(format!("synced {n} instance(s)"))
                        }
                    }
                    _ => Err(anyhow::anyhow!("action not applicable here")),
                })();
            out.unwrap_or_else(fail)
        }
        (Kind::Reenable, _) => "use the Re-enable button".into(),
    }
}

/// Run the false→observe→true toggle for a T3 instance id.
fn run_reenable(ctx: &Ctx, id: &str) -> Vec<String> {
    let Some(root) = ctx.t3_root.clone() else {
        return vec!["failed: T3 unavailable".into()];
    };
    match t3::reenable(
        &ctx.paths,
        &root,
        id,
        &t3::ReenableOpts {
            timeout_secs: 30,
            force: false,
            dry_run: false,
        },
    ) {
        Ok(report) => {
            let mut out = report.events;
            out.push(report.final_line);
            out
        }
        Err(e) => vec![format!("failed: {e:#}")],
    }
}

pub fn update(state: &mut State, ctx: Ctx, message: Message) -> Task<Message> {
    match message {
        Message::SnapshotLoaded(snapshot) => {
            state.snapshot = snapshot;
            Task::none()
        }
        Message::Act { group, key, kind } => {
            if kind == Kind::Reenable {
                state.busy = Some(group.clone());
                return Task::perform(
                    async move {
                        let report = run_reenable(&ctx, &group);
                        Message::ReenableDone { group, report }
                    },
                    |m| m,
                );
            }
            state.busy = Some(format!("{group} {key}"));
            Task::perform(
                async move {
                    match preview(&ctx, &group, &key, kind) {
                        Ok(pending) => Message::PreviewReady(Box::new(pending)),
                        Err(error) => Message::Done {
                            notice: format!("failed: {error}"),
                        },
                    }
                },
                |m| m,
            )
        }
        Message::PreviewReady(pending) => {
            state.pending = Some(*pending);
            state.busy = None;
            Task::none()
        }
        Message::Confirm => {
            let Some(pending) = state.pending.clone() else {
                return Task::none();
            };
            state.pending = None;
            state.busy = Some(format!("{} {}", pending.group, pending.key));
            Task::perform(
                async move {
                    let notice = apply(&ctx, &pending);
                    Message::Done { notice }
                },
                |m| m,
            )
        }
        Message::Cancel => {
            state.pending = None;
            Task::none()
        }
        Message::Done { notice } => {
            // A Codex fix on a T3 instance unlocks the Re-enable offer.
            if notice.starts_with("fixed ")
                && let Ok(reg) = Registry::load(&ctx.paths)
                && let Some(id) = notice
                    .strip_prefix("fixed ")
                    .and_then(|s| s.split_whitespace().next())
                && reg.get(id).is_some_and(|i| i.t3)
                && !state.reenable.contains(&id.to_string())
            {
                state.reenable.push(id.to_string());
            }
            state.notices.push(notice);
            while state.notices.len() > 4 {
                state.notices.remove(0);
            }
            state.pending = None;
            state.busy = None;
            Task::perform(async move { load(ctx) }, |m| m)
        }
        Message::ReenableDone { group, report } => {
            state.busy = None;
            state.notices.extend(report);
            while state.notices.len() > 4 {
                state.notices.remove(0);
            }
            state.reenable.retain(|id| *id != group);
            Task::perform(async move { load(ctx) }, |m| m)
        }
    }
}

/// Button label for an action, resolved here (not in the snapshot) so the
/// page follows a language switch without reloading.
fn action_label(action: &Action) -> String {
    match (&action.kind, &action.server) {
        (Kind::Import, Some(server)) => fl!(loader(), "drift-import-server", server = server),
        (Kind::Import, None) => fl!(loader(), "drift-import"),
        (Kind::Fix, _) => fl!(loader(), "drift-fix"),
        (Kind::Reenable, _) => fl!(loader(), "drift-reenable"),
        (Kind::Apply, _) => fl!(loader(), "drift-apply"),
    }
}

fn action_buttons<'a>(group: &str, item: &'a Item) -> Vec<Element<'a, Message>> {
    use crate::style as st;
    item.actions
        .iter()
        .map(|a| {
            let label = if group == sync::INSTRUCTIONS_GROUP_ID {
                match a.kind {
                    Kind::Apply => fl!(loader(), "drift-instr-use-shared"),
                    Kind::Import => fl!(loader(), "drift-instr-use-local"),
                    _ => action_label(a),
                }
            } else {
                action_label(a)
            };
            // Server names can be long; the label stays a button-sized line.
            st::btn(st::Btn::Quiet, st::ellipsize(&label, 40, st::Cut::Middle))
                .on_press(Message::Act {
                    group: group.to_string(),
                    key: a.key.clone(),
                    kind: a.kind,
                })
                .into()
        })
        .collect()
}

/// Items with something to do (not the "no drift" / "T3 unavailable" lines).
pub fn open_items(group: &Group) -> usize {
    group.items.iter().filter(|i| i.text_key.is_none()).count()
}

/// Item body: keyed lines and notes resolve here so snapshots stay
/// language-independent.
fn item_text(item: &Item) -> String {
    if let Some(key) = &item.text_key {
        return match key {
            TextKey::NoDrift => fl!(loader(), "drift-no-drift"),
            TextKey::T3Unavailable => fl!(loader(), "t3-unavailable"),
        };
    }
    match &item.note {
        Some(Note::NotInRegistry) => {
            format!("{}: {}", item.text, fl!(loader(), "drift-not-in-registry"))
        }
        None => item.text.clone(),
    }
}

/// Confirm button label: the Fix confirm names the path being replaced.
fn confirm_label(pending: &Pending) -> String {
    match &pending.confirm_path {
        Some(path) => fl!(loader(), "drift-confirm-replace", path = path),
        None => fl!(loader(), "drift-confirm"),
    }
}

pub fn modal(state: &State) -> Option<crate::overlay::Modal<'_, Message>> {
    use crate::style as st;
    let pending = state.pending.as_ref()?;
    if let Some(resolution) = &pending.instruction {
        let (confirmation, consequence, before, after) = match resolution.choice {
            shared::InstructionChoice::UseShared => (
                fl!(loader(), "drift-instr-confirm-shared"),
                fl!(
                    loader(),
                    "drift-instr-link-consequence",
                    path = resolution.target.display().to_string()
                ),
                &resolution.target,
                &resolution.source,
            ),
            shared::InstructionChoice::UseLocal => (
                fl!(loader(), "drift-instr-confirm-local"),
                fl!(loader(), "drift-instr-import-consequence"),
                &resolution.source,
                &resolution.target,
            ),
        };
        let mut body: Vec<Element<'_, Message>> = vec![st::body(consequence).into()];
        for change in &resolution.changes {
            let backup = match change {
                agentdeck_core::Change::BackupRemove { backup, .. } => Some(backup),
                agentdeck_core::Change::WriteFile { backup, .. } => backup.as_ref(),
                _ => None,
            };
            if let Some(path) = backup {
                body.push(
                    st::caption(fl!(
                        loader(),
                        "drift-instr-backup",
                        path = path.display().to_string()
                    ))
                    .into(),
                );
            }
        }
        body.push(st::mono(format!("--- {}", before.display())).into());
        body.push(st::mono(format!("+++ {}", after.display())).into());
        body.push(
            column(
                resolution
                    .diff
                    .iter()
                    .map(|line| st::mono(line.as_str()).into()),
            )
            .spacing(0)
            .into(),
        );
        return Some(crate::overlay::Modal {
            title: fl!(loader(), "drift-instr-compare"),
            body,
            actions: vec![
                st::btn(st::Btn::Quiet, fl!(loader(), "drift-cancel"))
                    .on_press(Message::Cancel)
                    .into(),
                st::btn(st::Btn::Primary, confirmation)
                    .on_press_maybe((!resolution.changes.is_empty()).then_some(Message::Confirm))
                    .into(),
            ],
            on_cancel: Message::Cancel,
            on_submit: None,
        });
    }
    let mut preview = pending.preview.clone();
    if preview.is_empty() {
        preview.push(fl!(loader(), "drift-no-drift"));
    }
    preview.truncate(30);
    Some(crate::overlay::Modal {
        title: fl!(loader(), "drift-preview"),
        body: preview.into_iter().map(|l| st::body(l).into()).collect(),
        actions: vec![
            st::btn(st::Btn::Quiet, fl!(loader(), "drift-cancel"))
                .on_press(Message::Cancel)
                .into(),
            st::btn(st::Btn::Primary, confirm_label(pending))
                .on_press(Message::Confirm)
                .into(),
        ],
        on_cancel: Message::Cancel,
        on_submit: Some(Message::Confirm),
    })
}

pub fn view(state: &State) -> Element<'_, Message> {
    use crate::style as st;
    use iced::widget::row;
    use iced::{Alignment, Length};
    let look = st::look();
    let mut col = column![].spacing(look.gap.max(st::SPACE_S));
    if let Some(error) = &state.snapshot.error {
        col = col.push(st::card(st::muted(fl!(
            loader(),
            "load-error",
            what = error
        ))));
    }
    if state.busy.is_some() {
        col = col.push(st::muted(fl!(loader(), "drift-working")));
    }
    if state.snapshot.groups.is_empty() {
        if state.snapshot.error.is_none() {
            // Groups exist once the check ran, even with nothing to report.
            col = col.push(st::card(st::muted(fl!(loader(), "drift-loading"))));
        }
        return col.into();
    }
    let mut any = false;
    for group in &state.snapshot.groups {
        // Re-enable is offered only after a Codex fix on a T3 instance.
        let reenable = group.harness == "codex" && state.reenable.contains(&group.id);
        if open_items(group) == 0 && !reenable {
            continue;
        }
        any = true;
        let title = group.title.clone().unwrap_or_else(|| {
            if group.harness == "instructions" {
                fl!(loader(), "shared-instr")
            } else {
                fl!(loader(), "drift-group-t3")
            }
        });
        let mut head = row![st::fit(title, st::Role::Heading, st::Cut::End)]
            .spacing(st::SPACE_S)
            .align_y(Alignment::Center);
        if !group.harness.is_empty() && group.harness != "instructions" {
            head = head.push(st::pill(group.harness.as_str(), st::Tone::Accent));
        }
        if reenable {
            head = head.push(
                st::btn(st::Btn::Quiet, fl!(loader(), "drift-reenable")).on_press(Message::Act {
                    group: group.id.clone(),
                    key: String::new(),
                    kind: Kind::Reenable,
                }),
            );
        }
        let mut card = column![head].spacing(look.row_gap);
        for item in group.items.iter().filter(|i| i.text_key.is_none()) {
            card = card.push(st::hairline());
            let buttons = action_buttons(&group.id, item);
            let mut line = column![st::body(item_text(item)).width(Length::Fill)].spacing(4);
            if !buttons.is_empty() {
                line = line.push(row(buttons).spacing(st::SPACE_S).wrap());
            }
            card = card.push(line);
        }
        col = col.push(st::card(card));
    }
    if !any {
        col = col.push(st::card(
            row![
                st::icon_colored(st::Icon::CircleCheck, look.body + 3.0, |c| c.ok),
                st::body(fl!(loader(), "drift-all-clear")),
            ]
            .spacing(st::SPACE_S)
            .align_y(Alignment::Center),
        ));
    }
    col.into()
}
