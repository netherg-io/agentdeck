//! Shared visual language: colour tokens (dark and light), layout
//! parameters, text roles, buttons, pills, cards and lucide icons. Only
//! presentation: no behaviour lives here.

use iced::widget::text::Wrapping;
use iced::widget::{
    Responsive, Space, button, column, container, progress_bar, responsive, row, svg, text,
};
use iced::{Background, Border, Color, Element, Font, Length, Shadow, Theme, Vector, color};

/// Layout parameters.
pub struct Look {
    pub radius: f32,
    pub card_pad: f32,
    /// Between cards / sections.
    pub gap: f32,
    /// Between lines inside a card.
    pub row_gap: f32,
    pub title: f32,
    pub heading: f32,
    pub body: f32,
    pub small: f32,
    pub sidebar_width: f32,
    pub page_pad: f32,
    pub content_max: f32,
    pub bar: f32,
    pub button_pad: [f32; 2],
}

const LOOK: Look = Look {
    radius: 8.0,
    card_pad: 12.0,
    gap: 8.0,
    row_gap: 6.0,
    title: 17.0,
    heading: 14.0,
    body: 13.0,
    small: 12.0,
    sidebar_width: 208.0,
    page_pad: 20.0,
    content_max: 980.0,
    bar: 4.0,
    button_pad: [4.0, 10.0],
};

pub fn look() -> &'static Look {
    &LOOK
}

/// Colour tokens.
pub struct Colors {
    pub bg: Color,
    pub sidebar: Color,
    pub surface: Color,
    pub raised: Color,
    pub hover: Color,
    pub border: Color,
    pub text: Color,
    pub muted: Color,
    pub faint: Color,
    pub accent: Color,
    pub on_accent: Color,
    pub danger: Color,
    pub ok: Color,
    pub warn: Color,
    pub track: Color,
    pub scrim: Color,
}

const DARK: Colors = Colors {
    bg: color!(0x0a0a0a),
    sidebar: color!(0x0a0a0a),
    surface: color!(0x121212),
    raised: color!(0x161616),
    hover: color!(0xffffff, 0.06),
    border: color!(0xffffff, 0.08),
    text: color!(0xf1f3f7),
    muted: color!(0xa3a3a3),
    faint: color!(0x737373),
    accent: color!(0x5b74f0),
    on_accent: color!(0xffffff),
    danger: color!(0xe5484d),
    ok: color!(0x3fb950),
    warn: color!(0xd29922),
    track: color!(0xffffff, 0.08),
    scrim: color!(0x000000, 0.6),
};

const LIGHT: Colors = Colors {
    bg: color!(0xfcfcfd),
    sidebar: color!(0xf6f6f7),
    surface: color!(0xffffff),
    raised: color!(0xffffff),
    hover: color!(0x000000, 0.05),
    border: color!(0x000000, 0.09),
    text: color!(0x18181b),
    muted: color!(0x5f5f68),
    faint: color!(0x8e8e96),
    accent: color!(0x4459e0),
    on_accent: color!(0xffffff),
    danger: color!(0xd92d32),
    ok: color!(0x1f883d),
    warn: color!(0x9a6700),
    track: color!(0x000000, 0.07),
    scrim: color!(0x000000, 0.35),
};

fn colors_for(dark: bool) -> &'static Colors {
    if dark { &DARK } else { &LIGHT }
}

pub fn colors(theme: &Theme) -> &'static Colors {
    colors_for(theme.extended_palette().is_dark)
}

/// The app theme; built-in widgets (inputs,
/// checkboxes, scrollbars) derive from this palette.
pub fn theme(dark: bool) -> Theme {
    let c = colors_for(dark);
    Theme::custom(
        if dark {
            "agentdeck-dark"
        } else {
            "agentdeck-light"
        }
        .to_string(),
        iced::theme::Palette {
            background: c.bg,
            text: c.text,
            primary: c.accent,
            success: c.ok,
            warning: c.warn,
            danger: c.danger,
        },
    )
}

