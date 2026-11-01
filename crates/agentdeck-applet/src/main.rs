use cosmic::iced::core::text::{Ellipsize, EllipsizeHeightLimit};
use cosmic::iced::platform_specific::shell::wayland::commands::popup::destroy_popup;
use cosmic::iced::{Alignment, Length, Rectangle, Subscription, window};
use cosmic::{Element, Task, app, widget};
use std::{
    collections::{BTreeSet, HashSet},
    path::PathBuf,
    sync::LazyLock,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

// Reuse the backend's schema so provider changes remain compatible with both desktops.
use agentdeck_usage::model::{self, AccountState, AppState, QuotaWindow};

static AUTOSIZE: LazyLock<widget::Id> = LazyLock::new(widget::Id::unique);

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

fn visible_accounts<'a>(
    accounts: &'a [AccountState],
    hidden: &BTreeSet<String>,
) -> Vec<&'a AccountState> {
    accounts
        .iter()
        .filter(|a| !hidden.contains(&a.provider))
        .collect()
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
    hidden: BTreeSet<String>,
    prefs_error: Option<String>,
}

#[derive(Debug, Clone)]
enum Message {
    Tick,
    TogglePopup(Rectangle<i32>),
    CloseRequested(window::Id),
    Reveal(String),
}

impl App {
    /// The agentdeck GUI edits the file; a malformed one keeps the last good set.
    fn reload_visibility(&mut self) {
        match agentdeck_usage::visibility::load(&agentdeck_usage::visibility::path()) {
            Ok(hidden) => {
                self.hidden = hidden;
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
            hidden: BTreeSet::new(),
            prefs_error: None,
        };
        app.reload_visibility();
        (app, Task::none())
    }
    fn style(&self) -> Option<cosmic::iced::theme::Style> {
        Some(cosmic::applet::style())
    }
    fn subscription(&self) -> Subscription<Message> {
        cosmic::iced::time::every(Duration::from_secs(5)).map(|_| Message::Tick)
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
            for account in visible_accounts(&state.accounts, &self.hidden) {
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
                let visible = visible_accounts(&state.accounts, &self.hidden);
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
                    if let Some(error) = &account.error {
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
                widget::container(
                    widget::scrollable(content)
                        .scroller_width(4.0)
                        .scrollbar_width(8.0)
                        .height(Length::Shrink),
                )
                .width(380)
                .max_height(480),
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
    fn hidden_providers_are_filtered() {
        let accounts = vec![
            test_account("a", "codex"),
            test_account("b", "claude"),
            test_account("c", "codex"),
        ];
        let mut hidden = BTreeSet::new();
        hidden.insert("codex".to_owned());
        let visible = visible_accounts(&accounts, &hidden);
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].provider, "claude");
        hidden.insert("claude".to_owned());
        assert!(visible_accounts(&accounts, &hidden).is_empty());
    }
}
