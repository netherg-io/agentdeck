use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::task::JoinSet;

use agentdeck_core::t3::{
    T3UsageWindow, antigravity_profile_dir, read_antigravity_instance_config,
    resolve_antigravity_binary,
};

use crate::config::AccountConfig;
use crate::model::{AccountState, AppState, FIVE_HOUR_MINS, QuotaWindow};
use crate::providers::t3::{self, T3Source, parse_rfc3339};
use crate::util;

/// Renew the 1-hour CLI OAuth access token before T3's 60-second expiry cutoff
/// so idle Antigravity accounts never slip into `sign-in has expired`.
const RENEW_LEAD_SECS: i64 = 300;
/// Same margin T3's `makeAntigravityUsageProbe` requires (`expiry > now + 60s`).
const T3_FRESH_MARGIN_SECS: i64 = 60;
const RENEW_TIMEOUT: Duration = Duration::from_secs(20);
const FAILURE_BACKOFF: Duration = Duration::from_secs(30 * 60);
const MAX_PARALLEL_RENEWALS: usize = 3;
const MAX_RPC_LINES: usize = 256;

const EXPIRED_SIGN_IN_MARKER: &str = "sign-in has expired";

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

pub async fn fetch(
    id: &str,
    name: &str,
    instance: Option<&str>,
    stale_seconds: i64,
) -> Result<AccountState> {
    let mut state = t3::fetch(
        T3Source {
            id,
            name,
            provider: "antigravity",
            instance,
            explicit_plan: None,
        },
        stale_seconds,
    )
    .await?;
    let profile = agentdeck_core::t3::t3_root()
        .ok()
        .zip(t3::resolve_instance_slug("antigravity", instance));
    if state.email.is_none()
        && let Some((root, inst)) = profile.as_ref()
    {
        state.email = read_profile_email(root, inst);
    }
    if state.status != "waiting" || !state.windows.is_empty() {
        return Ok(state);
    }
    // T3 0.1.13-nightly.20261125 dropped its Antigravity quota probe, so its cache never
    // gets usageLimits. shortcut: remove the direct poll once T3 fills them again.
    let Some((root, inst)) = profile else {
        return Ok(state);
    };
    let now = util::unix_now();
    Ok(match fetch_direct(&root, &inst, now).await {
        Ok(windows) => AccountState {
            status: "ok".to_owned(),
            windows,
            error: None,
            updated_at: now,
            ..state
        },
        Err(err) => AccountState {
            error: Some(format!(
                "{err:#}; T3 cache: {}",
                state.error.as_deref().unwrap_or("-")
            )),
            ..state
        },
    })
}

const QUOTA_BASE: &str = "https://daily-cloudcode-pa.googleapis.com/v1internal:";

fn http() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .connect_timeout(Duration::from_secs(5))
            // The Gemini group (gemini-5h, gemini-weekly) is only returned to `antigravity/cli/*` agents.
            .user_agent("antigravity/cli/1.5.17")
            .build()
            .expect("не удалось создать HTTP-клиент")
    })
}

/// Same endpoints the Antigravity CLI uses. Production `cloudcode-pa` answers too, but
/// with untouched sliding windows; only `daily-` matched T3's last figures (2026-11-25).
async fn fetch_direct(t3_root: &Path, instance_id: &str, now: i64) -> Result<Vec<QuotaWindow>> {
    let raw = fs::read(
        antigravity_profile_dir(t3_root, instance_id)?
            .join("antigravity-acp/antigravity-oauth-token"),
    )
    .context("нет файла OAuth-токена Antigravity")?;
    // Never propagate serde errors or response bodies: they can quote the token.
    let token = serde_json::from_slice::<Value>(&raw)
        .ok()
        .and_then(|v| {
            v.pointer("/token/access_token")?
                .as_str()
                .map(str::to_owned)
        })
        .filter(|t| !t.trim().is_empty())
        .context("файл OAuth-токена Antigravity повреждён")?;
    if parse_token_meta(&raw)
        .and_then(|m| m.expiry)
        .is_none_or(|exp| exp <= now + T3_FRESH_MARGIN_SECS)
    {
        bail!("OAuth-токен Antigravity истёк; agentdeck продлит его в следующем цикле");
    }
    let post = |method: &'static str| {
        let req = http()
            .post(format!("{QUOTA_BASE}{method}"))
            .bearer_auth(&token)
            .json(&serde_json::json!({}));
        async move {
            let res = req
                .send()
                .await
                .map_err(|_| anyhow::anyhow!("Google {method}: сетевая ошибка"))?;
            if !res.status().is_success() {
                bail!("Google {method}: HTTP {}", res.status());
            }
            res.json::<Value>()
                .await
                .map_err(|_| anyhow::anyhow!("Google {method}: некорректный JSON"))
        }
    };
    let (summary, models) = tokio::try_join!(
        post("retrieveUserQuotaSummary"),
        post("fetchAvailableModels")
    )?;
    let windows = t3::map_windows("antigravity", quota_windows(&summary, &models, now));
    if windows.is_empty() {
        bail!("Google не вернул ни одного окна квоты Antigravity");
    }
    Ok(windows)
}

