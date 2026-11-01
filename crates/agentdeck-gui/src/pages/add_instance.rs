//! Add-instance wizard, a stepper: Details (form), Preview (core change
//! list), Login (embedded PTY login), Finish (optional wrapper install and
//! T3 sync). Core logic lives in `agentdeck-core`; this
//! module is view state plus thin async wiring. Long work runs off the UI
//! thread via `Task::perform`; login PTY output is drained from the login
//! event channel on a 200 ms tick subscription (never blocking the UI).

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Duration;

use agentdeck_core::login::{self, LoginEvent, LoginSession, LoginTarget};
use agentdeck_core::registry::{AddSpec, Harness};
use agentdeck_core::{Paths, Registry};
use i18n_embed_fl::fl;
use iced::widget::{Space, column, container, radio, row, text_input};
use iced::{Alignment, Element, Length, Subscription, Task};

use crate::app::Message;
use crate::i18n::loader;
use crate::style as st;

const OUTPUT_TAIL: usize = 30;
const PORT_BUSY_HINT: &str = "1455";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HarnessChoice {
    #[default]
    Claude,
    Codex,
    Opencode,
    Agy,
}

impl HarnessChoice {
    fn as_harness(self) -> Harness {
        match self {
            Self::Claude => Harness::Claude,
            Self::Codex => Harness::Codex,
            Self::Opencode => Harness::Opencode,
            Self::Agy => Harness::Agy,
        }
    }
}

/// Suggested home for a new id: `~/.claude-<id>`, `~/.codex-t3/<id>`, an
/// OpenCode data dir (its parent becomes XDG_DATA_HOME) or an agy login dir.
pub fn suggest_home(harness: HarnessChoice, id: &str) -> String {
    let id = id.trim();
    match harness {
        HarnessChoice::Codex => format!("~/.codex-t3/{id}"),
        HarnessChoice::Opencode => format!("~/.opencode-{id}/opencode"),
        HarnessChoice::Agy => format!("~/.agy-{id}"),
        HarnessChoice::Claude => format!("~/.claude-{id}"),
    }
}

/// Expand a leading `~` against HOME (same rule as the CLI login command).
pub fn expand_home(value: &str) -> PathBuf {
    if (value == "~" || value.starts_with("~/"))
        && let Some(home) = std::env::var_os("HOME").map(PathBuf::from)
    {
        return if value == "~" {
            home
        } else {
            home.join(&value[2..])
        };
    }
    PathBuf::from(value)
}

