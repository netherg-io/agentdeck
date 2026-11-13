//! Image generation module settings: gateway, default model, transparent
//! model and the gateway key (`[imagegen]` in `instances.toml`).

use std::path::PathBuf;

use agentdeck_core::imagegen::{self as ig, Stored};
use agentdeck_core::settings::{PROVIDER_OPENROUTER, PROVIDER_VERCEL};
use agentdeck_core::{Paths, Registry};
use i18n_embed_fl::fl;
use iced::widget::{column, container, pick_list, row, text_input};
use iced::{Alignment, Element, Length, Task};

use crate::i18n::loader;
use crate::pages::settings::Provider;
use crate::style as st;

#[derive(Debug, Clone, Default)]
pub struct Loaded {
    pub stored: Stored,
    /// Whether a key is found, per provider (OpenRouter, Vercel).
    pub key_set: (bool, bool),
    pub error: Option<String>,
}

#[derive(Default)]
pub struct State {
    pub loaded: Loaded,
    pub provider: Provider,
    pub model: String,
    pub transparent_model: String,
    pub key: String,
    pub busy: bool,
    pub checking: bool,
    /// Last model check: (text, is error).
    pub check: Option<(String, bool)>,
    /// Last outcome (text, is error); the shell turns it into a toast.
    pub result: Option<(String, bool)>,
}

#[derive(Debug, Clone)]
pub enum Msg {
    Loaded(Loaded),
    Provider(Provider),
    Model(String),
    TransparentModel(String),
    Key(String),
    Save,
    Check,
    Checked(Result<String, String>),
    Done(Result<(), String>),
}

fn wire(p: Provider) -> &'static str {
    match p {
        Provider::OpenRouter => PROVIDER_OPENROUTER,
        Provider::Vercel => PROVIDER_VERCEL,
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

pub fn load(paths: &Paths) -> Msg {
    Msg::Loaded(match Registry::load(paths) {
        Ok(reg) => Loaded {
            stored: ig::stored(&reg),
            key_set: (
                ig::gateway_key(&reg, &home(), PROVIDER_OPENROUTER).is_some(),
                ig::gateway_key(&reg, &home(), PROVIDER_VERCEL).is_some(),
            ),
            error: None,
        },
        Err(e) => Loaded {
            error: Some(format!("{e:#}")),
            ..Default::default()
        },
    })
}

fn blank(s: &str) -> Option<String> {
    Some(s.trim().to_string()).filter(|s| !s.is_empty())
}

pub fn update(state: &mut State, paths: Paths, msg: Msg) -> Task<Msg> {
    match msg {
        Msg::Loaded(loaded) => {
            state.provider = if loaded.stored.provider.as_deref() == Some(PROVIDER_OPENROUTER) {
                Provider::OpenRouter
            } else {
                Provider::Vercel
            };
            state.model = loaded.stored.model.clone().unwrap_or_default();
            state.transparent_model = loaded.stored.transparent_model.clone().unwrap_or_default();
            state.loaded = loaded;
            Task::none()
        }
        Msg::Provider(p) => {
            state.provider = p;
            state.check = None;
            Task::none()
        }
        Msg::Model(v) => {
            state.model = v;
            Task::none()
        }
        Msg::TransparentModel(v) => {
            state.transparent_model = v;
            Task::none()
        }
        Msg::Key(v) => {
            state.key = v;
            Task::none()
        }
        Msg::Check => {
            let provider = wire(state.provider);
            let model = blank(&state.model).unwrap_or_else(|| ig::default_model(provider).into());
            let transparent = blank(&state.transparent_model)
                .unwrap_or_else(|| ig::DEFAULT_TRANSPARENT_MODEL.into());
            state.checking = true;
            state.check = None;
            Task::perform(check(provider, model, transparent), Msg::Checked)
        }
        Msg::Checked(r) => {
            state.checking = false;
            state.check = Some(match r {
                Ok(t) => (t, false),
                Err(e) => (e, true),
            });
            Task::none()
        }
        Msg::Save => {
            let provider = state.provider;
            let stored = Stored {
                // Vercel is the default, so it is not written out.
                provider: (provider == Provider::OpenRouter).then(|| PROVIDER_OPENROUTER.into()),
                model: blank(&state.model),
                transparent_model: blank(&state.transparent_model),
            };
            let key = std::mem::take(&mut state.key).trim().to_string();
            if !key.is_empty()
                && let Err(e) = agentdeck_usage::util::atomic_write_mode(
                    &provider.key_file(),
                    key.as_bytes(),
                    Some(0o600),
                )
            {
                state.result = Some((format!("{}: {e:#}", provider.key_file().display()), true));
                return Task::none();
            }
            state.busy = true;
            Task::perform(
                async move {
                    tokio::task::spawn_blocking(move || {
                        ig::write(&paths, &stored).map_err(|e| format!("{e:#}"))
                    })
                    .await
                    .map_err(|e| e.to_string())
                    .and_then(|r| r)
                },
                Msg::Done,
            )
        }
        Msg::Done(r) => {
            state.busy = false;
            state.result = Some(match r {
                Ok(()) => (fl!(loader(), "settings-saved"), false),
                Err(e) => (e, true),
            });
            Task::done(load(&paths))
        }
    }
}

/// Free check against the gateways' public model lists: both models exist
/// and output images; the transparent one must be a Vercel image model.
async fn check(
    provider: &'static str,
    model: String,
    transparent: String,
) -> Result<String, String> {
    let get = |url: &'static str| async move {
        reqwest::get(url)
            .await
            .map_err(|e| e.to_string())?
            .json::<serde_json::Value>()
            .await
            .map_err(|e| e.to_string())
    };
    let vercel = get("https://ai-gateway.vercel.sh/v1/models").await?;
    let on_vercel = |id: &str| {
        vercel["data"]
            .as_array()
            .and_then(|d| d.iter().find(|m| m["id"] == id))
            .cloned()
    };
    let makes_images = if provider == PROVIDER_OPENROUTER {
        let or = get("https://openrouter.ai/api/v1/models").await?;
        or["data"]
            .as_array()
            .and_then(|d| d.iter().find(|m| m["id"] == model.as_str()))
            .map(|m| {
                m.pointer("/architecture/output_modalities")
                    .and_then(|o| o.as_array())
                    .is_some_and(|o| o.iter().any(|x| x == "image"))
            })
    } else {
        on_vercel(&model).map(|m| {
            m["type"] == "image"
                || m["tags"]
                    .as_array()
                    .is_some_and(|t| t.iter().any(|x| x == "image-generation"))
        })
    };
    match makes_images {
        None => return Err(fl!(loader(), "imagegen-check-missing", model = model)),
        Some(false) => return Err(fl!(loader(), "imagegen-check-not-image", model = model)),
        Some(true) => {}
    }
    if on_vercel(&transparent).is_none_or(|m| m["type"] != "image") {
        return Err(fl!(
            loader(),
            "imagegen-check-transparent",
            model = transparent
        ));
    }
    Ok(fl!(
        loader(),
        "imagegen-check-ok",
        model = model,
        transparent = transparent
    ))
}

