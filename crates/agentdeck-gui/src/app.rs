//! Elm shell: sidebar pages, 5 s refresh tick, reads off the UI thread.

use std::time::Duration;

use i18n_embed_fl::fl;
use iced::widget::{button, column, container, row, scrollable, text, toggler};
use iced::{Alignment, Element, Length, Subscription, Task, Theme};

use crate::data::{self, Data};
use crate::i18n::loader;
use crate::overlay::{self, Modal, Toast};
use crate::pages::add_instance::{self, AddMsg};
use crate::pages::drift::{self, Ctx as DriftCtx};
use crate::pages::plugins::Pending;
use crate::pages::{self, harnesses, imagegen, pool, settings, shared, usage};
use crate::style as st;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Page {
    #[default]
    Instances,
    AddInstance,
    Drift,
    UsageSources,
    UsageWidget,
    UsagePoll,
    Limits,
    RouterSettings,
    ImagegenSettings,
    Modules,
    Processes,
    Pool,
    Shared,
    Harnesses,
}

impl Page {
    fn label(self) -> String {
        match self {
            Self::Instances => fl!(loader(), "nav-instances"),
            Self::AddInstance => fl!(loader(), "add-title"),
            Self::Drift => fl!(loader(), "nav-drift"),
            Self::UsageSources => fl!(loader(), "nav-usage-sources"),
            Self::UsageWidget => fl!(loader(), "nav-usage-widget"),
            Self::Limits => fl!(loader(), "nav-limits"),
            Self::UsagePoll => fl!(loader(), "nav-usage-poll"),
            Self::RouterSettings => fl!(loader(), "nav-router-settings"),
            Self::ImagegenSettings => fl!(loader(), "nav-imagegen-settings"),
            Self::Modules => fl!(loader(), "nav-modules"),
            Self::Processes => fl!(loader(), "nav-processes"),
            Self::Pool => fl!(loader(), "nav-pool"),
            Self::Shared => fl!(loader(), "nav-shared"),
            Self::Harnesses => fl!(loader(), "nav-harnesses"),
        }
    }
}

#[derive(Debug, Clone)]
pub enum Message {
    Tick,
    /// A second launch asked this instance to show itself (its activation token).
    Raise(String),
    Loaded(Data),
    SelectPage(Page),
    Add(AddMsg),
    LaunchCli(String),
    CliDone(Result<(), String>),
    ParallelAsk(String),
    ParallelInput(String),
    ParallelCancel,
    ParallelSave,
    ParallelDone(Result<(), String>),
    RenameAsk(String),
    RenameInput(String),
    RenameCancel,
    RenameSave,
    RenameDone(Result<(), String>),
    RemoveAsk(String),
    RemoveCancel,
    RemoveConfirm,
    RemoveDone(Result<RemoveOutcome, String>),
    Noop,
    EnablePlugin(String),
    DisablePlugin(String),
    CancelPlugin,
    ConfirmPlugin,
    PluginDone(Result<String, String>),
    Drift(drift::Message),
    ProcStop(usize),
    ProcConfirm,
    ProcCancel,
    ProcForce(bool),
    ProcDone {
        result: String,
        error: bool,
    },
    ProcToggle(u32, u64),
    ProcFilter(String),
    PluginDetail(String),
    Escape,
    Enter,
    ThemeMode(iced::theme::Mode),
    ToastClose(u64),
    ToastTick,
    Pool(pages::pool::Msg),
    Shared(pages::shared::Msg),
    Harnesses(pages::harnesses::Msg),
    Usage(usage::Msg),
    Settings(settings::Msg),
    Imagegen(imagegen::Msg),
    AutoApply(bool),
    AutoApplyDone(Result<(), String>),
}

#[derive(Debug, Clone)]
pub struct RemoveOutcome {
    pub id: String,
    pub lines: Vec<String>,
    pub home: String,
}

#[derive(Debug, Clone, Default)]
pub struct RemoveAsk {
    pub id: String,
    pub threads: Option<u64>,
    /// Pool models removed together with the instance.
    pub pool: Vec<String>,
    /// Quota sources removed together with the instance.
    pub usage: Vec<String>,
    pub error: Option<String>,
    pub working: bool,
}

fn load_data(paths: agentdeck_core::Paths) -> Task<Message> {
    Task::perform(
        async move {
            tokio::task::spawn_blocking(move || data::load(&paths))
                .await
                .unwrap_or_default()
        },
        Message::Loaded,
    )
}

fn load_drift(paths: &agentdeck_core::Paths) -> Task<Message> {
    let ctx = drift_ctx(paths);
    Task::perform(async move { drift::load(ctx) }, Message::Drift)
}

fn drift_ctx(paths: &agentdeck_core::Paths) -> DriftCtx {
    DriftCtx {
        paths: paths.clone(),
        home: std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_default(),
        t3_root: agentdeck_core::t3::t3_root().ok(),
    }
}

