use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::header::{ACCEPT, AUTHORIZATION};
use serde::Deserialize;

use crate::model::{AccountState, FIVE_HOUR_MINS, QuotaWindow, WEEKLY_MINS};
use crate::providers::t3::{self, T3Source, parse_rfc3339};

const MAX_ERROR_BODY: usize = 200;
// ponytail: the plan's monthly window follows the calendar month; 30 days is close enough for pacing.
const MONTHLY_MINS: u64 = 30 * 24 * 60;

fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .connect_timeout(Duration::from_secs(5))
            .user_agent(concat!("ai-usage/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("не удалось создать HTTP-клиент")
    })
}

#[derive(Debug, Deserialize)]
struct Envelope {
    usage: Usage,
}

#[derive(Debug, Deserialize)]
struct Usage {
    rolling: Option<Window>,
    weekly: Option<Window>,
    monthly: Option<Window>,
}

#[derive(Debug, Deserialize)]
struct Window {
    percent: f64,
    #[serde(default, rename = "resetsAt")]
    resets_at: Option<String>,
}

/// Сайт держит два входа: новый (вход в OpenCode Console, токен `st_…`) и
/// старый (API-ключ `sk-…` по адресу `/zen/go/v1`).
const CONSOLE_BASE: &str = "https://opencode.ai/inference/go/v1";
const KEY_BASE: &str = "https://opencode.ai/zen/go/v1";

/// OpenCode v2 хранит учётные данные в своей базе, а не в `auth.json`.
pub fn default_db_file() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("~/.local/share"))
        .join("opencode")
        .join("opencode.db")
}

#[derive(Debug, PartialEq)]
struct Credential {
    base: &'static str,
    token: String,
    org_id: Option<String>,
}

/// Читает учётные данные OpenCode на месте, без второй копии секрета.
/// ponytail: зависит от схемы `credential` OpenCode 2.x и `sqlite3` в PATH;
/// при смене схемы перейти на официальный способ, если OpenCode его даст.
async fn read_credential(db_file: &Path) -> Result<Credential> {
    let path = crate::util::expand_home(db_file);
    let output = tokio::process::Command::new("sqlite3")
        .arg("-readonly")
        .arg("-json")
        .arg(&path)
        .arg(
            "select integration_id, value from credential \
             where integration_id in ('opencode-go', 'opencode') \
             order by integration_id = 'opencode-go' desc, active desc limit 1",
        )
        .kill_on_drop(true)
        .output()
        .await
        .context("Не удалось запустить sqlite3")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("sqlite3 не прочитал {}: {}", path.display(), stderr.trim());
    }
    parse_credential(&output.stdout, crate::util::unix_now() * 1000)
}

fn parse_credential(stdout: &[u8], now_ms: i64) -> Result<Credential> {
    let missing = "OpenCode Go не подключён: войдите в OpenCode Console через /connect";
    // Ошибки разбора не пробрасываем: serde может процитировать секрет.
    let rows: Vec<serde_json::Value> = if stdout.iter().all(u8::is_ascii_whitespace) {
        Vec::new()
    } else {
        serde_json::from_slice(stdout)
            .ok()
            .context("sqlite3 вернул некорректный JSON")?
    };
    let row = rows.first().context(missing)?;
    let value: serde_json::Value = row["value"]
        .as_str()
        .and_then(|raw| serde_json::from_str(raw).ok())
        .context("Некорректная запись учётных данных OpenCode")?;
    let text = |pointer: &str| {
        value
            .pointer(pointer)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(str::to_owned)
    };

    if row["integration_id"] == "opencode-go" {
        return Ok(Credential {
            base: KEY_BASE,
            token: text("/key").context(missing)?,
            org_id: None,
        });
    }
    // Токен обновляет сам OpenCode при использовании; живёт около месяца.
    if value["expires"]
        .as_i64()
        .is_some_and(|expires| expires <= now_ms)
    {
        bail!("Токен OpenCode Console истёк: откройте OpenCode, чтобы он его обновил");
    }
    Ok(Credential {
        base: CONSOLE_BASE,
        token: text("/access").context(missing)?,
        org_id: text("/metadata/orgID"),
    })
}

/// T3 refreshes OpenCode Go limits only on manual refresh (checked 2026-11-13, T3
/// 0.1.10-nightly.20261113), so poll the Go endpoint directly and fall back to T3's cache.
/// shortcut: drop the direct poll once T3's health interval refreshes `opencode` usageLimits.
pub async fn fetch(
    id: &str,
    name: &str,
    instance: Option<&str>,
    db_file: Option<&Path>,
    stale_seconds: i64,
) -> Result<AccountState> {
    let direct = async {
        let credential = read_credential(db_file.unwrap_or(&default_db_file())).await?;
        fetch_with_credential(id, name, &credential).await
    };
    match direct.await {
        Ok(state) => Ok(state),
        Err(direct_error) => {
            let source = T3Source {
                id,
                name,
                provider: "opencode-go",
                instance,
                explicit_plan: None,
            };
            Ok(with_direct_error(
                t3::fetch(source, stale_seconds).await?,
                &direct_error,
            ))
        }
    }
}

/// A usable T3 state stays clean; a stale or failed one also names why the direct poll failed.
fn with_direct_error(mut state: AccountState, direct_error: &anyhow::Error) -> AccountState {
    if let Some(t3_error) = state.error.take() {
        state.error = Some(format!("{direct_error:#}; T3 cache: {t3_error}"));
    }
    state
}

