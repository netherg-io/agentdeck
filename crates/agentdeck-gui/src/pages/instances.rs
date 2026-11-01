//! Instances: one card per registry instance (identity, status pills,
//! threads, aligned quota bars) and standalone usage sources below.

use i18n_embed_fl::fl;
use iced::widget::{column, row, text_input, tooltip};
use iced::{Alignment, Element, Length};

use crate::app::{Message, RemoveAsk};
use crate::data::{Data, InstanceRow, QuotaBar, UsageQuotas};
use crate::i18n::loader;
use crate::overlay::Modal;
use crate::style as st;

#[derive(Debug, Clone)]
pub struct Rename {
    pub id: String,
    pub name: String,
    pub busy: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Parallel {
    pub id: String,
    pub value: String,
    pub busy: bool,
    pub error: Option<String>,
}

pub(crate) fn parse_parallel(value: &str) -> Result<Option<u32>, ()> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    if !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(());
    }
    value
        .parse::<u32>()
        .ok()
        .filter(|n| *n > 0)
        .map(Some)
        .ok_or(())
}

fn account_status(status: &str) -> String {
    match status {
        "ok" => fl!(loader(), "account-ok"),
        "stale" => fl!(loader(), "account-stale"),
        "error" => fl!(loader(), "account-error"),
        "waiting" => fl!(loader(), "account-waiting"),
        _ => status.to_string(),
    }
}

fn account_tone(status: &str) -> st::Tone {
    match status {
        "ok" => st::Tone::Ok,
        "error" => st::Tone::Danger,
        "stale" | "waiting" => st::Tone::Warn,
        _ => st::Tone::Neutral,
    }
}

/// "resets in 2h 15m" / "resets in 40m"; the countdown math lives in data.
fn reset_label(mins: i64) -> String {
    if mins >= 24 * 60 {
        let d: i64 = mins / (24 * 60);
        let h: i64 = mins % (24 * 60) / 60;
        fl!(loader(), "quota-in-days", d = d, h = h)
    } else if mins >= 60 {
        let h: i64 = mins / 60;
        let m: i64 = mins % 60;
        fl!(loader(), "quota-in-hours", h = h, m = m)
    } else {
        fl!(loader(), "quota-in-mins", m = mins.max(0))
    }
}

fn quota_row(bar: &QuotaBar) -> Element<'_, Message> {
    // Window labels stay as the provider data gives them; the scope marks a
    // per-model window so it is not confused with the shared one.
    let label = match &bar.scope {
        Some(scope) => format!("{} · {scope}", bar.label),
        None => bar.label.clone(),
    };
    // Fixed side columns keep every bar the same length across cards.
    let reset = bar.resets_in_mins.map(reset_label).unwrap_or_default();
    row![
        st::fit(label, st::Role::Muted, st::Cut::End).width(150),
        st::quota_bar(bar.used_percent as f32).length(Length::Fill),
        st::nowrap(st::body(format!("{:.0}%", bar.used_percent)))
            .width(44)
            .align_x(Alignment::End),
        st::fit(reset, st::Role::Caption, st::Cut::End).width(170),
    ]
    .spacing(st::SPACE_M)
    .align_y(Alignment::Center)
    .into()
}

fn status_pill(u: &UsageQuotas) -> Option<Element<'_, Message>> {
    (!u.status.is_empty()).then(|| st::pill(account_status(&u.status), account_tone(&u.status)))
}

/// `head`: a source line with its status; an instance card with a single
/// source shows the status in the card header instead.
fn quotas_view(u: &UsageQuotas, head: bool) -> Element<'_, Message> {
    let look = st::look();
    let mut col = column![].spacing(look.row_gap);
    if head {
        col = col.push(
            row![st::fit(u.display.as_str(), st::Role::Body, st::Cut::End)]
                .push(status_pill(u))
                .spacing(st::SPACE_S)
                .align_y(Alignment::Center),
        );
    }
    if u.bars.is_empty() {
        col = col.push(st::fit(
            match &u.error {
                Some(error) => error.clone(),
                None => fl!(loader(), "quotas-no-data"),
            },
            st::Role::Muted,
            st::Cut::End,
        ));
    }
    for bar in &u.bars {
        col = col.push(quota_row(bar));
    }
    col.into()
}

