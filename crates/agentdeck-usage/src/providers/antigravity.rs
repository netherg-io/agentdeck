use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::model::{AccountState, FIVE_HOUR_MINS, QuotaWindow, WEEKLY_MINS};

/// Минимальный интервал между прямыми сетевыми запросами к Google API квоты (2 минуты),
/// чтобы не создавать лишней нагрузки при быстром тике демона или частых `ai-usage once`.
pub const MIN_POLL_SECONDS: i64 = 120;
const REQUEST_TIMEOUT_SECS: u64 = 15;

#[derive(Debug, Clone, Deserialize)]
pub struct AcpToken {
    pub client_id: String,
    pub client_secret: String,
    pub refresh_token: String,
    #[serde(default = "default_token_uri")]
    pub token_uri: String,
    #[serde(default = "default_project_id")]
    pub project_id: String,
}

fn default_token_uri() -> String {
    "https://oauth2.googleapis.com/token".to_owned()
}

fn default_project_id() -> String {
    "aicode-consumers".to_owned()
}

#[derive(Debug, Clone, Deserialize)]
pub struct QuotaSummaryResponse {
    #[serde(default)]
    pub groups: Vec<QuotaGroup>,
    #[serde(default)]
    pub buckets: Vec<QuotaBucket>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct QuotaGroup {
    #[serde(rename = "displayName")]
    #[allow(dead_code)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub buckets: Vec<QuotaBucket>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct QuotaBucket {
    #[serde(rename = "bucketId")]
    pub bucket_id: Option<String>,
    #[serde(rename = "remainingFraction")]
    pub remaining_fraction: Option<f64>,
    #[serde(rename = "resetTime")]
    pub reset_time: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StatusPayload {
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    plan_tier: Option<String>,
    #[serde(default)]
    model: Option<ModelInfo>,
    #[serde(default)]
    quota: BTreeMap<String, QuotaInfo>,
}

#[derive(Debug, Deserialize)]
struct ModelInfo {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct QuotaInfo {
    #[serde(default)]
    remaining_fraction: Option<f64>,
    /// Абсолютный момент сброса. Запасной источник: `agy` 1.1.8 присылает и
    /// его, и `reset_in_seconds`, но опираться на одно поле рискованно.
    #[serde(default)]
    reset_time: Option<String>,
    #[serde(default)]
    reset_in_seconds: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub email: Option<String>,
    pub plan: Option<String>,
    pub model: Option<String>,
    pub windows: Vec<QuotaWindow>,
    pub updated_at: i64,
}

/// Снимок живёт вне tmpfs: `agy` / Google API обновляют его периодически, и после
/// перезагрузки восстановить квоту без этого файла невозможно до первого запроса.
/// File name of the quota snapshot, so `doctor` can report its age from an
/// injectable directory instead of the production state dir.
pub fn snapshot_file_name(account_id: &str) -> String {
    format!("antigravity-usage-{account_id}.json")
}

pub fn snapshot_path(account_id: &str) -> PathBuf {
    crate::util::persistent_state_dir().join(snapshot_file_name(account_id))
}

/// Путь, по которому снимок лежал до переезда на постоянный каталог.
fn legacy_snapshot_path(account_id: &str) -> PathBuf {
    crate::util::runtime_dir().join(format!("antigravity-usage-{account_id}.json"))
}

/// Official `agy` custom status-line hook.
///
/// Antigravity sends account and quota metadata as JSON on stdin. The hook
/// stores only the display fields needed by the widget; credentials and the
/// full provider response never leave `agy`.
pub fn status_line_from_payload(account_id: &str, raw: &str) -> Result<String> {
    crate::util::validate_id(account_id)?;
    let now = crate::util::unix_now();
    let Some(mut snapshot) = snapshot_from_payload(raw, now)? else {
        return Ok("AI Usage · ожидание квоты".to_owned());
    };

    let line = status_line_text(&snapshot);
    let path = snapshot_path(account_id);
    let prev: Option<Snapshot> = fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok());
    drop_argon_view_of_shared_windows(&mut snapshot);
    carry_missing_windows(&mut snapshot, prev.as_ref(), now);
    let bytes = serde_json::to_vec_pretty(&snapshot)?;
    crate::util::atomic_write_mode(&path, &bytes, Some(0o600))?;
    Ok(line)
}

/// With Argon active, agy reports gemini-5h/gemini-weekly as untouched windows that
/// restart on every call, while the quota API shows the real shared Flash pool
/// (2026-10-16). Argon spends only its own bucket, so its view of them is dropped.
fn drop_argon_view_of_shared_windows(snapshot: &mut Snapshot) {
    let argon = snapshot
        .model
        .as_deref()
        .is_some_and(|m| m.to_ascii_lowercase().contains("argon"));
    if argon {
        snapshot
            .windows
            .retain(|w| !matches!(w.key.as_str(), "gemini-5h" | "gemini-weekly"));
    }
}

/// Ищет файл `acp_token.json` в системе.
///
/// Порядок поиска:
/// 1. Явно указанный в конфигурации путь `token_file`.
/// 2. T3 Code: `~/.t3/userdata/providers/antigravity/*/antigravity-acp/acp_token.json`.
/// 3. Gemini harness: `~/.gemini/antigravity-acp/acp_token.json`.
/// 4. Стандартный каталог: `~/.config/antigravity/acp_token.json`.
pub fn find_token_file(custom: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = custom {
        let expanded = crate::util::expand_home(path);
        if expanded.is_file() {
            return Some(expanded);
        }
        return None;
    }

    let home = dirs::home_dir()?;

    // 1. T3 Code: ~/.t3/userdata/providers/antigravity/*/antigravity-acp/acp_token.json
    let t3_dir = home.join(".t3/userdata/providers/antigravity");
    if let Ok(entries) = fs::read_dir(t3_dir) {
        for entry in entries.flatten() {
            let candidate = entry.path().join("antigravity-acp").join("acp_token.json");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    // 2. Gemini CLI: ~/.gemini/antigravity-acp/acp_token.json
    let gemini_candidate = home.join(".gemini/antigravity-acp/acp_token.json");
    if gemini_candidate.is_file() {
        return Some(gemini_candidate);
    }

    // 3. Fallback: ~/.config/antigravity/acp_token.json
    let config_candidate = home.join(".config/antigravity/acp_token.json");
    if config_candidate.is_file() {
        return Some(config_candidate);
    }

    None
}

pub fn read_token_file(path: &Path) -> Result<AcpToken> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("Не удалось прочитать токен {}", path.display()))?;
    serde_json::from_str(&raw)
        .with_context(|| format!("Некорректный JSON токена в {}", path.display()))
}

async fn refresh_access_token(client: &reqwest::Client, acp: &AcpToken) -> Result<String> {
    let params = [
        ("client_id", acp.client_id.as_str()),
        ("client_secret", acp.client_secret.as_str()),
        ("refresh_token", acp.refresh_token.as_str()),
        ("grant_type", "refresh_token"),
    ];
    let response = client
        .post(&acp.token_uri)
        .form(&params)
        .send()
        .await
        .context("Не удалось связаться с Google OAuth сервером")?;

    // Тело ответа OAuth не выводим: ошибка уходит в состояние, журнал и UI.
    if !response.status().is_success() {
        bail!(
            "Ошибка обновления Google OAuth токена (HTTP {})",
            response.status()
        );
    }

    #[derive(Deserialize)]
    struct OauthResponse {
        access_token: String,
    }

    let parsed: OauthResponse = response
        .json()
        .await
        .context("Некорректный JSON ответа Google OAuth")?;
    Ok(parsed.access_token)
}

async fn fetch_user_email(client: &reqwest::Client, access_token: &str) -> Option<String> {
    let response = client
        .get("https://www.googleapis.com/oauth2/v1/userinfo")
        .bearer_auth(access_token)
        .send()
        .await
        .ok()?;

    if !response.status().is_success() {
        return None;
    }

    #[derive(Deserialize)]
    struct UserInfo {
        email: Option<String>,
    }
    response.json::<UserInfo>().await.ok().and_then(|u| u.email)
}

async fn request_quota_summary(
    client: &reqwest::Client,
    access_token: &str,
    project_id: &str,
) -> Result<QuotaSummaryResponse> {
    // The daily endpoint answers successfully but omits the Gemini buckets (checked
    // 2026-10-16), so production goes first and buckets from both are merged.
    let endpoints = [
        "https://cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary",
        "https://daily-cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary",
    ];

    let mut merged: Option<QuotaSummaryResponse> = None;
    let mut last_err = None;
    for endpoint in endpoints {
        let resp = client
            .post(endpoint)
            .bearer_auth(access_token)
            // The old "antigravity/1.1.9" UA gets only the 3p buckets back (2026-11-13).
            .header(
                "User-Agent",
                "antigravity/hub/2.24.0 (aidev_client; os_type=linux; arch=x64; cl=963137146)",
            )
            .json(&serde_json::json!({ "project": project_id }))
            .send()
            .await;

        match resp {
            Ok(res) if res.status().is_success() => {
                match res.json::<QuotaSummaryResponse>().await {
                    Ok(parsed) => merged = Some(merge_quota_responses(merged, parsed)),
                    Err(_) => last_err = Some("Некорректный JSON ответа квоты Google".to_owned()),
                }
            }
            Ok(res) => {
                last_err = Some(format!("HTTP {}", res.status()));
            }
            Err(e) => {
                last_err = Some(format!("Сетевая ошибка: {e}"));
            }
        }
    }

    if let Some(merged) = merged {
        return Ok(merged);
    }
    bail!(last_err.unwrap_or_else(|| "Не удалось получить квоту от Google API".to_owned()))
}

/// Flattens both responses; on a duplicate id, the more depleted bucket wins
/// (daily tracks Gemini Flash/Pro usage while production tracks Argon).
fn merge_quota_responses(
    first: Option<QuotaSummaryResponse>,
    next: QuotaSummaryResponse,
) -> QuotaSummaryResponse {
    let flatten = |r: QuotaSummaryResponse| {
        r.buckets
            .into_iter()
            .chain(r.groups.into_iter().flat_map(|g| g.buckets))
            .collect::<Vec<_>>()
    };
    let mut buckets = first.map(flatten).unwrap_or_default();
    for bucket in flatten(next) {
        if let Some(existing) = buckets.iter_mut().find(|b| b.bucket_id == bucket.bucket_id) {
            if bucket.remaining_fraction < existing.remaining_fraction {
                *existing = bucket;
            }
        } else {
            buckets.push(bucket);
        }
    }
    QuotaSummaryResponse {
        groups: Vec::new(),
        buckets,
    }
}

/// Keeps polled windows that an agy hook payload omits (Argon drops the Gemini
/// pool) until they reset. Live polls never carry: the quota API returns every
/// bucket, and a carried window froze at its old value until reset.
fn carry_missing_windows(snapshot: &mut Snapshot, prev: Option<&Snapshot>, now: i64) {
    let Some(prev) = prev else { return };
    let active = active_family(snapshot.model.as_deref());
    for window in &prev.windows {
        let fresh = window.resets_at.is_some_and(|reset| reset > now);
        if fresh && !snapshot.windows.iter().any(|w| w.key == window.key) {
            snapshot.windows.push(window.clone());
        }
    }
    sort_windows(&mut snapshot.windows, active);
}

pub fn snapshot_from_quota_response(
    response: QuotaSummaryResponse,
    email: Option<String>,
    plan: Option<String>,
    model: Option<String>,
    now: i64,
) -> Option<Snapshot> {
    let mut quota_map: BTreeMap<String, QuotaInfo> = BTreeMap::new();
    for bucket in response
        .buckets
        .into_iter()
        .chain(response.groups.into_iter().flat_map(|g| g.buckets))
    {
        if let Some(id) = bucket.bucket_id {
            quota_map.insert(
                id,
                QuotaInfo {
                    remaining_fraction: bucket.remaining_fraction,
                    reset_time: bucket.reset_time,
                    reset_in_seconds: None,
                },
            );
        }
    }

    let active = active_family(model.as_deref());
    let mut windows: Vec<QuotaWindow> = quota_map
        .into_iter()
        .filter_map(|(key, quota)| quota_window(&key, quota, now, active))
        .collect();

    if windows.is_empty() {
        return None;
    }

    sort_windows(&mut windows, active);

    Some(Snapshot {
        email: email.filter(|value| !value.trim().is_empty()),
        plan: plan.filter(|value| !value.trim().is_empty()),
        model,
        windows,
        updated_at: now,
    })
}

pub async fn live_quota(token_file: &Path, prev: Option<&Snapshot>) -> Result<Snapshot> {
    let acp = read_token_file(token_file)?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .build()
        .context("Не удалось инициализировать HTTP клиент")?;

    let access_token = refresh_access_token(&client, &acp).await?;
    let email = match fetch_user_email(&client, &access_token).await {
        Some(email) => Some(email),
        None => prev.and_then(|p| p.email.clone()),
    };

    let quota_res = request_quota_summary(&client, &access_token, &acp.project_id).await?;
    let now = crate::util::unix_now();
    let plan = prev
        .and_then(|p| p.plan.clone())
        .or_else(|| Some("Google AI Pro".to_owned()));
    let model = detect_model(prev);

    snapshot_from_quota_response(quota_res, email, plan, model, now)
        .context("Google API не вернул бакетов квоты")
}

fn detect_model(prev: Option<&Snapshot>) -> Option<String> {
    if let Some(model) = prev.and_then(|p| p.model.clone()) {
        return Some(model);
    }
    if let Some(home) = dirs::home_dir() {
        let settings_path = home.join(".gemini/antigravity-cli/settings.json");
        if let Ok(raw) = fs::read_to_string(&settings_path)
            && let Ok(val) = serde_json::from_str::<serde_json::Value>(&raw)
            && let Some(m) = val.get("model").and_then(|m| m.as_str())
            && !m.trim().is_empty()
        {
            return Some(m.to_owned());
        }
    }
    Some("Gemini 3.8 Flash (High)".to_owned())
}

pub async fn fetch(
    id: &str,
    name: &str,
    token_file: Option<&Path>,
    stale_seconds: i64,
) -> Result<AccountState> {
    let path = snapshot_path(id);
    adopt_legacy_snapshot(id, &path);

    let prev_snapshot: Option<Snapshot> = if path.exists() {
        fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
    } else {
        None
    };

    let now = crate::util::unix_now();
    let mut last_error = None;
    let mut current_snapshot = prev_snapshot;

    if let Some(token_path) = find_token_file(token_file) {
        let should_poll = quota_poll_due(current_snapshot.as_ref(), now);

        if should_poll {
            match live_quota(&token_path, current_snapshot.as_ref()).await {
                Ok(new_snapshot) => {
                    if let Ok(bytes) = serde_json::to_vec_pretty(&new_snapshot) {
                        let _ = crate::util::atomic_write_mode(&path, &bytes, Some(0o600));
                    }
                    current_snapshot = Some(new_snapshot);
                }
                Err(err) => {
                    last_error = Some(err.to_string());
                }
            }
        }
    }

    let Some(snapshot) = current_snapshot else {
        if let Some(err) = last_error {
            return Ok(AccountState::error(
                id,
                name,
                "antigravity",
                format!("Ошибка Google API: {err}"),
            ));
        }
        let mut state = AccountState::error(
            id,
            name,
            "antigravity",
            "Не найден токен (acp_token.json) и нет данных от agy status-line hook",
        );
        state.status = "waiting".to_owned();
        return Ok(state);
    };

    Ok(state_from_snapshot(
        id,
        name,
        snapshot,
        last_error,
        crate::util::unix_now(),
        stale_seconds,
    ))
}

fn quota_poll_due(snapshot: Option<&Snapshot>, now: i64) -> bool {
    snapshot
        .is_none_or(|s| now < s.updated_at || now.saturating_sub(s.updated_at) >= MIN_POLL_SECONDS)
}

/// Неудачный живой опрос сразу помечает кэш устаревшим, сохраняя последнюю
/// удачную квоту и её `updated_at`. Без опроса (только hook) — прежний
/// порог по возрасту снимка.
fn state_from_snapshot(
    id: &str,
    name: &str,
    snapshot: Snapshot,
    poll_error: Option<String>,
    now: i64,
    stale_seconds: i64,
) -> AccountState {
    let age = now.saturating_sub(snapshot.updated_at).max(0);
    let stale = poll_error.is_some()
        || snapshot.updated_at > now
        || age > stale_threshold(&snapshot.windows, stale_seconds);

    let error = if let Some(err) = poll_error {
        Some(format!(
            "Ошибка Google API: {err}; данные от {} назад",
            format_age(age)
        ))
    } else if snapshot.updated_at > now {
        Some("Системные часы изменились; ожидание свежей квоты Google".to_owned())
    } else if stale {
        Some(format!(
            "agy не обновлял квоту {}; открой agy или выполни /usage",
            format_age(age)
        ))
    } else {
        None
    };

    // The panel always represents Gemini; CLI snapshots still follow the active model.
    let mut windows = snapshot.windows;
    for window in &mut windows {
        let family = family_for_key(&window.key);
        if family == "gemini" {
            window.scope = None;
        } else if let Some(scope) = scope_for_family(&family) {
            window.scope = Some(scope);
        }
    }
    sort_windows(&mut windows, Some("gemini"));

    AccountState {
        id: id.to_owned(),
        name: name.to_owned(),
        provider: "antigravity".to_owned(),
        status: if stale { "stale" } else { "ok" }.to_owned(),
        plan: snapshot.plan.as_deref().map(crate::util::humanize_plan),
        email: snapshot.email,
        model: snapshot.model,
        windows,
        balances: Vec::new(),
        error,
        updated_at: snapshot.updated_at,
    }
}

/// Переносит снимок из tmpfs, если он остался там от прошлой версии.
fn adopt_legacy_snapshot(account_id: &str, path: &PathBuf) {
    if path.exists() {
        return;
    }
    let legacy = legacy_snapshot_path(account_id);
    if legacy == *path || !legacy.exists() {
        return;
    }
    let Ok(bytes) = fs::read(&legacy) else {
        return;
    };
    if crate::util::atomic_write_mode(path, &bytes, Some(0o600)).is_ok() {
        let _ = fs::remove_file(&legacy);
    }
}

/// Через сколько снимок `agy` перестаёт что-либо значить.
fn stale_threshold(windows: &[QuotaWindow], stale_seconds: i64) -> i64 {
    windows
        .iter()
        .filter_map(|window| window.duration_mins)
        .min()
        .and_then(|mins| i64::try_from(mins).ok())
        .map(|mins| mins.saturating_mul(60))
        .unwrap_or(stale_seconds)
        .max(stale_seconds)
}

fn snapshot_from_payload(raw: &str, now: i64) -> Result<Option<Snapshot>> {
    let payload: StatusPayload =
        serde_json::from_str(raw).context("Некорректный JSON от agy status line")?;
    let model = payload.model.and_then(|model| {
        model
            .display_name
            .filter(|value| !value.trim().is_empty())
            .or_else(|| model.id.filter(|value| !value.trim().is_empty()))
    });

    let active = active_family(model.as_deref());
    let mut windows: Vec<QuotaWindow> = payload
        .quota
        .into_iter()
        .filter_map(|(key, quota)| quota_window(&key, quota, now, active))
        .collect();

    if windows.is_empty() {
        return Ok(None);
    }

    sort_windows(&mut windows, active);

    Ok(Some(Snapshot {
        email: payload.email.filter(|value| !value.trim().is_empty()),
        plan: payload.plan_tier.filter(|value| !value.trim().is_empty()),
        model,
        windows,
        updated_at: now,
    }))
}

fn quota_window(
    key: &str,
    quota: QuotaInfo,
    now: i64,
    active: Option<&str>,
) -> Option<QuotaWindow> {
    let remaining = quota.remaining_fraction?;
    if !remaining.is_finite() {
        return None;
    }
    let remaining_percent = (remaining * 100.0).clamp(0.0, 100.0);
    let resets_at = quota
        .reset_in_seconds
        .filter(|seconds| *seconds > 0)
        .map(|seconds| now.saturating_add(seconds))
        .or_else(|| quota.reset_time.as_deref().and_then(parse_rfc3339));

    let family = family_for_key(key);
    let scope = if Some(family.as_str()) == active {
        None
    } else {
        scope_for_family(&family)
    };

    Some(
        QuotaWindow::new(
            key,
            label_for_key(key),
            100.0 - remaining_percent,
            duration_for_key(key),
            resets_at,
        )
        .with_scope(scope),
    )
}

/// Окна, которые `agy` вешает на одно семейство: `gemini-5h`, `gemini-weekly`.
const WINDOW_SUFFIXES: [&str; 5] = ["5h", "five_hour", "five-hour", "weekly", "week"];

fn duration_for_key(key: &str) -> Option<u64> {
    let normalized = key.to_ascii_lowercase();
    if normalized.contains("weekly") || normalized.contains("week") {
        Some(WEEKLY_MINS)
    } else if normalized.contains("five_hour")
        || normalized.contains("five-hour")
        || normalized.contains("5h")
    {
        Some(FIVE_HOUR_MINS)
    } else {
        None
    }
}

/// Семейство моделей, к которому привязан bucket.
fn family_for_key(key: &str) -> String {
    let normalized = key.to_ascii_lowercase();
    for suffix in WINDOW_SUFFIXES {
        for separator in ['-', '_'] {
            if let Some(head) = normalized.strip_suffix(&format!("{separator}{suffix}")) {
                return head.to_owned();
            }
        }
    }
    normalized
}

fn scope_for_family(family: &str) -> Option<String> {
    match family {
        "gemini" => Some("Gemini".to_owned()),
        f if f.starts_with("gemini-argon") => Some("Gemini Argon".to_owned()),
        "3p" | "third-party" | "third_party" => Some("Сторонние модели".to_owned()),
        _ => None,
    }
}

/// Семейство, которым `agy` пользуется прямо сейчас.
fn active_family(model: Option<&str>) -> Option<&'static str> {
    let model = model?.trim().to_ascii_lowercase();
    if model.is_empty() {
        None
    } else if model.contains("argon") {
        Some("gemini-argon-wtu")
    } else if model.contains("gemini") {
        Some("gemini")
    } else {
        Some("3p")
    }
}

fn label_for_key(key: &str) -> String {
    let family = family_for_key(key);
    let duration = duration_for_key(key);
    let window = crate::model::label_for_duration(duration);
    match (scope_for_family(&family), duration) {
        (Some(scope), _) => format!("{scope} · {window}"),
        (None, Some(_)) => format!("{} · {window}", title_case(&family)),
        (None, None) => title_case(key),
    }
}

fn title_case(value: &str) -> String {
    value
        .split(['-', '_'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn sort_windows(windows: &mut [QuotaWindow], active: Option<&str>) {
    windows.sort_by(|left, right| {
        let left_priority = family_priority(&left.key, active);
        let right_priority = family_priority(&right.key, active);
        left_priority
            .cmp(&right_priority)
            .then_with(|| left.duration_mins.cmp(&right.duration_mins))
            .then_with(|| left.key.cmp(&right.key))
    });
}

fn family_priority(key: &str, active: Option<&str>) -> u8 {
    match active {
        Some(active) if family_for_key(key) == active => 0,
        _ => 1,
    }
}

fn status_line_text(snapshot: &Snapshot) -> String {
    let general = snapshot
        .windows
        .iter()
        .filter(|window| window.scope.is_none())
        .map(|window| window.remaining_percent)
        .fold(f64::INFINITY, f64::min);
    let percent = if general.is_finite() {
        Some(general)
    } else {
        snapshot
            .windows
            .iter()
            .map(|window| window.remaining_percent)
            .reduce(f64::min)
    };
    match percent {
        Some(percent) => format!("AI Usage · {}%", percent.round() as i64),
        None => "AI Usage · ожидание квоты".to_owned(),
    }
}

/// Минимальный разбор RFC 3339 в unix-время.
pub(crate) fn parse_rfc3339(value: &str) -> Option<i64> {
    let (date, rest) = value.trim().split_once(['T', 't'])?;
    let (time, offset) = split_offset(rest)?;

    let mut date = date.split('-');
    let year: i64 = date.next()?.parse().ok()?;
    let month: i64 = date.next()?.parse().ok()?;
    let day: i64 = date.next()?.parse().ok()?;
    if date.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    let mut time = time.split(':');
    let hour: i64 = time.next()?.parse().ok()?;
    let minute: i64 = time.next()?.parse().ok()?;
    let second: i64 = time.next().unwrap_or("0").split('.').next()?.parse().ok()?;
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }

    Some(days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second - offset)
}

/// Делит хвост на время и смещение в секундах.
fn split_offset(rest: &str) -> Option<(&str, i64)> {
    if let Some(time) = rest.strip_suffix(['Z', 'z']) {
        return Some((time, 0));
    }
    let index = rest.rfind(['+', '-'])?;
    let (time, offset) = rest.split_at(index);
    let sign = if offset.starts_with('-') { -1 } else { 1 };
    let mut parts = offset[1..].split(':');
    let hours: i64 = parts.next()?.parse().ok()?;
    let minutes: i64 = parts.next().unwrap_or("0").parse().ok()?;
    Some((time, sign * (hours * 3_600 + minutes * 60)))
}

/// Число дней от 1970-01-01. Алгоритм `days_from_civil` Ховарда Хиннанта:
/// точен для любой даты григорианского календаря и не требует таблиц.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn format_age(seconds: i64) -> String {
    if seconds < 60 {
        format!("{seconds} с")
    } else if seconds < 3_600 {
        format!("{} мин", seconds / 60)
    } else if seconds < 86_400 {
        format!("{} ч {} мин", seconds / 3_600, (seconds % 3_600) / 60)
    } else {
        format!("{} д {} ч", seconds / 86_400, (seconds % 86_400) / 3_600)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "cwd": "/tmp",
      "model": {
        "id": "Gemini 3.6 Flash (High)",
        "display_name": "Gemini 3.6 Flash (High)",
        "effort": "high"
      },
      "version": "1.1.8",
      "product": "antigravity",
      "quota": {
        "3p-5h": {
          "remaining_fraction": 1,
          "reset_time": "2026-07-29T19:05:06Z",
          "reset_in_seconds": 17998
        },
        "3p-weekly": {
          "remaining_fraction": 1,
          "reset_time": "2026-08-05T14:05:06Z",
          "reset_in_seconds": 604798
        },
        "gemini-5h": {
          "remaining_fraction": 0.42,
          "reset_time": "2026-07-29T19:05:06Z",
          "reset_in_seconds": 17998
        },
        "gemini-weekly": {
          "remaining_fraction": 0.9378,
          "reset_time": "2026-08-05T14:05:06Z",
          "reset_in_seconds": 604798
        }
      },
      "agent_state": "initializing",
      "plan_tier": "Google AI Pro",
      "email": "developer@example.com"
    }"#;

    #[test]
    fn the_snapshot_does_not_live_on_tmpfs() {
        let path = snapshot_path("agy-main");
        assert_ne!(path, legacy_snapshot_path("agy-main"));
        assert!(
            !path.starts_with(crate::util::runtime_dir()),
            "снимок оказался в runtime-каталоге: {}",
            path.display()
        );
        assert!(path.ends_with("antigravity-usage-agy-main.json"));
    }

    #[test]
    fn a_snapshot_left_in_the_runtime_dir_is_adopted() {
        let legacy = legacy_snapshot_path("adopt-test");
        let target = std::env::temp_dir().join(format!(
            "ai-usage-adopt-{}-{:?}.json",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_file(&target);
        crate::util::atomic_write_mode(&legacy, b"{\"windows\":[]}", Some(0o600)).unwrap();

        adopt_legacy_snapshot("adopt-test", &target);

        assert!(target.exists(), "снимок не переехал");
        assert!(!legacy.exists(), "старая копия осталась на tmpfs");
        let _ = fs::remove_file(&target);
    }

    fn window<'a>(snapshot: &'a Snapshot, key: &str) -> &'a QuotaWindow {
        snapshot
            .windows
            .iter()
            .find(|window| window.key == key)
            .unwrap()
    }

    #[test]
    fn maps_official_payload_into_open_quota_windows() {
        let snapshot = snapshot_from_payload(SAMPLE, 1_000).unwrap().unwrap();
        assert_eq!(snapshot.email.as_deref(), Some("developer@example.com"));
        assert_eq!(snapshot.plan.as_deref(), Some("Google AI Pro"));
        assert_eq!(snapshot.model.as_deref(), Some("Gemini 3.6 Flash (High)"));
        assert_eq!(snapshot.windows.len(), 4);

        let gemini = window(&snapshot, "gemini-5h");
        assert_eq!(gemini.remaining_percent.round(), 42.0);
        assert_eq!(gemini.resets_at, Some(18_998));
        assert_eq!(gemini.duration_mins, Some(FIVE_HOUR_MINS));
        assert_eq!(gemini.label, "Gemini · 5 часов");

        assert_eq!(window(&snapshot, "gemini-weekly").label, "Gemini · Неделя");
        assert_eq!(
            window(&snapshot, "3p-5h").label,
            "Сторонние модели · 5 часов"
        );
        assert_eq!(
            window(&snapshot, "3p-weekly").label,
            "Сторонние модели · Неделя"
        );
    }

    #[test]
    fn every_window_of_the_active_family_counts_as_a_general_limit() {
        let snapshot = snapshot_from_payload(SAMPLE, 1_000).unwrap().unwrap();
        assert_eq!(window(&snapshot, "gemini-5h").scope, None);
        assert_eq!(window(&snapshot, "gemini-weekly").scope, None);
        assert_eq!(
            window(&snapshot, "3p-5h").scope.as_deref(),
            Some("Сторонние модели")
        );
        assert_eq!(
            window(&snapshot, "3p-weekly").scope.as_deref(),
            Some("Сторонние модели")
        );
    }

    #[test]
    fn active_third_party_model_prioritizes_3p_family() {
        let raw = SAMPLE.replace("Gemini 3.6 Flash (High)", "Claude Opus 4.1");
        let snapshot = snapshot_from_payload(&raw, 1_000).unwrap().unwrap();
        assert_eq!(snapshot.windows[0].key, "3p-5h");
        assert_eq!(window(&snapshot, "3p-weekly").scope, None);
        assert_eq!(
            window(&snapshot, "gemini-5h").scope.as_deref(),
            Some("Gemini")
        );
    }

    #[test]
    fn the_panel_keeps_gemini_primary_when_the_active_model_is_claude() {
        let raw = SAMPLE.replace("Gemini 3.6 Flash (High)", "Claude Opus 5.5");
        let snapshot = snapshot_from_payload(&raw, 1_000).unwrap().unwrap();
        assert_eq!(snapshot.windows[0].key, "3p-5h");
        assert_eq!(status_line_text(&snapshot), "AI Usage · 100%");
        for poll_error in [None, Some("HTTP 503".to_owned())] {
            let state = state_from_snapshot("agy", "Agy", snapshot.clone(), poll_error, 1_001, 900);
            assert_eq!(state.windows[0].key, "gemini-5h");
            assert_eq!(state.windows[0].scope, None);
            assert_eq!(state.windows[0].remaining_percent, 42.0);
            assert!(
                state
                    .windows
                    .iter()
                    .filter(|w| w.key.starts_with("3p-"))
                    .all(|w| w.scope.as_deref() == Some("Сторонние модели"))
            );
            assert_eq!(state.updated_at, snapshot.updated_at);
        }
    }

    #[test]
    fn a_clock_step_back_repolls_instead_of_treating_future_data_as_fresh() {
        let snapshot = snapshot_from_payload(SAMPLE, 1_000).unwrap().unwrap();
        assert!(quota_poll_due(Some(&snapshot), 900));
        assert!(!quota_poll_due(Some(&snapshot), 1_000));
        assert!(!quota_poll_due(Some(&snapshot), 1_119));
        assert!(quota_poll_due(Some(&snapshot), 1_120));
        assert!(quota_poll_due(None, 900));
        let state = state_from_snapshot("agy", "Agy", snapshot, None, 900, 900);
        assert_eq!(state.status, "stale");
        assert!(state.error.unwrap().contains("Системные часы изменились"));
    }

    #[test]
    fn an_unknown_active_model_leaves_every_family_scoped() {
        let raw = SAMPLE.replace(r#""display_name": "Gemini 3.6 Flash (High)","#, "");
        let raw = raw.replace(r#""id": "Gemini 3.6 Flash (High)","#, "");
        let snapshot = snapshot_from_payload(&raw, 1_000).unwrap().unwrap();
        assert_eq!(snapshot.model, None);
        assert!(snapshot.windows.iter().all(|window| window.scope.is_some()));
    }

    #[test]
    fn an_unknown_bucket_survives_with_a_readable_label() {
        let raw = SAMPLE.replace(
            r#""3p-5h": {"#,
            r#""future-pool": {"remaining_fraction": 0.25, "reset_in_seconds": 60}, "3p-5h": {"#,
        );
        let snapshot = snapshot_from_payload(&raw, 1_000).unwrap().unwrap();
        let future = window(&snapshot, "future-pool");
        assert_eq!(future.label, "Future Pool");
        assert_eq!(future.duration_mins, None);
        assert_eq!(future.scope, None);
    }

    #[test]
    fn falls_back_to_reset_time_when_seconds_are_missing() {
        let raw = r#"{
          "quota": {
            "gemini-weekly": {
              "remaining_fraction": 0.5,
              "reset_time": "2026-08-05T14:05:06Z"
            }
          }
        }"#;
        let snapshot = snapshot_from_payload(raw, 1_000).unwrap().unwrap();
        assert_eq!(
            window(&snapshot, "gemini-weekly").resets_at,
            Some(1_785_938_706)
        );
    }

    #[test]
    fn parses_rfc3339_with_and_without_offsets() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339("2026-07-29T19:05:06Z"), Some(1_785_351_906));
        assert_eq!(parse_rfc3339("2024-02-29T00:00:00Z"), Some(1_709_164_800));
        assert_eq!(
            parse_rfc3339("2026-07-29T22:05:06+03:00"),
            Some(1_785_351_906)
        );
        assert_eq!(
            parse_rfc3339("2026-07-29T19:05:06.500Z"),
            Some(1_785_351_906)
        );
        assert_eq!(parse_rfc3339("не дата"), None);
        assert_eq!(parse_rfc3339("2026-13-01T00:00:00Z"), None);
    }

    #[test]
    fn a_short_pause_in_agy_is_not_staleness() {
        let snapshot = snapshot_from_payload(SAMPLE, 1_000).unwrap().unwrap();
        assert_eq!(stale_threshold(&snapshot.windows, 900), 5 * 3_600);
    }

    #[test]
    fn a_snapshot_without_durations_falls_back_to_the_global_threshold() {
        let raw = r#"{"quota": {"future-pool": {"remaining_fraction": 0.5}}}"#;
        let snapshot = snapshot_from_payload(raw, 1_000).unwrap().unwrap();
        assert_eq!(stale_threshold(&snapshot.windows, 900), 900);
    }

    #[test]
    fn the_global_threshold_is_a_floor() {
        let snapshot = snapshot_from_payload(SAMPLE, 1_000).unwrap().unwrap();
        assert_eq!(stale_threshold(&snapshot.windows, 86_400), 86_400);
    }

    #[test]
    fn a_failed_live_poll_marks_a_recent_snapshot_stale_until_recovery() {
        let cached = snapshot_from_payload(SAMPLE, 1_000).unwrap().unwrap();

        let failed = state_from_snapshot(
            "agy",
            "Agy",
            cached.clone(),
            Some("HTTP 503 Service Unavailable".to_owned()),
            1_300,
            900,
        );
        assert_eq!(failed.status, "stale");
        assert_eq!(failed.updated_at, 1_000);
        let quota = |windows: &[QuotaWindow]| {
            windows
                .iter()
                .map(|window| (window.key.clone(), window.remaining_percent))
                .collect::<Vec<_>>()
        };
        assert_eq!(quota(&failed.windows), quota(&cached.windows));
        let error = failed.error.unwrap();
        assert!(error.contains("HTTP 503"), "{error}");

        let recovered = snapshot_from_payload(SAMPLE, 1_400).unwrap().unwrap();
        let ok = state_from_snapshot("agy", "Agy", recovered, None, 1_400, 900);
        assert_eq!((ok.status.as_str(), ok.error), ("ok", None));
        assert_eq!(ok.updated_at, 1_400);

        // Hook-only accounts keep the age-based grace window.
        let hook = state_from_snapshot("agy", "Agy", cached.clone(), None, 1_000 + 3_600, 900);
        assert_eq!(hook.status, "ok");
        let hook = state_from_snapshot("agy", "Agy", cached, None, 1_001 + 5 * 3_600, 900);
        assert_eq!(hook.status, "stale");
        assert!(hook.error.unwrap().starts_with("agy не обновлял квоту"));
    }

    #[test]
    fn the_status_line_reports_the_worst_general_window() {
        let snapshot = snapshot_from_payload(SAMPLE, 1_000).unwrap().unwrap();
        assert_eq!(status_line_text(&snapshot), "AI Usage · 42%");
    }

    #[test]
    fn empty_quota_does_not_overwrite_last_good_snapshot() {
        let result = snapshot_from_payload(r#"{"quota": {}, "plan_tier": "Pro"}"#, 1_000).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn clamps_provider_percentages() {
        let raw = r#"{
          "quota": {
            "too-high": {"remaining_fraction": 2.0, "reset_in_seconds": 10},
            "negative": {"remaining_fraction": -1.0, "reset_in_seconds": 10}
          }
        }"#;
        let snapshot = snapshot_from_payload(raw, 1_000).unwrap().unwrap();
        let high = snapshot
            .windows
            .iter()
            .find(|window| window.key == "too-high")
            .unwrap();
        let low = snapshot
            .windows
            .iter()
            .find(|window| window.key == "negative")
            .unwrap();
        assert_eq!(high.remaining_percent, 100.0);
        assert_eq!(low.remaining_percent, 0.0);
    }

    #[test]
    fn ignores_non_finite_quota_values() {
        let quota = QuotaInfo {
            remaining_fraction: Some(f64::NAN),
            reset_time: None,
            reset_in_seconds: Some(10),
        };
        assert!(quota_window("bad", quota, 1_000, None).is_none());
    }

    #[test]
    fn parses_google_quota_summary_response_into_windows() {
        let raw = r#"{
          "buckets": [
            {
              "bucketId": "3p-5h",
              "remainingFraction": 1.0,
              "resetTime": "2026-07-29T19:05:06Z"
            },
            {
              "bucketId": "3p-weekly",
              "remainingFraction": 0.85,
              "resetTime": "2026-08-05T14:05:06Z"
            },
            {
              "bucketId": "gemini-5h",
              "remainingFraction": 0.42,
              "resetTime": "2026-07-29T19:05:06Z"
            },
            {
              "bucketId": "gemini-weekly",
              "remainingFraction": 0.9378,
              "resetTime": "2026-08-05T14:05:06Z"
            }
          ]
        }"#;
        let resp: QuotaSummaryResponse = serde_json::from_str(raw).unwrap();
        let snapshot = snapshot_from_quota_response(
            resp,
            Some("test@example.com".to_owned()),
            Some("Google AI Pro".to_owned()),
            Some("Gemini 3.8 Flash (High)".to_owned()),
            1_700_000_000,
        )
        .unwrap();

        assert_eq!(snapshot.email.as_deref(), Some("test@example.com"));
        assert_eq!(snapshot.plan.as_deref(), Some("Google AI Pro"));
        assert_eq!(snapshot.model.as_deref(), Some("Gemini 3.8 Flash (High)"));
        assert_eq!(snapshot.windows.len(), 4);

        let g5 = window(&snapshot, "gemini-5h");
        assert_eq!(g5.remaining_percent.round(), 42.0);
        assert_eq!(g5.used_percent.round(), 58.0);
        assert_eq!(g5.scope, None); // active family is Gemini

        let p3 = window(&snapshot, "3p-weekly");
        assert_eq!(p3.remaining_percent.round(), 85.0);
        assert_eq!(p3.scope.as_deref(), Some("Сторонние модели"));
    }