pub fn app_style(theme: &Theme) -> iced::theme::Style {
    let c = colors(theme);
    iced::theme::Style {
        background_color: c.bg,
        text_color: c.text,
    }
}

/// Spacing scale (px) kept for pages not redesigned yet.
pub const SPACE_XS: f32 = 4.0;
pub const SPACE_S: f32 = 8.0;
pub const SPACE_M: f32 = 12.0;
pub const SPACE_L: f32 = 20.0;

const SEMIBOLD: Font = Font {
    weight: iced::font::Weight::Semibold,
    ..Font::DEFAULT
};

pub fn title<'a>(label: impl text::IntoFragment<'a>) -> text::Text<'a> {
    text(label).size(look().title).font(SEMIBOLD)
}

pub fn heading<'a>(label: impl text::IntoFragment<'a>) -> text::Text<'a> {
    text(label)
        .size(look().heading)
        .font(SEMIBOLD)
        .wrapping(Wrapping::WordOrGlyph)
}

// Free text breaks inside long tokens (paths, URLs) instead of leaving its
// container; single-line slots use `fit` or `nowrap` instead.
pub fn body<'a>(label: impl text::IntoFragment<'a>) -> text::Text<'a> {
    text(label)
        .size(look().body)
        .wrapping(Wrapping::WordOrGlyph)
}

/// Secondary line in the muted token colour.
pub fn muted<'a>(label: impl text::IntoFragment<'a>) -> text::Text<'a> {
    body(label).style(|t: &Theme| text::Style {
        color: Some(colors(t).muted),
    })
}

/// Small faint label: column headers, group captions, units.
pub fn caption<'a>(label: impl text::IntoFragment<'a>) -> text::Text<'a> {
    text(label)
        .size(look().small)
        .wrapping(Wrapping::WordOrGlyph)
        .style(|t: &Theme| text::Style {
            color: Some(colors(t).faint),
        })
}

/// Ids, paths, commands.
pub fn mono<'a>(label: impl text::IntoFragment<'a>) -> text::Text<'a> {
    text(label)
        .size(look().small)
        .font(Font::MONOSPACE)
        .wrapping(Wrapping::WordOrGlyph)
}

pub fn mono_muted<'a>(label: impl text::IntoFragment<'a>) -> text::Text<'a> {
    mono(label).style(|t: &Theme| text::Style {
        color: Some(colors(t).muted),
    })
}

/// Which text role a [`fit`] line renders with.
#[derive(Debug, Clone, Copy)]
pub enum Role {
    Heading,
    Body,
    Muted,
    Caption,
    Mono,
    MonoMuted,
}

impl Role {
    fn text<'a>(self, label: String) -> text::Text<'a> {
        match self {
            Role::Heading => heading(label),
            Role::Body => body(label),
            Role::Muted => muted(label),
            Role::Caption => caption(label),
            Role::Mono => mono(label),
            Role::MonoMuted => mono_muted(label),
        }
    }

