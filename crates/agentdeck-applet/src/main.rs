//! COSMIC panel applet for AI usage quotas and balances only.
//! Do not add Coordinator, Approver, or other modules to this widget.

use cosmic::iced::core::text::{Ellipsize, EllipsizeHeightLimit};
use cosmic::iced::core::{
    Clipboard, Layout, Shell, Size, Vector, Widget,
    event::{Event, Status},
    layout, mouse, overlay, renderer,
    widget::{Operation, Tree},
};
use cosmic::iced::platform_specific::shell::wayland::commands::popup::destroy_popup;
use cosmic::iced::{Alignment, Length, Rectangle, Subscription, window};
use cosmic::{Element, Task, app, widget};
use std::{
    collections::HashSet,
    path::PathBuf,
    sync::LazyLock,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

// Reuse the backend's schema so provider changes remain compatible with both desktops.
use agentdeck_usage::model::{self, AccountState, AppState, QuotaWindow};
use agentdeck_usage::visibility::Prefs;

static AUTOSIZE: LazyLock<widget::Id> = LazyLock::new(widget::Id::unique);
static SCROLL_ID: LazyLock<widget::Id> = LazyLock::new(widget::Id::unique);

/// Gain for touchpad `Pixels` deltas: libcosmic's sctk backend forwards raw
/// logical Wayland axis units 1:1 (unlike winit's HiDPI-scaled deltas and
/// unlike mouse-wheel `Lines`, which scroll 60px/notch).
const TOUCHPAD_PIXEL_MULTIPLIER: f32 = 2.0;

/// Scale touchpad `Pixels` deltas in-place; mouse-wheel `Lines` stay 1x.
fn scale_wheel_delta(delta: mouse::ScrollDelta) -> mouse::ScrollDelta {
    match delta {
        mouse::ScrollDelta::Pixels { x, y } => mouse::ScrollDelta::Pixels {
            x: x * TOUCHPAD_PIXEL_MULTIPLIER,
            y: y * TOUCHPAD_PIXEL_MULTIPLIER,
        },
        mouse::ScrollDelta::Lines { .. } => delta,
    }
}

/// Vertical scroll offset for `scrollable::scroll_by` when a wheel event lands
/// on the panel icon surface instead of the popup (positive scrolls DOWN).
fn panel_scroll_offset(delta: mouse::ScrollDelta) -> Option<f32> {
    match scale_wheel_delta(delta) {
        mouse::ScrollDelta::Lines { y, .. } if y != 0.0 => Some(-y * 60.0),
        mouse::ScrollDelta::Pixels { y, .. } if y != 0.0 => Some(-y),
        _ => None,
    }
}

/// Only forward unhandled wheel events (e.g. from the panel icon surface while
/// the popup is open); popup-surface scrolling is handled directly by
/// [`SmoothScroll`] without emitting a message or rebuilding `view()`.
fn applet_events(event: Event, status: Status, window_id: window::Id) -> Option<Message> {
    if status != Status::Ignored {
        return None;
    }
    match event {
        Event::Mouse(mouse::Event::WheelScrolled { delta }) => {
            Some(Message::PanelWheel(window_id, delta))
        }
        _ => None,
    }
}

/// Wraps the popup `Scrollable` to scale touchpad `Pixels` deltas in a single
/// pass (zero `Message`s, zero `view()` rebuilds, zero two-phase stutter) and
/// accept wheel events even while the pointer is parked outside the popup.
struct SmoothScroll<'a, Message> {
    content: Element<'a, Message>,
}

impl<'a, Message> SmoothScroll<'a, Message> {
    fn new(content: impl Into<Element<'a, Message>>) -> Self {
        Self {
            content: content.into(),
        }
    }
}

