//! Quotas module (ai-usage): `[[usage]]` sources with add/remove behind a
//! change preview, and which providers the COSMIC panel widget shows.

use std::collections::BTreeSet;
use std::path::PathBuf;

use agentdeck_core::usage::{self, UsageEntry, UsageKind};
use agentdeck_core::{Paths, Registry};
use i18n_embed_fl::fl;
use iced::widget::{column, container, pick_list, row, text_input, toggler};
use iced::{Alignment, Element, Length, Task};

use crate::data::Data;
use crate::i18n::loader;
use crate::style as st;

const KINDS: [UsageKind; 7] = [
    UsageKind::Claude,
    UsageKind::Codex,
    UsageKind::Antigravity,
    UsageKind::Deepseek,
    UsageKind::Openrouter,
    UsageKind::Vercel,
    UsageKind::OpencodeGo,
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kind(pub UsageKind);

impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0.as_str())
    }
}

#[derive(Debug, Clone, Default)]
pub struct Loaded {
    pub sources: Vec<UsageEntry>,
    pub instances: Vec<String>,
    pub hidden: BTreeSet<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Form {
    pub kind: Option<UsageKind>,
    pub id: String,
    pub display: String,
    /// Path or env-var name, depending on the kind (see [`source_field`]).
    pub source: String,
    pub instance: Option<String>,
    /// Key-based kinds: the key itself, written to `usage.env`, never to the registry.
    pub key: String,
}

#[derive(Debug, Clone)]
pub enum Op {
    Add(UsageEntry),
    Remove(String),
}

#[derive(Debug, Clone)]
pub struct Pending {
    /// `None` for a refusal: the panel then has no Apply.
    pub op: Option<Op>,
    pub lines: Vec<String>,
}

#[derive(Default)]
pub struct State {
    pub data: Loaded,
    pub form: Option<Form>,
    pub pending: Option<Pending>,
    pub busy: bool,
    /// Last outcome (text, is error); the shell turns it into a toast.
    pub result: Option<(String, bool)>,
}

#[derive(Debug, Clone)]
pub enum Msg {
    Loaded(Loaded),
    OpenAdd,
    CloseAdd,
    Kind(Kind),
    Id(String),
    Display(String),
    Source(String),
    Instance(String),
    Key(String),
    AskAdd,
    AskRemove(String),
    Confirm,
    Cancel,
    Done(Result<(), String>),
    /// Provider, shown.
    Show(String, bool),
}

pub fn load(paths: &Paths) -> Msg {
    let mut out = Loaded::default();
    match Registry::load(paths) {
        Ok(reg) => {
            out.sources = reg.usage.clone();
            out.instances = reg.instances.iter().map(|i| i.id.clone()).collect();
        }
        Err(e) => out.error = Some(format!("{e:#}")),
    }
    match agentdeck_usage::visibility::load(&visibility_path(paths)) {
        Ok(hidden) => out.hidden = hidden,
        Err(e) => out.error = Some(e),
    }
    Msg::Loaded(out)
}

/// Beside `instances.toml`, where the applet reads it.
fn visibility_path(paths: &Paths) -> PathBuf {
    paths.config_dir.join("ai-usage-visibility.json")
}

/// Label key and whether the field is required, per kind.
fn source_field(kind: UsageKind) -> (&'static str, bool) {
    match kind {
        UsageKind::Claude => ("usage-config-dir", true),
        UsageKind::Codex => ("usage-codex-home", true),
        UsageKind::Deepseek | UsageKind::Openrouter | UsageKind::Vercel => {
            ("usage-api-key-env", false)
        }
        UsageKind::Antigravity => ("usage-token-file", false),
        UsageKind::OpencodeGo => ("usage-db-file", false),
    }
}

fn is_key_kind(kind: UsageKind) -> bool {
    matches!(
        kind,
        UsageKind::Deepseek | UsageKind::Openrouter | UsageKind::Vercel
    )
}

/// Env var holding a key-based source's key when the user names none.
fn default_key_env(kind: UsageKind, id: &str) -> String {
    format!("AGENTDECK_{}_{}", kind.as_str(), id.trim())
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// Sets `var=key` in `usage.env` (0600), replacing an earlier line for `var`.
fn write_secret(paths: &Paths, var: &str, key: &str) -> anyhow::Result<()> {
    let path = agentdeck_usage::daemon::agentdeck_secrets_file(&paths.config_dir);
    let old = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => anyhow::bail!("{}: {e}", path.display()),
    };
    let mut out: String = old
        .lines()
        .filter(|l| l.split_once('=').is_none_or(|(k, _)| k.trim() != var))
        .map(|l| format!("{l}\n"))
        .collect();
    out.push_str(&format!("{var}={key}\n"));
    agentdeck_usage::util::atomic_write_mode(&path, out.as_bytes(), Some(0o600))
        .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
}

fn entry_of(form: &Form) -> anyhow::Result<UsageEntry> {
    let kind = form
        .kind
        .ok_or_else(|| anyhow::anyhow!(fl!(loader(), "usage-kind-missing")))?;
    let source = Some(form.source.trim().to_string()).filter(|s| !s.is_empty());
    let mut entry = UsageEntry {
        id: form.id.trim().to_string(),
        kind,
        instance: form.instance.clone(),
        display: form.display.trim().to_string(),
        config_dir: None,
        codex_home: None,
        token_file: None,
        api_key_env: None,
        db_file: None,
        extra: Default::default(),
    };
    *match kind {
        UsageKind::Claude => &mut entry.config_dir,
        UsageKind::Codex => &mut entry.codex_home,
        UsageKind::Deepseek | UsageKind::Openrouter | UsageKind::Vercel => &mut entry.api_key_env,
        UsageKind::Antigravity => &mut entry.token_file,
        UsageKind::OpencodeGo => &mut entry.db_file,
    } = source;
    if is_key_kind(kind) && entry.api_key_env.is_none() {
        entry.api_key_env = Some(default_key_env(kind, &form.id));
    }
    Ok(entry)
}

fn changes(paths: &Paths, op: &Op, dry_run: bool) -> anyhow::Result<Vec<String>> {
    let changes = match op {
        Op::Add(e) => usage::add_usage_entry(paths, e.clone(), dry_run)?,
        Op::Remove(id) => usage::remove_usage_entry(paths, id, dry_run)?,
    };
    Ok(changes.iter().map(|c| c.describe()).collect())
}

fn ask(state: &mut State, paths: &Paths, op: anyhow::Result<Op>) {
    state.result = None;
    state.pending = Some(
        match op.and_then(|op| Ok((changes(paths, &op, true)?, op))) {
            Ok((lines, op)) => Pending {
                op: Some(op),
                lines,
            },
            Err(e) => Pending {
                op: None,
                lines: vec![format!("{e:#}")],
            },
        },
    );
}

pub fn update(state: &mut State, paths: Paths, msg: Msg) -> Task<Msg> {
    let form = |state: &mut State, f: &dyn Fn(&mut Form)| {
        if let Some(form) = state.form.as_mut() {
            f(form);
        }
        Task::none()
    };
    match msg {
        Msg::Loaded(data) => {
            state.data = data;
            Task::none()
        }
        Msg::OpenAdd => {
            state.form = Some(Form::default());
            Task::none()
        }
        Msg::CloseAdd => {
            state.form = None;
            Task::none()
        }
        Msg::Kind(Kind(kind)) => form(state, &|f| f.kind = Some(kind)),
        Msg::Id(v) => form(state, &|f| f.id.clone_from(&v)),
        Msg::Display(v) => form(state, &|f| f.display.clone_from(&v)),
        Msg::Source(v) => form(state, &|f| f.source.clone_from(&v)),
        Msg::Instance(v) => form(state, &|f| {
            // Picking the current instance again clears the binding.
            f.instance = (f.instance.as_deref() != Some(v.as_str())).then(|| v.clone());
        }),
        Msg::Key(v) => form(state, &|f| f.key.clone_from(&v)),
        Msg::AskAdd => {
            if let Some(form) = state.form.clone() {
                ask(state, &paths, entry_of(&form).map(Op::Add));
                if let Some((var, _)) = secret(state)
                    && let Some(p) = state.pending.as_mut()
                {
                    let file = agentdeck_usage::daemon::agentdeck_secrets_file(&paths.config_dir);
                    p.lines.push(fl!(
                        loader(),
                        "usage-key-write",
                        var = var,
                        path = file.display().to_string()
                    ));
                }
            }
            Task::none()
        }
        Msg::AskRemove(id) => {
            ask(state, &paths, Ok(Op::Remove(id)));
            Task::none()
        }
        Msg::Confirm => match state.pending.as_ref().and_then(|p| p.op.clone()) {
            Some(op) => {
                let secret = secret(state);
                state.pending = None;
                state.busy = true;
                if matches!(op, Op::Add(_)) {
                    state.form = None;
                }
                Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || {
                            if let Some((var, key)) = &secret {
                                write_secret(&paths, var, key).map_err(|e| format!("{e:#}"))?;
                            }
                            changes(&paths, &op, false)
                                .map(|_| ())
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
            if let Err(e) = result {
                state.result = Some((e, true));
            }
            Task::done(load(&paths))
        }
        Msg::Show(provider, shown) => {
            let mut hidden = state.data.hidden.clone();
            if shown {
                hidden.remove(&provider);
            } else {
                hidden.insert(provider);
            }
            match agentdeck_usage::visibility::save(&visibility_path(&paths), &hidden) {
                Ok(()) => state.data.hidden = hidden,
                Err(e) => state.result = Some((e, true)),
            }
            Task::none()
        }
    }
}

/// The pending add's key env var and the typed key, when one was entered.
fn secret(state: &State) -> Option<(String, String)> {
    let key = state.form.as_ref()?.key.trim().to_string();
    match state.pending.as_ref()?.op.as_ref()? {
        Op::Add(e) if !key.is_empty() => e.api_key_env.clone().map(|var| (var, key)),
        _ => None,
    }
}

fn add_form(form: &Form, instances: &[String]) -> crate::overlay::Modal<'static, Msg> {
    let size = st::look().body;
    let field = |label: String, input: Element<'static, Msg>| -> Element<'static, Msg> {
        column![st::caption(label), input].spacing(4).into()
    };
    let input = |placeholder: &str, value: &str, on: fn(String) -> Msg| -> Element<'static, Msg> {
        text_input(placeholder, value)
            .on_input(on)
            .size(size)
            .into()
    };
    let mut body = vec![field(
        fl!(loader(), "usage-kind"),
        pick_list(KINDS.map(Kind).to_vec(), form.kind.map(Kind), Msg::Kind)
            .placeholder(fl!(loader(), "usage-kind"))
            .text_size(size)
            .width(Length::Fill)
            .into(),
    )];
    if let Some(kind) = form.kind {
        let (key, required) = source_field(kind);
        let label = if required {
            loader().get(key)
        } else {
            fl!(loader(), "usage-optional", what = loader().get(key))
        };
        body.push(field(
            fl!(loader(), "usage-id"),
            input(&format!("{}-main", kind.as_str()), &form.id, Msg::Id),
        ));
        body.push(field(
            fl!(loader(), "usage-display"),
            input("", &form.display, Msg::Display),
        ));
        if is_key_kind(kind) {
            body.push(field(
                fl!(loader(), "usage-api-key"),
                text_input(&fl!(loader(), "usage-api-key-placeholder"), &form.key)
                    .secure(true)
                    .on_input(Msg::Key)
                    .size(size)
                    .into(),
            ));
        }
        let placeholder = if is_key_kind(kind) {
            default_key_env(
                kind,
                if form.id.trim().is_empty() {
                    "main"
                } else {
                    &form.id
                },
            )
        } else {
            String::new()
        };
        body.push(field(label, input(&placeholder, &form.source, Msg::Source)));
        if matches!(kind, UsageKind::Claude | UsageKind::Codex) && !instances.is_empty() {
            body.push(field(
                fl!(loader(), "usage-instance"),
                pick_list(instances.to_vec(), form.instance.clone(), Msg::Instance)
                    .placeholder(fl!(loader(), "usage-instance-none"))
                    .text_size(size)
                    .width(Length::Fill)
                    .into(),
            ));
        }
    }
    let ready = form.kind.is_some() && !form.id.trim().is_empty();
    crate::overlay::Modal {
        title: fl!(loader(), "usage-add-title"),
        body,
        actions: vec![
            st::btn(st::Btn::Quiet, fl!(loader(), "action-cancel"))
                .on_press(Msg::CloseAdd)
                .into(),
            st::btn(st::Btn::Primary, fl!(loader(), "usage-preview-add"))
                .on_press_maybe(ready.then_some(Msg::AskAdd))
                .into(),
        ],
        on_cancel: Msg::CloseAdd,
        on_submit: ready.then_some(Msg::AskAdd),
    }
}

pub fn modal(state: &State) -> Option<crate::overlay::Modal<'_, Msg>> {
    if let Some(p) = &state.pending {
        return Some(crate::overlay::confirm(
            fl!(loader(), "usage-preview-title"),
            &p.lines,
            (
                fl!(loader(), "action-apply"),
                p.op.as_ref().map(|_| Msg::Confirm),
            ),
            (fl!(loader(), "action-cancel"), Msg::Cancel),
        ));
    }
    state
        .form
        .as_ref()
        .map(|f| add_form(f, &state.data.instances))
}

