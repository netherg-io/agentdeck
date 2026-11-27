//! Module settings forms: the router's decision provider, model and key,
//! and the quota polling interval / staleness threshold.

use std::path::PathBuf;

use agentdeck_core::settings::{
    self, ApproverCredentialsStatus, ApproverCredentialsUpdate, ApproverSettings,
    CoordinatorPoolEntry, CoordinatorSettings, JudgeFallbackSettings, Settings, UserRuleSource,
    UserRules,
};
use agentdeck_core::{Paths, Registry};
use i18n_embed_fl::fl;
use iced::widget::{Space, column, pick_list, row, text_input};
use iced::{Alignment, Element, Length, Task};

use crate::i18n::loader;
use crate::pages::eval_history::{self, History};
use crate::style as st;

#[derive(Debug, Clone, Default)]
pub struct Loaded {
    pub settings: Option<Settings>,
    pub fallback: JudgeFallbackSettings,
    pub rules: UserRules,
    /// Whether a key is available, per provider (OpenRouter, Vercel).
    pub key_set: (bool, bool),
    pub error: Option<String>,
    /// Read-only assessment metadata; refreshed on page open and on demand.
    pub history: History,
}

#[derive(Default)]
pub struct State {
    pub loaded: Loaded,
    pub provider: Provider,
    pub decision_model: String,
    pub key: String,
    pub fallback_provider: FallbackProvider,
    pub fallback_model: String,
    pub fallback_testing: bool,
    /// Last fallback channel check: (text, is error), shown under the fallback form.
    pub fallback_test: Option<(String, bool)>,
    /// Inline validation error for the fallback channel form.
    pub fallback_error: Option<String>,
    pub refresh: String,
    pub stale: String,
    /// Comma-separated rule patterns; saving stores them explicitly.
    pub no_review: String,
    pub deny: String,
    /// True once the registry holds explicit `[router.rules]` keys.
    pub rules_user: bool,
    pub busy: bool,
    pub testing: bool,
    /// Last provider check: (text, is error), shown under the form.
    pub test: Option<(String, bool)>,
    /// Last outcome (text, is error); the shell turns it into a toast.
    pub result: Option<(String, bool)>,
    pub approver: ApproverState,
    pub coord: CoordState,
}

#[derive(Debug, Clone)]
pub enum Msg {
    Loaded(Box<Loaded>),
    Provider(Provider),
    DecisionModel(String),
    Key(String),
    FallbackProvider(FallbackProvider),
    FallbackModel(String),
    TestFallback,
    TestedFallback(Result<String, String>),
    SaveFallback,
    Refresh(String),
    Stale(String),
    NoReview(String),
    Deny(String),
    SaveRouter,
    SaveRules,
    Test,
    Tested(Result<String, String>),
    SavePoll,
    Done(Result<(), String>),
    /// Re-read the assessment file after the page stayed open.
    RefreshHistory,
    HistoryLoaded(History),
    Approver(Box<ApproverMsg>),
    Coord(Box<CoordMsg>),
}

/// Optional shared fallback channel provider (`[judge_fallback]` in `instances.toml`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FallbackProvider {
    #[default]
    None,
    OpenRouter,
    Vercel,
}

impl std::fmt::Display for FallbackProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => f.write_str(&fl!(loader(), "settings-fallback-none")),
            Self::OpenRouter => f.write_str("OpenRouter"),
            Self::Vercel => f.write_str("Vercel AI Gateway"),
        }
    }
}

impl FallbackProvider {
    fn stored(self) -> Option<String> {
        match self {
            Self::None => None,
            Self::OpenRouter => Some(settings::PROVIDER_OPENROUTER.to_string()),
            Self::Vercel => Some(settings::PROVIDER_VERCEL.to_string()),
        }
    }

    fn from_stored(provider: Option<&str>) -> Self {
        match provider {
            Some(p) if p == settings::PROVIDER_OPENROUTER => Self::OpenRouter,
            Some(p) if p == settings::PROVIDER_VERCEL => Self::Vercel,
            _ => Self::None,
        }
    }

    fn as_provider(self) -> Option<Provider> {
        match self {
            Self::None => None,
            Self::OpenRouter => Some(Provider::OpenRouter),
            Self::Vercel => Some(Provider::Vercel),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Provider {
    #[default]
    OpenRouter,
    Vercel,
}

impl std::fmt::Display for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::OpenRouter => "OpenRouter",
            Self::Vercel => "Vercel AI Gateway",
        })
    }
}

impl Provider {
    /// The router reads this env var first, then [`Provider::key_file`].
    fn key_env(self) -> &'static str {
        match self {
            Self::OpenRouter => "JEV_OPENROUTER_API_KEY",
            Self::Vercel => "AI_GATEWAY_API_KEY",
        }
    }

    pub(crate) fn key_file(self) -> PathBuf {
        let name = match self {
            Self::OpenRouter => "openrouter.key",
            Self::Vercel => "vercel.key",
        };
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join(".config/model-route")
            .join(name)
    }

    fn key_set(self) -> bool {
        std::env::var(self.key_env()).is_ok_and(|k| !k.is_empty())
            || std::fs::read_to_string(self.key_file()).is_ok_and(|k| !k.trim().is_empty())
    }
}

pub fn load(paths: &Paths) -> Msg {
    let key_set = (Provider::OpenRouter.key_set(), Provider::Vercel.key_set());
    Msg::Loaded(Box::new(match Registry::load(paths) {
        Ok(reg) => {
            // Broken `[router.rules]` or `[judge_fallback]` tables keep their
            // errors visible instead of silently falling back; fields still
            // edit from defaults.
            let mut errors = Vec::new();
            let rules = match UserRules::from_registry(&reg) {
                Ok(rules) => rules,
                Err(e) => {
                    errors.push(format!("{e:#}"));
                    UserRules::default()
                }
            };
            let fallback = match settings::read_judge_fallback(&reg) {
                Ok(fallback) => fallback,
                Err(e) => {
                    errors.push(format!("{e:#}"));
                    JudgeFallbackSettings::default()
                }
            };
            Loaded {
                settings: Some(settings::read(&reg)),
                fallback,
                rules,
                key_set,
                error: (!errors.is_empty()).then(|| errors.join("; ")),
                history: eval_history::load(),
            }
        }
        Err(e) => Loaded {
            settings: None,
            fallback: JudgeFallbackSettings::default(),
            rules: UserRules::default(),
            key_set,
            error: Some(format!("{e:#}")),
            history: eval_history::load(),
        },
    }))
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

/// Commas and backslashes are escaped so every stored regex round-trips.
fn patterns(raw: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut part = String::new();
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if matches!(chars.peek(), Some('\\' | ',')) => part.push(chars.next().unwrap()),
            ',' => {
                if !part.trim().is_empty() {
                    parts.push(part.trim().to_owned());
                }
                part.clear();
            }
            _ => part.push(c),
        }
    }
    if !part.trim().is_empty() {
        parts.push(part.trim().to_owned());
    }
    parts
}