fn t3_pill(inst: &InstanceRow) -> Element<'_, Message> {
    match &inst.t3 {
        Some(info) => {
            let status = info.status.as_deref().unwrap_or("—");
            let tone = match status {
                "ready" => st::Tone::Ok,
                "disabled" | "—" => st::Tone::Neutral,
                _ => st::Tone::Warn,
            };
            st::pill(format!("{} {status}", fl!(loader(), "t3-label")), tone)
        }
        None => st::pill(fl!(loader(), "t3-unavailable"), st::Tone::Neutral),
    }
}

fn threads(inst: &InstanceRow) -> String {
    inst.t3
        .as_ref()
        .and_then(|i| i.threads)
        .map(|n| fl!(loader(), "threads-count", count = n))
        .unwrap_or_default()
}

fn remove_btn(inst: &InstanceRow) -> Element<'_, Message> {
    st::icon_btn(st::Icon::Trash)
        .on_press(Message::RemoveAsk(inst.id.clone()))
        .into()
}

fn instance_card<'a>(
    inst: &'a InstanceRow,
    rename: Option<&'a Rename>,
    parallel: Option<&'a Parallel>,
) -> Element<'a, Message> {
    let look = st::look();
    let single = inst.quotas.len() == 1;
    let editing = rename.filter(|r| r.id == inst.id);
    let mut identity = column![st::fit(
        inst.display_name.as_str(),
        st::Role::Heading,
        st::Cut::End
    )]
    .spacing(2)
    .width(Length::Fill);
    if inst.display_name != inst.id {
        identity = identity.push(st::fit(
            inst.id.as_str(),
            st::Role::MonoMuted,
            st::Cut::Middle,
        ));
    }
    let mut head = row![identity, st::pill(inst.harness.as_str(), st::Tone::Accent),]
        .spacing(st::SPACE_S)
        .align_y(Alignment::Center);
    if !inst.enabled {
        head = head.push(st::pill(fl!(loader(), "status-disabled"), st::Tone::Warn));
    }
    let mut head = head
        .push(st::nowrap(st::caption(threads(inst))))
        .push(t3_pill(inst));
    if single {
        head = head.push(status_pill(&inst.quotas[0]));
    }
    let head = head
        .push(
            st::btn(st::Btn::Quiet, fl!(loader(), "instances-cli"))
                .on_press(Message::LaunchCli(inst.id.clone())),
        )
        .push(tooltip(
            st::icon_btn(st::Icon::Pencil).on_press(Message::RenameAsk(inst.id.clone())),
            st::body(fl!(loader(), "instances-rename")),
            tooltip::Position::Bottom,
        ))
        .push(remove_btn(inst));
    let mut col = column![head].spacing(look.row_gap + 4.0);
    if let Some(edit) = editing {
        col = col.push(
            row![
                text_input(&fl!(loader(), "add-display-name"), &edit.name)
                    .on_input_maybe((!edit.busy).then_some(Message::RenameInput))
                    .on_submit_maybe((!edit.busy).then_some(Message::RenameSave))
                    .size(look.body)
                    .width(Length::Fill),
                st::btn(st::Btn::Quiet, fl!(loader(), "rename-save")).on_press_maybe(
                    (!edit.busy && !edit.name.trim().is_empty()).then_some(Message::RenameSave)
                ),
                st::icon_btn(st::Icon::X)
                    .on_press_maybe((!edit.busy).then_some(Message::RenameCancel)),
            ]
            .spacing(st::SPACE_XS)
            .align_y(Alignment::Center),
        );
    }
    if let Some(error) = editing.and_then(|edit| edit.error.as_ref()) {
        col = col.push(st::body(error.as_str()));
    }
    if let Some(edit) = parallel.filter(|e| e.id == inst.id) {
        col = col.push(
            row![
                st::body(fl!(loader(), "parallel-label")),
                text_input(&fl!(loader(), "parallel-unlimited"), &edit.value)
                    .on_input_maybe((!edit.busy).then_some(Message::ParallelInput))
                    .on_submit_maybe((!edit.busy).then_some(Message::ParallelSave))
                    .size(look.body)
                    .width(Length::Fill),
                st::btn(st::Btn::Quiet, fl!(loader(), "rename-save"))
                    .on_press_maybe((!edit.busy).then_some(Message::ParallelSave)),
                st::icon_btn(st::Icon::X)
                    .on_press_maybe((!edit.busy).then_some(Message::ParallelCancel)),
            ]
            .spacing(st::SPACE_XS)
            .align_y(Alignment::Center),
        );
        if let Some(error) = &edit.error {
            col = col.push(st::body(error.as_str()));
        }
    } else {
        let limit = inst
            .max_parallel
            .map(|n| n.to_string())
            .unwrap_or_else(|| fl!(loader(), "parallel-unlimited"));
        col = col.push(
            st::btn(
                st::Btn::Quiet,
                format!("{}: {limit}", fl!(loader(), "parallel-label")),
            )
            .on_press(Message::ParallelAsk(inst.id.clone())),
        );
    }
    for u in &inst.quotas {
        col = col.push(quotas_view(u, !single));
    }
    st::card(col)
}