pub fn header_action(state: &State) -> Element<'_, Msg> {
    st::btn_icon(st::Btn::Primary, st::Icon::Plus, fl!(loader(), "usage-add"))
        .on_press_maybe((!state.busy).then_some(Msg::OpenAdd))
        .into()
}

/// Poll status of one source from the shared snapshot, if the daemon reported it.
fn status_of<'a>(data: &'a Data, id: &str) -> Option<&'a crate::data::UsageQuotas> {
    data.instances
        .iter()
        .flat_map(|i| i.quotas.iter())
        .chain(data.standalone.iter())
        .find(|q| q.usage_id == id)
}

const KIND_W: f32 = 110.0;
const STATUS_W: f32 = 80.0;

pub fn sources_view<'a>(state: &'a State, data: &'a Data) -> Element<'a, Msg> {
    let look = st::look();
    let mut col = column![].spacing(look.gap.max(st::SPACE_S));
    if let Some(e) = &state.data.error {
        col = col.push(st::card(st::muted(fl!(loader(), "load-error", what = e))));
    }
    if state.data.sources.is_empty() {
        return col
            .push(st::card(st::muted(fl!(loader(), "usage-empty"))))
            .into();
    }
    let mut list = column![].spacing(look.row_gap);
    for (i, u) in state.data.sources.iter().enumerate() {
        if i > 0 {
            list = list.push(st::hairline());
        }
        let mut identity = column![st::fit(u.display.as_str(), st::Role::Heading, st::Cut::End)]
            .spacing(2)
            .width(Length::FillPortion(3));
        if u.display != u.id {
            identity = identity.push(st::fit(u.id.as_str(), st::Role::MonoMuted, st::Cut::Middle));
        }
        // Fixed columns so kind, instance and status line up across rows.
        let status = status_of(data, &u.id).map(|q| {
            let tone = match q.status.as_str() {
                "ok" => st::Tone::Ok,
                "stale" | "waiting" => st::Tone::Neutral,
                _ => st::Tone::Warn,
            };
            st::pill(q.status.clone(), tone)
        });
        let line = row![
            identity,
            container(st::pill(u.kind.as_str(), st::Tone::Accent)).width(KIND_W),
            container(u.instance.as_deref().map(|inst| st::fit(
                inst,
                st::Role::Muted,
                st::Cut::Middle
            )))
            .width(Length::FillPortion(2)),
            container(status).width(STATUS_W),
            st::icon_btn(st::Icon::Trash)
                .on_press_maybe((!state.busy).then(|| Msg::AskRemove(u.id.clone()))),
        ]
        .spacing(st::SPACE_M)
        .align_y(Alignment::Center);
        list = list.push(line);
    }
    col.push(st::card(list)).into()
}