    /// Widest likely average glyph advance (px), Cyrillic and digits
    /// included, so an estimate errs on cutting a little early.
    fn char_width(self) -> f32 {
        let l = look();
        match self {
            Role::Heading => l.heading * 0.6,
            Role::Body | Role::Muted => l.body * 0.57,
            Role::Caption => l.small * 0.57,
            Role::Mono | Role::MonoMuted => l.small * 0.6,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cut {
    /// Names and sentences: keep the start.
    End,
    /// Paths and ids: keep both ends.
    Middle,
}

/// `s` in at most `max` chars, with `…` where text was cut.
pub fn ellipsize(s: &str, max: usize, cut: Cut) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(1);
    match cut {
        Cut::End => s.chars().take(keep).collect::<String>() + "…",
        Cut::Middle => {
            let tail = keep / 2;
            let head: String = s.chars().take(keep - tail).collect();
            let end: String = s.chars().skip(n - tail).collect();
            format!("{head}…{end}")
        }
    }
}

/// One line that never leaves its slot: takes the width it is given (Fill
/// by default) and cuts with an ellipsis by estimated glyph width, since
/// iced text has no ellipsis of its own.
pub fn fit<'a, M: 'a>(label: impl Into<String>, role: Role, cut: Cut) -> Responsive<'a, M> {
    // Fluent wraps placeables in invisible bidi isolates; they must not count.
    let label: String = label
        .into()
        .chars()
        .filter(|c| !matches!(c, '\u{2068}' | '\u{2069}'))
        .collect();
    responsive(move |size| {
        let max = (size.width / role.char_width()).floor() as usize;
        role.text(ellipsize(&label, max, cut))
            .wrapping(Wrapping::None)
            .into()
    })
    .height(Length::Shrink)
}

/// Fixed-size label (counts, units, status): never wraps.
pub fn nowrap<'a>(t: text::Text<'a>) -> text::Text<'a> {
    t.wrapping(Wrapping::None)
}

#[derive(Debug, Clone, Copy)]
pub enum Tone {
    Neutral,
    Accent,
    Ok,
    Warn,
    Danger,
}

fn tone_color(c: &Colors, tone: Tone) -> Color {
    match tone {
        Tone::Neutral => c.muted,
        Tone::Accent => c.accent,
        Tone::Ok => c.ok,
        Tone::Warn => c.warn,
        Tone::Danger => c.danger,
    }
}

/// Small status badge.
pub fn pill<'a, M: 'a>(label: impl text::IntoFragment<'a>, tone: Tone) -> Element<'a, M> {
    container(
        text(label)
            .size(look().small)
            .wrapping(Wrapping::None)
            .style(move |t: &Theme| text::Style {
                color: Some(tone_color(colors(t), tone)),
            }),
    )
    .padding([1.0, 8.0])
    .style(move |t: &Theme| {
        let c = colors(t);
        let fg = tone_color(c, tone);
        container::Style {
            background: Some(Color { a: 0.13, ..fg }.into()),
            border: Border {
                color: Color { a: 0.28, ..fg },
                width: 1.0,
                radius: 999.0.into(),
            },
            ..container::Style::default()
        }
    })
    .into()
}

pub fn card_style(t: &Theme) -> container::Style {
    let c = colors(t);
    container::Style {
        background: Some(c.surface.into()),
        border: Border {
            color: c.border,
            width: 1.0,
            radius: look().radius.into(),
        },
        ..container::Style::default()
    }
}

/// Bordered card around a block of rows.
pub fn card<'a, M: 'a>(content: impl Into<Element<'a, M>>) -> Element<'a, M> {
    container(content)
        .padding(look().card_pad)
        .width(Length::Fill)
        .style(card_style)
        .into()
}

pub fn sidebar_style(t: &Theme) -> container::Style {
    container::Style {
        background: Some(colors(t).sidebar.into()),
        ..container::Style::default()
    }
}

/// Mostly opaque dialog surface with a soft shadow.
pub fn dialog_style(t: &Theme) -> container::Style {
    let c = colors(t);
    container::Style {
        background: Some(c.raised.into()),
        border: Border {
            color: c.border,
            width: 1.0,
            radius: (look().radius + 2.0).into(),
        },
        shadow: Shadow {
            color: color!(0x000000, 0.4),
            offset: Vector::new(0.0, 8.0),
            blur_radius: 32.0,
        },
        ..container::Style::default()
    }
}

pub fn scrim_style(t: &Theme) -> container::Style {
    container::Style {
        background: Some(colors(t).scrim.into()),
        ..container::Style::default()
    }
}

/// 1 px hairline, horizontal.
pub fn hairline<'a, M: 'a>() -> Element<'a, M> {
    container(Space::new())
        .width(Length::Fill)
        .height(1)
        .style(|t: &Theme| container::Style {
            background: Some(colors(t).border.into()),
            ..container::Style::default()
        })
        .into()
}

/// 1 px hairline, vertical.
pub fn hairline_v<'a, M: 'a>() -> Element<'a, M> {
    container(Space::new())
        .width(1)
        .height(Length::Fill)
        .style(|t: &Theme| container::Style {
            background: Some(colors(t).border.into()),
            ..container::Style::default()
        })
        .into()
}

