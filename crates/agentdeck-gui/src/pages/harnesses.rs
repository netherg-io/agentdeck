//! Harnesses page: detected version and install method per harness, with
//! Update / Install behind a confirmation that shows the exact official
//! command. Detection and the command itself run off the UI thread; the
//! outcome is plain result text.

use agentdeck_core::harness::{self, HarnessName, Status};
use i18n_embed_fl::fl;
use iced::widget::{column, container, row};
use iced::{Alignment, Element, Length, Task};

use crate::i18n::loader;
use crate::style as st;

#[derive(Debug, Clone)]
pub struct Pending {
    pub name: String,
    pub argv: Vec<String>,
    /// The exact command line shown in the panel.
    pub lines: Vec<String>,
}

#[derive(Default)]
pub struct State {
    pub rows: Vec<Status>,
    pub pending: Option<Pending>,
    pub busy: bool,
    /// Last outcome (text, is error); the shell turns it into a toast.
    pub result: Option<(String, bool)>,
}

#[derive(Debug, Clone)]
pub enum Msg {
    Loaded(Vec<Status>),
    Ask { name: String, install: bool },
    Confirm,
    Cancel,
    Done(String, bool),
}

/// Runs `<bin> --version` per harness: call off the UI thread.
pub async fn load() -> Msg {
    Msg::Loaded(
        tokio::task::spawn_blocking(|| harness::list(&harness::real_runner))
            .await
            .unwrap_or_default(),
    )
}

pub fn update(state: &mut State, msg: Msg) -> Task<Msg> {
    match msg {
        Msg::Loaded(rows) => {
            state.rows = rows;
            Task::none()
        }
        Msg::Ask { name, install } => {
            state.result = None;
            let Ok(h) = HarnessName::parse(&name) else {
                return Task::none();
            };
            let binary = state
                .rows
                .iter()
                .find(|r| r.name == name)
                .and_then(|r| r.binary.as_ref())
                .map_or_else(|| name.clone(), |b| b.display().to_string());
            let argv = if install {
                h.install_argv()
            } else {
                h.update_argv(&binary)
            };
            state.pending = Some(Pending {
                name,
                lines: vec![format!("+ {}", argv.join(" "))],
                argv,
            });
            Task::none()
        }
        Msg::Confirm => match state.pending.take() {
            Some(p) => {
                state.busy = true;
                Task::perform(
                    async move {
                        let name = p.name.clone();
                        let out = tokio::task::spawn_blocking(move || {
                            harness::real_runner(&p.argv[0], &p.argv[1..])
                        })
                        .await;
                        let (what, error) = match out {
                            Ok(Ok(text)) => (last_lines(&text), false),
                            Ok(Err(e)) => (format!("{e:#}"), true),
                            Err(e) => (e.to_string(), true),
                        };
                        Msg::Done(
                            fl!(loader(), "harness-result", name = name, what = what),
                            error,
                        )
                    },
                    |m| m,
                )
            }
            None => Task::none(),
        },
        Msg::Cancel => {
            state.pending = None;
            Task::none()
        }
        Msg::Done(text, error) => {
            state.busy = false;
            state.result = Some((text, error));
            Task::perform(load(), |m| m)
        }
    }
}

/// Installers are chatty: keep the last few lines.
fn last_lines(text: &str) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(4)..].join("\n")
}

pub fn modal(state: &State) -> Option<crate::overlay::Modal<'_, Msg>> {
    state.pending.as_ref().map(|p| {
        crate::overlay::confirm(
            fl!(loader(), "harness-run-title"),
            &p.lines,
            (fl!(loader(), "action-apply"), Some(Msg::Confirm)),
            (fl!(loader(), "action-cancel"), Msg::Cancel),
        )
    })
}

pub fn view(state: &State) -> Element<'_, Msg> {
    let look = st::look();
    let mut col = column![].spacing(look.gap.max(st::SPACE_S));
    if state.busy {
        col = col.push(st::muted(fl!(loader(), "action-working")));
    }
    if state.rows.is_empty() {
        return col
            .push(st::card(st::muted(fl!(loader(), "loading"))))
            .into();
    }
    let mut list = column![].spacing(look.row_gap);
    for (i, r) in state.rows.iter().enumerate() {
        if i > 0 {
            list = list.push(st::hairline());
        }
        let installed = r.binary.is_some();
        let (label, icon, install) = if installed {
            (fl!(loader(), "harness-update"), st::Icon::RefreshCw, false)
        } else {
            (fl!(loader(), "harness-install"), st::Icon::Download, true)
        };
        let version: Element<'_, Msg> = match (&r.version, installed) {
            (Some(v), _) => st::fit(v.as_str(), st::Role::Mono, st::Cut::End).into(),
            (None, true) => st::mono_muted("—").into(),
            (None, false) => st::pill(fl!(loader(), "harness-not-installed"), st::Tone::Neutral),
        };
        list = list.push(
            row![
                st::fit(r.name.as_str(), st::Role::Heading, st::Cut::End).width(90),
                container(version).width(Length::FillPortion(2)),
                st::fit(r.install_method.as_str(), st::Role::Muted, st::Cut::End)
                    .width(Length::FillPortion(3)),
                st::btn_icon(st::Btn::Quiet, icon, label).on_press_maybe((!state.busy).then(
                    || Msg::Ask {
                        name: r.name.clone(),
                        install,
                    }
                )),
            ]
            .spacing(st::SPACE_M)
            .align_y(Alignment::Center),
        );
    }
    col.push(st::card(list)).into()
}