pub fn view(data: &Data) -> Element<'_, Message> {
    view_with_rename(data, None)
}

pub fn view_with_rename<'a>(data: &'a Data, rename: Option<&'a Rename>) -> Element<'a, Message> {
    view_with_edits(data, rename, None)
}

pub fn view_with_edits<'a>(
    data: &'a Data,
    rename: Option<&'a Rename>,
    parallel: Option<&'a Parallel>,
) -> Element<'a, Message> {
    let look = st::look();
    let mut col = column![].spacing(look.gap.max(st::SPACE_S));
    if let Some(error) = &data.error {
        col = col.push(st::muted(fl!(loader(), "load-error", what = error)));
    }
    if !data.loaded {
        col = col.push(st::muted(fl!(loader(), "instances-loading")));
    } else if data.instances.is_empty() {
        col = col.push(st::card(st::muted(fl!(loader(), "instances-empty"))));
    } else {
        for inst in &data.instances {
            col = col.push(instance_card(inst, rename, parallel));
        }
    }
    if !data.standalone.is_empty() {
        let mut list = column![].spacing(look.row_gap + 4.0);
        for u in &data.standalone {
            list = list.push(quotas_view(u, true));
        }
        col = col.push(
            column![
                st::caption(fl!(loader(), "instances-standalone")),
                st::card(list)
            ]
            .spacing(st::SPACE_S),
        );
    }
    col.into()
}

pub fn remove_modal(ask: &RemoveAsk) -> Modal<'_, Message> {
    let mut body: Vec<Element<'_, Message>> = Vec::new();
    if let Some(n) = ask.threads.filter(|&n| n > 0) {
        body.push(st::body(fl!(loader(), "remove-threads", count = n)).into());
    }
    body.push(st::muted(fl!(loader(), "remove-keep-home")).into());
    if let Some(error) = &ask.error {
        body.push(st::body(fl!(loader(), "load-error", what = error)).into());
    }
    Modal {
        title: fl!(loader(), "remove-ask", id = ask.id.clone()),
        body,
        actions: vec![
            st::btn(st::Btn::Quiet, fl!(loader(), "remove-cancel"))
                .on_press(Message::RemoveCancel)
                .into(),
            // Destructive: the only red thing on screen.
            st::btn(st::Btn::Danger, fl!(loader(), "remove-confirm"))
                .on_press_maybe((!ask.working).then_some(Message::RemoveConfirm))
                .into(),
        ],
        on_cancel: Message::RemoveCancel,
        on_submit: None,
    }
}

#[cfg(test)]
mod tests {
    use super::parse_parallel;

    #[test]
    fn parallel_limit_accepts_blank_or_positive_u32() {
        for blank in ["", "  ", "\t"] {
            assert_eq!(parse_parallel(blank), Ok(None));
        }
        assert_eq!(parse_parallel(" 1 "), Ok(Some(1)));
        assert_eq!(parse_parallel("4294967295"), Ok(Some(u32::MAX)));
        for invalid in ["0", "-1", "+1", "1.5", "1e2", "abc", "4294967296"] {
            assert_eq!(parse_parallel(invalid), Err(()), "{invalid}");
        }
    }
}