/// Summary buckets (gemini-*, 3p-*). Without a Gemini group in the summary, one
/// Gemini window comes from the tightest per-model quota.
fn quota_windows(summary: &Value, models: &Value, now: i64) -> Vec<T3UsageWindow> {
    // Protobuf JSON omits a zero fraction: an exhausted quota carries only resetTime.
    let used = |q: &Value| {
        q.get("remainingFraction")
            .and_then(Value::as_f64)
            .or_else(|| q.get("resetTime").map(|_| 0.0))
            .map(|f| (1.0 - f) * 100.0)
    };
    let reset = |q: &Value| {
        q.get("resetTime")
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    let window = |id: &str, q: &Value| T3UsageWindow {
        id: id.to_owned(),
        kind: None,
        label: None,
        window_duration_mins: None,
        used_percent: used(q),
        resets_at: reset(q),
    };
    let groups = summary.get("groups").and_then(Value::as_array);
    let mut out: Vec<T3UsageWindow> = summary
        .get("buckets")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .chain(
            groups
                .into_iter()
                .flatten()
                .filter_map(|g| g.get("buckets")?.as_array())
                .flatten(),
        )
        .filter_map(|b| Some(window(b.get("bucketId")?.as_str()?, b)))
        .collect();
    if out.iter().any(|w| w.id.starts_with("gemini")) {
        return out;
    }
    let tightest = models
        .get("models")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter(|(name, _)| name.starts_with("gemini-"))
        .filter_map(|(_, m)| m.get("quotaInfo"))
        .filter(|q| reset(q).is_some())
        .min_by(|a, b| {
            used(b)
                .partial_cmp(&used(a))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    if let Some(q) = tightest {
        // shortcut: a weekly quota resetting within 5 h is shown as the 5-hour window.
        let weekly = reset(q)
            .as_deref()
            .and_then(parse_rfc3339)
            .is_some_and(|r| r - now > (FIVE_HOUR_MINS * 60) as i64);
        out.push(window(
            if weekly { "gemini-weekly" } else { "gemini-5h" },
            q,
        ));
    }
    out
}

/// Per-instance failure backoff so a revoked refresh token does not respawn
/// `antigravity acp` every polling cycle. Clears immediately when the live
/// token file changes on disk (e.g. after the user signs in again).
#[derive(Debug, Default)]
pub struct RenewBackoff {
    failures: HashMap<String, (Instant, u64)>,
}

impl RenewBackoff {
    fn should_skip(&self, instance_id: &str, fingerprint: u64) -> bool {
        self.failures
            .get(instance_id)
            .is_some_and(|(at, fp)| *fp == fingerprint && at.elapsed() < FAILURE_BACKOFF)
    }

    fn record_failure(&mut self, instance_id: String, fingerprint: u64) {
        self.failures
            .insert(instance_id, (Instant::now(), fingerprint));
    }

    fn clear(&mut self, instance_id: &str) {
        self.failures.remove(instance_id);
    }
}

struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct TokenMeta {
    has_refresh_token: bool,
    expiry: Option<i64>,
}

fn parse_token_email(raw: &[u8]) -> Option<String> {
    // Never propagate serde errors: they can quote pieces of the OAuth secret.
    let v: Value = serde_json::from_slice(raw).ok()?;
    v.get("id_token_email")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn read_profile_email(t3_root: &Path, instance_id: &str) -> Option<String> {
    let raw = fs::read(
        antigravity_profile_dir(t3_root, instance_id)
            .ok()?
            .join("antigravity-acp/antigravity-oauth-token"),
    )
    .ok()?;
    parse_token_email(&raw)
}

fn parse_token_meta(raw: &[u8]) -> Option<TokenMeta> {
    // Never propagate serde errors: they can quote pieces of the OAuth secret.
    let v: Value = serde_json::from_slice(raw).ok()?;
    let tok = v.get("token")?;
    let has_refresh_token = tok
        .get("refresh_token")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.trim().is_empty());
    let expiry = tok
        .get("expiry")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339);
    Some(TokenMeta {
        has_refresh_token,
        expiry,
    })
}

fn fingerprint_bytes(bytes: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
}

/// True when any Antigravity account's last state reported an expired sign-in
/// even though its on-disk `antigravity-oauth-token` is now fresh, meaning T3
/// only needs a `server.refreshProviders` kick to recover.
pub fn has_fresh_token_with_expired_cache(
    t3_root: &Path,
    accounts: &[AccountConfig],
    previous: &AppState,
    now: i64,
) -> bool {
    accounts.iter().any(|account| {
        let AccountConfig::Antigravity { id, instance, .. } = account else {
            return false;
        };
        let Some(prev) = previous
            .accounts
            .iter()
            .find(|a| a.id == *id && a.provider == "antigravity")
        else {
            return false;
        };
        if prev.status != "error"
            || !prev
                .error
                .as_deref()
                .is_some_and(|e| e.contains(EXPIRED_SIGN_IN_MARKER))
        {
            return false;
        }
        let Some(inst) = t3::resolve_instance_slug("antigravity", instance.as_deref()) else {
            return false;
        };
        let Ok(profile) = antigravity_profile_dir(t3_root, &inst) else {
            return false;
        };
        let Ok(raw) = fs::read(profile.join("antigravity-acp/antigravity-oauth-token")) else {
            return false;
        };
        parse_token_meta(&raw)
            .and_then(|m| m.expiry)
            .is_some_and(|exp| exp > now + T3_FRESH_MARGIN_SECS)
    })
}

/// Renew expired or near-expiry `antigravity-oauth-token` files for enabled T3
/// Antigravity instances via the official `antigravity acp` `authenticate`
/// handshake (no model turn, zero quota spent). Returns how many tokens were
/// refreshed on disk.
pub async fn renew_expiring_tokens(
    t3_root: &Path,
    accounts: &[AccountConfig],
    backoff: &mut RenewBackoff,
) -> usize {
    renew_expiring_tokens_at(t3_root, accounts, backoff, util::unix_now()).await
}

pub(crate) async fn renew_expiring_tokens_at(
    t3_root: &Path,
    accounts: &[AccountConfig],
    backoff: &mut RenewBackoff,
    now: i64,
) -> usize {
    struct Job {
        instance_id: String,
        auth_method: String,
        binary: PathBuf,
        token_path: PathBuf,
        live_bytes: Vec<u8>,
        fingerprint: u64,
        old_expiry: Option<i64>,
    }

    let mut jobs = Vec::new();
    for account in accounts {
        let AccountConfig::Antigravity { instance, .. } = account else {
            continue;
        };
        let Some(instance_id) = t3::resolve_instance_slug("antigravity", instance.as_deref())
        else {
            continue;
        };
        let Ok(Some(cfg)) = read_antigravity_instance_config(t3_root, &instance_id) else {
            continue;
        };
        if !cfg.enabled
            || !matches!(
                cfg.auth_method.as_str(),
                "oauth-personal" | "oauth-business"
            )
        {
            continue;
        }
        let Ok(profile) = antigravity_profile_dir(t3_root, &instance_id) else {
            continue;
        };
        let acp_dir = profile.join("antigravity-acp");
        // A legacy `acp_token.json` used to be refreshed by T3's quota probe; T3
        // 0.1.13-nightly.20261125 removed the probe, so renew those profiles too.
        let token_path = acp_dir.join("antigravity-oauth-token");
        let Ok(live_bytes) = fs::read(&token_path) else {
            continue;
        };
        let Some(meta) = parse_token_meta(&live_bytes) else {
            continue;
        };
        if !meta.has_refresh_token {
            continue;
        }
        if meta.expiry.is_some_and(|exp| exp > now + RENEW_LEAD_SECS) {
            backoff.clear(&instance_id);
            continue;
        }
        let fingerprint = fingerprint_bytes(&live_bytes);
        if backoff.should_skip(&instance_id, fingerprint) {
            continue;
        }
        let Some(binary) = resolve_antigravity_binary(t3_root, cfg.binary_path.as_deref()) else {
            eprintln!(
                "[antigravity] бинарник antigravity не найден для {instance_id}; пропуск обновления токена"
            );
            backoff.record_failure(instance_id, fingerprint);
            continue;
        };
        jobs.push(Job {
            instance_id,
            auth_method: cfg.auth_method,
            binary,
            token_path,
            live_bytes,
            fingerprint,
            old_expiry: meta.expiry,
        });
    }

    let mut renewed = 0;
    let mut tasks = JoinSet::new();
    let collect = |res: Result<(String, u64, Result<bool>), tokio::task::JoinError>,
                   backoff: &mut RenewBackoff,
                   renewed: &mut usize| match res {
        Ok((id, _, Ok(true))) => {
            backoff.clear(&id);
            *renewed += 1;
            eprintln!("[antigravity] обновил OAuth-токен для {id}");
        }
        Ok((id, _, Ok(false))) => {
            backoff.clear(&id);
        }
        Ok((id, fp, Err(err))) => {
            eprintln!("[antigravity] не удалось обновить OAuth-токен для {id}: {err:#}");
            backoff.record_failure(id, fp);
        }
        Err(err) => eprintln!("[antigravity] сбой задачи обновления токена: {err}"),
    };

    for job in jobs {
        if tasks.len() >= MAX_PARALLEL_RENEWALS
            && let Some(res) = tasks.join_next().await
        {
            collect(res, backoff, &mut renewed);
        }
        tasks.spawn(async move {
            let outcome = match tokio::time::timeout(
                RENEW_TIMEOUT,
                renew_single_token(
                    &job.binary,
                    &job.auth_method,
                    &job.token_path,
                    &job.live_bytes,
                    job.old_expiry,
                    now,
                ),
            )
            .await
            {
                Ok(res) => res,
                Err(_) => Err(anyhow::anyhow!(
                    "таймаут {} с при обновлении через antigravity acp",
                    RENEW_TIMEOUT.as_secs()
                )),
            };
            (job.instance_id, job.fingerprint, outcome)
        });
    }
    while let Some(res) = tasks.join_next().await {
        collect(res, backoff, &mut renewed);
    }
    renewed
}

async fn renew_single_token(
    binary: &Path,
    auth_method: &str,
    live_token_path: &Path,
    live_bytes: &[u8],
    old_expiry: Option<i64>,
    now: i64,
) -> Result<bool> {
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp_root =
        std::env::temp_dir().join(format!("agentdeck-agy-renew-{}-{seq}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp_root);
    fs::create_dir_all(&tmp_root)?;
    fs::set_permissions(&tmp_root, fs::Permissions::from_mode(0o700))?;
    let _guard = TempDirGuard(tmp_root.clone());

    let tmp_acp = tmp_root.join("antigravity-acp");
    fs::create_dir_all(&tmp_acp)?;
    fs::set_permissions(&tmp_acp, fs::Permissions::from_mode(0o700))?;
    let tmp_token = tmp_acp.join("antigravity-oauth-token");
    util::atomic_write_mode(&tmp_token, live_bytes, Some(0o600))?;

    let mut child = Command::new(binary)
        .args(["acp", "--gemini-dir"])
        .arg(&tmp_root)
        .args(["--app-data-dir", "antigravity-acp", "--disable-telemetry"])
        .current_dir(&tmp_root)
        .env("GEMINI_HOME", &tmp_root)
        .env("AGY_ACP_FORCE_FILE_STORAGE", "1")
        .env("AGY_CLI_DISABLE_AUTO_UPDATE", "1")
        .env("SSH_CONNECTION", "127.0.0.1 0 127.0.0.1 0")
        .env("BROWSER", "/bin/false")
        .env("TMPDIR", &tmp_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("не удалось запустить {}", binary.display()))?;

    let mut stdin = child
        .stdin
        .take()
        .context("отсутствует stdin процесса acp")?;
    let stdout = child
        .stdout
        .take()
        .context("отсутствует stdout процесса acp")?;
    let mut lines = BufReader::new(stdout).lines();

    let init_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": { "protocolVersion": 1, "clientCapabilities": {} }
    });
    stdin
        .write_all(format!("{init_req}\n").as_bytes())
        .await
        .context("не удалось отправить ACP initialize")?;
    stdin.flush().await?;
    read_rpc_response(&mut lines, 1, "initialize").await?;

    let auth_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "authenticate",
        "params": { "methodId": auth_method }
    });
    stdin
        .write_all(format!("{auth_req}\n").as_bytes())
        .await
        .context("не удалось отправить ACP authenticate")?;
    stdin.flush().await?;
    // Keep stdin open until `authenticate` replies: EOF cancels the in-flight
    // ACP request (`-32800 Sign-in was cancelled.`). `SSH_CONNECTION` +
    // `BROWSER=/bin/false` already force a fast error if interactive login is needed.
    read_rpc_response(&mut lines, 2, "authenticate").await?;
    drop(stdin);

    let _ = child.kill().await;
    let _ = child.wait().await;

    let refreshed_bytes = fs::read(&tmp_token).context("обновлённый файл токена не найден")?;
    let Some(new_meta) = parse_token_meta(&refreshed_bytes) else {
        bail!("обновлённый файл токена повреждён");
    };
    let has_access = serde_json::from_slice::<Value>(&refreshed_bytes)
        .ok()
        .and_then(|v| {
            v.pointer("/token/access_token")
                .and_then(Value::as_str)
                .map(|s| !s.trim().is_empty())
        })
        .unwrap_or(false);
    let Some(new_expiry) = new_meta.expiry else {
        bail!("обновлённый токен не содержит expiry");
    };
    if !has_access || new_expiry <= now + T3_FRESH_MARGIN_SECS {
        bail!("antigravity acp не продлил срок действия токена");
    }
    if old_expiry.is_some_and(|old| new_expiry <= old) {
        bail!("срок действия токена не увеличился после authenticate");
    }

    // Optimistic CAS: do not overwrite if T3 refreshed the live file concurrently.
    let current_live = fs::read(live_token_path).context("исходный файл токена исчез")?;
    if current_live != live_bytes {
        if parse_token_meta(&current_live)
            .and_then(|m| m.expiry)
            .is_some_and(|exp| exp > now + T3_FRESH_MARGIN_SECS)
        {
            return Ok(false);
        }
        bail!("файл токена изменился во время обновления");
    }
    util::atomic_write_mode(live_token_path, &refreshed_bytes, Some(0o600))?;
    Ok(true)
}

