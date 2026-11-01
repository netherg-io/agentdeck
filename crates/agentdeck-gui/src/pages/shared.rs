//! Shared page: skills, instruction links and MCP servers across harnesses.
//! Status is read-only (MCP values stay hidden); sync/link show the core
//! change preview and apply exactly the previewed changes off the UI thread.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use agentdeck_core::shared::{self, SkillState};
use agentdeck_core::{Change, Paths};
use i18n_embed_fl::fl;
use iced::widget::{Space, button, column, row};
use iced::{Alignment, Element, Length, Task};

use crate::i18n::loader;
use crate::style as st;

#[derive(Debug, Clone)]
pub struct Ctx {
    pub paths: Paths,
    pub home: PathBuf,
}

/// One target line: `~`-shortened path or name, then its status.
#[derive(Debug, Clone, Default)]
pub struct Target {
    pub name: String,
    pub status: String,
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    /// Skill target dir with (in sync, need sync) counts.
    pub skills: Vec<(String, usize, usize)>,
    pub instructions: Vec<Target>,
    pub mcp_servers: Vec<String>,
    pub mcp_targets: Vec<Target>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Kind {
    #[default]
    Skills,
    Instructions,
    Mcp,
}

#[derive(Debug, Clone, Default)]
pub struct Pending {
    pub lines: Vec<String>,
    pub changes: Vec<Change>,
}

#[derive(Default)]
pub struct State {
    pub snap: Snapshot,
    pub tab: Kind,
    pub pending: Option<Pending>,
    pub busy: bool,
    /// Last action outcome (text, is error); the shell turns it into a toast.
    pub result: Option<(String, bool)>,
}

#[derive(Debug, Clone)]
pub enum Msg {
    Loaded(Snapshot),
    Tab(Kind),
    Ask(Kind),
    Confirm,
    Cancel,
    Done(Result<String, String>),
}

/// `~`-shortened path text.
fn short(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

pub fn load(ctx: &Ctx) -> Msg {
    let mut snap = Snapshot::default();
    // Skills: one line per existing target dir (core manages only those).
    let mut per_dir: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for s in shared::skills_status(&ctx.home, &ctx.paths) {
        if matches!(s.state, SkillState::NoDir) {
            continue;
        }
        let entry = per_dir.entry(short(&s.target, &ctx.home)).or_default();
        match s.state {
            SkillState::Ok | SkillState::Foreign => entry.0 += 1,
            _ => entry.1 += 1,
        }
    }
    snap.skills = per_dir
        .into_iter()
        .map(|(dir, (ok, todo))| (dir, ok, todo))
        .collect();
    snap.instructions = shared::instructions_status(&ctx.home)
        .iter()
        .map(|s| Target {
            name: short(&s.path, &ctx.home),
            status: s.state.describe(),
        })
        .collect();
    match shared::claude_secondaries(&ctx.paths, &ctx.home) {
        Ok(secondaries) => {
            let (redacted, targets) = shared::mcp_status(&ctx.home, &ctx.paths, &secondaries);
            let mut names: Vec<String> = redacted
                .as_object()
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default();
            names.sort();
            snap.mcp_servers = names;
            snap.mcp_targets = targets
                .iter()
                .map(|t| Target {
                    name: t.name.replace(&ctx.home.display().to_string(), "~"),
                    status: t.detail.clone(),
                })
                .collect();
        }
        Err(e) => snap.error = Some(format!("{e:#}")),
    }
    Msg::Loaded(snap)
}

fn plan(ctx: &Ctx, kind: Kind) -> anyhow::Result<Pending> {
    let mut lines = Vec::new();
    let changes = match kind {
        Kind::Skills => shared::plan_skills_sync(&ctx.home, &ctx.paths)?,
        Kind::Instructions => {
            shared::plan_instructions_link(&ctx.home, &ctx.paths.state_dir, false)?
        }
        Kind::Mcp => {
            let secondaries = shared::claude_secondaries(&ctx.paths, &ctx.home)?;
            let (changes, skipped) =
                shared::plan_mcp_sync(&ctx.home, &ctx.paths.state_dir, &secondaries, false)?;
            lines.extend(
                skipped
                    .iter()
                    .map(|n| format!("skip {n}: unknown entry shape, left alone")),
            );
            changes
        }
    };
    lines.extend(changes.iter().map(shared::describe_mcp_change));
    Ok(Pending { lines, changes })
}

pub fn update(state: &mut State, ctx: Ctx, msg: Msg) -> Task<Msg> {
    match msg {
        Msg::Loaded(snap) => {
            state.snap = snap;
            Task::none()
        }
        Msg::Tab(kind) => {
            state.tab = kind;
            Task::none()
        }
        Msg::Ask(kind) => {
            state.result = None;
            state.pending = match plan(&ctx, kind) {
                Ok(p) if p.changes.is_empty() && p.lines.is_empty() => {
                    state.result = Some((fl!(loader(), "action-none"), false));
                    None
                }
                Ok(p) => Some(p),
                // A refusal shows as a panel with no changes, hence no Apply.
                Err(e) => Some(Pending {
                    lines: vec![format!("{e:#}")],
                    changes: Vec::new(),
                }),
            };
            Task::none()
        }
        Msg::Confirm => match state.pending.take().filter(|p| !p.changes.is_empty()) {
            Some(p) => {
                state.busy = true;
                Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || {
                            agentdeck_core::apply_changes(&p.changes)
                                .map(|()| p.lines.join("\n"))
                                .map_err(|e| format!("{e:#}"))
                        })
                        .await
                        .map_err(|e| e.to_string())
                        .and_then(|r| r)
                    },
                    Msg::Done,
                )
            }
            None => Task::none(),
        },
        Msg::Cancel => {
            state.pending = None;
            Task::none()
        }
        Msg::Done(result) => {
            state.busy = false;
            state.result = Some(match result {
                Ok(text) => (text, false),
                Err(e) => (e, true),
            });
            Task::done(load(&ctx))
        }
    }
}

