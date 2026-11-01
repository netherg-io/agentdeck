//! Processes page: stoppable agent-process groups only (T3 sessions and
//! scopes are not shown), collapsed by default, sorted by CPU then RSS,
//! with a text filter and an owner-approved stop behind a modal that lists
//! every process.

use std::collections::HashSet;

use agentdeck_core::processes::{ProcInfo, SessionGroup, StopOutcome};
use i18n_embed_fl::fl;
use iced::widget::{button, checkbox, column, container, row, text_input};
use iced::{Alignment, Element, Length};

use crate::app::Message;
use crate::data::Data;
use crate::i18n::loader;
use crate::overlay::Modal;
use crate::style as st;

#[derive(Debug, Clone, Default)]
pub struct PendingStop {
    pub group: usize,
    pub title: String,
    pub members: Vec<ProcInfo>,
}

#[derive(Debug, Clone, Default)]
pub struct ProcUi {
    pub pending: Option<PendingStop>,
    pub force: bool,
    pub filter: String,
    /// Expanded groups by (leader pid, start ticks): indices shift on refresh.
    pub expanded: HashSet<(u32, u64)>,
}

fn mega(bytes: u64) -> String {
    format!("{:.0}M", bytes as f64 / 1_048_576.0)
}

fn kind_label(g: &SessionGroup) -> String {
    match &g.kind {
        agentdeck_core::processes::GroupKind::Agent { .. } => {
            fl!(loader(), "processes-kind-agent")
        }
        agentdeck_core::processes::GroupKind::T3 { .. } => fl!(loader(), "processes-kind-t3"),
        agentdeck_core::processes::GroupKind::Harness { .. } => {
            fl!(loader(), "processes-kind-harness")
        }
        agentdeck_core::processes::GroupKind::Container { .. } => {
            fl!(loader(), "processes-kind-container")
        }
    }
}

pub fn describe_outcome(title: &str, outcome: &StopOutcome) -> String {
    let what = match outcome {
        StopOutcome::Terminated => fl!(loader(), "processes-stop-terminated"),
        StopOutcome::Killed => fl!(loader(), "processes-stop-killed"),
        StopOutcome::AlreadyExited => fl!(loader(), "processes-stop-exited"),
        StopOutcome::StillRunning => fl!(loader(), "processes-stop-running"),
    };
    fl!(
        loader(),
        "processes-stop-result",
        title = title,
        what = what
    )
}

/// Wrapper command lines run to pages of shell; one line is enough to recognize one.
fn short_cmd(cmd: &str) -> String {
    let line = cmd.lines().next().unwrap_or_default();
    match line.char_indices().nth(90) {
        Some((end, _)) => format!("{}…", &line[..end]),
        None => line.to_string(),
    }
}

/// One visible member line; identical commands fold into one with a count.
#[derive(Debug, PartialEq)]
struct Line {
    pid: Option<u32>,
    cpu: f64,
    rss: u64,
    cmd: String,
}

fn member_lines(procs: &[ProcInfo]) -> Vec<Line> {
    let mut groups: Vec<(String, Vec<&ProcInfo>)> = Vec::new();
    for p in procs {
        let cmd = short_cmd(&p.display_cmd());
        match groups.iter_mut().find(|(c, _)| *c == cmd) {
            Some((_, list)) => list.push(p),
            None => groups.push((cmd, vec![p])),
        }
    }
    groups
        .into_iter()
        .map(|(cmd, list)| Line {
            pid: (list.len() == 1).then(|| list[0].pid),
            cpu: list.iter().map(|p| p.cpu_percent).sum(),
            rss: list.iter().map(|p| p.rss_bytes).sum(),
            cmd: if list.len() == 1 {
                cmd
            } else {
                format!("{cmd} ×{}", list.len())
            },
        })
        .collect()
}

fn matches(g: &SessionGroup, needle: &str) -> bool {
    needle.is_empty()
        || g.title.to_lowercase().contains(needle)
        || g.processes
            .iter()
            .any(|p| p.display_cmd().to_lowercase().contains(needle))
}