pub struct State {
    page: Page,
    data: Data,
    loading: bool,
    drift: drift::State,
    paths: Option<agentdeck_core::Paths>,
    add: add_instance::Wizard,
    remove: Option<RemoveAsk>,
    rename: Option<pages::instances::Rename>,
    parallel: Option<pages::instances::Parallel>,
    pending: Option<Pending>,
    proc_ui: crate::pages::processes::ProcUi,
    /// Plugins whose technical status is expanded.
    plugin_open: std::collections::HashSet<String>,
    pool: pool::State,
    shared: shared::State,
    harnesses: harnesses::State,
    usage: usage::State,
    settings: settings::State,
    imagegen: imagegen::State,
    toasts: Vec<Toast>,
    next_toast: u64,
    dark: bool,
}

fn home_dir() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_default()
}

fn pool_ctx(paths: &agentdeck_core::Paths) -> pool::Ctx {
    pool::Ctx {
        paths: paths.clone(),
        t3_root: agentdeck_core::t3::t3_root().ok(),
    }
}

fn shared_ctx(paths: &agentdeck_core::Paths) -> shared::Ctx {
    shared::Ctx {
        paths: paths.clone(),
        home: home_dir(),
    }
}

impl State {
    /// Production state; tests build their own via [`state_for_tests`].
    pub fn new() -> (Self, Task<Message>) {
        let paths = agentdeck_core::Paths::from_env().ok();
        let first = paths.clone();
        (
            Self {
                page: Page::Instances,
                data: Data::default(),
                loading: first.is_some(),
                drift: drift::State::default(),
                paths,
                add: add_instance::Wizard::default(),
                remove: None,
                rename: None,
                parallel: None,
                pending: None,
                proc_ui: crate::pages::processes::ProcUi::default(),
                plugin_open: Default::default(),
                pool: pool::State::default(),
                shared: shared::State::default(),
                harnesses: harnesses::State::default(),
                usage: usage::State::default(),
                settings: settings::State::default(),
                imagegen: imagegen::State::default(),
                toasts: Vec::new(),
                next_toast: 0,
                dark: true,
            },
            Task::batch([
                match first {
                    // Drift loads up front too: the sidebar badge needs it.
                    Some(paths) => Task::batch([load_data(paths.clone()), load_drift(&paths)]),
                    None => Task::none(),
                },
                iced::system::theme().map(Message::ThemeMode),
            ]),
        )
    }

    /// Test seam: fixed paths and preloaded data, no async involved.
    pub fn for_tests(paths: agentdeck_core::Paths, data: Data) -> Self {
        Self {
            page: Page::Instances,
            data,
            loading: false,
            drift: drift::State::default(),
            paths: Some(paths),
            add: add_instance::Wizard::default(),
            remove: None,
            rename: None,
            parallel: None,
            pending: None,
            proc_ui: crate::pages::processes::ProcUi::default(),
            plugin_open: Default::default(),
            pool: pool::State::default(),
            shared: shared::State::default(),
            harnesses: harnesses::State::default(),
            usage: usage::State::default(),
            settings: settings::State::default(),
            imagegen: imagegen::State::default(),
            toasts: Vec::new(),
            next_toast: 0,
            dark: true,
        }
    }

    pub fn toasts(&self) -> &[Toast] {
        &self.toasts
    }

    fn toast(&mut self, text: String, error: bool) {
        self.next_toast += 1;
        self.toasts.push(Toast {
            id: self.next_toast,
            text,
            error,
            at: std::time::Instant::now(),
        });
    }

    pub fn page(&self) -> Page {
        self.page
    }

    pub fn proc_ui(&self) -> &crate::pages::processes::ProcUi {
        &self.proc_ui
    }
}