impl<Message> Widget<Message, cosmic::Theme, cosmic::Renderer> for SmoothScroll<'_, Message> {
    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn size_hint(&self) -> Size<Length> {
        self.content.as_widget().size_hint()
    }

    fn tag(&self) -> cosmic::iced::core::widget::tree::Tag {
        self.content.as_widget().tag()
    }

    fn state(&self) -> cosmic::iced::core::widget::tree::State {
        self.content.as_widget().state()
    }

    fn children(&self) -> Vec<Tree> {
        self.content.as_widget().children()
    }

    fn diff(&mut self, tree: &mut Tree) {
        self.content.as_widget_mut().diff(tree);
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &cosmic::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content.as_widget_mut().layout(tree, renderer, limits)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &cosmic::Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(tree, layout, renderer, operation);
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &cosmic::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        if let Event::Mouse(mouse::Event::WheelScrolled { delta }) = event {
            let scaled = Event::Mouse(mouse::Event::WheelScrolled {
                delta: scale_wheel_delta(*delta),
            });
            let bounds = layout.bounds();
            let effective_cursor = if cursor.position_over(bounds).is_some() {
                cursor
            } else {
                mouse::Cursor::Available(bounds.center())
            };
            self.content.as_widget_mut().update(
                tree,
                &scaled,
                layout,
                effective_cursor,
                renderer,
                clipboard,
                shell,
                viewport,
            );
            return;
        }
        self.content.as_widget_mut().update(
            tree, event, layout, cursor, renderer, clipboard, shell, viewport,
        );
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &cosmic::Renderer,
    ) -> mouse::Interaction {
        self.content
            .as_widget()
            .mouse_interaction(tree, layout, cursor, viewport, renderer)
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut cosmic::Renderer,
        theme: &cosmic::Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content
            .as_widget()
            .draw(tree, renderer, theme, style, layout, cursor, viewport);
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &cosmic::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, cosmic::Theme, cosmic::Renderer>> {
        self.content
            .as_widget_mut()
            .overlay(tree, layout, renderer, viewport, translation)
    }

    fn id(&self) -> Option<widget::Id> {
        self.content.as_widget().id()
    }

    fn set_id(&mut self, id: widget::Id) {
        self.content.as_widget_mut().set_id(id);
    }
}