async fn read_rpc_response(
    lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    expected_id: u64,
    method: &str,
) -> Result<()> {
    for _ in 0..MAX_RPC_LINES {
        let Some(line) = lines.next_line().await? else {
            bail!("antigravity acp закрыл stdout до ответа на {method}");
        };
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if msg.get("id").and_then(Value::as_u64) != Some(expected_id) {
            continue;
        }
        if let Some(err) = msg.get("error") {
            let detail = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown RPC error");
            let mut short = detail.trim().to_owned();
            short.truncate(160);
            bail!("ACP {method}: {short}");
        }
        if msg.get("result").is_some() {
            return Ok(());
        }
    }
    bail!("antigravity acp не прислал ответ на {method}");
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_795_513_914; // 2026-11-24T09:51:54Z
    const EXPIRED: &str = "2026-11-24T04:13:43.448654405+02:00";
    const FRESH: &str = "2026-11-24T12:56:05.360016764+02:00";

    fn temp_root(tag: &str) -> (PathBuf, TempDirGuard) {
        let dir = std::env::temp_dir().join(format!(
            "agentdeck-agy-test-{tag}-{}-{}",
            std::process::id(),
            TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("userdata")).unwrap();
        (dir.clone(), TempDirGuard(dir))
    }

    fn write_mock_acp(dir: &Path, script: &str) -> PathBuf {
        let path = dir.join("mock-antigravity");
        fs::write(&path, format!("#!/usr/bin/env bash\nset -eu\n{script}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn write_settings(t3: &Path, inst: &str, enabled: bool, auth_method: &str, bin: &Path) {
        fs::write(
            t3.join("userdata/settings.json"),
            serde_json::json!({
                "providerInstances": {
                    inst: {
                        "driver": "antigravity",
                        "enabled": enabled,
                        "config": {
                            "authMethod": auth_method,
                            "binaryPath": bin.display().to_string()
                        }
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
    }

    fn token_json(expiry: &str, access: &str) -> String {
        serde_json::json!({
            "token": {
                "access_token": access,
                "token_type": "Bearer",
                "refresh_token": "ref-1",
                "expiry": expiry,
                "expires_in": 3599
            },
            "auth_method": "consumer"
        })
        .to_string()
    }

    fn agy_account(inst: &str) -> AccountConfig {
        AccountConfig::Antigravity {
            id: inst.to_owned(),
            name: inst.to_owned(),
            instance: Some(inst.to_owned()),
        }
    }

    #[tokio::test]
    async fn renews_expired_token_in_isolated_dir_and_keeps_0600() {
        let (t3, _g) = temp_root("ok");
        let bin = write_mock_acp(
            &t3,
            &format!(
                r#"
read -r _init
printf '{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":1}}}}\n'
read -r _auth
# Regression guard: stdin must stay open while authenticate is in flight
# (real antigravity acp cancels the RPC on stdin EOF).
if ! read -t 0.05 -r _eof_check; then
  # rc=1 means EOF (closed); rc>128 means timeout (still open, expected).
  rc=$?
  if [ "$rc" -eq 1 ]; then
    printf '{{"jsonrpc":"2.0","id":2,"error":{{"code":-32800,"message":"Sign-in was cancelled."}}}}\n'
    exit 0
  fi
fi
printf '%s' '{}' > "$GEMINI_HOME/antigravity-acp/antigravity-oauth-token"
printf '{{"jsonrpc":"2.0","id":2,"result":{{"_meta":{{}}}}}}\n'
"#,
                token_json(FRESH, "fresh-access")
            ),
        );
        write_settings(&t3, "antigravity_3", true, "oauth-personal", &bin);
        let acp_dir = antigravity_profile_dir(&t3, "antigravity_3")
            .unwrap()
            .join("antigravity-acp");
        fs::create_dir_all(&acp_dir).unwrap();
        let tok_path = acp_dir.join("antigravity-oauth-token");
        fs::write(&tok_path, token_json(EXPIRED, "old-access")).unwrap();
        // Legacy profiles renew too, and their acp_token.json stays untouched.
        fs::write(acp_dir.join("acp_token.json"), "{}").unwrap();

        let mut backoff = RenewBackoff::default();
        let count =
            renew_expiring_tokens_at(&t3, &[agy_account("antigravity_3")], &mut backoff, NOW).await;
        assert_eq!(
            fs::read_to_string(acp_dir.join("acp_token.json")).unwrap(),
            "{}"
        );
        assert_eq!(count, 1);

        let updated: Value = serde_json::from_slice(&fs::read(&tok_path).unwrap()).unwrap();
        assert_eq!(updated["token"]["access_token"], "fresh-access");
        let mode = fs::metadata(&tok_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        // Second call sees fresh token and does nothing.
        let count2 =
            renew_expiring_tokens_at(&t3, &[agy_account("antigravity_3")], &mut backoff, NOW).await;
        assert_eq!(count2, 0);
    }

    #[tokio::test]
    async fn skips_non_oauth_methods() {
        let (t3, _g) = temp_root("skip");
        let marker = t3.join("should-not-run");
        let bin = write_mock_acp(&t3, &format!("touch {}\nexit 1", marker.display()));
        write_settings(&t3, "antigravity_4", true, "oauth-personal", &bin);
        let acp_dir = antigravity_profile_dir(&t3, "antigravity_4")
            .unwrap()
            .join("antigravity-acp");
        fs::create_dir_all(&acp_dir).unwrap();
        fs::write(
            acp_dir.join("antigravity-oauth-token"),
            token_json(EXPIRED, "old"),
        )
        .unwrap();

        let mut backoff = RenewBackoff::default();
        write_settings(&t3, "antigravity_4", true, "gemini-api-key", &bin);
        assert_eq!(
            renew_expiring_tokens_at(&t3, &[agy_account("antigravity_4")], &mut backoff, NOW).await,
            0
        );
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn failure_backs_off_until_token_file_changes() {
        let (t3, _g) = temp_root("backoff");
        let count_file = t3.join("invocations");
        let bin = write_mock_acp(
            &t3,
            &format!(
                r#"
echo x >> "{}"
read -r _init
printf '{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":1}}}}\n'
read -r _auth
printf '{{"jsonrpc":"2.0","id":2,"error":{{"code":-32800,"message":"Sign-in was cancelled."}}}}\n'
"#,
                count_file.display()
            ),
        );
        write_settings(&t3, "antigravity_5", true, "oauth-personal", &bin);
        let acp_dir = antigravity_profile_dir(&t3, "antigravity_5")
            .unwrap()
            .join("antigravity-acp");
        fs::create_dir_all(&acp_dir).unwrap();
        let tok_path = acp_dir.join("antigravity-oauth-token");
        fs::write(&tok_path, token_json(EXPIRED, "old-1")).unwrap();

        let mut backoff = RenewBackoff::default();
        assert_eq!(
            renew_expiring_tokens_at(&t3, &[agy_account("antigravity_5")], &mut backoff, NOW).await,
            0
        );
        assert_eq!(fs::read_to_string(&count_file).unwrap().lines().count(), 1);

        // Same file bytes -> skipped during backoff.
        assert_eq!(
            renew_expiring_tokens_at(&t3, &[agy_account("antigravity_5")], &mut backoff, NOW).await,
            0
        );
        assert_eq!(fs::read_to_string(&count_file).unwrap().lines().count(), 1);

        // User re-authenticated (different token bytes) -> retries immediately.
        fs::write(&tok_path, token_json(EXPIRED, "old-2")).unwrap();
        assert_eq!(
            renew_expiring_tokens_at(&t3, &[agy_account("antigravity_5")], &mut backoff, NOW).await,
            0
        );
        assert_eq!(fs::read_to_string(&count_file).unwrap().lines().count(), 2);
    }

    #[tokio::test]
    async fn cas_does_not_overwrite_concurrent_t3_refresh() {
        let (t3, _g) = temp_root("cas");
        let acp_dir = antigravity_profile_dir(&t3, "antigravity_6")
            .unwrap()
            .join("antigravity-acp");
        fs::create_dir_all(&acp_dir).unwrap();
        let tok_path = acp_dir.join("antigravity-oauth-token");
        fs::write(&tok_path, token_json(EXPIRED, "old-access")).unwrap();

        // Simulate T3 updating the live file while our isolated ACP process is running.
        let bin = write_mock_acp(
            &t3,
            &format!(
                r#"
read -r _init
printf '{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":1}}}}\n'
read -r _auth
printf '%s' '{}' > "{}"
printf '%s' '{}' > "$GEMINI_HOME/antigravity-acp/antigravity-oauth-token"
printf '{{"jsonrpc":"2.0","id":2,"result":{{"_meta":{{}}}}}}\n'
"#,
                token_json(FRESH, "t3-won"),
                tok_path.display(),
                token_json(FRESH, "agentdeck-lost"),
            ),
        );
        write_settings(&t3, "antigravity_6", true, "oauth-personal", &bin);

        let mut backoff = RenewBackoff::default();
        assert_eq!(
            renew_expiring_tokens_at(&t3, &[agy_account("antigravity_6")], &mut backoff, NOW).await,
            0
        );
        let live: Value = serde_json::from_slice(&fs::read(&tok_path).unwrap()).unwrap();
        assert_eq!(live["token"]["access_token"], "t3-won");
    }

    #[test]
    fn quota_windows_from_daily_endpoints() {
        // Shapes and figures from the live daily endpoints on 2026-11-25.
        let summary = serde_json::json!({"groups": [{"buckets": [
            {"bucketId": "3p-weekly", "remainingFraction": 0, "resetTime": "2026-11-27T23:24:24Z"},
            {"bucketId": "3p-5h", "remainingFraction": 1}
        ]}]});
        let models = serde_json::json!({"models": {
            "claude-opus-5-5-high": {"quotaInfo": {"remainingFraction": 1, "resetTime": "2026-11-26T01:17:27Z"}},
            "gemini-2.5-flash": {"quotaInfo": {"remainingFraction": 0.5, "resetTime": "2026-11-30T20:31:15Z"}},
            "gemini-3.8-flash-tiered": {"quotaInfo": {"remainingFraction": 0.10151518, "resetTime": "2026-11-30T20:31:15Z"}},
            "tab_flash_lite_preview": {"quotaInfo": {"remainingFraction": 1}}
        }});
        let w = quota_windows(&summary, &models, NOW);
        let get = |id: &str| w.iter().find(|x| x.id == id).unwrap();
        assert_eq!(w.len(), 3);
        assert_eq!(get("3p-weekly").used_percent, Some(100.0));
        assert_eq!(get("3p-5h").used_percent, Some(0.0));
        assert!((get("gemini-weekly").used_percent.unwrap() - 89.848482).abs() < 1e-6);

        // A Gemini quota resetting within 5 h is the 5-hour window.
        let soon = serde_json::json!({"models": {"gemini-3-flash": {"quotaInfo":
            {"remainingFraction": 0.9, "resetTime": "2026-11-24T12:00:00Z"}}}});
        assert_eq!(
            quota_windows(&serde_json::json!({}), &soon, NOW)[0].id,
            "gemini-5h"
        );

        // Exhausted: the fraction is omitted, only resetTime remains.
        let spent = serde_json::json!({"models": {"gemini-3-flash": {"quotaInfo":
            {"resetTime": "2026-11-30T20:03:40Z"}}}});
        let w = quota_windows(&serde_json::json!({}), &spent, NOW);
        assert_eq!(
            (w[0].id.as_str(), w[0].used_percent),
            ("gemini-weekly", Some(100.0))
        );
    }

    #[test]
    fn detects_fresh_token_behind_expired_sign_in_cache() {
        let (t3, _g) = temp_root("fresh-behind-cache");
        let acp_dir = antigravity_profile_dir(&t3, "antigravity_7")
            .unwrap()
            .join("antigravity-acp");
        fs::create_dir_all(&acp_dir).unwrap();
        let tok_path = acp_dir.join("antigravity-oauth-token");
        let accounts = [agy_account("antigravity_7")];
        let prev = AppState {
            schema_version: crate::model::SCHEMA_VERSION,
            updated_at: NOW,
            accounts: vec![AccountState::error(
                "antigravity_7",
                "Antigravity 7",
                "antigravity",
                "T3 reports no subscription limits for this instance: probeFailed (Antigravity's saved sign-in has expired. Start an Antigravity turn to renew it, then refresh limits.)",
            )],
        };

        fs::write(&tok_path, token_json(EXPIRED, "old")).unwrap();
        assert!(!has_fresh_token_with_expired_cache(
            &t3, &accounts, &prev, NOW
        ));

        fs::write(&tok_path, token_json(FRESH, "fresh")).unwrap();
        assert!(has_fresh_token_with_expired_cache(
            &t3, &accounts, &prev, NOW
        ));
    }

    #[test]
    fn reads_id_token_email_from_profile_token_file() {
        let (t3, _g) = temp_root("profile-email");
        let acp_dir = antigravity_profile_dir(&t3, "antigravity_8")
            .unwrap()
            .join("antigravity-acp");
        fs::create_dir_all(&acp_dir).unwrap();
        let tok_path = acp_dir.join("antigravity-oauth-token");

        assert_eq!(read_profile_email(&t3, "antigravity_8"), None);

        fs::write(
            &tok_path,
            serde_json::json!({
                "token": { "access_token": "tok" },
                "id_token_email": "  user@example.com  "
            })
            .to_string(),
        )
        .unwrap();
        assert_eq!(
            read_profile_email(&t3, "antigravity_8").as_deref(),
            Some("user@example.com")
        );

        fs::write(&tok_path, r#"{"id_token_email":"   "}"#).unwrap();
        assert_eq!(read_profile_email(&t3, "antigravity_8"), None);

        fs::write(&tok_path, b"not-json").unwrap();
        assert_eq!(read_profile_email(&t3, "antigravity_8"), None);
    }
}