pub fn update(state: &mut State, message: Message) -> Task<Message> {
    match message {
        Message::SelectPage(page) => {
            state.page = page;
            state.pending = None;
            state.remove = None;
            state.rename = None;
            state.parallel = None;
            if page == Page::AddInstance {
                state.add = add_instance::Wizard::default();
            }
            // Status for these pages is read when they open and after actions.
            match (page, state.paths.clone()) {
                (Page::Pool, Some(paths)) => {
                    let ctx = pool_ctx(&paths);
                    Task::perform(async move { pool::load(&ctx) }, Message::Pool)
                }
                (Page::Shared, Some(paths)) => {
                    let ctx = shared_ctx(&paths);
                    Task::perform(async move { shared::load(&ctx) }, Message::Shared)
                }
                (Page::Harnesses, _) => Task::perform(harnesses::load(), Message::Harnesses),
                (Page::UsageSources | Page::UsageWidget, Some(paths)) => {
                    Task::perform(async move { usage::load(&paths) }, Message::Usage)
                }
                (Page::UsagePoll | Page::RouterSettings, Some(paths)) => {
                    Task::perform(async move { settings::load(&paths) }, Message::Settings)
                }
                (Page::ImagegenSettings, Some(paths)) => {
                    Task::perform(async move { imagegen::load(&paths) }, Message::Imagegen)
                }
                _ => Task::none(),
            }
        }
        // A slow disk must not stack refreshes: each tick waits for the previous load.
        Message::Tick => match state.paths.clone() {
            Some(paths) if !state.loading => {
                state.loading = true;
                Task::batch([load_data(paths.clone()), load_drift(&paths)])
            }
            _ => Task::none(),
        },
        Message::Loaded(data) => {
            state.loading = false;
            state.data = data;
            Task::none()
        }
        Message::Add(msg) => match state.paths.clone() {
            Some(paths) => add_instance::update_wizard(&mut state.add, msg, &paths),
            None => Task::none(),
        },
        Message::LaunchCli(id) => {
            let Some(paths) = state.paths.clone() else {
                return Task::none();
            };
            Task::perform(
                async move {
                    tokio::task::spawn_blocking(move || {
                        agentdeck_core::launch::launch_cli(&paths, &id)
                            .map_err(|e| format!("{e:#}"))
                    })
                    .await
                    .map_err(|e| e.to_string())
                    .and_then(|r| r)
                },
                Message::CliDone,
            )
        }
        Message::CliDone(result) => {
            if let Err(error) = result {
                state.toast(fl!(loader(), "cli-open-error", what = error), true);
            }
            Task::none()
        }
        Message::ParallelAsk(id) => {
            if state.parallel.as_ref().is_some_and(|edit| edit.busy) {
                return Task::none();
            }
            if let Some(inst) = state.data.instances.iter().find(|i| i.id == id) {
                state.parallel = Some(pages::instances::Parallel {
                    id,
                    value: inst.max_parallel.map(|n| n.to_string()).unwrap_or_default(),
                    busy: false,
                    error: None,
                });
                return iced::widget::operation::focus(pages::instances::PARALLEL_INPUT);
            }
            Task::none()
        }
        Message::ParallelInput(value) => {
            if let Some(edit) = state.parallel.as_mut().filter(|e| !e.busy) {
                edit.value = value;
                edit.error = None;
            }
            Task::none()
        }
        Message::ParallelCancel => {
            if state.parallel.as_ref().is_none_or(|e| !e.busy) {
                state.parallel = None;
            }
            Task::none()
        }
        Message::ParallelSave => {
            let Some(edit) = state.parallel.as_mut().filter(|e| !e.busy) else {
                return Task::none();
            };
            let limit = match pages::instances::parse_parallel(&edit.value) {
                Ok(limit) => limit,
                Err(()) => {
                    edit.error = Some(fl!(loader(), "parallel-invalid"));
                    return Task::none();
                }
            };
            let Some(paths) = state.paths.clone() else {
                return Task::none();
            };
            edit.busy = true;
            edit.error = None;
            let id = edit.id.clone();
            Task::perform(
                async move {
                    tokio::task::spawn_blocking(move || {
                        agentdeck_core::registry::set_max_parallel(&paths, &id, limit, false)
                            .map(|_| ())
                            .map_err(|e| format!("{e:#}"))
                    })
                    .await
                    .map_err(|e| e.to_string())
                    .and_then(|r| r)
                },
                Message::ParallelDone,
            )
        }
        Message::ParallelDone(result) => {
            match result {
                Ok(()) => state.parallel = None,
                Err(error) => {
                    if let Some(edit) = state.parallel.as_mut() {
                        edit.busy = false;
                        edit.error = Some(error);
                    }
                }
            }
            match state.paths.clone() {
                Some(paths) => load_data(paths),
                None => Task::none(),
            }
        }
        Message::RenameAsk(id) => {
            if let Some(inst) = state.data.instances.iter().find(|i| i.id == id) {
                state.rename = Some(pages::instances::Rename {
                    id,
                    name: inst.display_name.clone(),
                    busy: false,
                    error: None,
                });
                return iced::widget::operation::focus(pages::instances::RENAME_INPUT);
            }
            Task::none()
        }
        Message::RenameInput(value) => {
            if let Some(rename) = state.rename.as_mut().filter(|r| !r.busy) {
                rename.name = value;
            }
            Task::none()
        }
        Message::RenameCancel => {
            if state.rename.as_ref().is_none_or(|r| !r.busy) {
                state.rename = None;
            }
            Task::none()
        }
        Message::RenameSave => {
            let Some(rename) = state.rename.as_mut().filter(|r| !r.busy) else {
                return Task::none();
            };
            let Some(paths) = state.paths.clone() else {
                return Task::none();
            };
            rename.busy = true;
            let id = rename.id.clone();
            let name = rename.name.clone();
            Task::perform(
                async move {
                    tokio::task::spawn_blocking(move || {
                        let root = agentdeck_core::t3::t3_root().ok();
                        agentdeck_core::t3::rename_instance(
                            &paths,
                            root.as_deref(),
                            &id,
                            &name,
                            false,
                        )
                        .map(|_| ())
                        .map_err(|e| format!("{e:#}"))
                    })
                    .await
                    .map_err(|e| e.to_string())
                    .and_then(|r| r)
                },
                Message::RenameDone,
            )
        }
        Message::RenameDone(result) => {
            match result {
                Ok(()) => state.rename = None,
                Err(error) => {
                    if let Some(rename) = state.rename.as_mut() {
                        rename.busy = false;
                        rename.error = Some(error);
                    }
                }
            }
            match state.paths.clone() {
                Some(paths) => load_data(paths),
                None => Task::none(),
            }
        }
        Message::RemoveAsk(id) => {
            // Read-only thread count plus the pool and usage entries that go
            // with the instance, for the confirmation.
            let threads = agentdeck_core::t3::t3_root()
                .ok()
                .and_then(|root| agentdeck_core::t3::thread_count(&root, &id).unwrap_or(None));
            let reg = state
                .paths
                .as_ref()
                .and_then(|p| agentdeck_core::registry::Registry::load(p).ok());
            let pool = reg
                .as_ref()
                .map(|reg| {
                    reg.pool
                        .iter()
                        .filter(|e| e.instance == id)
                        .map(|e| e.model.clone())
                        .collect()
                })
                .unwrap_or_default();
            let usage = reg
                .as_ref()
                .map(|reg| {
                    reg.usage
                        .iter()
                        .filter(|u| u.instance.as_deref() == Some(id.as_str()))
                        .map(|u| u.id.clone())
                        .collect()
                })
                .unwrap_or_default();
            state.remove = Some(RemoveAsk {
                id,
                threads,
                pool,
                usage,
                error: None,
                working: false,
            });
            Task::none()
        }
        Message::RemoveCancel => {
            state.remove = None;
            Task::none()
        }
        Message::RemoveConfirm => {
            let Some(ask) = state.remove.clone() else {
                return Task::none();
            };
            let id = ask.id.clone();
            match state.paths.clone() {
                Some(paths) => {
                    if let Some(current) = state.remove.as_mut() {
                        current.working = true;
                    }
                    Task::perform(
                        async move {
                            agentdeck_core::registry::remove_instance(&paths, &id, false)
                                .map(|(changes, home)| RemoveOutcome {
                                    id: id.clone(),
                                    lines: changes.iter().map(|c| c.describe()).collect(),
                                    home,
                                })
                                .map_err(|e| format!("{e:#}"))
                        },
                        Message::RemoveDone,
                    )
                }
                None => Task::none(),
            }
        }
        Message::RemoveDone(result) => {
            match result {
                Ok(outcome) => {
                    state.data.instances.retain(|i| i.id != outcome.id);
                    state.remove = None;
                    state.toast(fl!(loader(), "remove-done", id = outcome.id), false);
                }
                Err(e) => {
                    if let Some(ask) = state.remove.as_mut() {
                        ask.working = false;
                        ask.error = Some(e);
                    }
                }
            }
            match state.paths.clone() {
                Some(paths) => load_data(paths),
                None => Task::none(),
            }
        }
        Message::ProcStop(i) => {
            // Fresh identity-checked member list for the confirmation; the
            // snapshot rows are the fallback (e.g. the group just exited).
            let pending = state.data.processes.get(i).map(|g| {
                // Container processes have their own process groups; list the snapshot.
                let members = match g.kind {
                    agentdeck_core::processes::GroupKind::Container { .. } => g.processes.clone(),
                    _ => agentdeck_core::processes::group_members(
                        &agentdeck_core::processes::proc_root(),
                        g.pid,
                        g.start_ticks,
                    )
                    .unwrap_or_else(|_| g.processes.clone()),
                };
                crate::pages::processes::PendingStop {
                    group: i,
                    title: g.title.clone(),
                    members,
                }
            });
            state.proc_ui.pending = pending;
            state.proc_ui.force = false;
            Task::none()
        }
        Message::ProcConfirm => match state.proc_ui.pending.clone() {
            // The owner's click is the approval: stop off the UI thread.
            Some(pending) => {
                let group = state.data.processes.get(pending.group).cloned();
                let force = state.proc_ui.force;
                state.proc_ui.pending = None;
                match group.filter(|g| g.can_stop) {
                    Some(g) => Task::perform(
                        async move {
                            let outcome = tokio::task::spawn_blocking(move || match &g.kind {
                                agentdeck_core::processes::GroupKind::Container { id, .. } => {
                                    agentdeck_core::processes::stop_container(
                                        id,
                                        std::time::Duration::from_secs(if force { 5 } else { 10 }),
                                    )
                                }
                                _ => agentdeck_core::processes::stop_group(
                                    g.pid,
                                    g.start_ticks,
                                    std::time::Duration::from_secs(5),
                                    force,
                                ),
                            })
                            .await
                            .map_err(|e| anyhow::anyhow!("{e}"))
                            .and_then(|r| r);
                            match outcome {
                                Ok(o) => {
                                    let result = crate::pages::processes::describe_outcome(
                                        &g.title, &o.outcome,
                                    );
                                    Message::ProcDone {
                                        result,
                                        error: false,
                                    }
                                }
                                Err(e) => Message::ProcDone {
                                    result: format!("{}: {e:#}", g.title),
                                    error: true,
                                },
                            }
                        },
                        |m| m,
                    ),
                    None => Task::none(),
                }
            }
            None => Task::none(),
        },
        Message::ProcCancel => {
            state.proc_ui.pending = None;
            Task::none()
        }
        Message::ProcForce(force) => {
            state.proc_ui.force = force;
            Task::none()
        }
        Message::ProcDone { result, error } => {
            state.toast(result, error);
            // Refresh behind the toast.
            match state.paths.clone() {
                Some(paths) => load_data(paths),
                None => Task::none(),
            }
        }
        Message::Pool(msg) => {
            let task = match state.paths.clone() {
                Some(paths) => {
                    pool::update(&mut state.pool, pool_ctx(&paths), msg).map(Message::Pool)
                }
                None => Task::none(),
            };
            if let Some((text, error)) = state.pool.result.take()
                && (error || !text.is_empty())
            {
                state.toast(text, error);
            }
            task
        }
        Message::Shared(msg) => {
            let task = match state.paths.clone() {
                Some(paths) => {
                    shared::update(&mut state.shared, shared_ctx(&paths), msg).map(Message::Shared)
                }
                None => Task::none(),
            };
            if let Some((text, error)) = state.shared.result.take() {
                state.toast(text, error);
            }
            task
        }
        Message::Harnesses(msg) => {
            let task = harnesses::update(&mut state.harnesses, msg).map(Message::Harnesses);
            if let Some((text, error)) = state.harnesses.result.take() {
                state.toast(text, error);
            }
            task
        }
        Message::Usage(msg) => {
            let task = match state.paths.clone() {
                Some(paths) => usage::update(&mut state.usage, paths, msg).map(Message::Usage),
                None => Task::none(),
            };
            if let Some((text, error)) = state.usage.result.take() {
                state.toast(text, error);
            }
            task
        }
        Message::Imagegen(msg) => {
            let task = match state.paths.clone() {
                Some(paths) => {
                    imagegen::update(&mut state.imagegen, paths, msg).map(Message::Imagegen)
                }
                None => Task::none(),
            };
            if let Some((text, error)) = state.imagegen.result.take() {
                state.toast(text, error);
            }
            task
        }
        Message::Settings(msg) => {
            let task = match state.paths.clone() {
                Some(paths) => {
                    settings::update(&mut state.settings, paths, msg).map(Message::Settings)
                }
                None => Task::none(),
            };
            if let Some((text, error)) = state.settings.result.take() {
                state.toast(text, error);
            }
            task
        }
        Message::AutoApply(on) => match state.paths.clone() {
            Some(paths) => Task::perform(
                async move {
                    tokio::task::spawn_blocking(move || {
                        agentdeck_core::sync::set_auto_apply(&paths, on, false)
                            .map(|_| ())
                            .map_err(|e| format!("{e:#}"))
                    })
                    .await
                    .map_err(|e| e.to_string())
                    .and_then(|r| r)
                },
                Message::AutoApplyDone,
            ),
            None => Task::none(),
        },
        Message::AutoApplyDone(result) => {
            if let Err(error) = result {
                state.toast(error, true);
            }
            match state.paths.clone() {
                Some(paths) => load_data(paths),
                None => Task::none(),
            }
        }
        Message::PluginDetail(name) => {
            if !state.plugin_open.remove(&name) {
                state.plugin_open.insert(name);
            }
            Task::none()
        }
        Message::ProcToggle(pid, ticks) => {
            if !state.proc_ui.expanded.remove(&(pid, ticks)) {
                state.proc_ui.expanded.insert((pid, ticks));
            }
            Task::none()
        }
        Message::ProcFilter(filter) => {
            state.proc_ui.filter = filter;
            Task::none()
        }
        // Keys act on the open modal only; its messages decide what happens.
        Message::Escape if state.parallel.is_some() => update(state, Message::ParallelCancel),
        Message::Escape if state.rename.is_some() => update(state, Message::RenameCancel),
        Message::Escape => match modal(state).map(|m| m.on_cancel) {
            Some(msg) => update(state, msg),
            None => Task::none(),
        },
        Message::Enter => match modal(state).and_then(|m| m.on_submit) {
            Some(msg) => update(state, msg),
            None => Task::none(),
        },
        Message::Raise(token) => crate::single::raise(token),
        Message::ThemeMode(mode) => {
            state.dark = mode != iced::theme::Mode::Light;
            Task::none()
        }
        Message::ToastClose(id) => {
            state.toasts.retain(|t| t.id != id);
            Task::none()
        }
        Message::ToastTick => {
            let now = std::time::Instant::now();
            state.toasts.retain(|t| !overlay::expired(t, now));
            Task::none()
        }
        Message::Noop => Task::none(),
        Message::Drift(inner) => {
            let task = match state.paths.clone() {
                Some(paths) => {
                    drift::update(&mut state.drift, drift_ctx(&paths), inner).map(Message::Drift)
                }
                None => Task::none(),
            };
            for notice in std::mem::take(&mut state.drift.notices) {
                let error = notice.starts_with("failed");
                state.toast(notice, error);
            }
            task
        }
        Message::EnablePlugin(name) => {
            preview_plugin(state, &name, true);
            Task::none()
        }
        Message::DisablePlugin(name) => {
            preview_plugin(state, &name, false);
            Task::none()
        }
        Message::CancelPlugin => {
            state.pending = None;
            Task::none()
        }
        Message::ConfirmPlugin => match (state.paths.clone(), state.pending.clone()) {
            (Some(paths), Some(pending)) => {
                state.pending = None;
                Task::perform(
                    async move { apply_plugin(&paths, &pending) },
                    Message::PluginDone,
                )
            }
            _ => Task::none(),
        },
        Message::PluginDone(result) => {
            if let Some(paths) = state.paths.clone() {
                state.data = data::load(&paths);
            }
            match result {
                Ok(text) => state.toast(text, false),
                Err(error) => state.toast(error, true),
            }
            Task::none()
        }
    }
}