impl<'a, Message: 'a> From<SmoothScroll<'a, Message>> for Element<'a, Message> {
    fn from(wrapper: SmoothScroll<'a, Message>) -> Self {
        Element::new(wrapper)
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn remaining(window: &QuotaWindow, fresh: bool, timestamp: i64) -> f64 {
    if fresh
        && window
            .resets_at
            .is_some_and(|reset| reset > 0 && reset <= timestamp)
    {
        100.0
    } else {
        window.remaining_percent.clamp(0.0, 100.0)
    }
}

fn panel_window(account: &AccountState, timestamp: i64) -> Option<&QuotaWindow> {
    let general = account
        .windows
        .iter()
        .any(|w| w.scope.as_deref().is_none_or(str::is_empty));
    let pool: Vec<_> = account
        .windows
        .iter()
        .filter(|w| !general || w.scope.as_deref().is_none_or(str::is_empty))
        .collect();
    pool.iter()
        .copied()
        .find(|w| remaining(w, account.status != "stale", timestamp) <= 0.0)
        .or_else(|| {
            pool.iter()
                .copied()
                .find(|w| w.duration_mins == Some(model::FIVE_HOUR_MINS))
        })
        .or_else(|| {
            pool.iter()
                .copied()
                .find(|w| w.duration_mins == Some(model::WEEKLY_MINS))
        })
        .or_else(|| pool.first().copied())
}

fn money(value: &str, currency: &str) -> String {
    let Ok(number) = value.parse::<f64>() else {
        return format!("{value} {currency}");
    };
    if !number.is_finite() {
        return format!("{value} {currency}");
    }
    let mut amount = format!("{number:.4}");
    while amount.ends_with('0') && amount.split('.').nth(1).unwrap_or("").len() > 2 {
        amount.pop();
    }
    match currency {
        "USD" => format!("${amount}"),
        "CNY" => format!("¥{amount}"),
        "EUR" => format!("€{amount}"),
        _ => format!("{amount} {currency}"),
    }
}

fn headline(account: &AccountState, timestamp: i64) -> String {
    let suffix = if account.status == "stale" { "~" } else { "" };
    if let Some(w) = panel_window(account, timestamp) {
        format!(
            "{:.0}%{suffix}",
            remaining(w, account.status != "stale", timestamp)
        )
    } else if let Some(b) = account.balances.first() {
        format!("{}{suffix}", money(&b.total, &b.currency))
    } else {
        "!".into()
    }
}

fn reset_text(reset: Option<i64>, timestamp: i64) -> String {
    let seconds = reset.unwrap_or(timestamp).saturating_sub(timestamp);
    if seconds <= 0 {
        return String::new();
    }
    let (days, hours, minutes) = (seconds / 86400, seconds % 86400 / 3600, seconds % 3600 / 60);
    if days > 0 {
        format!("{days}д {hours}ч")
    } else if hours > 0 {
        format!("{hours}ч {minutes}м")
    } else {
        format!("{}м", minutes.max(1))
    }
}

fn updated_text(updated_at: i64, timestamp: i64) -> String {
    if updated_at <= 0 || updated_at > timestamp {
        "время обновления неизвестно".into()
    } else {
        format!(
            "обновлено {} мин назад",
            timestamp.saturating_sub(updated_at) / 60
        )
    }
}

// Fixed-width value columns keep percentages aligned across rows; the label
// gives way instead of wrapping, trimmed in the middle so a long scope still
// shows its duration.
fn quota_row<'a>(label: String, value: String, reset: Option<String>) -> Element<'a, Message> {
    let mut row = widget::row(vec![])
        .spacing(8)
        .align_y(Alignment::Center)
        .push(
            widget::text(label)
                .size(14)
                .width(Length::Fill)
                .ellipsize(Ellipsize::Middle(EllipsizeHeightLimit::Lines(1))),
        );
    let Some(reset) = reset else {
        return row.push(widget::text(value).size(14)).into();
    };
    row = row.push(
        widget::text(value)
            .size(14)
            .width(40)
            .align_x(Alignment::End),
    );
    let mut reset_cell = widget::row(vec![])
        .spacing(4)
        .align_y(Alignment::Center)
        .width(72);
    if !reset.is_empty() {
        reset_cell = reset_cell
            .push(widget::icon(widget::icon::from_name("view-refresh-symbolic").into()).size(14))
            .push(widget::text(reset).size(14));
    }
    row.push(reset_cell).into()
}

fn masked_email(email: &str) -> String {
    email
        .split_once('@')
        .map_or("•••".into(), |(local, domain)| {
            format!("{}•••@{domain}", local.chars().next().unwrap_or('•'))
        })
}

fn provider_icon(provider: &str) -> widget::icon::Handle {
    let bytes: &'static [u8] = match provider {
        "codex" => {
            include_bytes!("../icons/ai-usage-codex-symbolic.svg")
        }
        "claude" => {
            include_bytes!("../icons/ai-usage-claude-symbolic.svg")
        }
        "antigravity" => include_bytes!("../icons/ai-usage-antigravity-symbolic.svg"),
        "deepseek" => include_bytes!("../icons/ai-usage-deepseek-symbolic.svg"),
        "openrouter" => include_bytes!("../icons/ai-usage-openrouter-symbolic.svg"),
        "vercel" => include_bytes!("../icons/ai-usage-vercel-symbolic.svg"),
        "opencode-go" => include_bytes!("../icons/ai-usage-opencode-go-symbolic.svg"),
        _ => return widget::icon::from_name("dialog-information-symbolic").into(),
    };
    widget::icon::from_svg_bytes(bytes).symbolic(true)
}

fn visible_accounts<'a>(accounts: &'a [AccountState], prefs: &Prefs) -> Vec<&'a AccountState> {
    let mut shown: Vec<_> = accounts
        .iter()
        .filter(|a| !prefs.hidden.contains(&a.provider))
        .collect();
    shown.sort_by_key(|a| prefs.rank(&a.id));
    shown
}

fn new_state_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .map(|path| path.join("agentdeck").join("usage"))
        .unwrap_or_else(|| {
            std::env::temp_dir()
                .join(format!(
                    "agentdeck-{}",
                    std::env::var("USER").unwrap_or_else(|_| "unknown".into())
                ))
                .join("usage")
        })
        .join("state.json")
}

fn old_state_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .map(|path| path.join("ai-usage"))
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!(
                "ai-usage-{}",
                std::env::var("USER").unwrap_or_else(|_| "unknown".into())
            ))
        })
        .join("state.json")
}

fn pick_state_path(new: PathBuf, old: PathBuf) -> PathBuf {
    // The new path wins when present, so one build works before and after
    // cutover without an atomic release.
    if new.exists() { new } else { old }
}

fn state_path() -> PathBuf {
    pick_state_path(new_state_path(), old_state_path())
}

