//! Coordinator page: each coordinator thread's goal and checklist, plus the
//! open backlog per project. Shows the Autonomous Run phase, the Run Mandate
//! with the owner's Grant/Revoke button, the Owner Inbox with Resolve
//! buttons, and recorded Assumptions.

use agentdeck_core::backlog::Entry;
use agentdeck_core::thread_state::{Item, Report, RunMandate, RunPhase, ThreadState};
use i18n_embed_fl::fl;
use iced::widget::{column, container, row};
use iced::{Alignment, Element, Length};

use crate::app::Message;
use crate::data::{CoordRow, Data};
use crate::i18n::loader;
use crate::style as st;

/// Fluent wraps placeables in bidi isolates; strip them so exact-text
/// matching (headless tests, assistive readers of the plain string) works.
fn plain(s: String) -> String {
    s.chars()
        .filter(|c| !matches!(c, '\u{2068}' | '\u{2069}'))
        .collect()
}

fn status_key(status: &str) -> (&'static str, st::Tone) {
    match status {
        "done" => ("coord-status-done", st::Tone::Ok),
        "running" => ("coord-status-running", st::Tone::Accent),
        "review" => ("coord-status-review", st::Tone::Warn),
        "blocked" => ("coord-status-blocked", st::Tone::Danger),
        "held" => ("coord-status-held", st::Tone::Neutral),
        _ => ("coord-status-todo", st::Tone::Neutral),
    }
}

fn status_pill<'a>(status: &str) -> Element<'a, Message> {
    let (key, tone) = status_key(status);
    st::pill(loader().get(key), tone)
}

fn phase_pill<'a>(state: &ThreadState) -> Element<'a, Message> {
    let (label, tone) = match state.phase {
        RunPhase::Intake => (fl!(loader(), "coord-phase-intake"), st::Tone::Neutral),
        RunPhase::Running => (fl!(loader(), "coord-phase-running"), st::Tone::Accent),
        RunPhase::SleepingForQuota => (
            plain(fl!(
                loader(),
                "coord-phase-sleeping",
                until = state.sleep_until.as_deref().unwrap_or("—")
            )),
            st::Tone::Warn,
        ),
        RunPhase::Completed => (fl!(loader(), "coord-phase-completed"), st::Tone::Ok),
    };
    st::pill(label, tone)
}

fn reports_card(reports: &[Report]) -> Element<'_, Message> {
    let mut col = column![st::heading(fl!(loader(), "coord-reports"))].spacing(st::SPACE_S);
    for r in reports {
        let changes: Vec<String> = r
            .changes
            .iter()
            .map(|c| format!("{} — {}", c.title, loader().get(status_key(&c.to).0)))
            .collect();
        col = col.push(
            column![
                row![
                    container(st::fit(r.goal.as_str(), st::Role::Body, st::Cut::End))
                        .width(Length::Fill),
                    st::nowrap(st::caption(format!("{}/{}", r.done, r.total))),
                    st::nowrap(st::caption(stamp(&r.at))),
                ]
                .spacing(st::SPACE_S)
                .align_y(Alignment::Center),
                st::fit(changes.join("; "), st::Role::Muted, st::Cut::End),
            ]
            .spacing(2),
        );
    }
    st::card(col)
}

/// `2026-11-16T09:05:00Z` as `2026-11-16 09:05 UTC`.
fn stamp(s: &str) -> String {
    match (s.get(..10), s.get(11..16)) {
        (Some(d), Some(t)) => format!("{d} {t} UTC"),
        _ => s.to_string(),
    }
}

fn item_row(i: &Item) -> Element<'_, Message> {
    let mut text = column![st::fit(i.title.as_str(), st::Role::Body, st::Cut::End)].spacing(2);
    if !i.note.is_empty() {
        text = text.push(st::fit(i.note.as_str(), st::Role::Muted, st::Cut::End));
    }
    if !i.links.is_empty() {
        text = text.push(st::fit(
            i.links.join("  "),
            st::Role::MonoMuted,
            st::Cut::End,
        ));
    }
    row![
        container(status_pill(&i.status)).width(96),
        text.width(Length::Fill)
    ]
    .spacing(st::SPACE_S)
    .align_y(Alignment::Start)
    .into()
}

fn list_or_any(items: &[String]) -> String {
    if items.is_empty() {
        loader().get("coord-mandate-any")
    } else {
        items.join(", ")
    }
}