fn field<'a>(label: String, input: Element<'a, Msg>, hint: Option<String>) -> Element<'a, Msg> {
    let mut col = column![st::caption(label), input].spacing(4);
    if let Some(hint) = hint {
        col = col.push(st::muted(hint));
    }
    col.into()
}

pub fn view(state: &State) -> Element<'_, Msg> {
    let size = st::look().body;
    let key_set = match state.provider {
        Provider::OpenRouter => state.loaded.key_set.0,
        Provider::Vercel => state.loaded.key_set.1,
    };
    let check_line: Element<'_, Msg> = match &state.check {
        Some((text, false)) => st::pill(text.clone(), st::Tone::Ok),
        Some((text, true)) => st::body(text.as_str()).into(),
        None => iced::widget::Space::new().into(),
    };
    let form = column![
        field(
            fl!(loader(), "imagegen-provider"),
            pick_list(
                [Provider::Vercel, Provider::OpenRouter],
                Some(state.provider),
                Msg::Provider
            )
            .text_size(size)
            .width(Length::Fill)
            .into(),
            None,
        ),
        field(
            fl!(loader(), "imagegen-model"),
            text_input(ig::default_model(wire(state.provider)), &state.model)
                .on_input(Msg::Model)
                .on_submit(Msg::Save)
                .size(size)
                .into(),
            Some(fl!(loader(), "imagegen-model-hint")),
        ),
        field(
            fl!(loader(), "imagegen-transparent-model"),
            text_input(ig::DEFAULT_TRANSPARENT_MODEL, &state.transparent_model)
                .on_input(Msg::TransparentModel)
                .on_submit(Msg::Save)
                .size(size)
                .into(),
            Some(fl!(loader(), "imagegen-transparent-hint")),
        ),
        field(
            fl!(
                loader(),
                "settings-key",
                provider = state.provider.to_string()
            ),
            text_input(&fl!(loader(), "settings-key-placeholder"), &state.key)
                .secure(true)
                .on_input(Msg::Key)
                .on_submit(Msg::Save)
                .size(size)
                .into(),
            Some(if key_set {
                fl!(loader(), "imagegen-key-set")
            } else {
                fl!(loader(), "imagegen-key-missing")
            }),
        ),
        row![
            container(check_line).width(Length::Fill),
            st::btn(
                st::Btn::Quiet,
                if state.checking {
                    fl!(loader(), "settings-testing")
                } else {
                    fl!(loader(), "settings-test")
                }
            )
            .on_press_maybe((!state.checking && !state.busy).then_some(Msg::Check)),
            st::btn(st::Btn::Primary, fl!(loader(), "settings-save"))
                .on_press_maybe((!state.busy).then_some(Msg::Save)),
        ]
        .spacing(st::SPACE_S)
        .align_y(Alignment::Center),
    ]
    .spacing(st::SPACE_M);
    let mut col = column![].spacing(st::SPACE_S);
    if let Some(e) = &state.loaded.error {
        col = col.push(st::card(st::muted(fl!(loader(), "load-error", what = e))));
    }
    col.push(st::card(form)).into()
}