pub fn modal(state: &State) -> Option<crate::overlay::Modal<'_, Msg>> {
    state.pending.as_ref().map(|p| {
        crate::overlay::confirm(
            fl!(loader(), "shared-preview-title"),
            &p.lines,
            (
                fl!(loader(), "action-apply"),
                (!p.changes.is_empty()).then_some(Msg::Confirm),
            ),
            (fl!(loader(), "action-cancel"), Msg::Cancel),
        )
    })
}

fn count(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(0)
}

fn target_row(name: &str, status: Element<'static, Msg>) -> Element<'static, Msg> {
    row![
        st::fit(name.to_string(), st::Role::Mono, st::Cut::Middle),
        status
    ]
    .spacing(st::SPACE_M)
    .align_y(Alignment::Center)
    .into()
}

/// Rows separated by hairlines, or one helpful line when there are none.
fn rows_card<'a>(rows: Vec<Element<'a, Msg>>, empty: String) -> Element<'a, Msg> {
    if rows.is_empty() {
        return st::card(st::muted(empty));
    }
    let mut col = column![].spacing(st::look().row_gap);
    for (i, r) in rows.into_iter().enumerate() {
        if i > 0 {
            col = col.push(st::hairline());
        }
        col = col.push(r);
    }
    st::card(col)
}

/// Status text on the right: short, so it gets a bounded slot.
fn status_text(text: &str) -> Element<'static, Msg> {
    st::fit(text.to_string(), st::Role::Muted, st::Cut::End)
        .width(Length::FillPortion(1))
        .into()
}