fn format_patterns(patterns: &[String]) -> String {
    patterns
        .iter()
        .map(|p| p.replace('\\', "\\\\").replace(',', "\\,"))
        .collect::<Vec<_>>()
        .join(", ")
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

async fn probe_endpoint(
    provider: Option<&'static str>,
    model: String,
    key: Option<String>,
) -> Result<String, String> {
    let started = std::time::Instant::now();
    agentdeck_router::route::probe_decision(provider, &model, key.as_deref())
        .await
        .map(|(size, difficulty, cost)| {
            let ms = started.elapsed().as_millis().to_string();
            let cost = cost.map(|c| format!(", ${c:.6}")).unwrap_or_default();
            fl!(
                loader(),
                "settings-test-ok",
                model = model,
                size = format!("{size:.2}"),
                difficulty = format!("{difficulty:.2}"),
                ms = ms,
                cost = cost
            )
        })
        .map_err(|e| format!("{e:#}"))
}

fn fallback_probe_key(state: &State) -> Option<String> {
    (state.fallback_provider.as_provider() == Some(state.provider))
        .then(|| state.key.trim().to_string())
        .filter(|k| !k.is_empty())
}

fn fallback_settings(state: &State) -> JudgeFallbackSettings {
    match state.fallback_provider {
        FallbackProvider::None => JudgeFallbackSettings::default(),
        p => JudgeFallbackSettings {
            provider: p.stored(),
            model: blanked(&state.fallback_model),
        },
    }
}

pub fn update(state: &mut State, paths: Paths, msg: Msg) -> Task<Msg> {
    match msg {
        Msg::Loaded(loaded) => {
            let loaded = *loaded;
            if let Some(s) = &loaded.settings {
                let provider = if s.decision_provider.as_deref() == Some(settings::PROVIDER_VERCEL)
                {
                    Provider::Vercel
                } else {
                    Provider::OpenRouter
                };
                if state.provider != provider {
                    state.key.clear();
                }
                state.provider = provider;
                state.decision_model = s.decision_model.clone().unwrap_or_default();
                state.refresh = s.refresh_seconds.map(|n| n.to_string()).unwrap_or_default();
                state.stale = s.stale_seconds.map(|n| n.to_string()).unwrap_or_default();
            }
            state.fallback_provider =
                FallbackProvider::from_stored(loaded.fallback.provider.as_deref());
            state.fallback_model = loaded.fallback.model.clone().unwrap_or_default();
            state.fallback_error = None;
            state.no_review = format_patterns(&loaded.rules.no_review_models);
            state.deny = format_patterns(&loaded.rules.deny_models);
            state.rules_user = loaded.rules.source == UserRuleSource::User;
            state.loaded = loaded;
            Task::none()
        }
        Msg::Provider(p) => {
            if state.provider != p {
                state.key.clear();
            }
            state.provider = p;
            state.test = None;
            Task::none()
        }
        Msg::Test => {
            let provider = state.provider;
            let model = Some(state.decision_model.trim().to_string())
                .filter(|m| !m.is_empty())
                .or_else(|| {
                    (provider == Provider::OpenRouter)
                        .then(|| settings::DEFAULT_DECISION_MODEL.to_string())
                });
            let Some(model) = model else {
                state.test = Some((fl!(loader(), "settings-model-required"), true));
                return Task::none();
            };
            // An unsaved key in the field is what the user wants to check.
            let key = Some(state.key.trim().to_string()).filter(|k| !k.is_empty());
            state.testing = true;
            state.test = None;
            let wire = (provider == Provider::Vercel).then_some(settings::PROVIDER_VERCEL);
            Task::perform(probe_endpoint(wire, model, key), Msg::Tested)
        }
        Msg::Tested(result) => {
            state.testing = false;
            state.test = Some(match result {
                Ok(text) => (text, false),
                Err(e) => (e, true),
            });
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
        Msg::FallbackProvider(p) => {
            state.fallback_provider = p;
            state.fallback_test = None;
            state.fallback_error = None;
            Task::none()
        }
        Msg::FallbackModel(v) => {
            state.fallback_model = v;
            state.fallback_error = None;
            Task::none()
        }
        Msg::TestFallback => {
            if state.fallback_testing || state.busy {
                return Task::none();
            }
            let cfg = fallback_settings(state);
            if let Err(e) = cfg.validate() {
                state.fallback_error = Some(format!("{e:#}"));
                return Task::none();
            }
            let Some(endpoint) = cfg.endpoint() else {
                return Task::none();
            };
            let key = fallback_probe_key(state);
            state.fallback_error = None;
            state.fallback_testing = true;
            state.fallback_test = None;
            let wire = (endpoint.provider == settings::PROVIDER_VERCEL)
                .then_some(settings::PROVIDER_VERCEL);
            Task::perform(
                probe_endpoint(wire, endpoint.model, key),
                Msg::TestedFallback,
            )
        }
        Msg::TestedFallback(result) => {
            state.fallback_testing = false;
            state.fallback_test = Some(match result {
                Ok(text) => (text, false),
                Err(e) => (e, true),
            });
            Task::none()
        }
        Msg::SaveFallback => {
            if state.busy {
                return Task::none();
            }
            let cfg = fallback_settings(state);
            if let Err(e) = cfg.validate() {
                state.fallback_error = Some(format!("{e:#}"));
                return Task::none();
            }
            state.fallback_error = None;
            state.busy = true;
            Task::perform(
                async move {
                    tokio::task::spawn_blocking(move || {
                        settings::write_judge_fallback(&paths, &cfg).map_err(|e| format!("{e:#}"))
                    })
                    .await
                    .map_err(|e| e.to_string())
                    .and_then(|r| r)
                },
                Msg::Done,
            )
        }
        Msg::Refresh(v) => {
            state.refresh = v;
            Task::none()
        }
        Msg::Stale(v) => {
            state.stale = v;
            Task::none()
        }
        Msg::NoReview(v) => {
            state.no_review = v;
            Task::none()
        }
        Msg::Deny(v) => {
            state.deny = v;
            Task::none()
        }
        Msg::SaveRouter => {
            let model = Some(state.decision_model.trim().to_string()).filter(|m| !m.is_empty());
            let provider = state.provider;
            if provider == Provider::Vercel && model.is_none() {
                state.result = Some((fl!(loader(), "settings-model-required"), true));
                return Task::none();
            }
            state.busy = true;
            let key = std::mem::take(&mut state.key);
            let key = key.trim().to_string();
            if !key.is_empty()
                && let Err(e) = agentdeck_usage::util::atomic_write_mode(
                    &provider.key_file(),
                    key.as_bytes(),
                    Some(0o600),
                )
            {
                state.busy = false;
                state.result = Some((format!("{}: {e:#}", provider.key_file().display()), true));
                return Task::none();
            }
            save(paths, move |s| {
                s.decision_provider =
                    (provider == Provider::Vercel).then(|| settings::PROVIDER_VERCEL.to_string());
                s.decision_model = model;
            })
        }
        Msg::SaveRules => {
            // Validate first so a bad pattern blocks the write and keeps
            // the user's text; only the two rule keys are stored.
            let rules = UserRules {
                no_review_models: patterns(&state.no_review),
                deny_models: patterns(&state.deny),
                source: UserRuleSource::User,
            };
            if let Err(e) = rules.validate() {
                state.result = Some((format!("{e:#}"), true));
                return Task::none();
            }
            state.busy = true;
            Task::perform(
                async move {
                    tokio::task::spawn_blocking(move || {
                        settings::write_user_rules(&paths, &rules).map_err(|e| format!("{e:#}"))
                    })
                    .await
                    .map_err(|e| e.to_string())
                    .and_then(|r| r)
                },
                Msg::Done,
            )
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
        Msg::RefreshHistory => {
            Task::perform(async move { eval_history::load() }, Msg::HistoryLoaded)
        }
        Msg::HistoryLoaded(history) => {
            state.loaded.history = history;
            Task::none()
        }
        Msg::Approver(inner) => update_approver(&mut state.approver, paths, *inner),
        Msg::Coord(inner) => update_coord(&mut state.coord, paths, *inner),
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

/// The last check's outcome, filling the space left of the buttons.
fn test_line(test: &Option<(String, bool)>) -> Element<'_, Msg> {
    let line: Element<'_, Msg> = match test {
        Some((text, false)) => st::pill(text.clone(), st::Tone::Ok),
        Some((text, true)) => st::body(text.as_str()).into(),
        None => iced::widget::Space::new().into(),
    };
    iced::widget::container(line).width(Length::Fill).into()
}

pub fn router_view(state: &State) -> Element<'_, Msg> {
    let size = st::look().body;
    let key_set = match state.provider {
        Provider::OpenRouter => state.loaded.key_set.0,
        Provider::Vercel => state.loaded.key_set.1,
    };
    let (model_placeholder, model_hint) = match state.provider {
        Provider::OpenRouter => (
            settings::DEFAULT_DECISION_MODEL,
            fl!(loader(), "settings-decision-model-hint"),
        ),
        Provider::Vercel => (
            "provider/model",
            fl!(loader(), "settings-decision-model-vercel"),
        ),
    };
    let key_hint = if key_set {
        fl!(loader(), "settings-key-set")
    } else {
        fl!(loader(), "settings-key-missing")
    };
    let form = column![
        field(
            fl!(loader(), "settings-decision-provider"),
            pick_list(
                [Provider::OpenRouter, Provider::Vercel],
                Some(state.provider),
                Msg::Provider
            )
            .text_size(size)
            .width(Length::Fill)
            .into(),
            None,
        ),
        field(
            fl!(loader(), "settings-decision-model"),
            text_input(model_placeholder, &state.decision_model)
                .on_input(Msg::DecisionModel)
                .on_submit(Msg::SaveRouter)
                .size(size)
                .into(),
            Some(model_hint),
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
                .on_submit(Msg::SaveRouter)
                .size(size)
                .into(),
            Some(key_hint),
        ),
        row![
            test_line(&state.test),
            st::btn(
                st::Btn::Quiet,
                if state.testing {
                    fl!(loader(), "settings-testing")
                } else {
                    fl!(loader(), "settings-test")
                }
            )
            .on_press_maybe((!state.testing && !state.busy).then_some(Msg::Test)),
            st::btn(st::Btn::Primary, fl!(loader(), "settings-save"))
                .on_press_maybe((!state.busy).then_some(Msg::SaveRouter)),
        ]
        .spacing(st::SPACE_S)
        .align_y(Alignment::Center),
    ]
    .spacing(st::SPACE_M);
    column![]
        .push(error_card(state))
        .push(st::card(form))
        .push(fallback_card(state))
        .push(rules_card(state))
        .push(history_card(state))
        .spacing(st::SPACE_S)
        .into()
}

fn fallback_card(state: &State) -> Element<'_, Msg> {
    let size = st::look().body;
    let mut form = column![
        column![
            st::caption(fl!(loader(), "settings-fallback")),
            st::muted(fl!(loader(), "settings-fallback-note")),
        ]
        .spacing(4),
        field(
            fl!(loader(), "settings-fallback-provider"),
            pick_list(
                [
                    FallbackProvider::None,
                    FallbackProvider::OpenRouter,
                    FallbackProvider::Vercel,
                ],
                Some(state.fallback_provider),
                Msg::FallbackProvider
            )
            .text_size(size)
            .width(Length::Fill)
            .into(),
            None,
        ),
    ]
    .spacing(st::SPACE_M);
    if state.fallback_provider != FallbackProvider::None {
        let (model_placeholder, model_hint) = match state.fallback_provider {
            FallbackProvider::OpenRouter => (
                settings::DEFAULT_DECISION_MODEL,
                fl!(loader(), "settings-fallback-model-hint"),
            ),
            FallbackProvider::Vercel => (
                "provider/model",
                fl!(loader(), "settings-decision-model-vercel"),
            ),
            FallbackProvider::None => unreachable!(),
        };
        let mut model_field = column![
            st::caption(fl!(loader(), "settings-fallback-model")),
            text_input(model_placeholder, &state.fallback_model)
                .on_input(Msg::FallbackModel)
                .on_submit(Msg::SaveFallback)
                .size(size),
            st::muted(model_hint),
        ]
        .spacing(4);
        if let Some(err) = &state.fallback_error {
            model_field = model_field.push(st::body(err.as_str()));
        }
        form = form.push(model_field);
    } else if let Some(err) = &state.fallback_error {
        form = form.push(st::body(err.as_str()));
    }
    let can_test =
        state.fallback_provider != FallbackProvider::None && !state.fallback_testing && !state.busy;
    form = form.push(
        row![
            test_line(&state.fallback_test),
            st::btn(
                st::Btn::Quiet,
                if state.fallback_testing {
                    fl!(loader(), "settings-testing")
                } else {
                    fl!(loader(), "settings-test")
                }
            )
            .on_press_maybe(can_test.then_some(Msg::TestFallback)),
            st::btn(st::Btn::Primary, fl!(loader(), "settings-save"))
                .on_press_maybe((!state.busy).then_some(Msg::SaveFallback)),
        ]
        .spacing(st::SPACE_S)
        .align_y(Alignment::Center),
    );
    st::card(form)
}

fn rules_card(state: &State) -> Element<'_, Msg> {
    let size = st::look().body;
    let source = if state.rules_user {
        fl!(loader(), "settings-rules-source-user")
    } else {
        fl!(loader(), "settings-rules-source-default")
    };
    let form = column![
        st::caption(format!("{} · {source}", fl!(loader(), "settings-rules"))),
        field(
            fl!(loader(), "settings-no-review"),
            text_input(
                &fl!(loader(), "settings-rules-placeholder"),
                &state.no_review
            )
            .on_input(Msg::NoReview)
            .on_submit(Msg::SaveRules)
            .size(size)
            .into(),
            Some(fl!(loader(), "settings-no-review-hint")),
        ),
        field(
            fl!(loader(), "settings-deny"),
            text_input(&fl!(loader(), "settings-rules-placeholder"), &state.deny)
                .on_input(Msg::Deny)
                .on_submit(Msg::SaveRules)
                .size(size)
                .into(),
            Some(fl!(loader(), "settings-deny-hint")),
        ),
        st::muted(fl!(loader(), "settings-rules-note")),
        save_row(state, Msg::SaveRules),
    ]
    .spacing(st::SPACE_M);
    st::card(form)
}