#[derive(Debug, Clone, Default)]
pub enum Preview {
    #[default]
    Idle,
    Working,
    Ready(Vec<String>),
    Done(Vec<String>),
    Error(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum LoginStatus {
    #[default]
    Idle,
    Active,
    Done,
    Failed(String),
    Cancelled,
    SpawnError(String),
}

/// Live login panel state. The PTY child runs in `session`; its output
/// channel is drained on the tick subscription. Pasted codes stay in `code`
/// only until sent and are never logged.
pub struct LoginState {
    pub session: Option<LoginSession>,
    pub url: Option<String>,
    pub device_code: Option<String>,
    pub prompt: bool,
    pub code: String,
    pub tail: VecDeque<String>,
    pub status: LoginStatus,
    pub device_auth: bool,
    pub open_warning: Option<String>,
}

impl Default for LoginState {
    fn default() -> Self {
        Self {
            session: None,
            url: None,
            device_code: None,
            prompt: false,
            code: String::new(),
            tail: VecDeque::new(),
            status: LoginStatus::Idle,
            device_auth: false,
            open_warning: None,
        }
    }
}

impl LoginState {
    pub fn active(&self) -> bool {
        self.session.is_some()
    }

    fn push_output(&mut self, text: String) {
        for line in text.split('\n') {
            let line = line.trim_end_matches('\r');
            if line.is_empty() {
                continue;
            }
            if self.tail.len() >= OUTPUT_TAIL {
                self.tail.pop_front();
            }
            self.tail.push_back(line.to_string());
        }
    }

    /// Fold one channel event into the panel. Returns true on child exit.
    pub fn apply_event(&mut self, event: LoginEvent) -> bool {
        match event {
            LoginEvent::Url(url) => {
                self.url = Some(url);
            }
            LoginEvent::DeviceCode(code) => {
                self.device_code = Some(code);
            }
            LoginEvent::CodePrompt => {
                self.prompt = true;
            }
            LoginEvent::Output(text) => {
                self.push_output(text);
            }
            LoginEvent::Exited(code) => {
                self.session = None;
                self.status = if code == 0 {
                    LoginStatus::Done
                } else {
                    LoginStatus::Failed(format!("exit {code}"))
                };
                return true;
            }
        }
        false
    }
}

#[derive(Default)]
pub struct Wizard {
    pub harness: HarnessChoice,
    pub id: String,
    pub display_name: String,
    pub home: String,
    pub home_dirty: bool,
    pub preview: Preview,
    pub added_id: Option<String>,
    pub login: LoginState,
    pub wrapper: Preview,
    pub t3: Preview,
    /// Login step left for Finish.
    pub finish: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Details,
    Preview,
    Login,
    Finish,
}

/// The step follows from the wizard state, so every existing message keeps
/// working; only Next and Edit move between steps explicitly.
pub fn step(wiz: &Wizard) -> Step {
    match (&wiz.added_id, &wiz.preview) {
        (Some(_), _) if wiz.finish => Step::Finish,
        (Some(_), _) => Step::Login,
        (None, Preview::Ready(_) | Preview::Working) => Step::Preview,
        _ => Step::Details,
    }
}

#[derive(Debug, Clone)]
pub enum AddMsg {
    Harness(HarnessChoice),
    Id(String),
    DisplayName(String),
    Home(String),
    UseSuggestion,
    Preview,
    /// Back from Preview to Details.
    Edit,
    /// Login to Finish.
    Next,
    PreviewReady(Result<Vec<String>, String>),
    Apply,
    ApplyDone(Result<Vec<String>, String>),
    LoginStart,
    LoginPoll,
    LoginCodeInput(String),
    LoginSend,
    LoginSendDone,
    LoginCancel,
    LoginOpen,
    LoginCopy,
    DeviceAuth(bool),
    RetryDeviceAuth,
    WrapperPreview,
    WrapperReady(Result<Vec<String>, String>),
    WrapperApply,
    WrapperDone(Result<Vec<String>, String>),
    T3Preview,
    T3Ready(Result<Vec<String>, String>),
    T3Apply,
    T3Done(Result<Vec<String>, String>),
}

fn spec_of(wiz: &Wizard) -> anyhow::Result<AddSpec> {
    let harness = wiz.harness.as_harness();
    let id = wiz.id.trim().to_string();
    Ok(AddSpec {
        id,
        harness,
        display_name: Some(wiz.display_name.trim().to_string()).filter(|s| !s.is_empty()),
        home: wiz.home.trim().to_string(),
        binary: None,
        no_t3: false,
    })
}

fn primary_claude() -> (PathBuf, PathBuf) {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let primary = home.join(".claude");
    let json = home.join(".claude.json");
    (primary, json)
}

fn main_codex_home(reg: &Registry) -> PathBuf {
    reg.get("codex")
        .filter(|i| i.harness == Harness::Codex)
        .map(|i| expand_home(&i.home))
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default()
                .join(".codex")
        })
}

/// Registry add (dry-run) plus harness materialize, all as redacted lines.
pub async fn compute_preview(paths: Paths, spec: AddSpec) -> Result<Vec<String>, String> {
    let reg_changes =
        agentdeck_core::registry::add_instance(&paths, spec.clone(), true).map_err(err)?;
    let mut lines: Vec<String> = reg_changes.iter().map(|c| c.describe()).collect();
    let home = expand_home(&spec.home);
    match spec.harness {
        Harness::Claude => {
            let (primary, json) = primary_claude();
            lines.extend(
                agentdeck_core::claude::plan_materialize(&primary, &home, &paths.state_dir, &json)
                    .map_err(err)?
                    .iter()
                    .map(agentdeck_core::claude::describe_change),
            );
        }
        Harness::Codex => {
            let reg = Registry::load(&paths).map_err(err)?;
            let main = main_codex_home(&reg);
            lines.extend(
                agentdeck_core::codex::plan_materialize(&main, &home, &paths.state_dir)
                    .map_err(err)?
                    .iter()
                    .map(|c| c.describe()),
            );
        }
        // No shared profile to copy: the registry row is the whole change.
        Harness::Opencode | Harness::Agy => {}
    }
    Ok(lines)
}

/// Registry add plus harness materialize, for real.
pub async fn apply_add(paths: Paths, spec: AddSpec) -> Result<Vec<String>, String> {
    let home = expand_home(&spec.home);
    let harness = spec.harness;
    let mut lines: Vec<String> = agentdeck_core::registry::add_instance(&paths, spec, false)
        .map_err(err)?
        .iter()
        .map(|c| c.describe())
        .collect();
    let materialize = match harness {
        Harness::Claude => {
            let (primary, json) = primary_claude();
            agentdeck_core::claude::plan_materialize(&primary, &home, &paths.state_dir, &json)
                .map_err(err)?
        }
        Harness::Codex => {
            let reg = Registry::load(&paths).map_err(err)?;
            let main = main_codex_home(&reg);
            agentdeck_core::codex::plan_materialize(&main, &home, &paths.state_dir).map_err(err)?
        }
        Harness::Opencode | Harness::Agy => Vec::new(),
    };
    lines.extend(materialize.iter().map(|c| c.describe()));
    agentdeck_core::apply_changes(&materialize).map_err(err)?;
    Ok(lines)
}

