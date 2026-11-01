//! Module switches and status. Each module (ai-usage, router, agent-process)
//! is a sidebar group whose header carries its on/off switch; its pages end
//! with a status card. Enable/disable always goes through a [`Change`] preview.

use std::collections::HashSet;

use i18n_embed_fl::fl;
use iced::widget::{button, column, row, toggler, tooltip};
use iced::{Alignment, Element};

use crate::app::Message;
use crate::data::{Data, PluginRow};
use crate::i18n::loader;
use crate::style as st;

/// A confirmed-but-not-yet-applied enable/disable with its change preview.
#[derive(Debug, Clone)]
pub struct Pending {
    pub name: String,
    pub enable: bool,
    pub preview: Vec<String>,
}

pub fn modal(p: &Pending) -> crate::overlay::Modal<'_, Message> {
    let title = if p.enable {
        fl!(loader(), "plugins-preview-enable", name = p.name.clone())
    } else {
        fl!(loader(), "plugins-preview-disable", name = p.name.clone())
    };
    crate::overlay::confirm(
        title,
        &p.preview,
        (
            fl!(loader(), "plugins-confirm"),
            Some(Message::ConfirmPlugin),
        ),
        (fl!(loader(), "plugins-cancel"), Message::CancelPlugin),
    )
}

fn find<'a>(data: &'a Data, name: &str) -> Option<&'a PluginRow> {
    data.plugins.iter().find(|p| p.name == name)
}

/// The module's on/off switch for its sidebar group header.
pub fn switch<'a>(data: &'a Data, name: &'static str) -> Element<'a, Message> {
    let Some(plugin) = find(data, name) else {
        return row![].into();
    };
    let switch = toggler(plugin.enabled)
        .size(14)
        .on_toggle_maybe(plugin.available.then_some(move |on| {
            if on {
                Message::EnablePlugin(name.to_string())
            } else {
                Message::DisablePlugin(name.to_string())
            }
        }));
    if plugin.available {
        switch.into()
    } else {
        tooltip(
            switch,
            st::body(fl!(loader(), "plugins-unavailable")),
            tooltip::Position::Right,
        )
        .into()
    }
}

/// Module state plus its technical status behind an expander; ends each module page.
pub fn status_card<'a>(
    data: &'a Data,
    name: &'static str,
    open: &'a HashSet<String>,
) -> Element<'a, Message> {
    let look = st::look();
    let Some(plugin) = find(data, name) else {
        return column![].into();
    };
    let (state, tone) = if !plugin.available {
        (fl!(loader(), "plugins-unavailable"), st::Tone::Neutral)
    } else if plugin.enabled {
        (fl!(loader(), "plugins-enabled"), st::Tone::Ok)
    } else {
        (fl!(loader(), "plugins-disabled"), st::Tone::Neutral)
    };
    let expanded = open.contains(name);
    let expander = button(
        row![
            st::icon(
                if expanded {
                    st::Icon::ChevronDown
                } else {
                    st::Icon::ChevronRight
                },
                look.body
            ),
            st::nowrap(st::caption(fl!(loader(), "plugins-details"))),
        ]
        .spacing(4)
        .align_y(Alignment::Center),
    )
    .padding([2, 4])
    .style(st::row_style)
    .on_press(Message::PluginDetail(name.to_string()));
    let head = row![
        st::fit(name, st::Role::Muted, st::Cut::End),
        st::pill(state, tone),
        expander,
    ]
    .spacing(st::SPACE_M)
    .align_y(Alignment::Center);
    let mut card = column![head].spacing(look.row_gap);
    if expanded {
        let mut detail = column![].spacing(2).padding(iced::Padding {
            left: 20.0,
            ..iced::Padding::ZERO
        });
        if plugin.detail.is_empty() {
            detail = detail.push(st::muted(fl!(loader(), "plugins-no-details")));
        }
        for line in &plugin.detail {
            detail = detail.push(st::mono_muted(line.as_str()));
        }
        card = card.push(detail);
    }
    st::card(card)
}
