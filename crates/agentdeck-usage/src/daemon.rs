//! Polling loop and state publishing shared by `agentdeck daemon` and
//! `agentdeck usage once`.
//!
//! Per-provider deadline, last-known-good merge for balance sources, SIGTERM
//! drain, history writes, failure backoff. The daemon reads `[[usage]]` once at
//! startup (restart to apply).

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::task::JoinSet;

use crate::config::AccountConfig;
use crate::model::AppState;
use crate::{history, model, providers, util};

/// Single-writer lock is held on this file name inside the usage state dir.
pub const LOCK_FILE_NAME: &str = "usage.lock";

/// Returned when another daemon or `usage once` holds the lock.
/// Callers exit with code 3 and print the message.
#[derive(Debug)]
pub struct LockHeld(pub String);

impl std::fmt::Display for LockHeld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for LockHeld {}

/// Returned when the daemon refuses to start while `ai-usage.service` is
/// active. Callers exit with code 4 and print the message.
#[derive(Debug)]
pub struct OldServiceActive(pub String);

impl std::fmt::Display for OldServiceActive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for OldServiceActive {}

/// Every file a polling cycle reads or writes. All temp-dir injectable;
/// production instances come from [`DaemonPaths::for_agentdeck`].
#[derive(Debug, Clone)]
pub struct DaemonPaths {
    /// Canonical state, always 0600.
    pub canonical: PathBuf,
    /// Runtime copy on tmpfs (applet reads it), default mode.
    pub runtime: PathBuf,
    /// Old canonical copies, written with identical bytes + 0600 (empty = off).
    pub compat_canonical: Vec<PathBuf>,
    /// Old runtime copies, written with identical bytes (empty = off).
    pub compat_runtime: Vec<PathBuf>,
    /// Append-only usage history.
    pub history: PathBuf,
    /// Directory of provider cooldown/snapshot files (ensured to exist).
    pub cooldown_dir: PathBuf,
    /// Single-writer lock file.
    pub lock: PathBuf,
}

impl DaemonPaths {
    /// Production paths for the new daemon. Compat copies point at the old
    /// ai-usage state files the router and the current applet still read.
    /// They use the legacy (never overridden) paths even when the process
    /// override redirects providers into the new tree.
    pub fn for_agentdeck(state_dir: &Path, runtime_dir: &Path) -> Self {
        let dir = state_dir.join("usage");
        Self {
            canonical: dir.join("state.json"),
            runtime: runtime_dir.join("state.json"),
            compat_canonical: vec![util::legacy_persistent_state_dir().join("state.json")],
            compat_runtime: vec![util::state_file()],
            history: dir.join("history.jsonl"),
            cooldown_dir: dir.clone(),
            lock: dir.join(LOCK_FILE_NAME),
        }
    }

    /// Drop compat copies (for `usage once`, which never writes them by default).
    pub fn without_compat(mut self) -> Self {
        self.compat_canonical.clear();
        self.compat_runtime.clear();
        self
    }
}

/// Runtime dir for the new daemon, mirroring [`util::runtime_dir`].
pub fn agentdeck_runtime_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(dir).join("agentdeck").join("usage");
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".to_owned());
    std::env::temp_dir()
        .join(format!("agentdeck-{user}"))
        .join("usage")
}

/// True while `ai-usage.service` is active. Two pollers would double the 429
/// burn, so the new daemon refuses to start then. Missing systemctl counts as
/// inactive. Kept behind `fn() -> bool` so tests can inject it.
pub fn old_service_active() -> bool {
    match std::process::Command::new("systemctl")
        .arg("--user")
        .arg("is-active")
        .arg("ai-usage.service")
        .output()
    {
        Ok(out) => out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == "active",
        Err(_) => false,
    }
}

fn open_lock_file(lock_path: &Path, create: bool) -> Result<fs::File> {
    if create && let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .truncate(false)
        .open(lock_path)
        .with_context(|| format!("cannot open lock {}", lock_path.display()))
}