pub async fn preview_wrapper(paths: Paths, id: String) -> Result<Vec<String>, String> {
    let reg = Registry::load(&paths).map_err(err)?;
    let inst = reg
        .get(&id)
        .ok_or_else(|| format!("unknown instance id {id:?}"))?;
    let home = expand_home(&inst.home);
    let real_home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let changes = agentdeck_core::wrappers::plan_install(
        inst,
        &home,
        &agentdeck_core::wrappers::bin_dir(&real_home),
        &agentdeck_core::wrappers::apps_dir(&real_home),
        None,
        false,
    )
    .map_err(err)?;
    Ok(changes.iter().map(|c| c.describe()).collect())
}

pub async fn apply_wrapper(paths: Paths, id: String) -> Result<Vec<String>, String> {
    let reg = Registry::load(&paths).map_err(err)?;
    let inst = reg
        .get(&id)
        .cloned()
        .ok_or_else(|| format!("unknown instance id {id:?}"))?;
    let home = expand_home(&inst.home);
    let real_home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let changes = agentdeck_core::wrappers::plan_install(
        &inst,
        &home,
        &agentdeck_core::wrappers::bin_dir(&real_home),
        &agentdeck_core::wrappers::apps_dir(&real_home),
        None,
        false,
    )
    .map_err(err)?;
    let lines: Vec<String> = changes.iter().map(|c| c.describe()).collect();
    agentdeck_core::apply_changes(&changes).map_err(err)?;
    Ok(lines)
}

pub async fn preview_t3(paths: Paths) -> Result<Vec<String>, String> {
    let root = agentdeck_core::t3::t3_root().map_err(err)?;
    Ok(agentdeck_core::t3::sync_t3(&paths, &root, true)
        .map_err(err)?
        .iter()
        .map(|e| {
            format!(
                "{}: {} {}",
                e.id,
                e.action,
                serde_json::to_string(&agentdeck_core::t3::redact(&e.after)).unwrap_or_default()
            )
        })
        .collect())
}

pub async fn apply_t3(paths: Paths) -> Result<Vec<String>, String> {
    let root = agentdeck_core::t3::t3_root().map_err(err)?;
    Ok(agentdeck_core::t3::sync_t3(&paths, &root, false)
        .map_err(err)?
        .iter()
        .map(|e| format!("{}: {}", e.id, e.action))
        .collect())
}

fn err(e: anyhow::Error) -> String {
    format!("{e:#}")
}

fn start_login(paths: &Paths, wiz: &mut Wizard) {
    let reg = match Registry::load(paths) {
        Ok(reg) => reg,
        Err(e) => {
            wiz.login.status = LoginStatus::SpawnError(format!("{e:#}"));
            return;
        }
    };
    let Some(id) = wiz.added_id.clone() else {
        wiz.login.status = LoginStatus::SpawnError("apply the instance first".into());
        return;
    };
    let Some(inst) = reg.get(&id) else {
        wiz.login.status = LoginStatus::SpawnError(format!("unknown instance id {id:?}"));
        return;
    };
    // Pre-check the Codex callback port so the panel can offer device auth.
    if inst.harness == Harness::Codex
        && !wiz.login.device_auth
        && let Err(e) = login::ensure_codex_callback_port_free()
    {
        wiz.login.status = LoginStatus::SpawnError(format!("{e:#}"));
        return;
    }
    match login::start(&LoginTarget {
        home: expand_home(&inst.home),
        harness: inst.harness,
        binary: inst.binary.clone(),
        device_auth: wiz.login.device_auth,
    }) {
        Ok(session) => {
            wiz.login.session = Some(session);
            wiz.login.status = LoginStatus::Active;
        }
        Err(e) => {
            wiz.login.status = LoginStatus::SpawnError(format!("{e:#}"));
        }
    }
}

fn drain_login(wiz: &mut Wizard) -> Task<Message> {
    let mut exited = false;
    let events: Vec<LoginEvent> = wiz
        .login
        .session
        .as_mut()
        .map(|session| {
            let mut out = Vec::new();
            while let Ok(event) = session.events.try_recv() {
                out.push(event);
            }
            out
        })
        .unwrap_or_default();
    for event in events {
        if wiz.login.apply_event(event) {
            exited = true;
            break;
        }
    }
    if exited {
        // Reload quotas/T3 state after a successful login.
        Task::done(Message::Add(AddMsg::LoginSendDone))
    } else {
        Task::none()
    }
}

