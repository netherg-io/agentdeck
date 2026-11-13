use serde::{Deserialize, Serialize};

/// Формат `state.json`. Несовместимое изменение требует увеличения версии и
/// либо обратной совместимости UI, либо явной миграции.
///
/// v2: вместо фиксированных `five_hour`/`weekly` — открытый список `windows`.
/// Провайдеры добавляют и убирают лимиты (например, отдельный недельный лимит
/// на конкретную модель), и зашитый набор полей заставлял бы править код на
/// каждое такое изменение.
pub const SCHEMA_VERSION: u32 = 2;

pub const FIVE_HOUR_MINS: u64 = 300;
pub const WEEKLY_MINS: u64 = 10_080;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaWindow {
    /// Ключ провайдера как есть: `five_hour`, `seven_day_opus`, `primary`.
    pub key: String,
    /// Готовая к показу подпись.
    pub label: String,
    pub used_percent: f64,
    pub remaining_percent: f64,
    pub duration_mins: Option<u64>,
    pub resets_at: Option<i64>,
    /// Модель, к которой привязан лимит, если он не общий.
    ///
    /// Исчерпанный общий лимит блокирует работу, а исчерпанный лимит одной
    /// модели — нет: можно переключиться. UI обязан их различать, и опираться
    /// на формат ключа для этого слишком хрупко.
    #[serde(default)]
    pub scope: Option<String>,
}

impl QuotaWindow {
    pub fn new(
        key: impl Into<String>,
        label: impl Into<String>,
        used_percent: f64,
        duration_mins: Option<u64>,
        resets_at: Option<i64>,
    ) -> Self {
        // NaN не сравнивается, поэтому clamp его не спасёт: считаем такой
        // процент отсутствующим и показываем полный лимит.
        let used_percent = if used_percent.is_nan() {
            0.0
        } else {
            used_percent.clamp(0.0, 100.0)
        };
        Self {
            key: key.into(),
            label: label.into(),
            used_percent,
            remaining_percent: 100.0 - used_percent,
            duration_mins,
            resets_at,
            scope: None,
        }
    }

    pub fn with_scope(mut self, scope: Option<String>) -> Self {
        self.scope = scope;
        self
    }
}

