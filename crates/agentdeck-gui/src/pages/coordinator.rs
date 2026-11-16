//! Coordinator page: each coordinator thread's goal and checklist, then the
//! open backlog per project. Read-only; coordinators write through MCP.

use agentdeck_core::backlog::Entry;
use agentdeck_core::thread_state::Item;
use i18n_embed_fl::fl;
use iced::widget::{column, container, row};
use iced::{Alignment, Element, Length};

use crate::app::Message;
use crate::data::{CoordRow, Data};
use crate::i18n::loader;
use crate::style as st;

fn status_pill<'a>(status: &str) -> Element<'a, Message> {
    let (key, tone) = match status {
        "done" => ("coord-status-done", st::Tone::Ok),
        "running" => ("coord-status-running", st::Tone::Accent),
        "review" => ("coord-status-review", st::Tone::Warn),
        "blocked" => ("coord-status-blocked", st::Tone::Danger),
        "held" => ("coord-status-held", st::Tone::Neutral),
        _ => ("coord-status-todo", st::Tone::Neutral),
    };
    st::pill(loader().get(key), tone)
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

fn thread_card(c: &CoordRow) -> Element<'_, Message> {
    let (done, total) = c.state.progress();
    let name = c.title.clone().unwrap_or_else(|| c.thread.clone());
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
    let mut col = column![head].spacing(st::SPACE_S);
    if let Some(goal) = c.state.state.lines().next().filter(|g| !g.is_empty()) {
        col = col.push(st::muted(goal));
    }
    if total > 0 {
        col = col.push(st::progress(done as f32 * 100.0 / total as f32).length(Length::Fill));
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
    if data.coordinators.is_empty() && data.backlogs.is_empty() {
        return st::card(st::muted(fl!(loader(), "coord-empty")));
    }
    let mut col = column![].spacing(look.gap.max(st::SPACE_S));
    for c in &data.coordinators {
        col = col.push(thread_card(c));
    }
    for (project, items) in &data.backlogs {
        col = col.push(backlog_card(project, items));
    }
    col.into()
}