pub fn update_wizard(wiz: &mut Wizard, msg: AddMsg, paths: &Paths) -> Task<Message> {
    match msg {
        AddMsg::Harness(h) => {
            wiz.harness = h;
            if !wiz.home_dirty {
                wiz.home = suggest_home(h, &wiz.id.clone());
            }
            wiz.preview = Preview::Idle;
            Task::none()
        }
        AddMsg::Id(id) => {
            wiz.id = id.clone();
            if !wiz.home_dirty {
                wiz.home = suggest_home(wiz.harness, &id);
            }
            Task::none()
        }
        AddMsg::DisplayName(name) => {
            wiz.display_name = name;
            Task::none()
        }
        AddMsg::Home(home) => {
            wiz.home = home;
            wiz.home_dirty = true;
            Task::none()
        }
        AddMsg::UseSuggestion => {
            wiz.home = suggest_home(wiz.harness, &wiz.id);
            wiz.home_dirty = false;
            Task::none()
        }
        AddMsg::Edit => {
            wiz.preview = Preview::Idle;
            Task::none()
        }
        AddMsg::Next => {
            wiz.finish = true;
            Task::none()
        }
        AddMsg::Preview => match spec_of(wiz) {
            Ok(spec) => {
                wiz.preview = Preview::Working;
                let owned = paths.clone();
                Task::perform(compute_preview(owned, spec), |r| {
                    Message::Add(AddMsg::PreviewReady(r))
                })
            }
            Err(e) => {
                wiz.preview = Preview::Error(format!("{e:#}"));
                Task::none()
            }
        },
        AddMsg::PreviewReady(result) => {
            wiz.preview = match result {
                Ok(lines) => Preview::Ready(lines),
                Err(e) => Preview::Error(e),
            };
            Task::none()
        }
        AddMsg::Apply => match spec_of(wiz) {
            Ok(spec) => {
                wiz.preview = Preview::Working;
                let owned = paths.clone();
                Task::perform(apply_add(owned, spec.clone()), move |r| {
                    Message::Add(AddMsg::ApplyDone(r))
                })
            }
            Err(e) => {
                wiz.preview = Preview::Error(format!("{e:#}"));
                Task::none()
            }
        },
        AddMsg::ApplyDone(result) => {
            match result {
                Ok(lines) => {
                    wiz.added_id = Some(wiz.id.trim().to_string());
                    wiz.preview = Preview::Done(lines);
                }
                Err(e) => wiz.preview = Preview::Error(e),
            }
            let owned = paths.clone();
            Task::perform(async move { crate::data::load(&owned) }, Message::Loaded)
        }
        AddMsg::LoginStart => {
            start_login(paths, wiz);
            Task::none()
        }
        AddMsg::LoginPoll => drain_login(wiz),
        AddMsg::LoginSendDone => {
            let owned = paths.clone();
            Task::perform(async move { crate::data::load(&owned) }, Message::Loaded)
        }
        AddMsg::LoginCodeInput(code) => {
            wiz.login.code = code;
            Task::none()
        }
        AddMsg::LoginSend => {
            if let Some(session) = wiz.login.session.as_mut() {
                let code = wiz.login.code.clone();
                wiz.login.code.clear();
                // The pasted code is written to the child and never logged.
                if session.send_line(&code).is_err() {
                    wiz.login.status =
                        LoginStatus::Failed("could not write to the login process".into());
                }
            }
            Task::none()
        }
        AddMsg::LoginCancel => {
            // Dropping the session SIGTERMs only the child process group.
            wiz.login.session = None;
            wiz.login.status = LoginStatus::Cancelled;
            Task::none()
        }
        AddMsg::LoginOpen => {
            if let Some(url) = wiz.login.url.clone()
                && std::process::Command::new("xdg-open")
                    .arg(&url)
                    .spawn()
                    .is_err()
            {
                wiz.login.open_warning = Some(fl!(loader(), "add-open-failed"));
            }
            Task::none()
        }
        AddMsg::LoginCopy => match wiz.login.url.clone() {
            Some(url) => iced::clipboard::write(url).map(|()| Message::Noop),
            None => Task::none(),
        },
        AddMsg::DeviceAuth(on) => {
            wiz.login.device_auth = on;
            Task::none()
        }
        AddMsg::RetryDeviceAuth => {
            wiz.login.device_auth = true;
            wiz.login.status = LoginStatus::Idle;
            start_login(paths, wiz);
            Task::none()
        }
        AddMsg::WrapperPreview => match wiz.added_id.clone() {
            Some(id) => {
                wiz.wrapper = Preview::Working;
                let owned = paths.clone();
                Task::perform(preview_wrapper(owned, id), |r| {
                    Message::Add(AddMsg::WrapperReady(r))
                })
            }
            None => Task::none(),
        },
        AddMsg::WrapperReady(result) => {
            wiz.wrapper = match result {
                Ok(lines) => Preview::Ready(lines),
                Err(e) => Preview::Error(e),
            };
            Task::none()
        }
        AddMsg::WrapperApply => match wiz.added_id.clone() {
            Some(id) => {
                wiz.wrapper = Preview::Working;
                let owned = paths.clone();
                Task::perform(apply_wrapper(owned, id), |r| {
                    Message::Add(AddMsg::WrapperDone(r))
                })
            }
            None => Task::none(),
        },
        AddMsg::WrapperDone(result) => {
            wiz.wrapper = match result {
                Ok(lines) => Preview::Done(lines),
                Err(e) => Preview::Error(e),
            };
            Task::none()
        }
        AddMsg::T3Preview => {
            wiz.t3 = Preview::Working;
            let owned = paths.clone();
            Task::perform(preview_t3(owned), |r| Message::Add(AddMsg::T3Ready(r)))
        }
        AddMsg::T3Ready(result) => {
            wiz.t3 = match result {
                Ok(lines) => Preview::Ready(lines),
                Err(e) => Preview::Error(e),
            };
            Task::none()
        }
        AddMsg::T3Apply => {
            wiz.t3 = Preview::Working;
            let owned = paths.clone();
            Task::perform(apply_t3(owned), |r| Message::Add(AddMsg::T3Done(r)))
        }
        AddMsg::T3Done(result) => {
            wiz.t3 = match result {
                Ok(lines) => Preview::Done(lines),
                Err(e) => Preview::Error(e),
            };
            let owned = paths.clone();
            Task::perform(async move { crate::data::load(&owned) }, Message::Loaded)
        }
    }
}