/// Fluent wraps placeables in bidi isolates; strip them so exact-text
/// matching (headless tests, assistive readers of the plain string) works.
fn plain(s: String) -> String {
    s.chars()
        .filter(|c| !matches!(c, '\u{2068}' | '\u{2069}'))
        .collect()
}

/// One assessment row: source and verdict pills (plus a verbatim `workflow`
/// pill for workflow task evaluations) and the model; the muted line carries
/// task id, category, timestamp, evidence count/kind, confidence, self-check,
/// cost and, for workflow evaluations, the run/step join keys.
/// Verdicts are shown, never scored or compared.
fn entry_row(entry: &eval_history::Entry) -> Element<'_, Msg> {
    let tone = match entry.verdict.as_str() {
        "ok" | "pass" => st::Tone::Ok,
        "partial" => st::Tone::Warn,
        "fail" => st::Tone::Danger,
        _ => st::Tone::Neutral,
    };
    let age = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|now| now.as_secs().checked_sub(entry.t))
        .filter(|_| entry.t != 0)
        .map(|seconds| {
            if seconds < 3600 {
                fl!(
                    loader(),
                    "settings-eval-minutes-ago",
                    count = (seconds / 60).to_string()
                )
            } else if seconds < 86400 {
                fl!(
                    loader(),
                    "settings-eval-hours-ago",
                    count = (seconds / 3600).to_string()
                )
            } else {
                fl!(
                    loader(),
                    "settings-eval-days-ago",
                    count = (seconds / 86400).to_string()
                )
            }
        });
    let mut detail = format!("{} · {}", entry.task_id, entry.category);
    if let Some(age) = age {
        detail.push_str(&format!(" · {age}"));
    }
    if entry.source == eval_history::Source::Decider {
        let self_line = match entry.self_judged {
            Some(true) => fl!(loader(), "settings-eval-self"),
            _ => fl!(loader(), "settings-eval-self-unknown"),
        };
        let cost = match entry.cost {
            Some(c) => fl!(loader(), "settings-eval-cost", cost = format!("{c:.6}")),
            None => fl!(loader(), "settings-eval-cost-unknown"),
        };
        let evidence = fl!(
            loader(),
            "settings-eval-evidence-ids",
            count = entry.evidence_count.to_string()
        );
        detail.push_str(&format!(" · {evidence}"));
        if let Some(c) = entry.confidence {
            detail.push_str(&format!(
                " · {}",
                fl!(
                    loader(),
                    "settings-eval-confidence",
                    value = format!("{c:.2}")
                )
            ));
        }
        detail.push_str(&format!(" · {self_line} · {cost}"));
    } else {
        detail.push_str(&format!(" · {}", entry.evidence));
    }
    if let Some(judge) = &entry.judge {
        detail.push_str(&format!(
            " · {}",
            fl!(loader(), "settings-eval-judge", who = judge.clone())
        ));
    }
    // Workflow task evaluations carry the run/step join keys; plain decider
    // and coordinator rows never show this segment.
    let workflow = entry.run_id.as_ref().zip(entry.step.as_ref());
    if let Some((run, step)) = workflow {
        detail.push_str(&format!(
            " · {}",
            fl!(
                loader(),
                "settings-eval-workflow",
                run = run.clone(),
                step = step.clone()
            )
        ));
    }
    let mut pills = row![
        st::pill(entry.source.as_str(), st::Tone::Neutral),
        st::pill(entry.verdict.as_str(), tone),
        st::mono(entry.model.clone()),
    ];
    if workflow.is_some() {
        // Verbatim like the source pills; never ranked.
        pills = pills.push(st::pill("workflow", st::Tone::Accent));
    }
    column![
        pills.spacing(st::SPACE_S).align_y(Alignment::Center),
        st::mono_muted(plain(detail)),
    ]
    .spacing(2.0)
    .into()
}