/// Acquire the single-writer lock, held until the guard drops. Fails with
/// [`LockHeld`] while another daemon or `usage once` runs.
pub fn acquire_lock(lock_path: &Path) -> Result<fs::File> {
    let file = open_lock_file(lock_path, true)?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => Err(LockHeld(format!(
            "another agentdeck daemon or `usage once` holds {}; refusing a second writer",
            lock_path.display()
        ))
        .into()),
        Err(std::fs::TryLockError::Error(error)) => {
            Err(error).with_context(|| format!("cannot lock {}", lock_path.display()))
        }
    }
}

/// Read-only probe for `doctor`: never creates the file.
pub fn lock_held(lock_path: &Path) -> bool {
    let Ok(file) = open_lock_file(lock_path, false) else {
        return false;
    };
    match file.try_lock() {
        Ok(()) => false,
        Err(std::fs::TryLockError::WouldBlock) => true,
        Err(std::fs::TryLockError::Error(_)) => false,
    }
}

/// One polling cycle over already-loaded accounts. Pure fetch+merge: no
/// signals, no writes, so `once` and the daemon loop share it.
pub async fn refresh_cycle(
    accounts: &[AccountConfig],
    stale_seconds: i64,
    previous: Option<AppState>,
) -> AppState {
    let mut tasks = JoinSet::new();
    for (index, account) in accounts.iter().cloned().enumerate() {
        tasks.spawn(async move {
            let id = account.id().to_owned();
            let name = account.name().to_owned();
            let provider = account.provider();
            let fetch = providers::fetch(account, stale_seconds);
            let state = match tokio::time::timeout(crate::PROVIDER_DEADLINE, fetch).await {
                Ok(state) => state,
                Err(_) => model::AccountState::error(
                    &id,
                    &name,
                    provider,
                    format!(
                        "Провайдер не ответил за {} секунд",
                        crate::PROVIDER_DEADLINE.as_secs()
                    ),
                ),
            };
            (index, state)
        });
    }

    let mut ordered = vec![None; accounts.len()];
    while let Some(result) = tasks.join_next().await {
        match result {
            Ok((index, state)) => ordered[index] = Some(state),
            Err(error) => eprintln!("Ошибка задачи обновления: {error}"),
        }
    }

    let accounts = ordered.into_iter().flatten().collect();
    let accounts = model::merge_with_previous(previous, accounts);

    AppState {
        schema_version: model::SCHEMA_VERSION,
        updated_at: util::unix_now(),
        accounts,
    }
}

/// Serialize once and write the identical bytes to canonical, runtime and
/// compat copies. Returns the bytes so tests can assert byte identity.
pub fn write_state(paths: &DaemonPaths, state: &AppState) -> Result<Vec<u8>> {
    let bytes = serde_json::to_string_pretty(state)?.into_bytes();
    util::atomic_write_mode(&paths.canonical, &bytes, Some(0o600))?;
    util::atomic_write(&paths.runtime, &bytes)?;
    // Compat is best-effort: a read-only old tree must not crash the daemon.
    for path in &paths.compat_canonical {
        if let Err(error) = util::atomic_write_mode(path, &bytes, Some(0o600)) {
            eprintln!(
                "warning: compat write to {} failed: {error:#}",
                path.display()
            );
        }
    }
    for path in &paths.compat_runtime {
        if let Err(error) = util::atomic_write(path, &bytes) {
            eprintln!(
                "warning: compat write to {} failed: {error:#}",
                path.display()
            );
        }
    }
    Ok(bytes)
}

pub fn read_state(paths: &DaemonPaths) -> Option<AppState> {
    if let Ok(raw) = fs::read_to_string(&paths.runtime)
        && let Ok(state) = serde_json::from_str::<AppState>(&raw)
    {
        return Some(state);
    }
    let raw = fs::read_to_string(&paths.canonical).ok()?;
    serde_json::from_str(&raw).ok()
}

pub fn failure_backoff(consecutive_failures: u32, max_interval: u64) -> Duration {
    let backoff = crate::config::MIN_REFRESH_SECONDS
        .saturating_mul(1 << (consecutive_failures.saturating_sub(1)).min(3));
    Duration::from_secs(backoff.min(max_interval))
}