/// Read-only preview of the enable/disable changes; failures surface as a
/// one-line pending preview instead of a popup.
fn preview_plugin(state: &mut State, name: &str, enable: bool) {
    let Some(paths) = state.paths.clone() else {
        return;
    };
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_default();
    let exe = agentdeck_core::current_exe().unwrap_or_default();
    let files = agentdeck_core::plugins::default_instruction_files(&home);
    let planned = if enable {
        agentdeck_core::plugins::plan_enable(&paths, &home, &files, &exe, name, false)
    } else {
        agentdeck_core::plugins::plan_disable(&paths, &home, &files, &exe, name)
    };
    state.pending = Some(match planned {
        Ok(changes) => Pending {
            name: name.to_string(),
            enable,
            preview: changes.iter().map(|c| c.describe()).collect(),
        },
        Err(error) => Pending {
            name: name.to_string(),
            enable,
            preview: vec![format!("{error:#}")],
        },
    });
}

/// Apply a confirmed plugin change off the UI thread.
fn apply_plugin(paths: &agentdeck_core::Paths, pending: &Pending) -> Result<String, String> {
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_default();
    let exe = agentdeck_core::current_exe().unwrap_or_default();
    let files = agentdeck_core::plugins::default_instruction_files(&home);
    let planned = if pending.enable {
        agentdeck_core::plugins::plan_enable(paths, &home, &files, &exe, &pending.name, false)
    } else {
        agentdeck_core::plugins::plan_disable(paths, &home, &files, &exe, &pending.name)
    }
    .map_err(|e| format!("{e:#}"))?;
    agentdeck_core::plugins::apply_all(paths, &pending.name, pending.enable, &planned)
        .map_err(|e| format!("{e:#}"))?;
    agentdeck_core::plugins::sync_plugin_skills(paths, &home).map_err(|e| format!("{e:#}"))?;
    Ok(if pending.enable {
        format!("enabled {}", pending.name)
    } else {
        format!("disabled {}", pending.name)
    })
}

