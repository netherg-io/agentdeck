//! Module settings forms: the router's decision model with its OpenRouter key,
//! and the quota polling interval / staleness threshold.

use std::path::PathBuf;

use agentdeck_core::settings::{self, Settings};
use agentdeck_core::{Paths, Registry};
use i18n_embed_fl::fl;
use iced::widget::{column, row, text_input};
use iced::{Alignment, Element, Length, Task};

use crate::i18n::loader;
use crate::style as st;

#[derive(Debug, Clone, Default)]
pub struct Loaded {
    pub settings: Option<Settings>,
    pub key_set: bool,
    pub error: Option<String>,
}

#[derive(Default)]
pub struct State {
    pub loaded: Loaded,
    pub decision_model: String,
    pub key: String,
    pub refresh: String,
    pub stale: String,
    pub busy: bool,
    /// Last outcome (text, is error); the shell turns it into a toast.
    pub result: Option<(String, bool)>,
}

#[derive(Debug, Clone)]
pub enum Msg {
    Loaded(Loaded),
    DecisionModel(String),
    Key(String),
    Refresh(String),
    Stale(String),
    SaveRouter,
    SavePoll,
    Done(Result<(), String>),
}

/// The router reads `JEV_OPENROUTER_API_KEY` first, then this file.
fn key_file() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(".config/model-route/openrouter.key")
}

pub fn load(paths: &Paths) -> Msg {
    let key_set = std::env::var("JEV_OPENROUTER_API_KEY").is_ok_and(|k| !k.is_empty())
        || std::fs::read_to_string(key_file()).is_ok_and(|k| !k.trim().is_empty());
    Msg::Loaded(match Registry::load(paths) {
        Ok(reg) => Loaded {
            settings: Some(settings::read(&reg)),
            key_set,
            error: None,
        },
        Err(e) => Loaded {
            settings: None,
            key_set,
            error: Some(format!("{e:#}")),
        },
    })
}

/// Blank means "default"; anything else must be a positive whole number.
fn seconds(raw: &str) -> Result<Option<u64>, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    raw.parse::<u64>()
        .ok()
        .filter(|n| *n > 0)
        .map(Some)
        .ok_or_else(|| fl!(loader(), "settings-seconds-invalid"))
}

fn save(paths: Paths, f: impl FnOnce(&mut Settings) + Send + 'static) -> Task<Msg> {
    Task::perform(
        async move {
            tokio::task::spawn_blocking(move || {
                let reg = Registry::load(&paths).map_err(|e| format!("{e:#}"))?;
                let mut s = settings::read(&reg);
                f(&mut s);
                settings::write(&paths, &s).map_err(|e| format!("{e:#}"))
            })
            .await
            .map_err(|e| e.to_string())
            .and_then(|r| r)
        },
        Msg::Done,
    )
}

pub fn update(state: &mut State, paths: Paths, msg: Msg) -> Task<Msg> {
    match msg {
        Msg::Loaded(loaded) => {
            if let Some(s) = &loaded.settings {
                state.decision_model = s.decision_model.clone().unwrap_or_default();
                state.refresh = s.refresh_seconds.map(|n| n.to_string()).unwrap_or_default();
                state.stale = s.stale_seconds.map(|n| n.to_string()).unwrap_or_default();
            }
            state.loaded = loaded;
            Task::none()
        }
        Msg::DecisionModel(v) => {
            state.decision_model = v;
            Task::none()
        }
        Msg::Key(v) => {
            state.key = v;
            Task::none()
        }
        Msg::Refresh(v) => {
            state.refresh = v;
            Task::none()
        }
        Msg::Stale(v) => {
            state.stale = v;
            Task::none()
        }
        Msg::SaveRouter => {
            state.busy = true;
            let model = Some(state.decision_model.trim().to_string()).filter(|m| !m.is_empty());
            let key = std::mem::take(&mut state.key);
            let key = key.trim().to_string();
            if !key.is_empty()
                && let Err(e) = agentdeck_usage::util::atomic_write_mode(
                    &key_file(),
                    key.as_bytes(),
                    Some(0o600),
                )
            {
                state.busy = false;
                state.result = Some((format!("{}: {e:#}", key_file().display()), true));
                return Task::none();
            }
            save(paths, move |s| s.decision_model = model)
        }
        Msg::SavePoll => {
            let (refresh, stale) = match (seconds(&state.refresh), seconds(&state.stale)) {
                (Ok(r), Ok(s)) => (r, s),
                (Err(e), _) | (_, Err(e)) => {
                    state.result = Some((e, true));
                    return Task::none();
                }
            };
            state.busy = true;
            save(paths, move |s| {
                s.refresh_seconds = refresh;
                s.stale_seconds = stale;
            })
        }
        Msg::Done(result) => {
            state.busy = false;
            state.result = Some(match result {
                Ok(()) => (fl!(loader(), "settings-saved"), false),
                Err(e) => (e, true),
            });
            Task::done(load(&paths))
        }
    }
}