pub fn widget_view(state: &State) -> Element<'_, Msg> {
    let look = st::look();
    let providers: BTreeSet<&str> = state
        .data
        .sources
        .iter()
        .map(|u| u.kind.as_str())
        .chain(state.data.hidden.iter().map(String::as_str))
        .collect();
    let mut col =
        column![st::muted(fl!(loader(), "usage-widget-scope"))].spacing(look.gap.max(st::SPACE_S));
    if let Some(e) = &state.data.error {
        col = col.push(st::card(st::muted(fl!(loader(), "load-error", what = e))));
    }
    if providers.is_empty() {
        return col
            .push(st::card(st::muted(fl!(loader(), "usage-empty"))))
            .into();
    }
    let mut list = column![].spacing(look.row_gap);
    for (i, p) in providers.into_iter().enumerate() {
        if i > 0 {
            list = list.push(st::hairline());
        }
        let count = state
            .data
            .sources
            .iter()
            .filter(|u| u.kind.as_str() == p)
            .count();
        let provider = p.to_string();
        list = list.push(
            row![
                container(st::fit(p, st::Role::Heading, st::Cut::End)).width(Length::Fill),
                st::nowrap(st::caption(fl!(
                    loader(),
                    "usage-sources-count",
                    count = count
                ))),
                toggler(!state.data.hidden.contains(p))
                    .size(16)
                    .on_toggle(move |on| Msg::Show(provider.clone(), on)),
            ]
            .spacing(st::SPACE_M)
            .align_y(Alignment::Center),
        );
    }
    col.push(st::card(list)).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_puts_the_source_in_the_kind_specific_field() {
        let mut form = Form {
            kind: Some(UsageKind::Deepseek),
            id: " ds ".into(),
            source: " DEEPSEEK_KEY ".into(),
            ..Form::default()
        };
        let e = entry_of(&form).unwrap();
        assert_eq!(
            (e.id.as_str(), e.api_key_env.as_deref()),
            ("ds", Some("DEEPSEEK_KEY"))
        );
        assert!(e.config_dir.is_none() && e.codex_home.is_none());
        form.kind = Some(UsageKind::OpencodeGo);
        form.source = "  ".into();
        assert!(entry_of(&form).unwrap().db_file.is_none());
        form.kind = None;
        assert!(entry_of(&form).is_err());
    }
}

#[cfg(test)]
mod key_tests {
    use super::*;

    #[test]
    fn default_env_name_and_secret_replacement() {
        assert_eq!(
            default_key_env(UsageKind::Openrouter, "or-main"),
            "AGENTDECK_OPENROUTER_OR_MAIN"
        );
        let dir = std::env::temp_dir().join(format!("agentdeck-secret-{}", std::process::id()));
        let paths = Paths::new(dir.clone(), dir.join("state"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("usage.env"), "# keys\nA=1\nB=2\n").unwrap();
        write_secret(&paths, "A", "new").unwrap();
        let text = std::fs::read_to_string(dir.join("usage.env")).unwrap();
        assert_eq!(text, "# keys\nB=2\nA=new\n");
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.join("usage.env"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        std::fs::remove_dir_all(&dir).ok();
    }
}