/// Read-only assessment history: observations only, routing does not use them.
fn history_card(state: &State) -> Element<'_, Msg> {
    let mut col = column![
        row![
            st::caption(fl!(loader(), "settings-eval-title")),
            Space::new().width(Length::Fill),
            st::btn(st::Btn::Quiet, fl!(loader(), "settings-eval-refresh"))
                .on_press(Msg::RefreshHistory),
        ]
        .align_y(Alignment::Center),
        st::muted(fl!(loader(), "settings-eval-note")),
    ]
    .spacing(st::SPACE_S);
    match &state.loaded.history {
        History::Missing => {
            col = col.push(st::muted(fl!(loader(), "settings-eval-missing")));
        }
        History::Empty => {
            col = col.push(st::muted(fl!(loader(), "settings-eval-empty")));
        }
        History::Unreadable(what) => {
            col = col.push(st::body(plain(fl!(
                loader(),
                "settings-eval-unreadable",
                what = what.clone()
            ))));
        }
        History::Corrupt { line, what } => {
            col = col.push(st::body(plain(fl!(
                loader(),
                "settings-eval-corrupt",
                line = line.to_string(),
                what = what.clone()
            ))));
        }
        History::Loaded(entries) => {
            for entry in entries.iter().rev().take(eval_history::MAX_SHOWN) {
                col = col.push(entry_row(entry));
            }
        }
    }
    st::card(col)
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

// ---- Approver & Coordinator settings (long-horizon units) ----

/// One recent approver audit row: enough to see who decided what, and how.
#[derive(Debug, Clone, Default)]
pub struct AuditRow {
    pub at: String,
    pub host: String,
    pub kind: String,
    pub outcome: String,
    pub source: String,
}

#[derive(Debug, Clone, Default)]
pub struct ApproverLoaded {
    pub cfg: Option<ApproverSettings>,
    pub creds: ApproverCredentialsStatus,
    pub audit: Vec<AuditRow>,
    /// Reading the audit log failed (missing file stays an empty `audit`).
    pub audit_error: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct CoordLoaded {
    pub cfg: Option<CoordinatorSettings>,
    pub instances: Vec<String>,
    pub error: Option<String>,
}

/// Policy judge backend: blank in storage means the default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum JudgeProvider {
    #[default]
    Default,
    OpenRouter,
    Vercel,
}

impl std::fmt::Display for JudgeProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Default => f.write_str(&fl!(loader(), "approver-judge-default")),
            Self::OpenRouter => f.write_str("OpenRouter"),
            Self::Vercel => f.write_str("Vercel AI Gateway"),
        }
    }
}

impl JudgeProvider {
    fn stored(self) -> Option<String> {
        match self {
            Self::Default => None,
            Self::OpenRouter => Some(settings::PROVIDER_OPENROUTER.to_string()),
            Self::Vercel => Some(settings::PROVIDER_VERCEL.to_string()),
        }
    }

    fn from_stored(provider: Option<&str>) -> Self {
        match provider {
            Some(p) if p == settings::PROVIDER_VERCEL => Self::Vercel,
            Some(_) => Self::OpenRouter,
            None => Self::Default,
        }
    }
}

#[derive(Default)]
pub struct ApproverState {
    pub loaded: ApproverLoaded,
    /// Write-only Telegram bot token draft; cleared after save, never pre-filled.
    pub bot_token: String,
    pub owner_id: String,
    pub auto_timeout: String,
    pub inter_timeout: String,
    pub judge: JudgeProvider,
    pub judge_model: String,
    pub confidence: String,
    pub remote_hosts: String,
    pub socket_path: String,
    pub deny: String,
    pub busy: bool,
    /// Last outcome (text, is error), shown under the form.
    pub result: Option<(String, bool)>,
}

#[derive(Default)]
pub struct CoordState {
    pub loaded: CoordLoaded,
    pub entries: Vec<CoordinatorPoolEntry>,
    /// Kept across saves; the form does not edit it.
    pub token_file: Option<PathBuf>,
    pub new_instance: String,
    pub new_model: String,
    pub reserve: String,
    pub proactive: String,
    pub sleep_buffer: String,
    pub wake_grace: String,
    pub soft_runs: String,
    pub soft_items: String,
    pub confidence: String,
    pub min_disk: String,
    pub t3_endpoint: String,
    pub judge: JudgeProvider,
    pub judge_model: String,
    pub busy: bool,
    /// Last outcome (text, is error), shown under the form.
    pub result: Option<(String, bool)>,
}

#[derive(Debug, Clone)]
pub enum ApproverMsg {
    Loaded(Box<ApproverLoaded>),
    BotToken(String),
    OwnerId(String),
    AutoTimeout(String),
    InterTimeout(String),
    Judge(JudgeProvider),
    JudgeModel(String),
    Confidence(String),
    RemoteHosts(String),
    SocketPath(String),
    Deny(String),
    Save,
    Done(Result<(), String>),
    RefreshAudit,
    AuditLoaded(Result<Vec<AuditRow>, String>),
}

#[derive(Debug, Clone)]
pub enum CoordMsg {
    Loaded(CoordLoaded),
    NewInstance(String),
    NewModel(String),
    Add,
    Remove(usize),
    Toggle(usize, bool),
    Reserve(String),
    Proactive(String),
    SleepBuffer(String),
    WakeGrace(String),
    SoftRuns(String),
    SoftItems(String),
    Confidence(String),
    MinDisk(String),
    T3Endpoint(String),
    Judge(JudgeProvider),
    JudgeModel(String),
    Save,
    Done(Result<(), String>),
}

fn am(msg: ApproverMsg) -> Msg {
    Msg::Approver(Box::new(msg))
}

fn cm(msg: CoordMsg) -> Msg {
    Msg::Coord(Box::new(msg))
}

/// Blank means "default"; anything else must be a finite number.
fn number(raw: &str) -> Result<Option<f64>, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    raw.parse::<f64>()
        .ok()
        .filter(|n| n.is_finite())
        .map(Some)
        .ok_or_else(|| fl!(loader(), "settings-number-invalid"))
}

/// Blank means "default"; anything else must be a positive whole number.
fn count(raw: &str) -> Result<Option<u32>, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    raw.parse::<u32>()
        .ok()
        .filter(|n| *n > 0)
        .map(Some)
        .ok_or_else(|| fl!(loader(), "settings-number-invalid"))
}

fn blanked(raw: &str) -> Option<String> {
    Some(raw.trim().to_string()).filter(|s| !s.is_empty())
}

/// Days since the Unix epoch as a civil `YYYY-MM-DD` date (no timezone
/// crate: the audit log is UTC by construction).
fn civil_ymd(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let mut y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    y += (m <= 2) as i64;
    (y, m, d)
}

/// Epoch seconds as UTC `YYYY-MM-DD HH:MM:SS` (never a raw number in the UI).
fn fmt_ts(ts: u64) -> String {
    let (y, m, d) = civil_ymd((ts / 86_400) as i64);
    let s = ts % 86_400;
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        s / 3600,
        s % 3600 / 60,
        s % 60
    )
}