fn read_state() -> Result<AppState, String> {
    let bytes = std::fs::read(state_path())
        .map_err(|e| format!("Ожидание agentdeck-daemon.service: {e}"))?;
    let state: AppState =
        serde_json::from_slice(&bytes).map_err(|e| format!("Не удалось прочитать квоты: {e}"))?;
    if state.schema_version != model::SCHEMA_VERSION {
        return Err(format!(
            "Неподдерживаемая версия данных: {}",
            state.schema_version
        ));
    }
    Ok(state)
}

struct App {
    core: app::Core,
    state: Result<AppState, String>,
    popup: Option<window::Id>,
    revealed: HashSet<String>,
    prefs: Prefs,
    prefs_error: Option<String>,
}

#[derive(Debug, Clone)]
enum Message {
    Tick,
    TogglePopup(Rectangle<i32>),
    CloseRequested(window::Id),
    Reveal(String),
    PanelWheel(window::Id, mouse::ScrollDelta),
}

impl App {
    /// The agentdeck GUI edits the file; a malformed one keeps the last good set.
    fn reload_visibility(&mut self) {
        match agentdeck_usage::visibility::load(&agentdeck_usage::visibility::path()) {
            Ok(prefs) => {
                self.prefs = prefs;
                self.prefs_error = None;
            }
            Err(error) => self.prefs_error = Some(format!("Настройки видимости: {error}")),
        }
    }
}