/// Indices into `data.processes` of the groups shown, busiest first.
pub fn visible(data: &Data, filter: &str) -> Vec<usize> {
    let needle = filter.trim().to_lowercase();
    let mut idx: Vec<usize> = (0..data.processes.len())
        .filter(|&i| data.processes[i].can_stop && matches(&data.processes[i], &needle))
        .collect();
    idx.sort_by(|&a, &b| {
        let (a, b) = (&data.processes[a], &data.processes[b]);
        b.cpu_percent
            .total_cmp(&a.cpu_percent)
            .then(b.rss_bytes.cmp(&a.rss_bytes))
    });
    idx
}

const CPU_W: f32 = 64.0;
const COUNT_W: f32 = 84.0;
const RSS_W: f32 = 64.0;
const PID_W: f32 = 64.0;

/// Stop column wide enough for its label in the current language, so member
/// numbers line up with the group totals.
fn stop_w() -> f32 {
    st::icon_btn_width(&fl!(loader(), "processes-stop"))
}

/// Group titles carry a run id in parentheses; the row shows the name and
/// the expanded view the whole title.
fn short_title(title: &str) -> &str {
    title.split(" (").next().unwrap_or(title)
}

fn group_row<'a>(i: usize, g: &'a SessionGroup, open: bool) -> Element<'a, Message> {
    let look = st::look();
    let count = fl!(loader(), "processes-count", count = g.processes.len());
    let toggle = button(
        row![
            st::icon(
                if open {
                    st::Icon::ChevronDown
                } else {
                    st::Icon::ChevronRight
                },
                look.body + 1.0
            ),
            container(st::fit(
                short_title(&g.title),
                st::Role::Heading,
                st::Cut::End
            ))
            .width(Length::Fill),
            st::pill(kind_label(g), st::Tone::Neutral),
        ]
        .push(
            // Harness groups running inside a container get the podman mark too.
            g.container
                .as_ref()
                .filter(|_| {
                    !matches!(
                        g.kind,
                        agentdeck_core::processes::GroupKind::Container { .. }
                    )
                })
                .map(|name| st::pill(format!("podman · {name}"), st::Tone::Accent)),
        )
        .push(
            row![
                st::nowrap(st::caption(count)).width(COUNT_W),
                st::nowrap(st::body(format!("{:.1}%", g.cpu_percent)))
                    .width(CPU_W)
                    .align_x(Alignment::End),
                st::nowrap(st::body(mega(g.rss_bytes)))
                    .width(RSS_W)
                    .align_x(Alignment::End),
            ]
            .spacing(st::SPACE_S)
            .align_y(Alignment::Center),
        )
        .spacing(st::SPACE_S)
        .align_y(Alignment::Center),
    )
    .padding([look.button_pad[0], 4.0])
    .width(Length::Fill)
    .style(st::row_style)
    .on_press(Message::ProcToggle(g.pid, g.start_ticks));
    row![
        toggle,
        container(
            st::btn_icon(
                st::Btn::Quiet,
                st::Icon::Square,
                fl!(loader(), "processes-stop")
            )
            .on_press(Message::ProcStop(i))
        )
        .width(stop_w())
        .align_right(stop_w()),
    ]
    .spacing(st::SPACE_S)
    .align_y(Alignment::Center)
    .into()
}

fn members(g: &SessionGroup) -> Element<'_, Message> {
    let mut col = column![].spacing(2);
    if short_title(&g.title) != g.title {
        col = col.push(st::fit(
            g.title.as_str(),
            st::Role::MonoMuted,
            st::Cut::Middle,
        ));
    }
    for line in member_lines(&g.processes) {
        col = col.push(
            row![
                st::nowrap(st::mono_muted(
                    line.pid.map(|p| p.to_string()).unwrap_or_default()
                ))
                .width(PID_W),
                // Middle keeps a folded line's ×N count visible.
                st::fit(line.cmd, st::Role::Mono, st::Cut::Middle),
                st::nowrap(st::mono_muted(format!("{:.1}%", line.cpu)))
                    .width(CPU_W)
                    .align_x(Alignment::End),
                st::nowrap(st::mono_muted(mega(line.rss)))
                    .width(RSS_W)
                    .align_x(Alignment::End),
            ]
            .spacing(st::SPACE_S),
        );
    }
    // Indent under the chevron; right edge lines up with the group totals.
    container(col)
        .padding(iced::Padding {
            top: 2.0,
            bottom: 4.0,
            left: 28.0,
            right: stop_w() + st::SPACE_S + 4.0,
        })
        .into()
}