pub fn subscription(state: &State) -> Subscription<Message> {
    // The Processes page shows live resources; everything else keeps 5 s.
    let secs = if state.page == Page::Processes { 2 } else { 5 };
    let mut subs = vec![
        iced::time::every(Duration::from_secs(secs)).map(|_| Message::Tick),
        add_instance::subscription(&state.add),
        iced::keyboard::listen().filter_map(|event| match event {
            iced::keyboard::Event::KeyPressed {
                key: iced::keyboard::Key::Named(named),
                ..
            } => match named {
                iced::keyboard::key::Named::Escape => Some(Message::Escape),
                iced::keyboard::key::Named::Enter => Some(Message::Enter),
                _ => None,
            },
            _ => None,
        }),
        iced::system::theme_changes().map(Message::ThemeMode),
        crate::single::subscription().map(Message::Raise),
    ];
    if state.toasts.iter().any(|t| !t.error) {
        subs.push(iced::time::every(Duration::from_millis(500)).map(|_| Message::ToastTick));
    }
    Subscription::batch(subs)
}

/// The dialog open on the current page, if any.
fn modal(state: &State) -> Option<Modal<'_, Message>> {
    if let Some(p) = &state.pending {
        return Some(pages::plugins::modal(p));
    }
    match state.page {
        Page::Instances => state.remove.as_ref().map(pages::instances::remove_modal),
        Page::Processes => state
            .proc_ui
            .pending
            .as_ref()
            .map(|p| pages::processes::stop_modal(p, state.proc_ui.force)),
        Page::UsageSources | Page::UsageWidget => {
            usage::modal(&state.usage).map(|m| m.map(Message::Usage))
        }
        Page::Drift => drift::modal(&state.drift).map(|m| m.map(Message::Drift)),
        Page::Pool => pool::modal(&state.pool).map(|m| m.map(Message::Pool)),
        Page::Shared => shared::modal(&state.shared).map(|m| m.map(Message::Shared)),
        Page::Harnesses => harnesses::modal(&state.harnesses).map(|m| m.map(Message::Harnesses)),
        Page::AddInstance
        | Page::Limits
        | Page::UsagePoll
        | Page::RouterSettings
        | Page::ImagegenSettings
        | Page::Modules => None,
    }
}