impl cosmic::Application for App {
    type Executor = cosmic::SingleThreadExecutor;
    type Flags = ();
    type Message = Message;
    const APP_ID: &'static str = "io.github.netherguy4.AiUsage";
    fn core(&self) -> &app::Core {
        &self.core
    }
    fn core_mut(&mut self) -> &mut app::Core {
        &mut self.core
    }
    fn init(core: app::Core, _: ()) -> (Self, app::Task<Message>) {
        let mut app = Self {
            core,
            state: read_state(),
            popup: None,
            revealed: HashSet::new(),
            prefs: Prefs::default(),
            prefs_error: None,
        };
        app.reload_visibility();
        (app, Task::none())
    }
    fn style(&self) -> Option<cosmic::iced::theme::Style> {
        Some(cosmic::applet::style())
    }
    fn subscription(&self) -> Subscription<Message> {
        let tick = cosmic::iced::time::every(Duration::from_secs(5)).map(|_| Message::Tick);
        if self.popup.is_some() {
            Subscription::batch([tick, cosmic::iced::event::listen_with(applet_events)])
        } else {
            tick
        }
    }
    fn update(&mut self, message: Message) -> app::Task<Message> {
        match message {
            Message::Tick => {
                self.state = read_state();
                self.reload_visibility();
            }
            Message::Reveal(id) => {
                if !self.revealed.remove(&id) {
                    self.revealed.insert(id);
                }
            }
            Message::CloseRequested(id) => {
                if self.popup == Some(id) {
                    self.popup = None;
                    self.revealed.clear();
                }
            }
            Message::PanelWheel(window_id, delta) => {
                if self.popup.is_some_and(|p| p != window_id)
                    && let Some(y) = panel_scroll_offset(delta)
                {
                    return cosmic::iced::widget::scrollable::scroll_by(
                        SCROLL_ID.clone(),
                        cosmic::iced::widget::scrollable::AbsoluteOffset { x: 0.0, y },
                    );
                }
            }
            Message::TogglePopup(rectangle) => {
                self.revealed.clear();
                if let Some(id) = self.popup.take() {
                    return destroy_popup(id);
                }
                return cosmic::surface::surface_task(cosmic::surface::action::app_popup(
                    |_| Default::default(),
                    move |app: &mut Self| {
                        let id = window::Id::unique();
                        app.popup = Some(id);
                        let mut settings = app.core.applet.get_popup_settings(
                            app.core.main_window_id().unwrap(),
                            id,
                            None,
                            None,
                            None,
                        );
                        settings.positioner.anchor_rect = rectangle;
                        settings
                    },
                    None,
                ));
            }
        }
        Task::none()
    }
    fn view(&self) -> Element<'_, Message> {
        let timestamp = now();
        let mut row = widget::row(vec![]).spacing(12).align_y(Alignment::Center);
        if let Ok(state) = &self.state {
            let mut shown = 0;
            for account in visible_accounts(&state.accounts, &self.prefs) {
                shown += 1;
                row = row.push(
                    widget::row(vec![])
                        .spacing(4)
                        .align_y(Alignment::Center)
                        .push(widget::icon(provider_icon(&account.provider)).size(16))
                        .push(widget::text(headline(account, timestamp)).size(14)),
                );
            }
            if shown == 0 {
                row = row.push(widget::text("AI —").size(14));
            }
        } else {
            row = row.push(widget::text("AI !").size(14));
        }
        widget::autosize::autosize(
            widget::button::custom(row)
                .padding([0, self.core.applet.suggested_padding(true).0])
                .on_press_with_rectangle(|offset, bounds| {
                    Message::TogglePopup(Rectangle {
                        x: (bounds.x - offset.x) as i32,
                        y: (bounds.y - offset.y) as i32,
                        width: bounds.width.max(1.0) as i32,
                        height: bounds.height.max(1.0) as i32,
                    })
                })
                .description("AI Usage: открыть подробные квоты и балансы")
                .class(cosmic::theme::Button::AppletIcon),
            AUTOSIZE.clone(),
        )
        .into()
    }
    fn view_window(&self, _: window::Id) -> Element<'_, Message> {
        let timestamp = now();
        let mut content = widget::column(vec![])
            .spacing(8)
            .padding(12)
            .width(Length::Fill)
            .push(widget::text::title4("AI Usage"));
        match &self.state {
            Err(error) => content = content.push(widget::text(error)),
            Ok(state) if state.accounts.is_empty() => {
                content = content.push(widget::text("Нет настроенных аккаунтов"))
            }
            Ok(state) => {
                let visible = visible_accounts(&state.accounts, &self.prefs);
                for (index, account) in visible.iter().enumerate() {
                    if index > 0 {
                        content = content.push(widget::divider::horizontal::default());
                    }
                    let mut header = format!(
                        "{}{}",
                        account.name,
                        account
                            .plan
                            .as_ref()
                            .map(|p| format!(" · {p}"))
                            .unwrap_or_default()
                    );
                    if let Some(email) = &account.email {
                        header.push_str(&format!(
                            "\n{}",
                            if self.revealed.contains(&account.id) {
                                email.clone()
                            } else {
                                masked_email(email)
                            }
                        ));
                    }
                    content = content.push(
                        widget::button::custom(
                            widget::row(vec![])
                                .spacing(10)
                                .align_y(Alignment::Center)
                                .push(widget::icon(provider_icon(&account.provider)).size(20))
                                .push(widget::text(header).size(14)),
                        )
                        .on_press(Message::Reveal(account.id.clone()))
                        .class(cosmic::theme::Button::Text),
                    );
                    for w in &account.windows {
                        content = content.push(quota_row(
                            w.label.clone(),
                            format!("{:.0}%", remaining(w, account.status != "stale", timestamp)),
                            Some(reset_text(w.resets_at, timestamp)),
                        ));
                    }
                    for b in &account.balances {
                        content = content.push(quota_row(
                            "Баланс".into(),
                            money(&b.total, &b.currency),
                            None,
                        ));
                    }
                    // Rows already show depletion; the status line is for a
                    // missing or stale snapshot, as in the GNOME extension.
                    let no_data = account.windows.is_empty() && account.balances.is_empty();
                    if account.status == "stale" || (no_data && account.status != "ok") {
                        let status = match account.status.as_str() {
                            "stale" => "Данные устарели",
                            "waiting" => "Ожидание первого запроса",
                            "unauthenticated" => "Требуется вход",
                            "depleted" => "Баланс исчерпан",
                            _ => "Провайдер недоступен",
                        };
                        content = content.push(widget::text(format!(
                            "{status} · {}",
                            updated_text(account.updated_at, timestamp)
                        )));
                    }
                    // The stale status line already says it; skip the duplicate.
                    if let Some(error) =
                        account.error.as_ref().filter(|_| account.status != "stale")
                    {
                        content = content.push(widget::text(error));
                    }
                }
            }
        }
        if let Some(error) = &self.prefs_error {
            content = content.push(widget::text(error.clone()).size(13));
        }
        self.core
            .applet
            .popup_container(
                widget::container(SmoothScroll::new(
                    widget::scrollable(content)
                        .id(SCROLL_ID.clone())
                        .scroller_width(4.0)
                        .scrollbar_width(8.0)
                        .height(Length::Shrink),
                ))
                .width(360)
                .max_height(600),
            )
            .into()
    }
    fn on_close_requested(&self, id: window::Id) -> Option<Message> {
        Some(Message::CloseRequested(id))
    }
}