/// Structured Run Mandate summary plus the owner's Grant/Revoke button. The
/// button carries the exact displayed scope: Grant seals it only when the
/// on-disk mandate still matches, Revoke clears just `owner_granted`.
fn mandate_block<'a>(
    phase: &RunPhase,
    thread: &'a str,
    mandate: Option<&'a RunMandate>,
) -> Element<'a, Message> {
    let mut col = column![st::caption(fl!(loader(), "coord-mandate"))].spacing(4);
    let Some(m) = mandate else {
        // No scope to show, and nothing safe to grant.
        col = col.push(st::muted(fl!(loader(), "coord-mandate-none")));
        return col.into();
    };
    let (state, tone) = if m.owner_granted {
        (fl!(loader(), "coord-mandate-granted"), st::Tone::Ok)
    } else {
        (fl!(loader(), "coord-mandate-pending"), st::Tone::Warn)
    };
    let mut status = row![
        st::pill(state, tone),
        iced::widget::Space::new().width(Length::Fill),
    ]
    .spacing(st::SPACE_S)
    .align_y(Alignment::Center);
    if m.owner_granted {
        status = status.push(
            st::btn(st::Btn::Quiet, fl!(loader(), "coord-revoke")).on_press(
                Message::CoordMandate {
                    thread: thread.to_string(),
                    expected: m.clone(),
                    grant: false,
                },
            ),
        );
    } else if !matches!(phase, RunPhase::Completed) {
        status = status.push(
            st::btn(st::Btn::Quiet, fl!(loader(), "coord-grant")).on_press(
                Message::CoordMandate {
                    thread: thread.to_string(),
                    expected: m.clone(),
                    grant: true,
                },
            ),
        );
    }
    col = col.push(status);
    for (label, value) in [
        (
            fl!(loader(), "coord-mandate-project"),
            m.project.clone(),
        ),
        (
            fl!(loader(), "coord-mandate-hosts"),
            list_or_any(&m.allowed_hosts),
        ),
        (
            fl!(loader(), "coord-mandate-apps"),
            list_or_any(&m.allowed_apps),
        ),
        (
            fl!(loader(), "coord-mandate-releases"),
            if m.allow_releases {
                loader().get("coord-mandate-releases-allowed")
            } else {
                loader().get("coord-mandate-releases-blocked")
            },
        ),
    ] {
        col = col.push(
            row![
                container(st::caption(label)).width(96),
                st::fit(value, st::Role::Body, st::Cut::End),
            ]
            .spacing(st::SPACE_S)
            .align_y(Alignment::Center),
        );
    }
    col = col.push(st::mono_muted(plain(fl!(
        loader(),
        "coord-mandate-caps",
        fix = m.fix_forward_max,
        rev = m.review_rounds_max
    ))));
    if !m.notes.is_empty() {
        col = col.push(
            row![
                container(st::caption(fl!(loader(), "coord-mandate-notes"))).width(96),
                st::fit(m.notes.clone(), st::Role::Body, st::Cut::End),
            ]
            .spacing(st::SPACE_S)
            .align_y(Alignment::Center),
        );
    }
    col.into()
}

/// Open Owner Inbox items with a Resolve button each; resolved ones stay out.
fn inbox_block<'a>(thread: &'a str, state: &'a ThreadState) -> Option<Element<'a, Message>> {
    let open: Vec<_> = state.owner_inbox.iter().filter(|i| !i.resolved).collect();
    if open.is_empty() {
        return None;
    }
    let mut col = column![st::caption(fl!(loader(), "coord-inbox"))].spacing(4);
    for item in open {
        let mut text = column![st::fit(item.title.as_str(), st::Role::Body, st::Cut::End)].spacing(2);
        if !item.reason.is_empty() {
            text = text.push(st::fit(item.reason.as_str(), st::Role::Muted, st::Cut::End));
        }
        if !item.action.is_empty() {
            text = text.push(st::fit(item.action.as_str(), st::Role::Muted, st::Cut::End));
        }
        if !item.blocked_items.is_empty() {
            text = text.push(st::mono_muted(plain(fl!(
                loader(),
                "coord-inbox-blocks",
                items = item.blocked_items.join(", ")
            ))));
        }
        col = col.push(
            row![
                text.width(Length::Fill),
                st::btn(st::Btn::Quiet, fl!(loader(), "coord-resolve")).on_press(
                    Message::CoordResolve {
                        thread: thread.to_string(),
                        item: item.id.clone(),
                    }
                ),
            ]
            .spacing(st::SPACE_S)
            .align_y(Alignment::Center),
        );
    }
    Some(col.into())
}