fn drift_count(state: &State) -> usize {
    state
        .drift
        .snapshot
        .groups
        .iter()
        .map(drift::open_items)
        .sum()
}

fn nav_item(state: &State, page: Page, icon: st::Icon) -> Element<'_, Message> {
    let look = st::look();
    let selected =
        state.page == page || (page == Page::Instances && state.page == Page::AddInstance);
    let mut line = row![
        st::icon_colored(icon, look.body + 2.0, move |c| if selected {
            c.text
        } else {
            c.muted
        }),
        text(page.label()).size(look.body).width(Length::Fill),
    ]
    .spacing(10)
    .align_y(Alignment::Center);
    if page == Page::Drift && drift_count(state) > 0 {
        line = line.push(st::pill(drift_count(state).to_string(), st::Tone::Warn));
    }
    button(line)
        .width(Length::Fill)
        .padding([look.button_pad[0] + 2.0, 10.0])
        .style(st::nav_style(selected))
        .on_press(Message::SelectPage(page))
        .into()
}

fn sidebar(state: &State) -> Element<'_, Message> {
    let look = st::look();
    let group = |label: String, items: &[(Page, st::Icon)]| {
        let mut col = column![container(st::caption(label)).padding([0, 10])].spacing(2);
        for &(page, icon) in items {
            col = col.push(nav_item(state, page, icon));
        }
        col
    };
    // A module group: its name, its on/off switch, then its pages.
    let module = |label: String, plugin: &'static str, items: &[(Page, st::Icon)]| {
        let mut col = column![
            row![
                container(st::caption(label)).width(Length::Fill),
                pages::plugins::switch(&state.data, plugin),
            ]
            .align_y(Alignment::Center)
            .padding([0, 10])
        ]
        .spacing(2);
        for &(page, icon) in items {
            col = col.push(nav_item(state, page, icon));
        }
        col
    };
    let col = column![
        container(st::heading(fl!(loader(), "app-title"))).padding([4, 10]),
        group(
            fl!(loader(), "nav-group-accounts"),
            &[
                (Page::Instances, st::Icon::Users),
                (Page::Drift, st::Icon::Compare),
            ],
        ),
        module(
            fl!(loader(), "nav-group-usage"),
            agentdeck_core::plugins::PLUGIN_AI_USAGE,
            &[
                (Page::UsageSources, st::Icon::Activity),
                (Page::UsageWidget, st::Icon::Square),
                (Page::UsagePoll, st::Icon::RefreshCw),
            ],
        ),
        module(
            fl!(loader(), "nav-group-router"),
            agentdeck_core::plugins::PLUGIN_ROUTER,
            &[
                (Page::Pool, st::Icon::Layers),
                (Page::Limits, st::Icon::Gauge),
                (Page::RouterSettings, st::Icon::Split),
            ],
        ),
        module(
            fl!(loader(), "nav-group-imagegen"),
            agentdeck_core::plugins::PLUGIN_IMAGEGEN,
            &[(Page::ImagegenSettings, st::Icon::Image)],
        ),
        module(
            fl!(loader(), "nav-group-agents"),
            agentdeck_core::plugins::PLUGIN_AGENT_PROCESS,
            &[(Page::Processes, st::Icon::Terminal)],
        ),
        group(
            fl!(loader(), "nav-group-system"),
            &[
                (Page::Shared, st::Icon::Share),
                (Page::Harnesses, st::Icon::Wrench),
                (Page::Modules, st::Icon::Plug),
            ],
        ),
    ]
    .spacing(look.gap.max(12.0) + 2.0);
    // Scrolls when the window is shorter than the full module list.
    container(
        scrollable(col)
            .direction(scrollable::Direction::Vertical(
                scrollable::Scrollbar::new()
                    .width(4)
                    .scroller_width(4)
                    .spacing(4),
            ))
            .height(Length::Fill),
    )
    .width(look.sidebar_width)
    .height(Length::Fill)
    .padding([look.page_pad, 10.0])
    .style(st::sidebar_style)
    .into()
}

