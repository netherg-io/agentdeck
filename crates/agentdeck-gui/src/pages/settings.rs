//! Module settings forms: the router's decision provider, model and key,
//! and the quota polling interval / staleness threshold.

use std::path::PathBuf;

use agentdeck_core::settings::{self, Settings, UserRuleSource, UserRules};
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
}

#[derive(Debug, Clone)]
pub enum Msg {
    Loaded(Loaded),
    Provider(Provider),
    DecisionModel(String),
    Key(String),
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

    fn key_file(self) -> PathBuf {
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
    Msg::Loaded(match Registry::load(paths) {
        Ok(reg) => {
            // A broken `[router.rules]` table keeps its error visible instead
            // of silently falling back; the fields still edit from defaults.
            let (rules, error) = match UserRules::from_registry(&reg) {
                Ok(rules) => (rules, None),
                Err(e) => (UserRules::default(), Some(format!("{e:#}"))),
            };
            Loaded {
                settings: Some(settings::read(&reg)),
                rules,
                key_set,
                error,
                history: eval_history::load(),
            }
        }
        Err(e) => Loaded {
            settings: None,
            rules: UserRules::default(),
            key_set,
            error: Some(format!("{e:#}")),
            history: eval_history::load(),
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

pub fn update(state: &mut State, paths: Paths, msg: Msg) -> Task<Msg> {
    match msg {
        Msg::Loaded(loaded) => {
            if let Some(s) = &loaded.settings {
                state.provider =
                    if s.decision_provider.as_deref() == Some(settings::PROVIDER_VERCEL) {
                        Provider::Vercel
                    } else {
                        Provider::OpenRouter
                    };
                state.decision_model = s.decision_model.clone().unwrap_or_default();
                state.refresh = s.refresh_seconds.map(|n| n.to_string()).unwrap_or_default();
                state.stale = s.stale_seconds.map(|n| n.to_string()).unwrap_or_default();
            }
            state.no_review = format_patterns(&loaded.rules.no_review_models);
            state.deny = format_patterns(&loaded.rules.deny_models);
            state.rules_user = loaded.rules.source == UserRuleSource::User;
            state.loaded = loaded;
            Task::none()
        }
        Msg::Provider(p) => {
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
            Task::perform(
                async move {
                    let started = std::time::Instant::now();
                    let wire = (provider == Provider::Vercel).then_some(settings::PROVIDER_VERCEL);
                    agentdeck_router::route::probe_decision(wire, &model, key.as_deref())
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
                },
                Msg::Tested,
            )
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
fn test_line(state: &State) -> Element<'_, Msg> {
    let line: Element<'_, Msg> = match &state.test {
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
            test_line(state),
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
        .push(rules_card(state))
        .push(history_card(state))
        .spacing(st::SPACE_S)
        .into()
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

/// One assessment row: source and verdict pills plus the model; the muted line
/// carries task id, category, timestamp, evidence count/kind, self-check and
/// cost. Verdicts are shown, never scored or compared.
fn entry_row(entry: &eval_history::Entry) -> Element<'_, Msg> {
    let tone = match entry.verdict.as_str() {
        "ok" | "pass" => st::Tone::Ok,
        "partial" => st::Tone::Warn,
        "fail" => st::Tone::Danger,
        _ => st::Tone::Neutral,
    };
    let self_line = match entry.self_judged {
        Some(true) => fl!(loader(), "settings-eval-self"),
        _ => fl!(loader(), "settings-eval-self-unknown"),
    };
    let cost = match entry.cost {
        Some(c) => fl!(loader(), "settings-eval-cost", cost = format!("{c:.6}")),
        None => fl!(loader(), "settings-eval-cost-unknown"),
    };
    let evidence = if entry.source == eval_history::Source::Decider {
        fl!(
            loader(),
            "settings-eval-evidence-ids",
            count = entry.evidence_count.to_string()
        )
    } else {
        entry.evidence.clone()
    };
    let mut detail = format!(
        "{} · {} · t={} · {evidence} · {self_line} · {cost}",
        entry.task_id, entry.category, entry.t,
    );
    if let Some(judge) = &entry.judge {
        detail.push_str(&format!(
            " · {}",
            fl!(loader(), "settings-eval-judge", who = judge.clone())
        ));
    }
    column![
        row![
            st::pill(entry.source.as_str(), st::Tone::Neutral),
            st::pill(entry.verdict.as_str(), tone),
            st::mono(entry.model.clone()),
        ]
        .spacing(st::SPACE_S)
        .align_y(Alignment::Center),
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

#[cfg(test)]
mod tests {
    use super::{format_patterns, patterns, seconds};
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
}