fn assumptions_block(state: &ThreadState) -> Option<Element<'_, Message>> {
    if state.assumptions.is_empty() {
        return None;
    }
    let mut col = column![st::caption(fl!(loader(), "coord-assumptions"))].spacing(4);
    for a in &state.assumptions {
        let mut text =
            column![st::fit(a.decision.as_str(), st::Role::Body, st::Cut::End)].spacing(2);
        if !a.rationale.is_empty() {
            text = text.push(st::fit(a.rationale.as_str(), st::Role::Muted, st::Cut::End));
        }
        if !a.revert_hint.is_empty() {
            text = text.push(st::mono_muted(plain(fl!(
                loader(),
                "coord-assumption-revert",
                hint = a.revert_hint.clone()
            ))));
        }
        col = col.push(text);
    }
    Some(col.into())
}

fn thread_card(c: &CoordRow) -> Element<'_, Message> {
    let (done, total) = c.state.progress();
    let name = c.title.clone().unwrap_or_else(|| c.thread.clone());
    // Row 1 never clips: title takes the free width, progress and timestamp
    // keep fixed slots at the right.
    let mut head =
        row![container(st::fit(name, st::Role::Heading, st::Cut::End)).width(Length::Fill)]
            .spacing(st::SPACE_S)
            .align_y(Alignment::Center);
    if total > 0 {
        head = head.push(st::pill(
            format!("{done}/{total}"),
            if done == total {
                st::Tone::Ok
            } else {
                st::Tone::Accent
            },
        ));
    }
    head = head.push(st::nowrap(st::caption(stamp(&c.state.updated))));
    // Row 2 is the wrapping status strip: phase, inbox count and the stale
    // badge fold onto the next line instead of pushing each other off-screen.
    let mut strip = row![phase_pill(&c.state)]
        .spacing(st::SPACE_S)
        .align_y(Alignment::Center);
    let open = c.state.open_inbox_count();
    if open > 0 {
        strip = strip.push(st::pill(
            plain(fl!(loader(), "coord-inbox-badge", count = open)),
            st::Tone::Warn,
        ));
    }
    if c.state.state_stale() {
        strip = strip.push(st::pill(fl!(loader(), "coord-stale"), st::Tone::Danger));
    }
    let mut col = column![head, strip.wrap()].spacing(st::SPACE_S);
    if let Some(goal) = c.state.state.lines().next().filter(|g| !g.is_empty()) {
        col = col.push(st::muted(goal));
    }
    if total > 0 {
        col = col.push(st::progress(done as f32 * 100.0 / total as f32).length(Length::Fill));
    }
    col = col.push(mandate_block(&c.state.phase, &c.thread, c.state.mandate.as_ref()));
    if let Some(inbox) = inbox_block(&c.thread, &c.state) {
        col = col.push(inbox);
    }
    if let Some(assumptions) = assumptions_block(&c.state) {
        col = col.push(assumptions);
    }
    if c.state.checklist.is_empty() {
        col = col.push(st::caption(fl!(loader(), "coord-no-checklist")));
    }
    for i in &c.state.checklist {
        col = col.push(item_row(i));
    }
    st::card(col)
}

fn backlog_card<'a>(project: &'a str, items: &'a [Entry]) -> Element<'a, Message> {
    // Fluent's bidi isolates would break exact-text matching.
    let heading: String = fl!(
        loader(),
        "coord-backlog",
        project = project,
        count = items.len()
    )
    .chars()
    .filter(|c| !matches!(c, '\u{2068}' | '\u{2069}'))
    .collect();
    let mut col = column![st::heading(heading)].spacing(4);
    for e in items {
        let mut line = row![
            st::nowrap(st::mono_muted(format!("#{}", e.id))).width(48),
            container(st::fit(e.title.as_str(), st::Role::Body, st::Cut::End)).width(Length::Fill),
        ]
        .spacing(st::SPACE_S)
        .align_y(Alignment::Center);
        for l in &e.labels {
            line = line.push(st::pill(l.as_str(), st::Tone::Neutral));
        }
        col = col.push(line);
    }
    st::card(col)
}

pub fn view(data: &Data) -> Element<'_, Message> {
    let look = st::look();
    if data.coordinators.is_empty() && data.backlogs.is_empty() && data.reports.is_empty() {
        return st::card(st::muted(fl!(loader(), "coord-empty")));
    }
    let mut col = column![].spacing(look.gap.max(st::SPACE_S));
    for c in &data.coordinators {
        col = col.push(thread_card(c));
    }
    if !data.reports.is_empty() {
        col = col.push(reports_card(&data.reports));
    }
    for (project, items) in &data.backlogs {
        col = col.push(backlog_card(project, items));
    }
    col.into()
}