/// 200 ms drain tick while a login child is attached; idle otherwise.
pub fn subscription(wiz: &Wizard) -> Subscription<Message> {
    if wiz.login.active() {
        iced::time::every(Duration::from_millis(200)).map(|_| Message::Add(AddMsg::LoginPoll))
    } else {
        Subscription::none()
    }
}

fn preview_lines(preview: &Preview) -> Vec<Element<'_, Message>> {
    match preview {
        Preview::Idle => vec![],
        Preview::Working => vec![st::muted(fl!(loader(), "add-working")).into()],
        Preview::Ready(lines) | Preview::Done(lines) => lines
            .iter()
            .map(|line| st::mono(line.as_str()).into())
            .collect(),
        Preview::Error(e) => vec![st::body(fl!(loader(), "load-error", what = e)).into()],
    }
}

fn optional_section(
    title: String,
    preview: &Preview,
    on_preview: AddMsg,
    on_apply: AddMsg,
) -> Element<'_, Message> {
    let ready = matches!(preview, Preview::Ready(_));
    st::panel(
        title,
        preview_lines(preview),
        vec![
            st::btn(st::Btn::Quiet, fl!(loader(), "add-preview"))
                .on_press(Message::Add(on_preview))
                .into(),
            st::btn(st::Btn::Quiet, fl!(loader(), "add-apply"))
                .on_press_maybe(ready.then_some(Message::Add(on_apply)))
                .into(),
        ],
    )
}