/// Kept for pages not redesigned yet.
pub fn divider<'a, M: 'a>() -> Element<'a, M> {
    hairline()
}

/// Thin quota bar; warn colour from 90 % so red stays for destructive buttons.
pub fn quota_bar(percent: f32) -> progress_bar::ProgressBar<'static> {
    progress_bar(0.0..=100.0, percent)
        .girth(Length::Fixed(look().bar))
        .style(move |t: &Theme| {
            let c = colors(t);
            progress_bar::Style {
                background: c.track.into(),
                bar: if percent >= 90.0 { c.warn } else { c.accent }.into(),
                border: Border {
                    radius: (look().bar / 2.0).into(),
                    ..Border::default()
                },
            }
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Btn {
    Primary,
    /// Bordered, transparent: secondary actions.
    Quiet,
    /// No border: icon buttons, row toggles.
    Ghost,
    Danger,
}

fn btn_style(kind: Btn) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |t, status| {
        let c = colors(t);
        let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
        let (bg, fg, border) = match kind {
            Btn::Primary => (
                Some(Color {
                    a: if hovered { 0.88 } else { 1.0 },
                    ..c.accent
                }),
                c.on_accent,
                Color::TRANSPARENT,
            ),
            Btn::Danger => (
                Some(Color {
                    a: if hovered { 0.88 } else { 1.0 },
                    ..c.danger
                }),
                Color::WHITE,
                Color::TRANSPARENT,
            ),
            Btn::Quiet => (hovered.then_some(c.hover), c.text, c.border),
            Btn::Ghost => (hovered.then_some(c.hover), c.text, Color::TRANSPARENT),
        };
        let disabled = matches!(status, button::Status::Disabled);
        button::Style {
            background: bg.map(Background::Color),
            text_color: if disabled {
                Color { a: 0.45, ..fg }
            } else {
                fg
            },
            border: Border {
                color: border,
                width: 1.0,
                radius: look().radius.min(8.0).into(),
            },
            ..button::Style::default()
        }
    }
}

pub fn btn<'a, M: 'a>(kind: Btn, label: impl text::IntoFragment<'a>) -> button::Button<'a, M> {
    button(text(label).size(look().body).wrapping(Wrapping::None))
        .padding(look().button_pad)
        .style(btn_style(kind))
}

/// Icon plus label, e.g. the page's primary action.
pub fn btn_icon<'a, M: 'a>(
    kind: Btn,
    i: Icon,
    label: impl text::IntoFragment<'a>,
) -> button::Button<'a, M> {
    let size = look().body;
    button(
        row![
            icon_colored(i, size, move |c| match kind {
                Btn::Primary => c.on_accent,
                Btn::Danger => Color::WHITE,
                _ => c.text,
            }),
            text(label).size(size).wrapping(Wrapping::None)
        ]
        .spacing(6)
        .align_y(iced::Alignment::Center),
    )
    .padding(look().button_pad)
    .style(btn_style(kind))
}

/// Estimated width of a [`btn_icon`] with `label`, for fixed action slots.
pub fn icon_btn_width(label: &str) -> f32 {
    let l = look();
    label.chars().count() as f32 * Role::Body.char_width()
        + l.body
        + 6.0
        + 2.0 * l.button_pad[1]
        + 4.0
}

/// Square icon-only button.
pub fn icon_btn<'a, M: 'a>(i: Icon) -> button::Button<'a, M> {
    button(icon(i, look().body + 1.0))
        .padding(look().button_pad[0] + 1.0)
        .style(btn_style(Btn::Ghost))
}