/// Last 20 valid audit records, oldest first, from a bounded tail read: the
/// file stays off the UI thread and a huge log never loads fully. A missing
/// file is an empty log; any other IO failure is an error.
fn read_audit(state_dir: &std::path::Path) -> Result<Vec<AuditRow>, String> {
    use std::io::{Read, Seek, SeekFrom};
    let path = state_dir.join("approver").join("audit.jsonl");
    let mut f = match std::fs::File::open(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("{}: {e}", path.display())),
        Ok(f) => f,
    };
    // Last 64 KiB only; a torn first line simply fails to parse and is skipped.
    const TAIL: u64 = 64 * 1024;
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if len > TAIL {
        f.seek(SeekFrom::Start(len - TAIL))
            .map_err(|e| e.to_string())?;
    }
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).map_err(|e| e.to_string())?;
    let mut rows = Vec::new();
    for line in String::from_utf8_lossy(&buf).lines().rev() {
        if rows.len() >= 20 {
            break;
        }
        if line.trim().is_empty() {
            continue;
        }
        if let Some(row) = parse_audit_line(line) {
            rows.push(row);
        }
    }
    rows.reverse();
    Ok(rows)
}

fn parse_audit_line(line: &str) -> Option<AuditRow> {
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    let s = |key: &str| {
        v.get(key)
            .and_then(|x| x.as_str())
            .unwrap_or("—")
            .to_string()
    };
    Some(AuditRow {
        at: v
            .get("timestamp")
            .and_then(|t| t.as_u64())
            .map(fmt_ts)
            .unwrap_or_else(|| "—".into()),
        host: s("host"),
        kind: s("kind"),
        outcome: s("outcome"),
        source: s("source"),
    })
}

pub fn load_approver(paths: &Paths) -> Msg {
    let creds = settings::read_approver_credentials(&paths.config_dir);
    am(ApproverMsg::Loaded(Box::new(match Registry::load(paths) {
        Ok(reg) => match settings::read_approver(&reg) {
            Ok(cfg) => {
                // Bounded read; the page itself loads inside a Task, Refresh
                // re-reads off-thread via spawn_blocking.
                let (audit, audit_error) = match read_audit(&paths.state_dir) {
                    Ok(audit) => (audit, None),
                    Err(e) => (Vec::new(), Some(e)),
                };
                ApproverLoaded {
                    cfg: Some(cfg),
                    creds,
                    audit,
                    audit_error,
                    error: None,
                }
            }
            Err(e) => ApproverLoaded {
                creds,
                error: Some(format!("{e:#}")),
                ..Default::default()
            },
        },
        Err(e) => ApproverLoaded {
            creds,
            error: Some(format!("{e:#}")),
            ..Default::default()
        },
    })))
}

pub fn load_coord(paths: &Paths) -> Msg {
    cm(CoordMsg::Loaded(match Registry::load(paths) {
        Ok(reg) => match settings::read_coordinator(&reg) {
            Ok(cfg) => CoordLoaded {
                cfg: Some(cfg),
                instances: reg.instances.iter().map(|i| i.id.clone()).collect(),
                error: None,
            },
            Err(e) => CoordLoaded {
                error: Some(format!("{e:#}")),
                ..Default::default()
            },
        },
        Err(e) => CoordLoaded {
            error: Some(format!("{e:#}")),
            ..Default::default()
        },
    }))
}

fn approver_cfg(s: &ApproverState) -> Result<ApproverSettings, String> {
    let d = ApproverSettings::default();
    let cfg = ApproverSettings {
        socket_path: blanked(&s.socket_path).map(PathBuf::from),
        autonomous_timeout_secs: seconds(&s.auto_timeout)?.unwrap_or(d.autonomous_timeout_secs),
        interactive_timeout_secs: seconds(&s.inter_timeout)?.unwrap_or(d.interactive_timeout_secs),
        judge_provider: s.judge.stored(),
        judge_model: blanked(&s.judge_model),
        judge_confidence_threshold: number(&s.confidence)?.unwrap_or(d.judge_confidence_threshold),
        remote_hosts: patterns(&s.remote_hosts),
        extra_hard_deny_patterns: patterns(&s.deny),
    };
    // Core bounds (timeout ranges, loopback rules live in the coordinator
    // cfg): reject here so the draft survives with an inline error.
    cfg.validate().map_err(|e| format!("{e:#}"))?;
    Ok(cfg)
}

fn coord_cfg(s: &CoordState) -> Result<CoordinatorSettings, String> {
    let d = CoordinatorSettings::default();
    let cfg = CoordinatorSettings {
        pool: s.entries.clone(),
        min_weekly_reserve_pct: number(&s.reserve)?.unwrap_or(d.min_weekly_reserve_pct),
        proactive_rotate_below_5h_pct: number(&s.proactive)?
            .unwrap_or(d.proactive_rotate_below_5h_pct),
        quota_sleep_buffer_secs: seconds(&s.sleep_buffer)?.unwrap_or(d.quota_sleep_buffer_secs),
        idle_wake_grace_secs: seconds(&s.wake_grace)?.unwrap_or(d.idle_wake_grace_secs),
        checkpoint_soft_runs: count(&s.soft_runs)?.unwrap_or(d.checkpoint_soft_runs),
        checkpoint_soft_items: count(&s.soft_items)?.unwrap_or(d.checkpoint_soft_items),
        checkpoint_confidence_threshold: number(&s.confidence)?
            .unwrap_or(d.checkpoint_confidence_threshold),
        min_free_disk_gb: seconds(&s.min_disk)?.unwrap_or(d.min_free_disk_gb),
        t3_endpoint: blanked(&s.t3_endpoint),
        t3_token_file: s.token_file.clone(),
        judge_provider: s.judge.stored(),
        judge_model: blanked(&s.judge_model),
    };
    // Core bounds, duplicate pool keys and the loopback endpoint check: an
    // inline error keeps the draft instead of a wiped failed save.
    cfg.validate().map_err(|e| format!("{e:#}"))?;
    Ok(cfg)
}

fn parse_owner_id(raw: &str) -> Result<Option<i64>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    trimmed
        .parse::<i64>()
        .ok()
        .filter(|id| *id != 0)
        .map(Some)
        .ok_or_else(|| fl!(loader(), "approver-owner-id-invalid"))
}

fn approver_cred_update(s: &ApproverState) -> Result<ApproverCredentialsUpdate, String> {
    let token = s.bot_token.trim();
    if token.chars().any(|c| c.is_control()) {
        return Err(fl!(loader(), "approver-bot-token-invalid"));
    }
    let owner_id = parse_owner_id(&s.owner_id)?;
    Ok(ApproverCredentialsUpdate {
        telegram_bot_token: (!token.is_empty()).then(|| Some(token.to_string())),
        telegram_owner_id: (owner_id != s.loaded.creds.owner_id).then_some(owner_id),
    })
}