fn login_panel(wiz: &Wizard) -> Element<'_, Message> {
    let login = &wiz.login;
    let mut col = column![st::heading(fl!(loader(), "add-login-title"))].spacing(st::SPACE_S);
    match &login.status {
        LoginStatus::Idle | LoginStatus::Cancelled => {
            if wiz.harness == HarnessChoice::Agy {
                // No login CLI exists: show the manual step instead.
                return column![
                    st::heading(fl!(loader(), "add-login-title")),
                    st::muted(fl!(loader(), "add-login-agy"))
                ]
                .spacing(st::SPACE_S)
                .into();
            }
            let is_codex = wiz.harness == HarnessChoice::Codex;
            col = col.push(
                st::btn(st::Btn::Quiet, fl!(loader(), "add-login-start"))
                    .on_press(Message::Add(AddMsg::LoginStart)),
            );
            if is_codex {
                col = col.push(
                    iced::widget::checkbox(login.device_auth)
                        .label(fl!(loader(), "add-login-device-auth"))
                        .on_toggle(|on| Message::Add(AddMsg::DeviceAuth(on))),
                );
            }
            if login.status == LoginStatus::Cancelled {
                col = col.push(st::muted(fl!(loader(), "add-login-cancelled")));
            }
        }
        LoginStatus::SpawnError(e) => {
            col = col.push(st::body(fl!(loader(), "load-error", what = e)));
            if e.contains(PORT_BUSY_HINT) {
                col = col.push(
                    st::btn(st::Btn::Quiet, fl!(loader(), "add-login-retry-device"))
                        .on_press(Message::Add(AddMsg::RetryDeviceAuth)),
                );
            } else {
                col = col.push(
                    st::btn(st::Btn::Quiet, fl!(loader(), "add-login-start"))
                        .on_press(Message::Add(AddMsg::LoginStart)),
                );
            }
        }
        LoginStatus::Active => {
            if let Some(url) = &login.url {
                col = col.push(st::fit(url.as_str(), st::Role::Mono, st::Cut::Middle));
                col = col.push(
                    row![
                        st::btn(st::Btn::Quiet, fl!(loader(), "add-login-copy"))
                            .on_press(Message::Add(AddMsg::LoginCopy)),
                        st::btn(st::Btn::Quiet, fl!(loader(), "add-login-open"))
                            .on_press(Message::Add(AddMsg::LoginOpen)),
                    ]
                    .spacing(st::SPACE_S),
                );
                if let Some(warning) = &login.open_warning {
                    col = col.push(st::body(warning.clone()));
                }
            }
            if let Some(code) = &login.device_code {
                col = col.push(st::body(fl!(
                    loader(),
                    "add-login-device-code",
                    code = code
                )));
            }
            if login.prompt {
                col = col.push(
                    row![
                        text_input(&fl!(loader(), "add-login-code"), &login.code)
                            .on_input(|c| Message::Add(AddMsg::LoginCodeInput(c))),
                        st::btn(st::Btn::Quiet, fl!(loader(), "add-login-send"))
                            .on_press(Message::Add(AddMsg::LoginSend)),
                    ]
                    .spacing(st::SPACE_S),
                );
            }
            let mut tail = column![].spacing(st::SPACE_XS);
            for line in &login.tail {
                tail = tail.push(st::mono_muted(line.as_str()));
            }
            col = col.push(tail);
            col = col.push(
                st::btn(st::Btn::Quiet, fl!(loader(), "add-login-cancel"))
                    .on_press(Message::Add(AddMsg::LoginCancel)),
            );
        }
        LoginStatus::Done => {
            col = col.push(st::muted(fl!(loader(), "add-login-done")));
        }
        LoginStatus::Failed(reason) => {
            col = col.push(st::body(fl!(loader(), "add-login-failed", reason = reason)));
            col = col.push(
                st::btn(st::Btn::Quiet, fl!(loader(), "add-login-start"))
                    .on_press(Message::Add(AddMsg::LoginStart)),
            );
        }
    }
    col.into()
}

fn stepper(current: Step) -> Element<'static, Message> {
    let look = st::look();
    let steps = [
        (Step::Details, fl!(loader(), "add-step-details")),
        (Step::Preview, fl!(loader(), "add-step-preview")),
        (Step::Login, fl!(loader(), "add-step-login")),
        (Step::Finish, fl!(loader(), "add-step-finish")),
    ];
    let at = steps.iter().position(|(s, _)| *s == current).unwrap_or(0);
    let mut line = row![].spacing(st::SPACE_S).align_y(Alignment::Center);
    for (n, (_, label)) in steps.into_iter().enumerate() {
        if n > 0 {
            line = line.push(st::hairline());
        }
        let tone = match n.cmp(&at) {
            std::cmp::Ordering::Less => st::Tone::Ok,
            std::cmp::Ordering::Equal => st::Tone::Accent,
            std::cmp::Ordering::Greater => st::Tone::Neutral,
        };
        let label = if n == at {
            st::nowrap(st::body(label))
        } else {
            st::nowrap(st::muted(label))
        };
        line = line.push(
            row![st::pill((n + 1).to_string(), tone), label]
                .spacing(6)
                .align_y(Alignment::Center),
        );
    }
    container(line).padding([0.0, look.card_pad]).into()
}

fn field<'a>(label: String, input: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    column![st::caption(label), input.into()].spacing(4).into()
}

/// Step body in a card with its buttons right-aligned below.
fn step_card<'a>(
    body: Vec<Element<'a, Message>>,
    actions: Vec<Element<'a, Message>>,
) -> Element<'a, Message> {
    let look = st::look();
    let footer = row![Space::new().width(Length::Fill)]
        .extend(actions)
        .spacing(st::SPACE_S)
        .align_y(Alignment::Center);
    st::card(
        column(body)
            .push(st::hairline())
            .push(footer)
            .spacing(look.gap.max(12.0)),
    )
}