/// Sidebar item: selected gets a quiet fill, never the accent block.
pub fn nav_style(selected: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |t, status| {
        let c = colors(t);
        let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
        button::Style {
            background: (selected || hovered).then_some(Background::Color(if selected {
                Color {
                    a: c.hover.a * 1.8,
                    ..c.hover
                }
            } else {
                c.hover
            })),
            text_color: if selected { c.text } else { c.muted },
            border: Border {
                radius: look().radius.min(8.0).into(),
                ..Border::default()
            },
            ..button::Style::default()
        }
    }
}

/// Transparent full-width row button (expand/collapse).
pub fn row_style(t: &Theme, status: button::Status) -> button::Style {
    let c = colors(t);
    button::Style {
        background: matches!(status, button::Status::Hovered).then_some(Background::Color(c.hover)),
        text_color: c.text,
        border: Border {
            radius: look().radius.min(8.0).into(),
            ..Border::default()
        },
        ..button::Style::default()
    }
}

/// Lucide icons (ISC licence, see the crate README), inner markup copied
/// from the lucide 1.8.0 package.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    Pencil,
    Users,
    Compare,
    Plug,
    Layers,
    Share,
    Wrench,
    Activity,
    Plus,
    Trash,
    Search,
    ChevronRight,
    ChevronDown,
    X,
    Square,
    RefreshCw,
    Download,
    ArrowLeft,
    CircleCheck,
}

impl Icon {
    fn markup(self) -> &'static str {
        match self {
            Icon::Pencil => r#"<path d="m16 3 5 5L8 21H3v-5L16 3Z"/><path d="m14 5 5 5"/>"#,
            Icon::Users => {
                r#"<path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2"/><path d="M16 3.128a4 4 0 0 1 0 7.744"/><path d="M22 21v-2a4 4 0 0 0-3-3.87"/><circle cx="9" cy="7" r="4"/>"#
            }
            Icon::Compare => {
                r#"<circle cx="18" cy="18" r="3"/><circle cx="6" cy="6" r="3"/><path d="M13 6h3a2 2 0 0 1 2 2v7"/><path d="M11 18H8a2 2 0 0 1-2-2V9"/>"#
            }
            Icon::Plug => {
                r#"<path d="M12 22v-5"/><path d="M15 8V2"/><path d="M17 8a1 1 0 0 1 1 1v4a4 4 0 0 1-4 4h-4a4 4 0 0 1-4-4V9a1 1 0 0 1 1-1z"/><path d="M9 8V2"/>"#
            }
            Icon::Layers => {
                r#"<path d="M12.83 2.18a2 2 0 0 0-1.66 0L2.6 6.08a1 1 0 0 0 0 1.83l8.58 3.91a2 2 0 0 0 1.66 0l8.58-3.9a1 1 0 0 0 0-1.83z"/><path d="M2 12a1 1 0 0 0 .58.91l8.6 3.91a2 2 0 0 0 1.65 0l8.58-3.9A1 1 0 0 0 22 12"/><path d="M2 17a1 1 0 0 0 .58.91l8.6 3.91a2 2 0 0 0 1.65 0l8.58-3.9A1 1 0 0 0 22 17"/>"#
            }
            Icon::Share => {
                r#"<circle cx="18" cy="5" r="3"/><circle cx="6" cy="12" r="3"/><circle cx="18" cy="19" r="3"/><line x1="8.59" x2="15.42" y1="13.51" y2="17.49"/><line x1="15.41" x2="8.59" y1="6.51" y2="10.49"/>"#
            }
            Icon::Wrench => {
                r#"<path d="M14.7 6.3a1 1 0 0 0 0 1.4l1.6 1.6a1 1 0 0 0 1.4 0l3.106-3.105c.32-.322.863-.22.983.218a6 6 0 0 1-8.259 7.057l-7.91 7.91a1 1 0 0 1-2.999-3l7.91-7.91a6 6 0 0 1 7.057-8.259c.438.12.54.662.219.984z"/>"#
            }
            Icon::Activity => {
                r#"<path d="M22 12h-2.48a2 2 0 0 0-1.93 1.46l-2.35 8.36a.25.25 0 0 1-.48 0L9.24 2.18a.25.25 0 0 0-.48 0l-2.35 8.36A2 2 0 0 1 4.49 12H2"/>"#
            }
            Icon::Plus => r#"<path d="M5 12h14"/><path d="M12 5v14"/>"#,
            Icon::Trash => {
                r#"<path d="M10 11v6"/><path d="M14 11v6"/><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6"/><path d="M3 6h18"/><path d="M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/>"#
            }
            Icon::Search => r#"<path d="m21 21-4.34-4.34"/><circle cx="11" cy="11" r="8"/>"#,
            Icon::ChevronRight => r#"<path d="m9 18 6-6-6-6"/>"#,
            Icon::ChevronDown => r#"<path d="m6 9 6 6 6-6"/>"#,
            Icon::X => r#"<path d="M18 6 6 18"/><path d="m6 6 12 12"/>"#,
            Icon::Square => r#"<rect width="18" height="18" x="3" y="3" rx="2"/>"#,
            Icon::RefreshCw => {
                r#"<path d="M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1 6.74 2.74L21 8"/><path d="M21 3v5h-5"/><path d="M21 12a9 9 0 0 1-9 9 9.75 9.75 0 0 1-6.74-2.74L3 16"/><path d="M8 16H3v5"/>"#
            }
            Icon::Download => {
                r#"<path d="M12 15V3"/><path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><path d="m7 10 5 5 5-5"/>"#
            }
            Icon::ArrowLeft => r#"<path d="m12 19-7-7 7-7"/><path d="M19 12H5"/>"#,
            Icon::CircleCheck => r#"<circle cx="12" cy="12" r="10"/><path d="m9 12 2 2 4-4"/>"#,
        }
    }

    fn handle(self) -> svg::Handle {
        svg::Handle::from_memory(
            format!(
                r#"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="black" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">{}</svg>"#,
                self.markup()
            )
            .into_bytes(),
        )
    }
}