/// Человеческая подпись окна по длительности. Используется провайдерами,
/// которые сообщают длительность, но не имя (Codex).
pub fn label_for_duration(duration_mins: Option<u64>) -> String {
    match duration_mins {
        Some(FIVE_HOUR_MINS) => "5 часов".to_owned(),
        Some(WEEKLY_MINS) => "Неделя".to_owned(),
        Some(mins) if mins % (60 * 24) == 0 => format!("{} д", mins / (60 * 24)),
        Some(mins) if mins % 60 == 0 => format!("{} ч", mins / 60),
        Some(mins) => format!("{mins} мин"),
        None => "Лимит".to_owned(),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BalanceInfo {
    pub currency: String,
    pub total: String,
    pub granted: Option<String>,
    pub topped_up: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountState {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub status: String,
    pub plan: Option<String>,
    pub email: Option<String>,
    pub model: Option<String>,
    /// Все лимиты, которые вернул провайдер, в порядке показа.
    #[serde(default)]
    pub windows: Vec<QuotaWindow>,
    pub balances: Vec<BalanceInfo>,
    pub error: Option<String>,
    pub updated_at: i64,
}

impl AccountState {
    pub fn error(id: &str, name: &str, provider: &str, message: impl Into<String>) -> Self {
        Self {
            id: id.to_owned(),
            name: name.to_owned(),
            provider: provider.to_owned(),
            status: "error".to_owned(),
            plan: None,
            email: None,
            model: None,
            windows: Vec::new(),
            balances: Vec::new(),
            error: Some(message.into()),
            updated_at: crate::util::unix_now(),
        }
    }

    fn has_data(&self) -> bool {
        !self.windows.is_empty() || !self.balances.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppState {
    pub schema_version: u32,
    pub updated_at: i64,
    pub accounts: Vec<AccountState>,
}

pub(crate) fn is_t3_subscription(provider: &str) -> bool {
    matches!(provider, "claude" | "codex" | "antigravity" | "opencode-go")
}

/// Сохраняет последние удачные данные при разовом сбое провайдера баланса.
///
/// Без этого одна сетевая ошибка стирала бы баланс из панели, хотя минутной
/// давности цифры всё ещё полезны. T3-провайдеры квот (`claude`, `codex`,
/// `antigravity`, `opencode-go`) уже кешируются на диске самим T3: ошибка без
/// окон (например, отключённый инстанс или битый кеш) никогда не должна
/// воскрешать старые цифры.
pub fn merge_with_previous(
    previous: Option<AppState>,
    fresh: Vec<AccountState>,
) -> Vec<AccountState> {
    let Some(previous) = previous else {
        return fresh;
    };
    if previous.schema_version != SCHEMA_VERSION {
        return fresh;
    }

    fresh
        .into_iter()
        .map(|account| {
            if account.status != "error"
                || account.has_data()
                || is_t3_subscription(&account.provider)
            {
                return account;
            }
            let Some(old) = previous
                .accounts
                .iter()
                .find(|item| item.id == account.id && item.provider == account.provider)
            else {
                return account;
            };
            if !old.has_data() {
                return account;
            }

            AccountState {
                status: "stale".to_owned(),
                plan: old.plan.clone(),
                email: old.email.clone(),
                model: old.model.clone(),
                windows: old.windows.clone(),
                balances: old.balances.clone(),
                updated_at: old.updated_at,
                ..account
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(key: &str, used: f64) -> QuotaWindow {
        QuotaWindow::new(key, "Неделя", used, Some(WEEKLY_MINS), Some(42))
    }

    fn good(id: &str) -> AccountState {
        AccountState {
            id: id.to_owned(),
            name: "Codex".to_owned(),
            provider: "codex".to_owned(),
            status: "ok".to_owned(),
            plan: Some("plus".to_owned()),
            email: Some("user@example.com".to_owned()),
            model: None,
            windows: vec![window("primary", 54.0)],
            balances: Vec::new(),
            error: None,
            updated_at: 1_000,
        }
    }

    fn previous(accounts: Vec<AccountState>) -> Option<AppState> {
        Some(AppState {
            schema_version: SCHEMA_VERSION,
            updated_at: 1_000,
            accounts,
        })
    }

    #[test]
    fn clamps_percentage_into_range() {
        assert_eq!(window("k", -5.0).used_percent, 0.0);
        assert_eq!(window("k", -5.0).remaining_percent, 100.0);
        assert_eq!(window("k", 150.0).used_percent, 100.0);
        assert_eq!(window("k", 150.0).remaining_percent, 0.0);
    }

    #[test]
    fn treats_nan_as_zero_used() {
        let window = window("k", f64::NAN);
        assert_eq!(window.used_percent, 0.0);
        assert_eq!(window.remaining_percent, 100.0);
    }

    #[test]
    fn labels_common_durations() {
        assert_eq!(label_for_duration(Some(FIVE_HOUR_MINS)), "5 часов");
        assert_eq!(label_for_duration(Some(WEEKLY_MINS)), "Неделя");
        assert_eq!(label_for_duration(Some(60)), "1 ч");
        assert_eq!(label_for_duration(Some(2880)), "2 д");
        assert_eq!(label_for_duration(Some(45)), "45 мин");
        assert_eq!(label_for_duration(None), "Лимит");
    }

    fn good_balance(id: &str) -> AccountState {
        AccountState {
            id: id.to_owned(),
            name: "OpenRouter".to_owned(),
            provider: "openrouter".to_owned(),
            status: "ok".to_owned(),
            plan: Some("Credits".to_owned()),
            email: None,
            model: None,
            windows: Vec::new(),
            balances: vec![BalanceInfo {
                currency: "USD".to_owned(),
                total: "12.50".to_owned(),
                granted: None,
                topped_up: None,
            }],
            error: None,
            updated_at: 1_000,
        }
    }

    #[test]
    fn keeps_last_good_data_when_balance_provider_fails() {
        let fresh = vec![AccountState::error(
            "openrouter-main",
            "OpenRouter",
            "openrouter",
            "сеть",
        )];
        let merged = merge_with_previous(previous(vec![good_balance("openrouter-main")]), fresh);

        assert_eq!(merged[0].status, "stale");
        assert_eq!(merged[0].error.as_deref(), Some("сеть"));
        assert_eq!(merged[0].balances[0].total, "12.50");
        assert_eq!(merged[0].plan.as_deref(), Some("Credits"));
        // Возраст берётся от удачного ответа, иначе UI покажет свежесть, которой нет.
        assert_eq!(merged[0].updated_at, 1_000);
    }

    #[test]
    fn subscription_error_never_resurfaces_previous_windows() {
        for (id, provider) in [
            ("claude-main", "claude"),
            ("codex-main", "codex"),
            ("agy-main", "antigravity"),
            ("opencode-go", "opencode-go"),
        ] {
            let mut prev = good(id);
            prev.provider = provider.to_owned();
            let fresh = vec![AccountState::error(id, id, provider, "disabled in T3")];
            let merged = merge_with_previous(previous(vec![prev]), fresh);
            assert_eq!(merged[0].status, "error", "{provider}");
            assert!(merged[0].windows.is_empty(), "{provider}");
            assert_eq!(
                merged[0].error.as_deref(),
                Some("disabled in T3"),
                "{provider}"
            );
        }
    }

    #[test]
    fn successful_refresh_overwrites_old_data() {
        let mut fresh = good("codex-main");
        fresh.windows = vec![window("primary", 70.0)];
        let merged = merge_with_previous(previous(vec![good("codex-main")]), vec![fresh]);
        assert_eq!(merged[0].status, "ok");
        assert_eq!(merged[0].windows[0].used_percent, 70.0);
    }

    #[test]
    fn a_disappearing_limit_is_not_resurrected_from_history() {
        // Провайдер может убрать временный лимит. Успешный ответ без него —
        // это не сбой, и подмешивать старое окно нельзя.
        let mut fresh = good("codex-main");
        fresh.windows = vec![window("primary", 10.0)];
        let mut old = good("codex-main");
        old.windows = vec![window("primary", 5.0), window("seven_day_fable", 80.0)];

        let merged = merge_with_previous(previous(vec![old]), vec![fresh]);
        assert_eq!(merged[0].windows.len(), 1);
        assert_eq!(merged[0].windows[0].key, "primary");
    }

    #[test]
    fn error_without_history_stays_an_error() {
        let fresh = vec![AccountState::error(
            "openrouter-new",
            "OpenRouter",
            "openrouter",
            "сеть",
        )];
        let merged = merge_with_previous(previous(vec![good_balance("openrouter-main")]), fresh);
        assert_eq!(merged[0].status, "error");
        assert!(merged[0].balances.is_empty());
    }

    #[test]
    fn does_not_borrow_data_from_a_different_provider_with_the_same_id() {
        let mut other = good_balance("shared");
        other.provider = "deepseek".to_owned();
        let fresh = vec![AccountState::error(
            "shared",
            "OpenRouter",
            "openrouter",
            "сеть",
        )];
        let merged = merge_with_previous(previous(vec![other]), fresh);
        assert_eq!(merged[0].status, "error");
    }

    #[test]
    fn ignores_state_written_by_a_different_schema_version() {
        let stale_schema = Some(AppState {
            schema_version: SCHEMA_VERSION + 1,
            updated_at: 1_000,
            accounts: vec![good_balance("openrouter-main")],
        });
        let fresh = vec![AccountState::error(
            "openrouter-main",
            "OpenRouter",
            "openrouter",
            "сеть",
        )];
        let merged = merge_with_previous(stale_schema, fresh);
        assert_eq!(merged[0].status, "error");
    }

    #[test]
    fn waiting_claude_account_is_not_backfilled() {
        // status 'waiting' — не ошибка, подменять его старыми данными нельзя.
        let mut fresh = AccountState::error("claude-main", "Claude", "claude", "ждём");
        fresh.status = "waiting".to_owned();
        let mut old = good("claude-main");
        old.provider = "claude".to_owned();

        let merged = merge_with_previous(previous(vec![old]), vec![fresh]);
        assert_eq!(merged[0].status, "waiting");
        assert!(merged[0].windows.is_empty());
    }
}