    #[test]
    fn parses_grouped_google_quota_summary() {
        let raw = r#"{
          "groups": [
            {
              "displayName": "Internal Group",
              "buckets": [
                {
                  "bucketId": "gemini-5h",
                  "remainingFraction": 0.77,
                  "resetTime": "2026-07-29T19:05:06Z"
                }
              ]
            }
          ]
        }"#;
        let resp: QuotaSummaryResponse = serde_json::from_str(raw).unwrap();
        let snapshot = snapshot_from_quota_response(resp, None, None, None, 1_700_000_000).unwrap();
        assert_eq!(snapshot.windows.len(), 1);
        assert_eq!(snapshot.windows[0].remaining_percent.round(), 77.0);
    }

    #[test]
    fn more_depleted_buckets_win_and_missing_buckets_are_added() {
        let parse = |raw: &str| serde_json::from_str::<QuotaSummaryResponse>(raw).unwrap();
        let prod = parse(
            r#"{"buckets":[{"bucketId":"gemini-weekly","remainingFraction":1.0},{"bucketId":"gemini-argon-wtu-weekly","remainingFraction":0.68},{"bucketId":"3p-5h","remainingFraction":0.61}]}"#,
        );
        let daily = parse(
            r#"{"groups":[{"buckets":[{"bucketId":"gemini-weekly","remainingFraction":0.99},{"bucketId":"gemini-argon-wtu-weekly","remainingFraction":0.92},{"bucketId":"3p-5h","remainingFraction":0.62},{"bucketId":"x-weekly","remainingFraction":0.9}]}]}"#,
        );
        let merged = merge_quota_responses(Some(prod), daily);
        let ids: Vec<_> = merged
            .buckets
            .iter()
            .map(|b| (b.bucket_id.clone().unwrap(), b.remaining_fraction.unwrap()))
            .collect();
        assert_eq!(
            ids,
            [
                ("gemini-weekly".into(), 0.99),
                ("gemini-argon-wtu-weekly".into(), 0.68),
                ("3p-5h".into(), 0.61),
                ("x-weekly".into(), 0.9)
            ]
        );
        let snap = snapshot_from_quota_response(merged, None, None, None, 1_700_000_000).unwrap();
        let state = state_from_snapshot("agy", "Agy", snap, None, 1_700_000_000, 900);
        let argon = state
            .windows
            .iter()
            .find(|w| w.key == "gemini-argon-wtu-weekly")
            .unwrap();
        assert_eq!(argon.label, "Gemini Argon · Неделя");
        assert_eq!(argon.scope.as_deref(), Some("Gemini Argon"));
        assert_eq!(state.windows[0].key, "gemini-weekly");
        assert_eq!(state.windows[0].scope, None);
    }

    #[test]
    fn argon_hook_keeps_the_shared_gemini_pool_from_the_last_poll() {
        let now = 1_000;
        let polled = Snapshot {
            email: None,
            plan: None,
            model: Some("Gemini 3.8 Flash (High)".into()),
            updated_at: now - 60,
            windows: vec![
                QuotaWindow::new(
                    "gemini-weekly",
                    "w",
                    50.0,
                    Some(WEEKLY_MINS),
                    Some(now + 3600),
                ),
                QuotaWindow::new("old-weekly", "o", 10.0, Some(WEEKLY_MINS), Some(now - 1)),
            ],
        };
        let mut hook = Snapshot {
            email: None,
            plan: None,
            model: Some("Gemini 4 Argon (High)".into()),
            updated_at: now,
            windows: vec![
                QuotaWindow::new(
                    "gemini-weekly",
                    "w",
                    0.0,
                    Some(WEEKLY_MINS),
                    Some(now + 604_800),
                ),
                QuotaWindow::new(
                    "gemini-argon-wtu-weekly",
                    "a",
                    7.2,
                    Some(WEEKLY_MINS),
                    Some(now + 600_000),
                ),
            ],
        };
        drop_argon_view_of_shared_windows(&mut hook);
        carry_missing_windows(&mut hook, Some(&polled), now);
        let used = |k: &str| {
            hook.windows
                .iter()
                .find(|w| w.key == k)
                .map(|w| w.used_percent)
        };
        assert_eq!(used("gemini-weekly"), Some(50.0));
        assert_eq!(used("gemini-argon-wtu-weekly"), Some(7.2));
        assert_eq!(used("old-weekly"), None, "an expired window is not carried");
    }

    #[test]
    fn reads_acp_token_file_format() {
        let temp = std::env::temp_dir().join(format!(
            "ai-usage-acp-test-{}-{:?}.json",
            std::process::id(),
            std::thread::current().id()
        ));
        let token_json = r#"{
          "client_id": "test-client-id.apps.googleusercontent.com",
          "client_secret": "test-secret",
          "refresh_token": "1//test-refresh-token",
          "token_uri": "https://oauth2.googleapis.com/token",
          "project_id": "aicode-consumers"
        }"#;
        fs::write(&temp, token_json).unwrap();

        let parsed = read_token_file(&temp).unwrap();
        assert_eq!(
            parsed.client_id,
            "test-client-id.apps.googleusercontent.com"
        );
        assert_eq!(parsed.client_secret, "test-secret");
        assert_eq!(parsed.refresh_token, "1//test-refresh-token");
        assert_eq!(parsed.project_id, "aicode-consumers");

        let _ = fs::remove_file(temp);
    }

    #[test]
    fn find_token_file_respects_custom_path() {
        let temp = std::env::temp_dir().join(format!(
            "ai-usage-custom-acp-{}-{:?}.json",
            std::process::id(),
            std::thread::current().id()
        ));
        fs::write(&temp, "{}").unwrap();

        assert_eq!(find_token_file(Some(&temp)), Some(temp.clone()));
        let missing = temp.join("missing.json");
        assert_eq!(find_token_file(Some(&missing)), None);

        let _ = fs::remove_file(temp);
    }
}