fn details(wiz: &Wizard) -> Element<'_, Message> {
    let size = st::look().body;
    let harness = |label: &'static str, h: HarnessChoice| {
        radio(label, h, Some(wiz.harness), |h| {
            Message::Add(AddMsg::Harness(h))
        })
        .text_size(size)
    };
    let mut body: Vec<Element<'_, Message>> = vec![
        field(
            fl!(loader(), "add-harness"),
            row![
                harness("Claude", HarnessChoice::Claude),
                harness("Codex", HarnessChoice::Codex),
                harness("OpenCode", HarnessChoice::Opencode),
                harness("agy", HarnessChoice::Agy),
            ]
            .spacing(st::SPACE_L)
            .wrap(),
        ),
        column![
            st::caption(fl!(loader(), "add-id")),
            text_input("claude-second", &wiz.id)
                .on_input(|s| Message::Add(AddMsg::Id(s)))
                .size(size),
            st::caption(fl!(loader(), "add-id-note")),
        ]
        .spacing(4)
        .into(),
        field(
            fl!(loader(), "add-display-name"),
            text_input("", &wiz.display_name)
                .on_input(|s| Message::Add(AddMsg::DisplayName(s)))
                .size(size),
        ),
        field(
            fl!(loader(), "add-home"),
            row![
                text_input(&suggest_home(wiz.harness, &wiz.id), &wiz.home)
                    .on_input(|s| Message::Add(AddMsg::Home(s)))
                    .size(size)
                    .width(Length::Fill),
                st::btn(st::Btn::Quiet, fl!(loader(), "add-home-use-suggestion"))
                    .on_press(Message::Add(AddMsg::UseSuggestion)),
            ]
            .spacing(st::SPACE_S)
            .align_y(Alignment::Center),
        ),
    ];
    body.extend(preview_lines(&wiz.preview));
    step_card(
        body,
        vec![
            st::btn(st::Btn::Primary, fl!(loader(), "add-preview"))
                .on_press(Message::Add(AddMsg::Preview))
                .into(),
        ],
    )
}