fn field<'a>(label: String, input: Element<'a, Msg>, hint: Option<String>) -> Element<'a, Msg> {
    let mut col = column![st::caption(label), input].spacing(4);
    if let Some(hint) = hint {
        col = col.push(st::muted(hint));
    }
    col.into()
}

fn error_card(state: &State) -> Option<Element<'_, Msg>> {
    state
        .loaded
        .error
        .as_ref()
        .map(|e| st::card(st::muted(fl!(loader(), "load-error", what = e))))
}

fn save_row(state: &State, msg: Msg) -> Element<'_, Msg> {
    row![
        iced::widget::Space::new().width(Length::Fill),
        st::btn(st::Btn::Primary, fl!(loader(), "settings-save"))
            .on_press_maybe((!state.busy).then_some(msg)),
    ]
    .align_y(Alignment::Center)
    .into()
}

pub fn router_view(state: &State) -> Element<'_, Msg> {
    let size = st::look().body;
    let key_hint = if state.loaded.key_set {
        fl!(loader(), "settings-key-set")
    } else {
        fl!(loader(), "settings-key-missing")
    };
    let form = column![
        field(
            fl!(loader(), "settings-decision-model"),
            text_input(settings::DEFAULT_DECISION_MODEL, &state.decision_model)
                .on_input(Msg::DecisionModel)
                .on_submit(Msg::SaveRouter)
                .size(size)
                .into(),
            Some(fl!(loader(), "settings-decision-model-hint")),
        ),
        field(
            fl!(loader(), "settings-openrouter-key"),
            text_input(&fl!(loader(), "settings-key-placeholder"), &state.key)
                .secure(true)
                .on_input(Msg::Key)
                .on_submit(Msg::SaveRouter)
                .size(size)
                .into(),
            Some(key_hint),
        ),
        save_row(state, Msg::SaveRouter),
    ]
    .spacing(st::SPACE_M);
    column![]
        .push(error_card(state))
        .push(st::card(form))
        .spacing(st::SPACE_S)
        .into()
}

pub fn poll_view(state: &State) -> Element<'_, Msg> {
    let size = st::look().body;
    let form = column![
        field(
            fl!(loader(), "settings-refresh"),
            text_input(
                &settings::DEFAULT_REFRESH_SECONDS.to_string(),
                &state.refresh
            )
            .on_input(Msg::Refresh)
            .on_submit(Msg::SavePoll)
            .size(size)
            .into(),
            Some(fl!(
                loader(),
                "settings-refresh-hint",
                min = settings::MIN_REFRESH_SECONDS
            )),
        ),
        field(
            fl!(loader(), "settings-stale"),
            text_input(&settings::DEFAULT_STALE_SECONDS.to_string(), &state.stale)
                .on_input(Msg::Stale)
                .on_submit(Msg::SavePoll)
                .size(size)
                .into(),
            Some(fl!(loader(), "settings-stale-hint")),
        ),
        save_row(state, Msg::SavePoll),
    ]
    .spacing(st::SPACE_M);
    column![]
        .push(error_card(state))
        .push(st::card(form))
        .spacing(st::SPACE_S)
        .into()
}

#[cfg(test)]
mod tests {
    use super::seconds;

    #[test]
    fn blank_is_default_and_only_positive_numbers_pass() {
        assert_eq!(seconds("  "), Ok(None));
        assert_eq!(seconds(" 30 "), Ok(Some(30)));
        for bad in ["0", "-5", "1.5", "abc"] {
            assert!(seconds(bad).is_err(), "{bad}");
        }
    }
}