pub(crate) fn has_transient_error(state: &AppState) -> bool {
    state.accounts.iter().any(|a| {
        if model::is_t3_subscription(&a.provider) {
            providers::t3::is_transient_account_state(a)
                || (a.status == "error"
                    && a.error
                        .as_deref()
                        .is_some_and(|e| e.starts_with("Провайдер не ответил")))
        } else {
            a.status == "error" || (a.status == "stale" && a.error.is_some())
        }
    })
}

/// Однострочный итог цикла в journal: статус, возраст данных и ошибка каждого
/// аккаунта. Смотреть: journalctl --user -u agentdeck-usage.service
fn log_cycle(state: &AppState) {
    let summary = state
        .accounts
        .iter()
        .map(|account| {
            let age = state.updated_at.saturating_sub(account.updated_at);
            let mut part = format!(
                "{}={} (данные: {})",
                account.id,
                account.status,
                providers::claude::humanize_age(age)
            );
            if let Some(error) = &account.error {
                part.push_str(&format!(" [{}]", error.replace('\n', "; ")));
            }
            part
        })
        .collect::<Vec<_>>()
        .join(" | ");
    eprintln!("[цикл] {summary}");
}

/// Run the polling loop until SIGTERM/SIGINT. `load` provides fresh
/// `(accounts, refresh_seconds, stale_seconds)` every cycle.
/// Holds the single-writer lock for life; refuses while the old service is
/// active.
pub async fn run_daemon(
    paths: &DaemonPaths,
    old_service_active: fn() -> bool,
    mut load: impl FnMut() -> Result<(Vec<AccountConfig>, u64, i64)>,
) -> Result<()> {
    if old_service_active() {
        return Err(OldServiceActive(
            "ai-usage.service is active; refusing a second poller (two pollers double the 429 burn). \
             Migration runbook step: `systemctl --user stop ai-usage.service` first"
                .to_owned(),
        )
        .into());
    }
    let _lock = acquire_lock(&paths.lock)?;
    if !paths.cooldown_dir.exists() {
        fs::create_dir_all(&paths.cooldown_dir)?;
    }
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut consecutive_failures: u32 = 0;
    let mut last_history = None;
    loop {
        let (accounts, refresh_seconds, stale_seconds) = load()?;
        let interval = refresh_seconds.max(crate::config::MIN_REFRESH_SECONDS);
        let state = refresh_cycle(&accounts, stale_seconds, read_state(paths)).await;
        write_state(paths, &state)?;
        log_cycle(&state);
        // Журнал вторичен: его сбой не должен останавливать обновление панели.
        if let Err(error) = history::append(&paths.history, &state, &mut last_history) {
            eprintln!("Не удалось дописать history.jsonl: {error}");
        }

        let sleep_duration = if has_transient_error(&state) {
            consecutive_failures = consecutive_failures.saturating_add(1);
            failure_backoff(consecutive_failures, interval)
        } else {
            consecutive_failures = 0;
            Duration::from_secs(interval)
        };

        tokio::select! {
            _ = tokio::time::sleep(sleep_duration) => {},
            _ = terminate.recv() => break,
            _ = interrupt.recv() => break,
        }
    }
    Ok(())
}

/// One cycle. Refuses against the lock like the daemon, but never against the
/// old service (the runbook smoke step runs while it is active). No history
/// write, matching the old `once`.
pub async fn run_once(
    paths: &DaemonPaths,
    accounts: &[AccountConfig],
    stale_seconds: i64,
    dry_run: bool,
) -> Result<AppState> {
    let _lock = acquire_lock(&paths.lock)?;
    let state = refresh_cycle(accounts, stale_seconds, read_state(paths)).await;
    if !dry_run {
        write_state(paths, &state)?;
    }
    Ok(state)
}

/// 0600 env file with provider keys. Never prints values.
pub fn agentdeck_secrets_file(config_dir: &Path) -> PathBuf {
    config_dir.join("usage.env")
}

/// Load [`agentdeck_secrets_file`] with the shared env loader. A wrong mode is
/// fixed to 0600 with a warning (path only, never values).
pub fn load_agentdeck_secrets(config_dir: &Path) -> Result<()> {
    let path = agentdeck_secrets_file(config_dir);
    if !path.exists() {
        return Ok(());
    }
    if let Ok(meta) = fs::metadata(&path) {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o777 != 0o600 {
            eprintln!(
                "warning: {} must be 0600 (provider keys); fixing",
                path.display()
            );
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
        }
    }
    util::load_secrets_env_from(&path)
}