fn main() -> cosmic::iced::Result {
    cosmic::applet::run::<App>(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn state_path_prefers_new_runtime_file_falls_back_to_old() {
        let dir = std::env::temp_dir().join(format!(
            "agentdeck-applet-path-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let new = dir.join("new.json");
        let old = dir.join("old.json");
        std::fs::create_dir_all(&dir).unwrap();
        // Only old exists: use it (pre-cutover).
        std::fs::write(&old, "{}").unwrap();
        assert_eq!(pick_state_path(new.clone(), old.clone()), old);
        // New appears: switch without an atomic release.
        std::fs::write(&new, "{}").unwrap();
        assert_eq!(pick_state_path(new.clone(), old.clone()), new);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn future_or_missing_update_times_do_not_claim_zero_minutes_ago() {
        assert_eq!(updated_text(1_001, 1_000), "время обновления неизвестно");
        assert_eq!(updated_text(0, 1_000), "время обновления неизвестно");
        assert_eq!(updated_text(1_000, 1_000), "обновлено 0 мин назад");
        assert_eq!(updated_text(941, 1_000), "обновлено 0 мин назад");
        assert_eq!(updated_text(940, 1_000), "обновлено 1 мин назад");
    }

    #[test]
    fn panel_respects_general_exhaustion_staleness_and_resets() {
        let mut account = AccountState::error("test", "test", "codex", "");
        account.status = "ok".into();
        account.windows = vec![
            QuotaWindow::new("short", "5h", 25.0, Some(300), Some(2000)),
            QuotaWindow::new("model", "model", 100.0, Some(10080), Some(2000))
                .with_scope(Some("opus".into())),
            QuotaWindow::new("week", "week", 100.0, Some(10080), Some(1100)),
        ];
        assert_eq!(headline(&account, 1000), "0%");
        assert_eq!(headline(&account, 1200), "75%");
        account.status = "stale".into();
        assert_eq!(headline(&account, 1200), "0%~");
        assert_eq!(masked_email("name@example.com"), "n•••@example.com");
        assert_eq!(money("0.0123", "USD"), "$0.0123");
        assert_eq!(reset_text(Some(999), 1000), "");
    }

    fn test_account(id: &str, provider: &str) -> AccountState {
        AccountState::error(id, id, provider, "")
    }

    #[test]
    fn wheel_scaling_boosts_touchpad_pixels_and_preserves_mouse_lines() {
        assert_eq!(
            scale_wheel_delta(mouse::ScrollDelta::Lines { x: 0.0, y: -1.0 }),
            mouse::ScrollDelta::Lines { x: 0.0, y: -1.0 }
        );
        assert_eq!(
            scale_wheel_delta(mouse::ScrollDelta::Pixels { x: 2.0, y: -10.0 }),
            mouse::ScrollDelta::Pixels {
                x: 2.0 * TOUCHPAD_PIXEL_MULTIPLIER,
                y: -10.0 * TOUCHPAD_PIXEL_MULTIPLIER,
            }
        );
        assert_eq!(
            panel_scroll_offset(mouse::ScrollDelta::Lines { x: 0.0, y: -1.0 }),
            Some(60.0)
        );
        assert_eq!(
            panel_scroll_offset(mouse::ScrollDelta::Pixels { x: 0.0, y: -10.0 }),
            Some(10.0 * TOUCHPAD_PIXEL_MULTIPLIER)
        );
        assert_eq!(
            panel_scroll_offset(mouse::ScrollDelta::Lines { x: 0.0, y: 0.0 }),
            None
        );
        assert_eq!(
            panel_scroll_offset(mouse::ScrollDelta::Pixels { x: 0.0, y: 0.0 }),
            None
        );
    }

    #[test]
    fn hidden_providers_are_filtered() {
        let accounts = vec![
            test_account("a", "codex"),
            test_account("b", "claude"),
            test_account("c", "codex"),
        ];
        let mut prefs = Prefs {
            order: vec!["c".into()],
            ..Prefs::default()
        };
        let ids = |v: Vec<&AccountState>| v.iter().map(|a| a.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(visible_accounts(&accounts, &prefs)), ["c", "a", "b"]);
        prefs.hidden.insert("codex".to_owned());
        let visible = visible_accounts(&accounts, &prefs);
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].provider, "claude");
        prefs.hidden.insert("claude".to_owned());
        assert!(visible_accounts(&accounts, &prefs).is_empty());
    }
}