/// Icon in the muted token colour.
pub fn icon<'a>(i: Icon, size: f32) -> svg::Svg<'a> {
    icon_colored(i, size, |c| c.muted)
}

pub fn icon_colored<'a>(i: Icon, size: f32, pick: impl Fn(&Colors) -> Color + 'a) -> svg::Svg<'a> {
    svg(i.handle())
        .width(size)
        .height(size)
        .style(move |t: &Theme, _| svg::Style {
            color: Some(pick(colors(t))),
        })
}

/// Generic bordered block kept for inline sections (add wizard).
pub fn panel<'a, Message: 'a>(
    heading_label: String,
    lines: Vec<Element<'a, Message>>,
    actions: Vec<Element<'a, Message>>,
) -> Element<'a, Message> {
    let mut col = column![heading(heading_label)].spacing(look().row_gap);
    for line in lines {
        col = col.push(line);
    }
    if !actions.is_empty() {
        col = col.push(row(actions).spacing(SPACE_S));
    }
    card(col)
}

/// Buttons side by side.
pub fn actions<'a, Message: 'a>(items: Vec<Element<'a, Message>>) -> Element<'a, Message> {
    row(items).spacing(SPACE_S).into()
}

/// Slim scrollbar: no rail, a hairline-coloured thumb.
pub fn scroll_style(
    t: &Theme,
    status: iced::widget::scrollable::Status,
) -> iced::widget::scrollable::Style {
    let c = colors(t);
    let mut s = iced::widget::scrollable::default(t, status);
    for rail in [&mut s.vertical_rail, &mut s.horizontal_rail] {
        rail.background = None;
        rail.border = Border::default();
        rail.scroller.background = Color {
            a: c.border.a * 2.5,
            ..c.border
        }
        .into();
        rail.scroller.border = Border {
            radius: 3.0.into(),
            ..Border::default()
        };
    }
    s
}
