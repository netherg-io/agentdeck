//! Full instance catalogs with explicit routing selections.

use std::collections::BTreeMap;
use std::path::PathBuf;

use agentdeck_core::{Paths, PoolEntry, Registry, pool};
use i18n_embed_fl::fl;
use iced::widget::{Space, column, container, pick_list, row, text_input, toggler, tooltip};
use iced::{Alignment, Element, Length, Task};

use crate::i18n::loader;
use crate::style as st;

/// Explicit roots for every read/write (tests pass temp dirs).
#[derive(Debug, Clone)]
pub struct Ctx {
    pub paths: Paths,
    pub t3_root: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub enum Op {
    Add(PoolEntry),
    Remove(PoolEntry),
    Toggle(PoolEntry),
}

#[derive(Debug, Clone)]
pub struct Pending {
    /// `None` for a refusal: the panel then has no Apply.
    pub op: Option<Op>,
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Loaded {
    pub entries: Vec<PoolEntry>,
    pub instances: Vec<String>,
    pub names: BTreeMap<String, String>,
    /// Saved entries the catalog does not provide (custom model or options):
    /// only these can be deleted; catalog models are just switched off.
    pub custom: Vec<PoolEntry>,
    pub error: Option<String>,
}

#[derive(Default)]
pub struct State {
    pub data: Loaded,
    pub search: String,
    pub filter_instance: Option<String>,
    pub sort: Sort,
    /// Model slugs from the T3 cache of the chosen instance.
    pub models: Vec<String>,
    pub instance: Option<String>,
    pub model: String,
    pub options: String,
    /// The Add form modal is open.
    pub adding: bool,
    pub pending: Option<Pending>,
    pub busy: bool,
    /// Last action outcome (text, is error); the shell turns it into a toast.
    pub result: Option<(String, bool)>,
}

#[derive(Debug, Clone)]
pub enum Msg {
    Loaded(Loaded),
    Instance(String),
    Model(String),
    Options(String),
    OpenAdd,
    CloseAdd,
    AskAdd,
    AskRemove(usize),
    AskToggle(usize),
    Search(String),
    FilterInstance(InstanceChoice),
    Sort(Sort),
    Confirm,
    Cancel,
    Done(Result<String, String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceChoice {
    pub id: Option<String>,
    pub label: String,
}

impl std::fmt::Display for InstanceChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Sort {
    #[default]
    Instance,
    Model,
    Enabled,
}

impl std::fmt::Display for Sort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&match self {
            Self::Instance => fl!(loader(), "pool-sort-instance"),
            Self::Model => fl!(loader(), "pool-sort-model"),
            Self::Enabled => fl!(loader(), "pool-sort-enabled"),
        })
    }
}

pub fn load(ctx: &Ctx) -> Msg {
    Msg::Loaded(match Registry::load(&ctx.paths) {
        Ok(reg) => {
            let catalog = pool::catalog_pool(&reg, ctx.t3_root.as_deref());
            let error = catalog.as_ref().err().map(|e| format!("{e:#}"));
            Loaded {
                entries: catalog.unwrap_or_else(|_| reg.pool.clone()),
                instances: reg.instances.iter().map(|i| i.id.clone()).collect(),
                names: reg
                    .instances
                    .iter()
                    .map(|i| (i.id.clone(), i.display_name.clone()))
                    .collect(),
                custom: reg
                    .pool
                    .iter()
                    .filter(|e| {
                        !e.options.is_empty()
                            || !pool::cached_models(ctx.t3_root.as_deref(), &e.instance)
                                .unwrap_or_default()
                                .contains(&e.model)
                    })
                    .cloned()
                    .collect(),
                error,
            }
        }
        Err(e) => Loaded {
            error: Some(format!("{e:#}")),
            ..Loaded::default()
        },
    })
}

/// Keep original indices so filtering and sorting cannot target another entry.
pub fn visible_entries(state: &State) -> Vec<(usize, &PoolEntry)> {
    let query = state.search.to_lowercase();
    let terms: Vec<_> = query.split_whitespace().collect();
    let mut entries: Vec<_> = state
        .data
        .entries
        .iter()
        .enumerate()
        .filter(|(_, e)| {
            if state
                .filter_instance
                .as_ref()
                .is_some_and(|i| i != &e.instance)
            {
                return false;
            }
            let name = state
                .data
                .names
                .get(&e.instance)
                .map(String::as_str)
                .unwrap_or("");
            let text = format!(
                "{} {} {} {}",
                e.instance,
                name,
                e.model,
                e.options
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            )
            .to_lowercase();
            terms.iter().all(|term| text.contains(term))
        })
        .collect();
    entries.sort_by(|(_, a), (_, b)| {
        let name = |e: &PoolEntry| {
            state
                .data
                .names
                .get(&e.instance)
                .unwrap_or(&e.instance)
                .to_lowercase()
        };
        let order = match state.sort {
            Sort::Instance => name(a)
                .cmp(&name(b))
                .then(a.instance.cmp(&b.instance))
                .then(a.model.cmp(&b.model)),
            Sort::Model => a.model.cmp(&b.model).then(name(a).cmp(&name(b))),
            Sort::Enabled => b
                .enabled
                .cmp(&a.enabled)
                .then(name(a).cmp(&name(b)))
                .then(a.model.cmp(&b.model)),
        };
        order.then(a.options.cmp(&b.options))
    });
    entries
}

fn form_entry(state: &State) -> anyhow::Result<PoolEntry> {
    let instance = state
        .instance
        .clone()
        .ok_or_else(|| anyhow::anyhow!("choose an instance"))?;
    let args: Vec<String> = state
        .options
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    Ok(PoolEntry {
        instance,
        model: state.model.trim().to_string(),
        options: pool::parse_options(&args)?,
        enabled: true,
    })
}

fn lines_of(changes: &[agentdeck_core::Change]) -> Vec<String> {
    changes.iter().map(|c| c.describe()).collect()
}

/// Read-only preview of `op` through the core dry-run.
fn preview(ctx: &Ctx, op: &Op) -> anyhow::Result<Vec<String>> {
    match op {
        Op::Add(e) => {
            let (changes, warn) = pool::add_pool_entry(&ctx.paths, e.clone(), true)?;
            let mut lines = lines_of(&changes);
            let warn =
                warn.or_else(|| pool::check_model(ctx.t3_root.as_deref(), &e.instance, &e.model));
            lines.extend(warn.map(|w| fl!(loader(), "pool-warning", what = w)));
            Ok(lines)
        }
        Op::Remove(e) => Ok(lines_of(&pool::remove_pool_entry(
            &ctx.paths,
            &e.instance,
            &e.model,
            &e.options,
            true,
        )?)),
        Op::Toggle(e) => toggle(ctx, e, true).map(|changes| lines_of(&changes)),
    }
}

pub fn apply(ctx: &Ctx, op: &Op) -> anyhow::Result<String> {
    let changes = match op {
        Op::Add(e) => pool::add_pool_entry(&ctx.paths, e.clone(), false)?.0,
        Op::Remove(e) => {
            pool::remove_pool_entry(&ctx.paths, &e.instance, &e.model, &e.options, false)?
        }
        Op::Toggle(e) => toggle(ctx, e, false)?,
    };
    Ok(lines_of(&changes).join("\n"))
}

fn toggle(
    ctx: &Ctx,
    entry: &PoolEntry,
    dry_run: bool,
) -> anyhow::Result<Vec<agentdeck_core::Change>> {
    let reg = Registry::load(&ctx.paths)?;
    if reg.pool.iter().any(|e| e.key_eq(entry)) {
        pool::set_pool_enabled(
            &ctx.paths,
            &entry.instance,
            &entry.model,
            &entry.options,
            !entry.enabled,
            dry_run,
        )
    } else {
        let mut selected = entry.clone();
        selected.enabled = !entry.enabled;
        pool::add_pool_entry(&ctx.paths, selected, dry_run).map(|(changes, _)| changes)
    }
}

fn run(state: &mut State, ctx: Ctx, op: Op) -> Task<Msg> {
    state.busy = true;
    Task::perform(
        async move {
            tokio::task::spawn_blocking(move || apply(&ctx, &op).map_err(|e| format!("{e:#}")))
                .await
                .map_err(|e| e.to_string())
                .and_then(|r| r)
        },
        Msg::Done,
    )
}

pub fn update(state: &mut State, ctx: Ctx, msg: Msg) -> Task<Msg> {
    match msg {
        Msg::Loaded(data) => {
            state.data = data;
            Task::none()
        }
        Msg::Instance(id) => {
            state.models = pool::cached_models(ctx.t3_root.as_deref(), &id).unwrap_or_default();
            state.instance = Some(id);
            state.model.clear();
            Task::none()
        }
        Msg::Model(m) => {
            state.model = m;
            Task::none()
        }
        Msg::Options(o) => {
            state.options = o;
            Task::none()
        }
        Msg::OpenAdd => {
            state.adding = true;
            Task::none()
        }
        Msg::CloseAdd => {
            state.adding = false;
            Task::none()
        }
        Msg::AskAdd => {
            state.adding = false;
            ask(state, &ctx, form_entry(state).map(Op::Add));
            Task::none()
        }
        Msg::AskRemove(i) => {
            let op = state.data.entries.get(i).cloned().map(Op::Remove);
            ask(
                state,
                &ctx,
                op.ok_or_else(|| anyhow::anyhow!("entry vanished")),
            );
            Task::none()
        }
        Msg::AskToggle(i) => {
            if state.busy {
                return Task::none();
            }
            match state.data.entries.get(i).cloned() {
                Some(e) => run(state, ctx, Op::Toggle(e)),
                None => Task::none(),
            }
        }
        Msg::Search(value) => {
            state.search = value;
            Task::none()
        }
        Msg::FilterInstance(value) => {
            state.filter_instance = value.id;
            Task::none()
        }
        Msg::Sort(sort) => {
            state.sort = sort;
            Task::none()
        }
        Msg::Confirm => match state.pending.take().and_then(|p| p.op) {
            Some(op) => run(state, ctx, op),
            None => Task::none(),
        },
        Msg::Cancel => {
            state.pending = None;
            Task::none()
        }
        Msg::Done(result) => {
            state.busy = false;
            state.result = Some(match result {
                Ok(_) => (String::new(), false),
                Err(e) => (e, true),
            });
            Task::done(load(&ctx))
        }
    }
}

fn ask(state: &mut State, ctx: &Ctx, op: anyhow::Result<Op>) {
    state.result = None;
    state.pending = Some(match op.and_then(|op| Ok((preview(ctx, &op)?, op))) {
        Ok((lines, op)) => Pending {
            op: Some(op),
            lines,
        },
        // A refusal (duplicate, unknown instance, …) shows as a panel with no Apply.
        Err(e) => Pending {
            op: None,
            lines: vec![format!("{e:#}")],
        },
    });
}

fn add_form(state: &State) -> crate::overlay::Modal<'_, Msg> {
    let size = st::look().body;
    let model: Element<'_, Msg> = if state.models.is_empty() {
        column![
            text_input(&fl!(loader(), "pool-model"), &state.model)
                .on_input(Msg::Model)
                .size(size),
            st::muted(fl!(loader(), "pool-no-models")),
        ]
        .spacing(st::SPACE_XS)
        .into()
    } else {
        pick_list(
            state.models.clone(),
            Some(state.model.clone()).filter(|m| !m.is_empty()),
            Msg::Model,
        )
        .placeholder(fl!(loader(), "pool-model"))
        .text_size(size)
        .width(Length::Fill)
        .into()
    };
    let field = |label: String, input: Element<'static, Msg>| -> Element<'static, Msg> {
        column![st::caption(label), input].spacing(4).into()
    };
    crate::overlay::Modal {
        title: fl!(loader(), "pool-add-title"),
        body: vec![
            field(
                fl!(loader(), "pool-instance"),
                pick_list(
                    state.data.instances.clone(),
                    state.instance.clone(),
                    Msg::Instance,
                )
                .placeholder(fl!(loader(), "pool-instance"))
                .text_size(size)
                .width(Length::Fill)
                .into(),
            ),
            column![st::caption(fl!(loader(), "pool-model")), model]
                .spacing(4)
                .into(),
            column![
                st::caption(fl!(loader(), "pool-options")),
                text_input("effort=high", &state.options)
                    .on_input(Msg::Options)
                    .size(size),
            ]
            .spacing(4)
            .into(),
        ],
        actions: vec![
            st::btn(st::Btn::Quiet, fl!(loader(), "action-cancel"))
                .on_press(Msg::CloseAdd)
                .into(),
            st::btn(st::Btn::Primary, fl!(loader(), "pool-preview-add"))
                .on_press(Msg::AskAdd)
                .into(),
        ],
        on_cancel: Msg::CloseAdd,
        on_submit: Some(Msg::AskAdd),
    }
}