async fn fetch_with_credential(
    id: &str,
    name: &str,
    credential: &Credential,
) -> Result<AccountState> {
    let mut request = client()
        .get(format!("{}/usage", credential.base))
        .header(ACCEPT, "application/json")
        .header(AUTHORIZATION, format!("Bearer {}", credential.token));
    if let Some(org_id) = &credential.org_id {
        request = request.header("x-opencode-org-id", org_id);
    }
    let response = request
        .send()
        .await
        .context("Не удалось подключиться к OpenCode Go")?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        let body: String = body.trim().chars().take(MAX_ERROR_BODY).collect();
        bail!("OpenCode Go вернул HTTP {status}: {body}");
    }

    let payload: Envelope = response
        .json()
        .await
        .context("OpenCode Go вернул некорректный JSON в /usage")?;

    let windows: Vec<QuotaWindow> = [
        ("rolling", "5 часов", FIVE_HOUR_MINS, payload.usage.rolling),
        ("weekly", "Неделя", WEEKLY_MINS, payload.usage.weekly),
        ("monthly", "Месяц", MONTHLY_MINS, payload.usage.monthly),
    ]
    .into_iter()
    .filter_map(|(key, label, mins, window)| {
        let window = window?;
        let resets_at = window.resets_at.as_deref().and_then(parse_rfc3339);
        Some(QuotaWindow::new(
            key,
            label,
            window.percent,
            Some(mins),
            resets_at,
        ))
    })
    .collect();
    if windows.is_empty() {
        bail!("OpenCode Go не вернул ни одного окна лимита");
    }

    let depleted = windows.iter().any(|window| window.remaining_percent <= 0.0);
    Ok(AccountState {
        id: id.to_owned(),
        name: name.to_owned(),
        provider: "opencode-go".to_owned(),
        status: if depleted { "depleted" } else { "ok" }.to_owned(),
        plan: Some("Go".to_owned()),
        email: None,
        model: None,
        windows,
        balances: Vec::new(),
        // The 0% window row already says it; an error line would repeat it.
        error: None,
        updated_at: crate::util::unix_now(),
    })
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    async fn serve_once(code: u16, body: &'static str) -> Credential {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0_u8; 4096];
            let read = socket.read(&mut buffer).await.unwrap();
            let request = String::from_utf8_lossy(&buffer[..read]);
            assert!(request.starts_with("GET /go/v1/usage "), "{request}");
            assert!(
                request.contains("authorization: Bearer st_test"),
                "{request}"
            );
            assert!(request.contains("x-opencode-org-id: org_1"), "{request}");
            let response = format!(
                "HTTP/1.1 {code} X\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        let base = format!("http://{address}/go/v1");
        Credential {
            base: Box::leak(base.into_boxed_str()),
            token: "st_test".to_owned(),
            org_id: Some("org_1".to_owned()),
        }
    }

    #[tokio::test]
    async fn parses_three_windows() {
        let credential = serve_once(
            200,
            r#"{"usage":{"rolling":{"status":"ok","percent":7,"resetsAt":"2026-10-11T12:00:00Z"},"weekly":{"status":"ok","percent":100},"monthly":{"status":"ok","percent":1}}}"#,
        )
        .await;
        let state = fetch_with_credential("go", "Go", &credential)
            .await
            .unwrap();

        let keys: Vec<_> = state.windows.iter().map(|w| w.key.as_str()).collect();
        assert_eq!(keys, ["rolling", "weekly", "monthly"]);
        assert_eq!(state.windows[0].used_percent, 7.0);
        assert_eq!(state.windows[0].resets_at, Some(1_791_720_000));
        assert_eq!(state.windows[2].duration_mins, Some(MONTHLY_MINS));
        assert_eq!(state.status, "depleted");
    }

    #[test]
    fn fallback_keeps_clean_t3_state_and_names_direct_error_otherwise() {
        let direct = anyhow::anyhow!("sqlite3 failed");
        let ok = AccountState::error("go", "Go", "opencode-go", String::new());
        let ok = AccountState { error: None, ..ok };
        assert_eq!(with_direct_error(ok, &direct).error, None);
        let stale = AccountState::error("go", "Go", "opencode-go", "cache 51 min old");
        assert_eq!(
            with_direct_error(stale, &direct).error.as_deref(),
            Some("sqlite3 failed; T3 cache: cache 51 min old")
        );
    }

    #[tokio::test]
    async fn unauthorized_is_an_error() {
        let credential = serve_once(401, r#"{"error":"bad key"}"#).await;
        let err = fetch_with_credential("go", "Go", &credential)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("401"), "{err}");
    }

    fn row(integration: &str, value: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!([{"integration_id": integration, "value": value}]))
            .unwrap()
    }

    #[test]
    fn parses_console_and_key_credentials() {
        let console = row(
            "opencode",
            r#"{"type":"oauth","access":"st_a","expires":2000,"metadata":{"orgID":"org_1"}}"#,
        );
        assert_eq!(
            parse_credential(&console, 1000).unwrap(),
            Credential {
                base: CONSOLE_BASE,
                token: "st_a".to_owned(),
                org_id: Some("org_1".to_owned()),
            }
        );
        let expired = parse_credential(&console, 2000).unwrap_err().to_string();
        assert!(expired.contains("истёк"), "{expired}");

        let key = row("opencode-go", r#"{"type":"api","key":" sk-go "}"#);
        let credential = parse_credential(&key, 0).unwrap();
        assert_eq!(
            (credential.base, credential.token.as_str()),
            (KEY_BASE, "sk-go")
        );

        let none = parse_credential(b"", 0).unwrap_err().to_string();
        assert!(none.contains("/connect"), "{none}");
        let broken = row("opencode", r#"{"access":"st_secret"#);
        assert!(!format!("{:#}", parse_credential(&broken, 0).unwrap_err()).contains("st_secret"));
    }
}