fn update_approver(state: &mut ApproverState, paths: Paths, msg: ApproverMsg) -> Task<Msg> {
    match msg {
        ApproverMsg::Loaded(loaded) => {
            let loaded = *loaded;
            if let Some(cfg) = &loaded.cfg {
                state.auto_timeout = cfg.autonomous_timeout_secs.to_string();
                state.inter_timeout = cfg.interactive_timeout_secs.to_string();
                state.judge = JudgeProvider::from_stored(cfg.judge_provider.as_deref());
                state.judge_model = cfg.judge_model.clone().unwrap_or_default();
                state.confidence = cfg.judge_confidence_threshold.to_string();
                state.remote_hosts = cfg.remote_hosts.join(", ");
                state.socket_path = cfg
                    .socket_path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                // Escaped form: patterns with literal commas or backslashes
                // survive an unrelated save byte-for-byte.
                state.deny = format_patterns(&cfg.extra_hard_deny_patterns);
            }
            state.owner_id = loaded
                .creds
                .owner_id
                .map(|id| id.to_string())
                .unwrap_or_default();
            state.loaded = loaded;
            Task::none()
        }
        ApproverMsg::BotToken(v) => {
            state.bot_token = v;
            Task::none()
        }
        ApproverMsg::OwnerId(v) => {
            state.owner_id = v;
            Task::none()
        }
        ApproverMsg::AutoTimeout(v) => {
            state.auto_timeout = v;
            Task::none()
        }
        ApproverMsg::InterTimeout(v) => {
            state.inter_timeout = v;
            Task::none()
        }
        ApproverMsg::Judge(p) => {
            state.judge = p;
            Task::none()
        }
        ApproverMsg::JudgeModel(v) => {
            state.judge_model = v;
            Task::none()
        }
        ApproverMsg::Confidence(v) => {
            state.confidence = v;
            Task::none()
        }
        ApproverMsg::RemoteHosts(v) => {
            state.remote_hosts = v;
            Task::none()
        }
        ApproverMsg::SocketPath(v) => {
            state.socket_path = v;
            Task::none()
        }
        ApproverMsg::Deny(v) => {
            state.deny = v;
            Task::none()
        }
        ApproverMsg::Save => {
            // A second Save (or Enter) while a write is in flight is ignored.
            if state.busy {
                return Task::none();
            }
            let cred_update = match approver_cred_update(state) {
                Ok(u) => u,
                Err(e) => {
                    state.result = Some((e, true));
                    return Task::none();
                }
            };
            match approver_cfg(state) {
                Err(e) => {
                    state.result = Some((e, true));
                    Task::none()
                }
                Ok(cfg) => {
                    state.busy = true;
                    Task::perform(
                        async move {
                            tokio::task::spawn_blocking(move || {
                                settings::write_approver(&paths, &cfg, false)
                                    .map_err(|e| format!("{e:#}"))?;
                                if cred_update.telegram_bot_token.is_some()
                                    || cred_update.telegram_owner_id.is_some()
                                {
                                    settings::write_approver_credentials(
                                        &paths.config_dir,
                                        &cred_update,
                                    )
                                    .map_err(|e| format!("{e:#}"))?;
                                }
                                Ok(())
                            })
                            .await
                            .map_err(|e| e.to_string())
                            .and_then(|r| r)
                        },
                        |r| am(ApproverMsg::Done(r)),
                    )
                }
            }
        }
        ApproverMsg::Done(result) => {
            state.busy = false;
            // A failed write keeps the draft: only a success reloads.
            let ok = result.is_ok();
            if ok {
                state.bot_token.clear();
            }
            state.result = Some(match result {
                Ok(()) => (fl!(loader(), "settings-saved"), false),
                Err(e) => (e, true),
            });
            if ok {
                Task::done(load_approver(&paths))
            } else {
                Task::none()
            }
        }
        ApproverMsg::RefreshAudit => {
            let dir = paths.state_dir.clone();
            Task::perform(
                async move {
                    tokio::task::spawn_blocking(move || read_audit(&dir))
                        .await
                        .map_err(|e| e.to_string())
                        .and_then(|r| r)
                },
                |r| am(ApproverMsg::AuditLoaded(r)),
            )
        }
        ApproverMsg::AuditLoaded(result) => {
            match result {
                Ok(audit) => {
                    state.loaded.audit = audit;
                    state.loaded.audit_error = None;
                }
                Err(e) => state.loaded.audit_error = Some(e),
            }
            Task::none()
        }
    }
}

fn update_coord(state: &mut CoordState, paths: Paths, msg: CoordMsg) -> Task<Msg> {
    match msg {
        CoordMsg::Loaded(loaded) => {
            if let Some(cfg) = &loaded.cfg {
                state.entries = cfg.pool.clone();
                state.token_file = cfg.t3_token_file.clone();
                state.reserve = cfg.min_weekly_reserve_pct.to_string();
                state.proactive = cfg.proactive_rotate_below_5h_pct.to_string();
                state.sleep_buffer = cfg.quota_sleep_buffer_secs.to_string();
                state.wake_grace = cfg.idle_wake_grace_secs.to_string();
                state.soft_runs = cfg.checkpoint_soft_runs.to_string();
                state.soft_items = cfg.checkpoint_soft_items.to_string();
                state.confidence = cfg.checkpoint_confidence_threshold.to_string();
                state.min_disk = cfg.min_free_disk_gb.to_string();
                state.t3_endpoint = cfg.t3_endpoint.clone().unwrap_or_default();
                state.judge = JudgeProvider::from_stored(cfg.judge_provider.as_deref());
                state.judge_model = cfg.judge_model.clone().unwrap_or_default();
            }
            state.loaded = loaded;
            Task::none()
        }
        CoordMsg::NewInstance(v) => {
            state.new_instance = v;
            Task::none()
        }
        CoordMsg::NewModel(v) => {
            state.new_model = v;
            Task::none()
        }
        CoordMsg::Add => {
            let (instance, model) = (
                state.new_instance.trim().to_string(),
                state.new_model.trim().to_string(),
            );
            // The Add button stays disabled until both fields name a new entry.
            if instance.is_empty()
                || model.is_empty()
                || state
                    .entries
                    .iter()
                    .any(|e| e.instance_id == instance && e.model == model)
            {
                return Task::none();
            }
            state.entries.push(CoordinatorPoolEntry {
                instance_id: instance,
                model,
                options: Default::default(),
                enabled: true,
            });
            state.new_instance.clear();
            state.new_model.clear();
            state.result = None;
            Task::none()
        }
        CoordMsg::Remove(i) => {
            if i < state.entries.len() {
                state.entries.remove(i);
            }
            Task::none()
        }
        CoordMsg::Toggle(i, on) => {
            if let Some(e) = state.entries.get_mut(i) {
                e.enabled = on;
            }
            Task::none()
        }
        CoordMsg::Reserve(v) => {
            state.reserve = v;
            Task::none()
        }
        CoordMsg::Proactive(v) => {
            state.proactive = v;
            Task::none()
        }
        CoordMsg::SleepBuffer(v) => {
            state.sleep_buffer = v;
            Task::none()
        }
        CoordMsg::WakeGrace(v) => {
            state.wake_grace = v;
            Task::none()
        }
        CoordMsg::SoftRuns(v) => {
            state.soft_runs = v;
            Task::none()
        }
        CoordMsg::SoftItems(v) => {
            state.soft_items = v;
            Task::none()
        }
        CoordMsg::Confidence(v) => {
            state.confidence = v;
            Task::none()
        }
        CoordMsg::MinDisk(v) => {
            state.min_disk = v;
            Task::none()
        }
        CoordMsg::T3Endpoint(v) => {
            state.t3_endpoint = v;
            Task::none()
        }
        CoordMsg::Judge(p) => {
            state.judge = p;
            Task::none()
        }
        CoordMsg::JudgeModel(v) => {
            state.judge_model = v;
            Task::none()
        }
        CoordMsg::Save => {
            if state.busy {
                return Task::none();
            }
            match coord_cfg(state) {
                Err(e) => {
                    state.result = Some((e, true));
                    Task::none()
                }
                Ok(cfg) => {
                    state.busy = true;
                    Task::perform(
                        async move {
                            tokio::task::spawn_blocking(move || {
                                settings::write_coordinator(&paths, &cfg, false)
                                    .map(|_| ())
                                    .map_err(|e| format!("{e:#}"))
                            })
                            .await
                            .map_err(|e| e.to_string())
                            .and_then(|r| r)
                        },
                        |r| cm(CoordMsg::Done(r)),
                    )
                }
            }
        }
        CoordMsg::Done(result) => {
            state.busy = false;
            let ok = result.is_ok();
            state.result = Some(match result {
                Ok(()) => (fl!(loader(), "settings-saved"), false),
                Err(e) => (e, true),
            });
            if ok {
                Task::done(load_coord(&paths))
            } else {
                Task::none()
            }
        }
    }
}

fn sub_save_row(busy: bool, msg: Msg) -> Element<'static, Msg> {
    row![
        iced::widget::Space::new().width(Length::Fill),
        st::btn(st::Btn::Primary, fl!(loader(), "settings-save"))
            .on_press_maybe((!busy).then_some(msg)),
    ]
    .align_y(Alignment::Center)
    .into()
}