fn tab_content(state: &State) -> Element<'_, Msg> {
    let snap = &state.snap;
    let (rows, empty, action, kind): (Vec<Element<'_, Msg>>, String, String, Kind) = match state.tab
    {
        Kind::Skills => (
            snap.skills
                .iter()
                .map(|(dir, ok, todo)| {
                    let mut pills = row![st::pill(
                        fl!(loader(), "shared-ok-count", count = count(*ok)),
                        st::Tone::Ok
                    )]
                    .spacing(st::SPACE_XS);
                    if *todo > 0 {
                        pills = pills.push(st::pill(
                            fl!(loader(), "shared-todo-count", count = count(*todo)),
                            st::Tone::Warn,
                        ));
                    }
                    target_row(dir, pills.into())
                })
                .collect(),
            fl!(loader(), "shared-skills-none"),
            fl!(loader(), "shared-skills-sync"),
            Kind::Skills,
        ),
        Kind::Instructions => (
            snap.instructions
                .iter()
                .map(|t| {
                    row![
                        st::fit(t.name.clone(), st::Role::Mono, st::Cut::Middle)
                            .width(Length::FillPortion(2)),
                        status_text(&t.status),
                    ]
                    .spacing(st::SPACE_M)
                    .align_y(Alignment::Center)
                    .into()
                })
                .collect(),
            fl!(loader(), "shared-instr-none"),
            fl!(loader(), "shared-instr-link"),
            Kind::Instructions,
        ),
        Kind::Mcp => {
            let servers: Element<'_, Msg> = if snap.mcp_servers.is_empty() {
                st::muted(fl!(loader(), "shared-mcp-none")).into()
            } else {
                row(snap
                    .mcp_servers
                    .iter()
                    .map(|n| st::pill(n.as_str(), st::Tone::Neutral)))
                .spacing(st::SPACE_XS)
                .wrap()
                .into()
            };
            let mut rows: Vec<Element<'_, Msg>> = vec![
                column![
                    st::caption(fl!(loader(), "shared-mcp-servers")),
                    servers,
                    st::caption(fl!(loader(), "shared-mcp-hidden")),
                ]
                .spacing(6)
                .into(),
            ];
            if !snap.mcp_targets.is_empty() {
                rows.push(st::caption(fl!(loader(), "shared-mcp-targets")).into());
            }
            rows.extend(snap.mcp_targets.iter().map(|t| {
                row![
                    st::fit(t.name.clone(), st::Role::Mono, st::Cut::Middle)
                        .width(Length::FillPortion(2)),
                    status_text(&t.status),
                ]
                .spacing(st::SPACE_M)
                .align_y(Alignment::Center)
                .into()
            }));
            (
                rows,
                fl!(loader(), "shared-mcp-none"),
                fl!(loader(), "shared-mcp-sync"),
                Kind::Mcp,
            )
        }
    };
    column![
        rows_card(rows, empty),
        row![
            Space::new().width(Length::Fill),
            st::btn_icon(st::Btn::Quiet, st::Icon::RefreshCw, action)
                .on_press_maybe((!state.busy).then_some(Msg::Ask(kind))),
        ],
    ]
    .spacing(st::SPACE_S)
    .into()
}

pub fn view(state: &State) -> Element<'_, Msg> {
    let look = st::look();
    let snap = &state.snap;
    let mut col = column![].spacing(look.gap.max(st::SPACE_S));
    if let Some(error) = &snap.error {
        col = col.push(st::card(st::muted(fl!(
            loader(),
            "load-error",
            what = error
        ))));
    }
    let tab = |kind: Kind, label: String| {
        button(st::nowrap(st::body(label)))
            .padding(look.button_pad)
            .style(st::nav_style(state.tab == kind))
            .on_press(Msg::Tab(kind))
    };
    col = col.push(
        row![
            tab(Kind::Skills, fl!(loader(), "shared-skills")),
            tab(Kind::Instructions, fl!(loader(), "shared-instr")),
            tab(Kind::Mcp, fl!(loader(), "shared-mcp")),
        ]
        .spacing(st::SPACE_XS),
    );
    if state.busy {
        col = col.push(st::muted(fl!(loader(), "action-working")));
    }
    col.push(tab_content(state)).into()
}