pub fn modal(state: &State) -> Option<crate::overlay::Modal<'_, Msg>> {
    if state.adding {
        return Some(add_form(state));
    }
    state.pending.as_ref().map(|p| {
        crate::overlay::confirm(
            fl!(loader(), "pool-preview-title"),
            &p.lines,
            (
                fl!(loader(), "action-apply"),
                p.op.as_ref().map(|_| Msg::Confirm),
            ),
            (fl!(loader(), "action-cancel"), Msg::Cancel),
        )
    })
}

/// Add a custom model or an option variant.
pub fn header_actions(state: &State) -> Element<'_, Msg> {
    let idle = !state.busy;
    row![
        st::btn_icon(st::Btn::Primary, st::Icon::Plus, fl!(loader(), "pool-add"))
            .on_press_maybe(idle.then_some(Msg::OpenAdd)),
    ]
    .spacing(st::SPACE_S)
    .into()
}

const TOGGLE_W: f32 = 64.0;
const REMOVE_W: f32 = 28.0;

pub fn view(state: &State) -> Element<'_, Msg> {
    let look = st::look();
    let mut instances: Vec<_> = state
        .data
        .instances
        .iter()
        .map(|id| {
            let name = state.data.names.get(id).unwrap_or(id);
            InstanceChoice {
                id: Some(id.clone()),
                label: if name == id {
                    id.clone()
                } else {
                    format!("{name} ({id})")
                },
            }
        })
        .collect();
    instances.sort_by_key(|i| i.label.to_lowercase());
    instances.insert(
        0,
        InstanceChoice {
            id: None,
            label: fl!(loader(), "pool-all-instances"),
        },
    );
    let selected = instances
        .iter()
        .find(|i| i.id == state.filter_instance)
        .cloned();
    let controls = row![
        text_input(&fl!(loader(), "pool-search"), &state.search)
            .on_input(Msg::Search)
            .size(look.body)
            .width(Length::Fill),
        pick_list(instances, selected, Msg::FilterInstance)
            .placeholder(fl!(loader(), "pool-all-instances"))
            .text_size(look.body)
            .width(190),
        pick_list(
            vec![Sort::Instance, Sort::Model, Sort::Enabled],
            Some(state.sort),
            Msg::Sort
        )
        .text_size(look.body)
        .width(170),
    ]
    .spacing(st::SPACE_S)
    .align_y(Alignment::Center);
    let mut col = column![controls].spacing(look.gap.max(st::SPACE_S));
    if let Some(error) = &state.data.error {
        col = col.push(st::card(st::muted(fl!(
            loader(),
            "load-error",
            what = error
        ))));
    }
    if state.busy {
        col = col.push(st::muted(fl!(loader(), "action-working")));
    }
    if state.data.entries.is_empty() {
        if state.data.error.is_none() {
            col = col.push(st::card(st::muted(fl!(loader(), "pool-empty"))));
        }
        return col.into();
    }
    let header = row![
        st::nowrap(st::caption(fl!(loader(), "pool-instance"))).width(Length::FillPortion(2)),
        st::nowrap(st::caption(fl!(loader(), "pool-model"))).width(Length::FillPortion(3)),
        st::nowrap(st::caption(fl!(loader(), "pool-col-options"))).width(Length::FillPortion(2)),
        st::nowrap(st::caption(fl!(loader(), "pool-col-on"))).width(TOGGLE_W),
        Space::new().width(REMOVE_W),
    ]
    .spacing(st::SPACE_M);
    let mut table = column![header].spacing(look.row_gap);
    let visible = visible_entries(state);
    if visible.is_empty() {
        return col.push(st::muted(fl!(loader(), "pool-no-match"))).into();
    }
    for (i, e) in visible {
        let options = e
            .options
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(" ");
        table = table.push(st::hairline()).push(
            row![
                st::fit(
                    state
                        .data
                        .names
                        .get(&e.instance)
                        .unwrap_or(&e.instance)
                        .as_str(),
                    st::Role::Body,
                    st::Cut::Middle
                )
                .width(Length::FillPortion(2)),
                st::fit(e.model.as_str(), st::Role::Mono, st::Cut::Middle)
                    .width(Length::FillPortion(3)),
                st::fit(options, st::Role::MonoMuted, st::Cut::End).width(Length::FillPortion(2)),
                container(
                    toggler(e.enabled)
                        .size(16)
                        .on_toggle_maybe((!state.busy).then_some(move |_| Msg::AskToggle(i))),
                )
                .width(TOGGLE_W),
                if state.data.custom.iter().any(|c| c.key_eq(e)) {
                    Element::from(tooltip(
                        st::icon_btn(st::Icon::Trash)
                            .on_press_maybe((!state.busy).then_some(Msg::AskRemove(i))),
                        st::body(fl!(loader(), "pool-remove-tip")),
                        tooltip::Position::Left,
                    ))
                } else {
                    Element::from(Space::new().width(REMOVE_W).height(REMOVE_W))
                },
            ]
            .spacing(st::SPACE_M)
            .align_y(Alignment::Center),
        );
    }
    col.push(st::card(table)).into()
}