fn sub_result(result: &Option<(String, bool)>) -> Option<Element<'_, Msg>> {
    result.as_ref().map(|(text, error)| {
        if *error {
            st::body(text.as_str()).into()
        } else {
            st::pill(text.clone(), st::Tone::Ok)
        }
    })
}

/// Broker outcome to its localized pill (unknown values pass through raw).
fn outcome_label(outcome: &str) -> (String, st::Tone) {
    match outcome {
        "approved" => (fl!(loader(), "approver-outcome-approved"), st::Tone::Ok),
        "denied" => (fl!(loader(), "approver-outcome-denied"), st::Tone::Danger),
        "needs_owner" => (
            fl!(loader(), "approver-outcome-needs-owner"),
            st::Tone::Warn,
        ),
        "client_cancelled" => (
            fl!(loader(), "approver-outcome-cancelled"),
            st::Tone::Neutral,
        ),
        other => (other.to_string(), st::Tone::Neutral),
    }
}

/// Broker audit source to its localized pill (unknown values pass through).
fn source_label(source: &str) -> String {
    match source {
        "PolicyJudge" => fl!(loader(), "approver-source-judge"),
        "Telegram" => fl!(loader(), "approver-source-telegram"),
        "Denied" => fl!(loader(), "approver-source-denied"),
        "Timeout" => fl!(loader(), "approver-source-timeout"),
        "ClientCancelled" => fl!(loader(), "approver-source-cancelled"),
        other => other.to_string(),
    }
}

fn audit_card(state: &ApproverState) -> Element<'_, Msg> {
    let mut col = column![
        row![
            st::caption(fl!(loader(), "approver-audit")),
            iced::widget::Space::new().width(Length::Fill),
            st::btn(st::Btn::Quiet, fl!(loader(), "approver-audit-refresh"))
                .on_press(am(ApproverMsg::RefreshAudit)),
        ]
        .align_y(Alignment::Center),
    ]
    .spacing(st::SPACE_S);
    if let Some(error) = &state.loaded.audit_error {
        col = col.push(st::body(fl!(loader(), "approver-audit-error")));
        col = col.push(st::muted(error.as_str()));
    } else if state.loaded.audit.is_empty() {
        col = col.push(st::muted(fl!(loader(), "approver-audit-empty")));
    }
    for row_ in state.loaded.audit.iter().rev().take(20) {
        let (outcome, tone) = outcome_label(&row_.outcome);
        col = col.push(
            column![
                row![
                    st::pill(source_label(&row_.source), st::Tone::Neutral),
                    st::pill(outcome, tone),
                    st::mono(row_.kind.clone()),
                ]
                .spacing(st::SPACE_S)
                .align_y(Alignment::Center),
                st::mono_muted(format!("{} · {}", row_.at, row_.host)),
            ]
            .spacing(2),
        );
    }
    st::card(col)
}

pub fn approver_view(state: &State) -> Element<'_, Msg> {
    let s = &state.approver;
    let size = st::look().body;
    let input = |value: &str, msg: fn(String) -> ApproverMsg| {
        text_input("", value)
            .on_input(move |v| am(msg(v)))
            .on_submit(am(ApproverMsg::Save))
            .size(size)
            .width(Length::Fill)
            .into()
    };
    let (token_placeholder, token_hint) = if s.loaded.creds.bot_token_set {
        (
            fl!(loader(), "approver-bot-token-placeholder"),
            fl!(loader(), "approver-bot-token-set"),
        )
    } else {
        (
            "123456:ABC-...".to_string(),
            fl!(loader(), "approver-bot-token-missing"),
        )
    };
    let form = column![
        field(
            fl!(loader(), "approver-bot-token"),
            text_input(&token_placeholder, &s.bot_token)
                .secure(true)
                .on_input(|v| am(ApproverMsg::BotToken(v)))
                .on_submit(am(ApproverMsg::Save))
                .size(size)
                .width(Length::Fill)
                .into(),
            Some(token_hint),
        ),
        field(
            fl!(loader(), "approver-owner-id"),
            text_input("123456789", &s.owner_id)
                .on_input(|v| am(ApproverMsg::OwnerId(v)))
                .on_submit(am(ApproverMsg::Save))
                .size(size)
                .width(Length::Fill)
                .into(),
            Some(fl!(loader(), "approver-owner-id-hint")),
        ),
        field(
            fl!(loader(), "approver-auto-timeout"),
            input(&s.auto_timeout, ApproverMsg::AutoTimeout),
            None,
        ),
        field(
            fl!(loader(), "approver-interactive-timeout"),
            input(&s.inter_timeout, ApproverMsg::InterTimeout),
            None,
        ),
        field(
            fl!(loader(), "approver-judge-provider"),
            iced::widget::pick_list(
                [
                    JudgeProvider::Default,
                    JudgeProvider::OpenRouter,
                    JudgeProvider::Vercel
                ],
                Some(s.judge),
                |p| am(ApproverMsg::Judge(p)),
            )
            .text_size(size)
            .width(Length::Fill)
            .into(),
            None,
        ),
        field(
            fl!(loader(), "approver-judge-model"),
            input(&s.judge_model, ApproverMsg::JudgeModel),
            Some(fl!(loader(), "approver-judge-hint")),
        ),
        field(
            fl!(loader(), "approver-confidence"),
            input(&s.confidence, ApproverMsg::Confidence),
            None,
        ),
        field(
            fl!(loader(), "approver-remote-hosts"),
            input(&s.remote_hosts, ApproverMsg::RemoteHosts),
            Some(fl!(loader(), "approver-remote-hint")),
        ),
        field(
            fl!(loader(), "approver-socket-path"),
            input(&s.socket_path, ApproverMsg::SocketPath),
            Some(fl!(loader(), "approver-socket-hint")),
        ),
        field(
            fl!(loader(), "approver-deny"),
            input(&s.deny, ApproverMsg::Deny),
            Some(fl!(loader(), "approver-deny-hint")),
        ),
        sub_save_row(s.busy, am(ApproverMsg::Save)),
    ]
    .spacing(st::SPACE_M);
    let mut col = column![].spacing(st::SPACE_S);
    if let Some(e) = &s.loaded.error {
        col = col.push(st::card(st::muted(fl!(loader(), "load-error", what = e))));
    }
    if let Some(e) = &s.loaded.creds.error {
        col = col.push(st::card(st::muted(fl!(loader(), "load-error", what = e))));
    }
    col = col.push(st::card(form));
    if let Some(result) = sub_result(&s.result) {
        col = col.push(result);
    }
    col.push(audit_card(s)).into()
}

fn pool_row(entry: &CoordinatorPoolEntry, index: usize) -> Element<'_, Msg> {
    row![
        column![
            st::fit(entry.model.as_str(), st::Role::Body, st::Cut::End),
            st::mono_muted(entry.instance_id.as_str()),
        ]
        .spacing(2)
        .width(Length::Fill),
        iced::widget::toggler(entry.enabled)
            .size(14)
            .on_toggle(move |on| cm(CoordMsg::Toggle(index, on))),
        st::btn(st::Btn::Quiet, fl!(loader(), "coord-pool-remove"))
            .on_press(cm(CoordMsg::Remove(index))),
    ]
    .spacing(st::SPACE_S)
    .align_y(Alignment::Center)
    .into()
}