pub fn filter_input(ui: &ProcUi) -> Element<'_, Message> {
    text_input(&fl!(loader(), "processes-filter"), &ui.filter)
        .on_input(Message::ProcFilter)
        .size(st::look().body)
        .padding([5, 10])
        .width(240)
        .into()
}

pub fn view<'a>(data: &'a Data, ui: &'a ProcUi) -> Element<'a, Message> {
    let look = st::look();
    if !data.processes.iter().any(|g| g.can_stop) {
        return st::card(st::muted(fl!(loader(), "processes-empty")));
    }
    let shown = visible(data, &ui.filter);
    if shown.is_empty() {
        return st::card(st::muted(fl!(loader(), "processes-no-match")));
    }
    let blocks = shown.into_iter().map(|i| {
        let g = &data.processes[i];
        let open = ui.expanded.contains(&(g.pid, g.start_ticks));
        let mut block = column![group_row(i, g, open)].spacing(2);
        if open {
            block = block.push(members(g));
        }
        block
    });
    column(blocks.map(st::card))
        .spacing(look.gap.max(st::SPACE_S))
        .into()
}

pub fn stop_modal(pending: &PendingStop, force: bool) -> Modal<'_, Message> {
    let mut body: Vec<Element<'_, Message>> =
        vec![st::body(fl!(loader(), "processes-confirm-will")).into()];
    let mut list = column![].spacing(2);
    for p in &pending.members {
        list = list.push(st::mono(format!(
            "{}  {}",
            p.pid,
            short_cmd(&p.display_cmd())
        )));
    }
    body.push(st::card(list));
    body.push(
        checkbox(force)
            .label(fl!(loader(), "processes-force"))
            .text_size(st::look().body)
            .on_toggle(Message::ProcForce)
            .into(),
    );
    Modal {
        title: fl!(
            loader(),
            "processes-confirm-title",
            title = pending.title.as_str()
        ),
        body,
        actions: vec![
            st::btn(st::Btn::Quiet, fl!(loader(), "processes-cancel"))
                .on_press(Message::ProcCancel)
                .into(),
            // Destructive: the only red thing on screen.
            st::btn(st::Btn::Danger, fl!(loader(), "processes-confirm-ok"))
                .on_press(Message::ProcConfirm)
                .into(),
        ],
        on_cancel: Message::ProcCancel,
        on_submit: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(pid: u32, argv: &[&str], cpu: f64) -> ProcInfo {
        ProcInfo {
            pid,
            ppid: 1,
            pgrp: 1,
            comm: argv[0].into(),
            argv: argv.iter().map(|s| s.to_string()).collect(),
            cpu_percent: cpu,
            rss_bytes: 1_048_576,
            read_bytes: None,
            write_bytes: None,
            start_ticks: 1,
            scope: None,
            owner_tag: None,
            container: None,
        }
    }

    #[test]
    fn identical_commands_fold_with_a_count() {
        let lines = member_lines(&[
            proc(1, &["claude"], 1.0),
            proc(2, &["node", "mcp.js"], 0.5),
            proc(3, &["node", "mcp.js"], 0.25),
            proc(4, &["node", "other.js"], 0.0),
        ]);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].pid, Some(1));
        assert_eq!(lines[1].cmd, "node mcp.js ×2");
        assert_eq!(lines[1].pid, None);
        assert_eq!(lines[1].cpu, 0.75);
        assert_eq!(lines[1].rss, 2 * 1_048_576);
        assert_eq!(lines[2].cmd, "node other.js");
    }
}