fn page_header<'a>(title: String, action: Option<Element<'a, Message>>) -> Element<'a, Message> {
    let look = st::look();
    container(
        row![container(st::title(title)).width(Length::Fill)]
            .push(action)
            .spacing(st::SPACE_M)
            .align_y(Alignment::Center),
    )
    .padding([look.page_pad * 0.7, look.page_pad])
    .into()
}

fn header(state: &State) -> Element<'_, Message> {
    let t = |key: &str| loader().get(key);
    let (title, action): (String, Option<Element<'_, Message>>) = match state.page {
        Page::Instances => (
            t("instances-title"),
            Some(
                st::btn_icon(st::Btn::Primary, st::Icon::Plus, t("instances-add"))
                    .on_press(Message::SelectPage(Page::AddInstance))
                    .into(),
            ),
        ),
        Page::AddInstance => (
            t("add-title"),
            Some(
                st::btn_icon(st::Btn::Quiet, st::Icon::ArrowLeft, t("add-back"))
                    .on_press(Message::SelectPage(Page::Instances))
                    .into(),
            ),
        ),
        Page::Drift => (
            t("drift-title"),
            Some(
                row![
                    st::nowrap(st::caption(t("sync-auto-apply"))),
                    toggler(state.data.auto_apply)
                        .size(16)
                        .on_toggle(Message::AutoApply),
                ]
                .spacing(st::SPACE_S)
                .align_y(Alignment::Center)
                .into(),
            ),
        ),
        Page::UsageSources => (
            t("usage-sources-title"),
            Some(usage::header_action(&state.usage).map(Message::Usage)),
        ),
        Page::UsageWidget => (t("usage-widget-title"), None),
        Page::Limits => (t("limits-title"), None),
        Page::UsagePoll => (t("usage-poll-title"), None),
        Page::RouterSettings => (t("router-settings-title"), None),
        Page::ImagegenSettings => (t("plugin-imagegen"), None),
        Page::Modules => (t("modules-title"), None),
        Page::Pool => (
            t("pool-title"),
            Some(pool::header_actions(&state.pool).map(Message::Pool)),
        ),
        Page::Shared => (t("shared-title"), None),
        Page::Harnesses => (t("harness-title"), None),
        Page::Processes => (
            t("processes-title"),
            Some(pages::processes::filter_input(&state.proc_ui)),
        ),
    };
    page_header(title, action)
}