pub fn coord_view(state: &State) -> Element<'_, Msg> {
    let s = &state.coord;
    let size = st::look().body;
    let input = |value: &str, msg: fn(String) -> CoordMsg| {
        text_input("", value)
            .on_input(move |v| cm(msg(v)))
            .on_submit(cm(CoordMsg::Save))
            .size(size)
            .width(Length::Fill)
            .into()
    };
    let mut pool = column![st::caption(fl!(loader(), "coord-pool"))].spacing(4);
    if s.entries.is_empty() {
        pool = pool.push(st::muted(fl!(loader(), "coord-pool-empty")));
    }
    for (i, entry) in s.entries.iter().enumerate() {
        pool = pool.push(pool_row(entry, i));
    }
    pool = pool.push(
        row![
            pick_list(
                s.loaded.instances.clone(),
                (!s.new_instance.is_empty()).then(|| s.new_instance.clone()),
                |v| cm(CoordMsg::NewInstance(v)),
            )
            .placeholder(fl!(loader(), "coord-pool-instance"))
            .text_size(size)
            .width(Length::Fill),
            text_input(&fl!(loader(), "coord-pool-model"), &s.new_model)
                .on_input(|v| cm(CoordMsg::NewModel(v)))
                .on_submit(cm(CoordMsg::Add))
                .size(size)
                .width(Length::Fill),
            st::btn(st::Btn::Quiet, fl!(loader(), "coord-pool-add")).on_press_maybe(
                (!s.new_instance.trim().is_empty()
                    && !s.new_model.trim().is_empty()
                    && !s
                        .entries
                        .iter()
                        .any(|e| e.instance_id == s.new_instance.trim()
                            && e.model == s.new_model.trim()))
                .then_some(cm(CoordMsg::Add))
            ),
        ]
        .spacing(st::SPACE_S)
        .align_y(Alignment::Center),
    );
    let form = column![
        field(
            fl!(loader(), "coord-reserve"),
            input(&s.reserve, CoordMsg::Reserve),
            None
        ),
        field(
            fl!(loader(), "coord-proactive"),
            input(&s.proactive, CoordMsg::Proactive),
            None
        ),
        field(
            fl!(loader(), "coord-sleep-buffer"),
            input(&s.sleep_buffer, CoordMsg::SleepBuffer),
            None,
        ),
        field(
            fl!(loader(), "coord-wake-grace"),
            input(&s.wake_grace, CoordMsg::WakeGrace),
            None,
        ),
        field(
            fl!(loader(), "coord-soft-runs"),
            input(&s.soft_runs, CoordMsg::SoftRuns),
            None,
        ),
        field(
            fl!(loader(), "coord-soft-items"),
            input(&s.soft_items, CoordMsg::SoftItems),
            None,
        ),
        field(
            fl!(loader(), "coord-confidence"),
            input(&s.confidence, CoordMsg::Confidence),
            None,
        ),
        field(
            fl!(loader(), "coord-min-disk"),
            input(&s.min_disk, CoordMsg::MinDisk),
            None,
        ),
        field(
            fl!(loader(), "coord-t3-endpoint"),
            input(&s.t3_endpoint, CoordMsg::T3Endpoint),
            Some(fl!(loader(), "coord-t3-hint")),
        ),
        field(
            fl!(loader(), "approver-judge-provider"),
            iced::widget::pick_list(
                [
                    JudgeProvider::Default,
                    JudgeProvider::OpenRouter,
                    JudgeProvider::Vercel
                ],
                Some(s.judge),
                |p| cm(CoordMsg::Judge(p)),
            )
            .text_size(size)
            .width(Length::Fill)
            .into(),
            None,
        ),
        field(
            fl!(loader(), "approver-judge-model"),
            input(&s.judge_model, CoordMsg::JudgeModel),
            Some(fl!(loader(), "approver-judge-hint")),
        ),
        sub_save_row(s.busy, cm(CoordMsg::Save)),
    ]
    .spacing(st::SPACE_M);
    let mut col = column![].spacing(st::SPACE_S);
    if let Some(e) = &s.loaded.error {
        col = col.push(st::card(st::muted(fl!(loader(), "load-error", what = e))));
    }
    col = col.push(st::card(pool));
    col = col.push(st::card(form));
    if let Some(result) = sub_result(&s.result) {
        col = col.push(result);
    }
    col.into()
}

#[cfg(test)]
mod tests {
    use super::{civil_ymd, fmt_ts, format_patterns, patterns, seconds};
    use agentdeck_core::settings::{UserRuleSource, UserRules};

    #[test]
    fn blank_is_default_and_only_positive_numbers_pass() {
        assert_eq!(seconds("  "), Ok(None));
        assert_eq!(seconds(" 30 "), Ok(Some(30)));
        for bad in ["0", "-5", "1.5", "abc"] {
            assert!(seconds(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn commas_split_and_blanks_drop_for_an_explicit_empty_list() {
        assert_eq!(patterns(""), Vec::<String>::new());
        assert_eq!(patterns(" , "), Vec::<String>::new());
        assert_eq!(patterns("a, b ,,c "), vec!["a", "b", "c"]);
        let original = vec![r"model-{1,3}\d".to_string(), r"x\,y".to_string()];
        assert_eq!(patterns(&format_patterns(&original)), original);
    }

    #[test]
    fn epoch_seconds_render_as_utc_dates() {
        assert_eq!(civil_ymd(0), (1970, 1, 1));
        assert_eq!(fmt_ts(0), "1970-01-01 00:00:00");
        assert_eq!(fmt_ts(123), "1970-01-01 00:02:03");
        assert_eq!(fmt_ts(1_791_234_567), "2026-10-05 21:09:27");
    }

    #[test]
    fn parsed_fields_validate_like_stored_rules() {
        let rules = UserRules {
            no_review_models: patterns("gemini-.*argon"),
            deny_models: patterns(""),
            source: UserRuleSource::User,
        };
        assert!(rules.validate().is_ok());
        let bad = UserRules {
            no_review_models: patterns("(["),
            deny_models: Vec::new(),
            source: UserRuleSource::User,
        };
        assert!(bad.validate().is_err());
    }

    #[test]
    fn fallback_probe_never_reuses_draft_key_from_another_provider() {
        use super::{FallbackProvider, Loaded, Msg, Provider, State, fallback_probe_key, update};
        use agentdeck_core::Paths;
        use agentdeck_core::settings::{JudgeFallbackSettings, PROVIDER_OPENROUTER, Settings};

        let paths = Paths::new(
            "/tmp/agentdeck-gui-test-cfg".into(),
            "/tmp/agentdeck-gui-test-state".into(),
        );
        let mut state = State::default();
        assert_eq!(state.provider, Provider::OpenRouter);

        let _ = update(&mut state, paths.clone(), Msg::Key("sk-or-secret".into()));
        let _ = update(
            &mut state,
            paths.clone(),
            Msg::FallbackProvider(FallbackProvider::Vercel),
        );
        assert_eq!(fallback_probe_key(&state), None);

        // Switching primary provider to Vercel must not let the OpenRouter draft key
        // be reused by the Vercel fallback probe.
        let _ = update(&mut state, paths.clone(), Msg::Provider(Provider::Vercel));
        assert_eq!(
            fallback_probe_key(&state),
            None,
            "OpenRouter draft key must not be sent to Vercel fallback after provider switch"
        );
        assert!(state.key.is_empty(), "provider switch must clear draft key");

        // A draft key typed after switching to Vercel applies only to Vercel.
        let _ = update(
            &mut state,
            paths.clone(),
            Msg::Key("sk-vercel-draft".into()),
        );
        assert_eq!(
            fallback_probe_key(&state).as_deref(),
            Some("sk-vercel-draft")
        );
        let _ = update(
            &mut state,
            paths.clone(),
            Msg::FallbackProvider(FallbackProvider::OpenRouter),
        );
        assert_eq!(fallback_probe_key(&state), None);

        // Reloading settings that switch primary back to OpenRouter must also clear
        // the Vercel draft key so OpenRouter fallback cannot receive it.
        let _ = update(
            &mut state,
            paths,
            Msg::Loaded(Box::new(Loaded {
                settings: Some(Settings {
                    decision_provider: None,
                    decision_model: None,
                    refresh_seconds: None,
                    stale_seconds: None,
                }),
                fallback: JudgeFallbackSettings {
                    provider: Some(PROVIDER_OPENROUTER.into()),
                    model: None,
                },
                ..Loaded::default()
            })),
        );
        assert_eq!(state.provider, Provider::OpenRouter);
        assert_eq!(state.fallback_provider, FallbackProvider::OpenRouter);
        assert_eq!(
            fallback_probe_key(&state),
            None,
            "Vercel draft key must not leak to OpenRouter fallback after reload"
        );
    }
}