pub fn view(wiz: &Wizard) -> Element<'_, Message> {
    let look = st::look();
    let current = step(wiz);
    let content = match current {
        Step::Details => details(wiz),
        Step::Preview => {
            let mut body = vec![st::heading(fl!(loader(), "add-step-preview")).into()];
            body.extend(preview_lines(&wiz.preview));
            step_card(
                body,
                vec![
                    st::btn(st::Btn::Quiet, fl!(loader(), "add-edit"))
                        .on_press(Message::Add(AddMsg::Edit))
                        .into(),
                    st::btn(st::Btn::Primary, fl!(loader(), "add-apply"))
                        .on_press_maybe(
                            matches!(wiz.preview, Preview::Ready(_))
                                .then(|| Message::Add(AddMsg::Apply)),
                        )
                        .into(),
                ],
            )
        }
        Step::Login => step_card(
            vec![
                row![
                    st::icon_colored(st::Icon::CircleCheck, look.body + 2.0, |c| c.ok),
                    st::fit(
                        fl!(
                            loader(),
                            "add-added",
                            id = wiz.added_id.clone().unwrap_or_default()
                        ),
                        st::Role::Body,
                        st::Cut::Middle,
                    ),
                ]
                .spacing(st::SPACE_S)
                .align_y(Alignment::Center)
                .into(),
                login_panel(wiz),
            ],
            vec![
                st::btn(st::Btn::Primary, fl!(loader(), "add-next"))
                    .on_press_maybe((!wiz.login.active()).then_some(Message::Add(AddMsg::Next)))
                    .into(),
            ],
        ),
        Step::Finish => column![
            optional_section(
                fl!(loader(), "add-wrapper-title"),
                &wiz.wrapper,
                AddMsg::WrapperPreview,
                AddMsg::WrapperApply,
            ),
            optional_section(
                fl!(loader(), "add-t3-title"),
                &wiz.t3,
                AddMsg::T3Preview,
                AddMsg::T3Apply,
            ),
            row![
                Space::new().width(Length::Fill),
                st::btn(st::Btn::Primary, fl!(loader(), "add-done"))
                    .on_press(Message::SelectPage(crate::app::Page::Instances)),
            ],
        ]
        .spacing(look.gap.max(st::SPACE_S))
        .into(),
    };
    column![stepper(current), content]
        .spacing(look.gap.max(12.0))
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wizard(id: &str, harness: HarnessChoice) -> Wizard {
        Wizard {
            harness,
            id: id.to_string(),
            display_name: String::new(),
            home: suggest_home(harness, id),
            ..Wizard::default()
        }
    }

    #[test]
    fn suggestions_and_home_expansion() {
        assert_eq!(
            suggest_home(HarnessChoice::Claude, "second"),
            "~/.claude-second"
        );
        assert_eq!(
            suggest_home(HarnessChoice::Codex, "second"),
            "~/.codex-t3/second"
        );
        // Absolute paths never consult HOME.
        assert_eq!(expand_home("/abs"), PathBuf::from("/abs"));
    }

    #[test]
    fn login_events_fold_into_panel() {
        let mut login = LoginState::default();
        assert!(!login.apply_event(LoginEvent::Url("https://example.com/a".into())));
        assert!(!login.apply_event(LoginEvent::CodePrompt));
        assert!(!login.apply_event(LoginEvent::Output("hello\n".into())));
        assert_eq!(login.url.as_deref(), Some("https://example.com/a"));
        assert!(login.prompt);
        assert!(login.tail.iter().any(|l| l == "hello"));
        assert!(login.apply_event(LoginEvent::Exited(0)));
        assert_eq!(login.status, LoginStatus::Done);
        let mut login = LoginState::default();
        assert!(login.apply_event(LoginEvent::Exited(3)));
        assert_eq!(login.status, LoginStatus::Failed("exit 3".into()));
    }

    #[test]
    fn fake_login_script_through_real_pty() {
        let dir = std::env::temp_dir().join(format!("agentdeck-gui-login-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("fake-login.sh");
        std::fs::write(
            &script,
            "#!/bin/bash\n\
             printf 'visit https://example.com/login?x=2 now\\r\\n'\n\
             printf 'Paste code here if prompted > '\n\
             IFS= read -r line\n\
             printf 'done\\r\\n'\n\
             exit 0\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let mut session = login::start(&LoginTarget {
            home: dir.clone(),
            harness: Harness::Claude,
            binary: Some(script.to_string_lossy().into_owned()),
            device_auth: false,
        })
        .expect("spawn fake login");
        let mut panel = LoginState::default(); // no session here; events fold the same way
        let mut saw_url = false;
        let mut saw_prompt = false;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let exit = loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match session.events.recv_timeout(left) {
                Ok(LoginEvent::Url(url)) => {
                    assert_eq!(url, "https://example.com/login?x=2");
                    saw_url = true;
                    panel.apply_event(LoginEvent::Url(url));
                }
                Ok(LoginEvent::CodePrompt) => {
                    saw_prompt = true;
                    panel.apply_event(LoginEvent::CodePrompt);
                    session.send_line("test-code-9").unwrap();
                }
                Ok(LoginEvent::Exited(code)) => break code,
                Ok(other) => {
                    panel.apply_event(other);
                }
                Err(_) => panic!("timed out waiting for fake login exit"),
            }
        };
        assert!(saw_url && saw_prompt);
        assert_eq!(exit, 0);
        assert_eq!(panel.url.as_deref(), Some("https://example.com/login?x=2"));
        assert!(panel.prompt);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn preview_then_apply_roundtrip_on_temp_dirs() {
        // HOME is process-global: hold the lock and restore it afterwards.
        static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _lock = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old_home = std::env::var_os("HOME");
        let dir = std::env::temp_dir().join(format!(
            "agentdeck-gui-add-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let home = dir.join("home");
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::write(home.join(".claude/settings.json"), "{}\n").unwrap();
        std::fs::write(home.join(".claude.json"), "{\"mcpServers\": {}}\n").unwrap();
        unsafe {
            std::env::set_var("HOME", &home);
        }
        let paths = Paths::new(dir.join("config"), dir.join("state"));
        let wiz = wizard("second", HarnessChoice::Claude);
        let spec = spec_of(&wiz).unwrap();
        let lines = iced::futures::executor::block_on(compute_preview(paths.clone(), spec.clone()))
            .unwrap();
        assert!(
            !lines.is_empty(),
            "preview shows registry + materialize lines"
        );
        let before = Registry::load(&paths).unwrap();
        assert!(before.get("second").is_none());
        let applied = iced::futures::executor::block_on(apply_add(paths.clone(), spec)).unwrap();
        assert!(!applied.is_empty());
        let after = Registry::load(&paths).unwrap();
        assert!(after.get("second").is_some());
        // A second preview for the same id fails (duplicate id, by core).
        assert!(
            iced::futures::executor::block_on(compute_preview(paths.clone(), wiz2_spec())).is_err()
        );
        std::fs::remove_dir_all(&dir).ok();
        unsafe {
            match old_home {
                Some(h) => std::env::set_var("HOME", h),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    fn wiz2_spec() -> AddSpec {
        AddSpec {
            id: "second".into(),
            harness: Harness::Claude,
            display_name: None,
            home: "~/.claude-second".into(),
            binary: None,
            no_t3: false,
        }
    }
}