pub fn view(state: &State) -> Element<'_, Message> {
    let look = st::look();
    let content: Element<'_, Message> = match state.page {
        Page::Instances => pages::instances::view_with_rename(&state.data, state.rename.as_ref()),
        Page::AddInstance => add_instance::view(&state.add),
        Page::Drift => drift::view(&state.drift).map(Message::Drift),
        Page::UsageSources => usage::sources_view(&state.usage, &state.data).map(Message::Usage),
        Page::UsageWidget => usage::widget_view(&state.usage).map(Message::Usage),
        Page::UsagePoll => settings::poll_view(&state.settings).map(Message::Settings),
        Page::Limits => pages::instances::limits_view(&state.data, state.parallel.as_ref()),
        Page::RouterSettings => settings::router_view(&state.settings).map(Message::Settings),
        Page::ImagegenSettings => imagegen::view(&state.imagegen).map(Message::Imagegen),
        Page::Modules => pages::plugins::view(&state.data, &state.plugin_open),
        Page::Processes => pages::processes::view(&state.data, &state.proc_ui),
        Page::Pool => pool::view(&state.pool).map(Message::Pool),
        Page::Shared => shared::view(&state.shared).map(Message::Shared),
        Page::Harnesses => harnesses::view(&state.harnesses).map(Message::Harnesses),
    };
    let main = column![header(state), st::hairline()];
    let body = container(
        container(content)
            .max_width(look.content_max)
            .padding(look.page_pad),
    )
    .center_x(Length::Fill);
    let main = main.push(
        scrollable(body)
            .direction(scrollable::Direction::Vertical(
                scrollable::Scrollbar::new()
                    .width(6)
                    .scroller_width(6)
                    .margin(2),
            ))
            .style(st::scroll_style)
            .height(Length::Fill)
            .width(Length::Fill),
    );
    let shell = row![sidebar(state), st::hairline_v(), main].height(Length::Fill);
    overlay::layer(
        shell.into(),
        overlay::toasts(&state.toasts, Message::ToastClose),
        modal(state),
    )
}

/// Palette following the system dark/light preference.
pub fn theme(state: &State) -> Option<Theme> {
    Some(st::theme(state.dark))
}

pub fn style(_state: &State, theme: &Theme) -> iced::theme::Style {
    st::app_style(theme)
}

pub fn title(_state: &State) -> String {
    fl!(loader(), "app-title")
}