// `usage doctor`: read-only, no network. -------------------------------

#[derive(Debug, Clone, serde::Serialize)]
pub struct DoctorSource {
    pub id: String,
    pub kind: String,
    pub instance: Option<String>,
    /// Source pointer: path or env-var NAME, never a secret value.
    pub source: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DoctorBackoff {
    pub id: String,
    pub file: PathBuf,
    pub age_secs: Option<i64>,
    pub retry_in_secs: Option<i64>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DoctorStateFile {
    pub path: PathBuf,
    pub updated_at: Option<i64>,
    pub age_secs: Option<i64>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DoctorReport {
    pub sources: Vec<DoctorSource>,
    pub backoffs: Vec<DoctorBackoff>,
    pub canonical: DoctorStateFile,
    pub runtime: DoctorStateFile,
    pub compat_canonical: Vec<DoctorStateFile>,
    pub compat_runtime: Vec<DoctorStateFile>,
    pub old_service_active: bool,
    pub lock_held: bool,
}

fn state_file_info(path: &Path, now: i64) -> DoctorStateFile {
    let updated_at = fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|v| v.get("updated_at").and_then(serde_json::Value::as_i64));
    DoctorStateFile {
        path: path.to_owned(),
        age_secs: updated_at.map(|t| now.saturating_sub(t)),
        updated_at,
    }
}

pub fn doctor(
    paths: &DaemonPaths,
    accounts: &[AccountConfig],
    old_service_active: fn() -> bool,
) -> DoctorReport {
    let now = util::unix_now();
    let t3_root = agentdeck_core::t3::t3_root().ok();
    let t3_source = |kind: &str, instance: Option<&str>| -> (Option<String>, String) {
        let resolved = providers::t3::resolve_instance_slug(kind, instance);
        let source = match (&resolved, &t3_root) {
            (Some(inst), Some(root)) => agentdeck_core::t3::cache_file(root, inst)
                .display()
                .to_string(),
            (Some(inst), None) => format!("t3:caches/{inst}.json"),
            (None, _) => "missing T3 instance".to_owned(),
        };
        (resolved, source)
    };

    let sources = accounts
        .iter()
        .map(|a| {
            let (kind, instance, source) = match a {
                AccountConfig::Claude { instance, .. } => {
                    let (inst, src) = t3_source("claude", instance.as_deref());
                    ("claude", inst, src)
                }
                AccountConfig::Codex { instance, .. } => {
                    let (inst, src) = t3_source("codex", instance.as_deref());
                    ("codex", inst, src)
                }
                AccountConfig::Deepseek { api_key_env, .. } => {
                    ("deepseek", None, api_key_env.clone())
                }
                AccountConfig::Openrouter { api_key_env, .. } => {
                    ("openrouter", None, api_key_env.clone())
                }
                AccountConfig::Vercel { api_key_env, .. } => ("vercel", None, api_key_env.clone()),
                AccountConfig::Antigravity { instance, .. } => {
                    let (inst, src) = t3_source("antigravity", instance.as_deref());
                    ("antigravity", inst, src)
                }
                AccountConfig::OpencodeGo { instance, .. } => {
                    let (inst, src) = t3_source("opencode-go", instance.as_deref());
                    ("opencode-go", inst, src)
                }
            };
            DoctorSource {
                id: a.id().to_owned(),
                kind: kind.to_owned(),
                instance,
                source,
            }
        })
        .collect::<Vec<_>>();
    DoctorReport {
        sources,
        backoffs: Vec::new(),
        canonical: state_file_info(&paths.canonical, now),
        runtime: state_file_info(&paths.runtime, now),
        compat_canonical: paths
            .compat_canonical
            .iter()
            .map(|p| state_file_info(p, now))
            .collect(),
        compat_runtime: paths
            .compat_runtime
            .iter()
            .map(|p| state_file_info(p, now))
            .collect(),
        old_service_active: old_service_active(),
        lock_held: lock_held(&paths.lock),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static N: AtomicU64 = AtomicU64::new(0);

    struct Guard(PathBuf);
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn temp_paths(tag: &str) -> (DaemonPaths, Guard) {
        let dir = std::env::temp_dir().join(format!(
            "agentdeck-daemon-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&dir);
        let usage = dir.join("usage");
        let runtime = dir.join("runtime");
        let compat = dir.join("compat");
        (
            DaemonPaths {
                canonical: usage.join("state.json"),
                runtime: runtime.join("state.json"),
                compat_canonical: vec![compat.join("state.json")],
                compat_runtime: vec![compat.join("run-state.json")],
                history: usage.join("history.jsonl"),
                cooldown_dir: usage.clone(),
                lock: usage.join(LOCK_FILE_NAME),
            },
            Guard(dir),
        )
    }

    fn fixture_state() -> AppState {
        let raw = serde_json::json!({
            "schema_version": model::SCHEMA_VERSION,
            "updated_at": 1_700_000_000,
            "accounts": [{
                "id": "claude-main", "name": "Claude", "provider": "claude",
                "status": "ok", "plan": "Max 5x", "email": null, "model": null,
                "windows": [], "balances": [], "error": null, "updated_at": 1_700_000_000
            }]
        });
        serde_json::from_value(raw).unwrap()
    }

    #[test]
    fn failure_backoff_steps_up_and_caps_at_max_interval() {
        assert_eq!(failure_backoff(1, 120), Duration::from_secs(15));
        assert_eq!(failure_backoff(2, 120), Duration::from_secs(30));
        assert_eq!(failure_backoff(3, 120), Duration::from_secs(60));
        assert_eq!(failure_backoff(4, 120), Duration::from_secs(120));
        assert_eq!(failure_backoff(5, 120), Duration::from_secs(120));
        assert_eq!(failure_backoff(1, 20), Duration::from_secs(15));
        assert_eq!(failure_backoff(2, 20), Duration::from_secs(20));
    }

    #[test]
    fn read_state_falls_back_to_canonical_storage() {
        let (paths, _g) = temp_paths("read");
        let app_state = fixture_state();
        let raw = serde_json::to_string(&app_state).unwrap();
        fs::create_dir_all(paths.canonical.parent().unwrap()).unwrap();
        fs::write(&paths.canonical, &raw).unwrap();

        let loaded = read_state(&paths).unwrap();
        assert_eq!(loaded.updated_at, 1_700_000_000);

        fs::create_dir_all(paths.runtime.parent().unwrap()).unwrap();
        let mut runtime_state = app_state.clone();
        runtime_state.updated_at = 67890;
        fs::write(
            &paths.runtime,
            serde_json::to_string(&runtime_state).unwrap(),
        )
        .unwrap();
        let loaded = read_state(&paths).unwrap();
        assert_eq!(loaded.updated_at, 67890);
    }

    #[test]
    fn write_state_uses_identical_bytes_everywhere() {
        let (paths, _g) = temp_paths("bytes");
        let state = fixture_state();
        let returned = write_state(&paths, &state).unwrap();

        for path in [&paths.canonical]
            .into_iter()
            .chain(paths.compat_canonical.iter())
            .chain(paths.compat_runtime.iter())
            .chain([&paths.runtime])
        {
            assert_eq!(fs::read(path).unwrap(), returned, "{}", path.display());
        }
        // Account ids survive byte-for-byte (router compares them verbatim).
        assert!(String::from_utf8(returned).unwrap().contains("claude-main"));

        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&paths.canonical).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "canonical state got {mode:o}");
    }

    #[tokio::test]
    async fn second_writer_exits_3_while_lock_is_held() {
        let (paths, _g) = temp_paths("lock");
        let _first = acquire_lock(&paths.lock).unwrap();
        assert!(lock_held(&paths.lock));

        let error = acquire_lock(&paths.lock).unwrap_err();
        assert!(error.downcast_ref::<LockHeld>().is_some(), "{error:?}");

        // `once` refuses the same way, without touching provider APIs
        // (empty accounts spawn no fetches).
        let error = run_once(&paths, &[], 3600, false).await.unwrap_err();
        assert!(error.downcast_ref::<LockHeld>().is_some(), "{error:?}");
    }

    #[tokio::test]
    async fn daemon_refuses_while_old_service_is_active() {
        let (paths, _g) = temp_paths("refuse");
        let error = run_daemon(
            &paths,
            || true,
            || unreachable!("must refuse before loading"),
        )
        .await
        .unwrap_err();
        assert!(
            error.downcast_ref::<OldServiceActive>().is_some(),
            "{error:?}"
        );
        assert!(
            !paths.lock.exists(),
            "refusal must not create the lock file"
        );
    }

    #[tokio::test]
    async fn dry_run_writes_nothing() {
        let (paths, _g) = temp_paths("dry");
        let state = run_once(&paths, &[], 3600, true).await.unwrap();
        assert_eq!(state.schema_version, model::SCHEMA_VERSION);
        assert!(state.accounts.is_empty());
        assert!(!paths.canonical.exists());
        assert!(!paths.runtime.exists());
    }

    #[test]
    fn secrets_file_is_fixed_to_0600_without_printing_values() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "agentdeck-secrets-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let var = format!(
            "AGENTDECK_TEST_SECRET_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        );
        let path = agentdeck_secrets_file(&dir);
        // Written 0644 on purpose (note: mode passes through umask, so fix explicitly).
        fs::write(&path, format!("{var}=s3cr3t-value\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        load_agentdeck_secrets(&dir).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "secrets file got {mode:o}");
        assert_eq!(std::env::var(&var).unwrap(), "s3cr3t-value");
        unsafe { std::env::remove_var(&var) };
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn doctor_is_read_only_and_reports_everything() {
        let (paths, _g) = temp_paths("doctor");
        let accounts = vec![
            AccountConfig::Antigravity {
                id: "agy-main".to_owned(),
                name: "Agy".to_owned(),
                instance: Some("antigravity".to_owned()),
                token_file: None,
            },
            AccountConfig::OpencodeGo {
                id: "opencode-go".to_owned(),
                name: "OpenCode Go".to_owned(),
                instance: None,
                db_file: None,
            },
        ];
        let report = doctor(&paths, &accounts, || false);
        assert_eq!(report.sources.len(), 2);
        assert_eq!(report.sources[0].id, "agy-main");
        assert_eq!(report.sources[0].instance.as_deref(), Some("antigravity"));
        assert!(report.sources[0].source.contains("antigravity.json"));
        assert_eq!(report.sources[1].instance.as_deref(), Some("opencode"));
        assert!(report.sources[1].source.contains("opencode.json"));
        assert!(report.backoffs.is_empty());
        assert!(!report.old_service_active);
        assert!(!report.lock_held);
        assert!(report.canonical.updated_at.is_none());
        assert_eq!(report.compat_canonical.len(), 1);
        // Nothing was created as a side effect.
        assert!(!paths.canonical.exists());
        assert!(!paths.lock.exists());

        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("agy-main"));
    }

    #[tokio::test]
    async fn refresh_cycle_does_not_resurface_old_windows_on_subscription_error() {
        let prev = AppState {
            schema_version: model::SCHEMA_VERSION,
            updated_at: 1_700_000_000,
            accounts: vec![model::AccountState {
                id: "claude-main".to_owned(),
                name: "Claude".to_owned(),
                provider: "claude".to_owned(),
                status: "ok".to_owned(),
                plan: Some("Pro".to_owned()),
                email: None,
                model: None,
                windows: vec![model::QuotaWindow::new(
                    "session",
                    "5 часов",
                    42.0,
                    Some(model::FIVE_HOUR_MINS),
                    None,
                )],
                balances: Vec::new(),
                error: None,
                updated_at: 1_700_000_000,
            }],
        };
        let accounts = vec![AccountConfig::Claude {
            id: "claude-main".to_owned(),
            name: "Claude".to_owned(),
            plan: None,
            instance: None,
            config_dir: PathBuf::new(),
        }];
        let state = refresh_cycle(&accounts, 900, Some(prev)).await;
        assert_eq!(state.accounts.len(), 1);
        assert_eq!(state.accounts[0].status, "error");
        assert!(state.accounts[0].windows.is_empty());
        assert!(state.accounts[0].error.is_some());
    }

    #[test]
    fn t3_non_transient_errors_do_not_trigger_short_backoff() {
        let dir = std::env::temp_dir().join(format!(
            "agentdeck-daemon-retry-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("userdata")).unwrap();
        fs::create_dir_all(dir.join("caches")).unwrap();
        fs::write(
            dir.join("userdata/settings.json"),
            r#"{"providerInstances":{
                "claude-ok":{"driver":"claudeAgent","enabled":true},
                "claude-dis":{"driver":"claudeAgent","enabled":false}
            }}"#,
        )
        .unwrap();
        let _guard = Guard(dir.clone());

        let wrap = |account: model::AccountState| AppState {
            schema_version: model::SCHEMA_VERSION,
            updated_at: 1_794_570_000,
            accounts: vec![account],
        };
        let src = |id: &'static str, inst: Option<&'static str>| providers::t3::T3Source {
            id,
            name: id,
            provider: "claude",
            instance: inst,
            explicit_plan: None,
        };

        // 1. Instance disabled in T3 -> not transient
        let disabled = providers::t3::fetch_from_root(
            &dir,
            src("claude-dis", Some("claude-dis")),
            900,
            1_794_570_000,
        );
        assert_eq!(disabled.status, "error");
        assert!(!has_transient_error(&wrap(disabled)));

        // 2. Source without T3 instance -> not transient
        let no_inst = providers::t3::fetch_from_root(&dir, src("claude-none", None), 900, 1_794_570_000);
        assert_eq!(no_inst.status, "error");
        assert!(!has_transient_error(&wrap(no_inst)));

        // 3. usageLimits.unavailable -> not transient
        fs::write(
            agentdeck_core::t3::cache_file(&dir, "claude-ok"),
            r#"{"enabled":true,"status":"ready","usageLimits":{"checkedAt":"2026-11-13T11:35:00Z","windows":[],"unavailable":{"reason":"unsupported"}}}"#,
        )
        .unwrap();
        let unavail = providers::t3::fetch_from_root(
            &dir,
            src("claude-ok", Some("claude-ok")),
            900,
            1_794_570_000,
        );
        assert_eq!(unavail.status, "error");
        assert!(!has_transient_error(&wrap(unavail)));

        // 4. Stale T3 cache -> not transient
        fs::write(
            agentdeck_core::t3::cache_file(&dir, "claude-ok"),
            r#"{"enabled":true,"status":"ready","usageLimits":{"checkedAt":"2026-11-13T10:00:00Z","windows":[{"id":"five_hour","usedPercent":10}]}}"#,
        )
        .unwrap();
        let stale = providers::t3::fetch_from_root(
            &dir,
            src("claude-ok", Some("claude-ok")),
            900,
            1_794_570_000,
        );
        assert_eq!(stale.status, "stale");
        assert!(!has_transient_error(&wrap(stale)));

        // 5. Half-written / parse failure -> transient
        fs::write(
            agentdeck_core::t3::cache_file(&dir, "claude-ok"),
            r#"{"enabled":true,"status":"ready","usageLimits":{"#,
        )
        .unwrap();
        let parse_err = providers::t3::fetch_from_root(
            &dir,
            src("claude-ok", Some("claude-ok")),
            900,
            1_794_570_000,
        );
        assert_eq!(parse_err.status, "error");
        assert!(has_transient_error(&wrap(parse_err)));

        // 6. Missing cache file during rewrite -> transient
        fs::remove_file(agentdeck_core::t3::cache_file(&dir, "claude-ok")).unwrap();
        let missing = providers::t3::fetch_from_root(
            &dir,
            src("claude-ok", Some("claude-ok")),
            900,
            1_794_570_000,
        );
        assert_eq!(missing.status, "waiting");
        assert!(has_transient_error(&wrap(missing)));

        // 7. Balance provider error/stale -> still transient
        let bal_err = model::AccountState::error("or", "OpenRouter", "openrouter", "network timeout");
        assert!(has_transient_error(&wrap(bal_err)));
    }
}
