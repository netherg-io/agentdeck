//! Shared LLM judge channel selection, fallback trigger classification, and
//! cross-process 429 block state reused by router, approver and coordinator.
//!
//! No fixed RPM cap: HTTP 429 + `Retry-After` (default 30s) is the only rate-limit
//! signal. When a `(provider, model)` pair receives 429, a private atomic block
//! file is written under `<runtime_base>/agentdeck-judge-blocks-<uid>/<sha>.json`
//! (where `<runtime_base>` is `$XDG_RUNTIME_DIR` or `$HOME/.cache/agentdeck/runtime`,
//! mode `0700`) so no consumer process hammers the blocked pair until the block expires.

use std::ffi::OsStr;
use std::future::Future;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Default `Retry-After` when a 429 response omits the header or sends a non-integer.
pub const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(30);
/// Base directory name under the trusted per-user runtime dir storing per-pair 429 blocks.
pub const JUDGE_BLOCKS_DIR_NAME: &str = "agentdeck-judge-blocks";
/// Upper bound on rate-limit wait loops within one deadline so a broken server
/// replying `Retry-After: 0` cannot spin forever.
const MAX_RATE_LIMIT_RETRIES: usize = 8;
/// Maximum byte length enforced symmetrically on both reads and writes of
/// private runtime JSON files (429 blocks, Jev cache, and queue `.req`/`.done`
/// entries).
///
/// The router itself accepts unbounded briefs, while T3/workflow enforces a
/// 120 000-character task limit (`MAX_TASK`). Because queue `.req` files store
/// both `request` and JSON-escaped `body`, a 120 000-char brief with multibyte
/// UTF-8 or heavy escaping reaches ~1–1.6 MiB; 4 MiB gives >2.5x headroom above
/// T3's maximum while still bounding memory against corrupt files.
pub const MAX_PRIVATE_FILE_BYTES: u64 = 4 * 1024 * 1024;
/// Upper bound on waiting for a per-pair block lock file for direct sync callers.
pub const DEFAULT_BLOCK_LOCK_WAIT: Duration = Duration::from_secs(1);
/// Tighter lock-wait ceiling inside [`dispatch_once`] so a suspended lock holder
/// neither stalls callers nor starves a subsequent fallback attempt.
const DISPATCH_BLOCK_LOCK_WAIT: Duration = Duration::from_millis(200);
const BLOCK_LOCK_POLL: Duration = Duration::from_millis(10);

/// Which judge channel produced a decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JudgeChannel {
    Primary,
    Fallback,
}

impl JudgeChannel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Fallback => "fallback",
        }
    }
}

/// Concrete `(provider, model)` judge target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JudgeEndpoint {
    pub provider: String,
    pub model: String,
}

impl JudgeEndpoint {
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
        }
    }
}

/// How a judge call failure should interact with fallback and block state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackTrigger {
    /// HTTP 429: record a cross-process block for `retry_after`, then fall back
    /// immediately (or wait if no fallback and the block ends before deadline).
    RateLimited { retry_after: Duration },
    /// HTTP 5xx, network error, or timeout: fall back without writing a 429 block.
    Transient,
    /// HTTP 401/403/other 4xx, response parse error, or missing key: never fall back.
    None,
}

/// Parse an HTTP `Retry-After` header value (integer seconds); defaults to 30s.
pub fn parse_retry_after(header_value: Option<&str>) -> Duration {
    header_value
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_RETRY_AFTER)
}

/// Classify a non-success HTTP status code into a fallback trigger.
pub fn classify_http_status(status: u16, retry_after: Option<Duration>) -> FallbackTrigger {
    match status {
        429 => FallbackTrigger::RateLimited {
            retry_after: retry_after.unwrap_or(DEFAULT_RETRY_AFTER),
        },
        500..=599 => FallbackTrigger::Transient,
        _ => FallbackTrigger::None,
    }
}

/// Human-readable error when a 429 block cannot finish within the remaining deadline.
pub fn format_rate_limit_exceeded(
    endpoint: &JudgeEndpoint,
    retry_after: Duration,
    remaining_budget: Duration,
) -> String {
    format!(
        "judge {}/{} rate limited (429): retry after {}s exceeds remaining deadline ({}s)",
        endpoint.provider,
        endpoint.model,
        retry_after.as_secs(),
        remaining_budget.as_secs(),
    )
}

/// Format an active 429 block with its UTC `blocked until` timestamp and
/// remaining seconds (used by interactive probes such as GUI/CLI checks).
pub fn format_blocked_until(
    endpoint: &JudgeEndpoint,
    remaining: Duration,
    now: SystemTime,
) -> String {
    let until = now + remaining;
    let secs = remaining.as_secs().max(u64::from(!remaining.is_zero()));
    format!(
        "judge {}/{} rate limited (429): blocked until {} ({secs}s remaining)",
        endpoint.provider,
        endpoint.model,
        format_utc_iso(until),
    )
}

fn format_utc_iso(t: SystemTime) -> String {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let mut days = secs.div_euclid(86400);
    let tod = secs.rem_euclid(86400);
    let (mut y, mut m, mut d) = (1970i64, 1i64, 1i64);
    while days >= 365 {
        let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
        let len = if leap { 366 } else { 365 };
        if days < len {
            break;
        }
        days -= len;
        y += 1;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    for (i, len) in [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ]
    .into_iter()
    .enumerate()
    {
        if days < len {
            m = i as i64 + 1;
            d = days + 1;
            break;
        }
        days -= len;
    }
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

/// Remaining per-request HTTP timeout bounded by both the endpoint's own
/// timeout and the caller's single absolute monotonic deadline. Returns `None`
/// once the deadline has already passed.
pub fn request_timeout(own_timeout: Duration, deadline: Instant) -> Option<Duration> {
    let rem = deadline.saturating_duration_since(Instant::now());
    (!rem.is_zero()).then(|| own_timeout.min(rem))
}

/// Budget parameters for a judge dispatch call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DispatchBudget {
    /// Total end-to-end monotonic budget from call start.
    pub total: Duration,
    /// Small fixed time reserved for an unblocked fallback so a hanging primary
    /// cannot starve it while normal slow primary calls keep their full HTTP timeout.
    /// Applied only when a distinct, unblocked fallback is configured (and capped at
    /// half of remaining time when the remaining budget is tiny).
    pub fallback_reserve: Duration,
    /// Optional cap on total time spent waiting for 429 blocks to expire
    /// (`None` = bounded only by `total`).
    pub max_rate_limit_wait: Option<Duration>,
}

impl DispatchBudget {
    pub const fn new(total: Duration, fallback_reserve: Duration) -> Self {
        Self {
            total,
            fallback_reserve,
            max_rate_limit_wait: None,
        }
    }

    pub const fn with_max_rate_limit_wait(mut self, max_wait: Duration) -> Self {
        self.max_rate_limit_wait = Some(max_wait);
        self
    }
}

/// Compute the primary attempt's monotonic deadline. When `fallback_reserve` is
/// non-zero (an unblocked fallback is configured), subtract up to
/// `fallback_reserve` (capped at half of `remaining` for tiny budgets) so normal
/// slow primary calls keep their full per-request timeout while a hanging
/// primary still leaves time for fallback.
pub fn primary_attempt_deadline(
    now: Instant,
    deadline: Instant,
    fallback_reserve: Duration,
) -> Instant {
    if fallback_reserve.is_zero() {
        return deadline;
    }
    let remaining = deadline.saturating_duration_since(now);
    if remaining.is_zero() {
        return now;
    }
    let reserve = fallback_reserve.min(remaining / 2);
    let share = remaining
        .saturating_sub(reserve)
        .max(Duration::from_millis(1))
        .min(remaining);
    now + share
}

#[cfg(unix)]
fn current_euid() -> u32 {
    // SAFETY: `geteuid` has no preconditions and never fails.
    unsafe { libc::geteuid() }
}

/// Resolve the trusted per-user runtime base directory from explicit env inputs:
/// 1. `xdg_runtime_dir` when set to an existing, absolute, non-symlink directory
///    owned by the current user with no group or other permissions (`mode & 0o077 == 0`).
/// 2. Otherwise fall back to `<home>/.cache/agentdeck/runtime` created `0700`
///    and verified owner/type/mode before use.
///
/// Never falls back to a predictable shared `/tmp` path.
pub fn runtime_base_from(
    xdg_runtime_dir: Option<&OsStr>,
    home: Option<&OsStr>,
) -> std::io::Result<PathBuf> {
    if let Some(xdg) = xdg_runtime_dir.filter(|v| !v.is_empty()) {
        let dir = PathBuf::from(xdg);
        if dir.is_absolute()
            && let Ok(meta) = std::fs::symlink_metadata(&dir)
        {
            #[cfg(unix)]
            let trusted = verify_owner_and_private_mode(&meta, current_euid(), true).is_ok();
            #[cfg(not(unix))]
            let trusted = !meta.file_type().is_symlink() && meta.file_type().is_dir();
            if trusted {
                return Ok(dir);
            }
        }
    }
    let home = home.filter(|v| !v.is_empty()).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "cannot resolve runtime dir: XDG_RUNTIME_DIR is unusable and HOME is not set",
        )
    })?;
    let dir = PathBuf::from(home).join(".cache/agentdeck/runtime");
    create_private_dir(&dir)?;
    Ok(dir)
}

/// Trusted per-user runtime base directory shared across all consumers (Jev cache,
/// queue spool, and 429 block store): `$XDG_RUNTIME_DIR` when it is an existing,
/// absolute, user-owned `0700`-private directory, otherwise
/// `$HOME/.cache/agentdeck/runtime` created `0700` and verified.
pub fn default_runtime_base() -> std::io::Result<PathBuf> {
    runtime_base_from(
        std::env::var_os("XDG_RUNTIME_DIR").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

/// Per-user subdirectory name for cross-process 429 block files.
pub fn blocks_dir_name() -> String {
    #[cfg(unix)]
    {
        format!("{JUDGE_BLOCKS_DIR_NAME}-{}", current_euid())
    }
    #[cfg(not(unix))]
    {
        JUDGE_BLOCKS_DIR_NAME.to_string()
    }
}

/// Resolve the per-user judge block directory inside a runtime base directory.
pub fn blocks_dir_for_runtime(runtime_base: &Path) -> PathBuf {
    runtime_base.join(blocks_dir_name())
}

#[cfg(unix)]
fn verify_owner_and_private_mode(
    meta: &std::fs::Metadata,
    expected_uid: u32,
    must_be_dir: bool,
) -> std::io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let ft = meta.file_type();
    if ft.is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "refusing symlink in judge block store",
        ));
    }
    if must_be_dir && !ft.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "judge block store path is not a directory",
        ));
    }
    if !must_be_dir && !ft.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "judge block entry is not a regular file",
        ));
    }
    if meta.uid() != expected_uid {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "judge block store owned by uid {}, expected {expected_uid}",
                meta.uid()
            ),
        ));
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("judge block store permissions {mode:o} are accessible to group/others"),
        ));
    }
    Ok(())
}

/// Ensure `dir` exists as a real, current-user-owned directory with `0700`
/// permissions (never following a symlink or accepting another user's dir).
pub fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
        let euid = current_euid();
        match std::fs::symlink_metadata(dir) {
            Ok(meta) => {
                if !meta.file_type().is_dir() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "judge private dir is not a real directory",
                    ));
                }
                if meta.uid() != euid {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "judge private dir is owned by another user",
                    ));
                }
                std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if let Some(parent) = dir.parent()
                    && !parent.as_os_str().is_empty()
                {
                    std::fs::create_dir_all(parent)?;
                }
                match std::fs::DirBuilder::new().mode(0o700).create(dir) {
                    Ok(()) => {}
                    Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(err) => return Err(err),
                }
                let meta = std::fs::symlink_metadata(dir)?;
                if !meta.file_type().is_dir() || meta.uid() != euid {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "judge private dir failed ownership/type check after creation",
                    ));
                }
                std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
            }
            Err(e) => return Err(e),
        }
        let verified = std::fs::symlink_metadata(dir)?;
        verify_owner_and_private_mode(&verified, euid, true)?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}

fn verify_private_dir_for_read(dir: &Path) -> std::io::Result<bool> {
    let meta = match std::fs::symlink_metadata(dir) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    #[cfg(unix)]
    {
        verify_owner_and_private_mode(&meta, current_euid(), true)?;
    }
    #[cfg(not(unix))]
    {
        if !meta.file_type().is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "judge block store path is not a directory",
            ));
        }
    }
    Ok(true)
}

fn read_private_file(path: &Path) -> std::io::Result<Option<String>> {
    let mut opts = std::fs::File::options();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    let file = match opts.open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let meta = file.metadata()?;
    #[cfg(unix)]
    {
        verify_owner_and_private_mode(&meta, current_euid(), false)?;
    }
    #[cfg(not(unix))]
    {
        if !meta.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "judge block entry is not a regular file",
            ));
        }
    }
    let mut text = String::new();
    file.take(MAX_PRIVATE_FILE_BYTES + 1)
        .read_to_string(&mut text)?;
    if text.len() as u64 > MAX_PRIVATE_FILE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "private runtime entry exceeds size cap",
        ));
    }
    Ok(Some(text))
}

/// Verify `dir` is a real, current-user-owned `0700` directory and read `path`
/// without following symlinks, enforcing regular-file type, current-user
/// ownership, private permissions (`mode & 0o077 == 0`), and a size cap.
/// Returns `Ok(None)` when either `dir` or `path` does not exist.
pub fn read_private_entry(dir: &Path, path: &Path) -> std::io::Result<Option<String>> {
    if !verify_private_dir_for_read(dir)? {
        return Ok(None);
    }
    read_private_file(path)
}

/// Atomically write `text` to `path` with private `0600` permissions via a
/// unique sibling `.tmp` file so concurrent processes cannot clobber each other.
/// Enforces [`MAX_PRIVATE_FILE_BYTES`] at write time so entries written here
/// can never exceed [`read_private_entry`]'s read limit.
pub fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    if text.len() as u64 > MAX_PRIVATE_FILE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "private runtime entry ({} bytes) exceeds {MAX_PRIVATE_FILE_BYTES}-byte limit",
                text.len()
            ),
        ));
    }
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let tmp = PathBuf::from(format!(
        "{}.{}.{}.tmp",
        path.display(),
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let mut opts = std::fs::File::options();
    opts.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut f = opts.open(&tmp)?;
    let write_res = (|| -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        f.write_all(text.as_bytes())?;
        Ok(())
    })();
    drop(f);
    if let Err(e) = write_res {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
struct BlockRecord {
    provider: String,
    model: String,
    blocked_until_ms: u64,
}

fn system_time_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

fn endpoint_stem(endpoint: &JudgeEndpoint) -> String {
    let mut h = Sha256::new();
    h.update(endpoint.provider.as_bytes());
    h.update([0u8]);
    h.update(endpoint.model.as_bytes());
    format!("{:x}", h.finalize())
}

/// Cross-process 429 block store backed by private JSON files per `(provider, model)`.
#[derive(Debug, Clone)]
pub struct BlockStore {
    dir: PathBuf,
}

impl BlockStore {
    pub fn default_runtime() -> std::io::Result<Self> {
        Ok(Self::for_runtime_dir(&default_runtime_base()?))
    }

    /// Resolve the shared per-user block directory inside `runtime_dir` so all
    /// consumers (router, approver, coordinator, probes) share one location.
    pub fn for_runtime_dir(runtime_dir: &Path) -> Self {
        Self {
            dir: blocks_dir_for_runtime(runtime_dir),
        }
    }

    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn block_path(&self, endpoint: &JudgeEndpoint) -> PathBuf {
        self.dir.join(format!("{}.json", endpoint_stem(endpoint)))
    }

    fn lock_path(&self, endpoint: &JudgeEndpoint) -> PathBuf {
        self.dir.join(format!("{}.lock", endpoint_stem(endpoint)))
    }

    fn read_until_ms(&self, endpoint: &JudgeEndpoint) -> Option<u64> {
        let path = self.block_path(endpoint);
        let text = match read_private_entry(&self.dir, &path) {
            Ok(Some(t)) => t,
            Ok(None) => return None,
            Err(e) => {
                eprintln!(
                    "agentdeck: refusing untrusted judge block entry {}: {e}",
                    path.display()
                );
                return None;
            }
        };
        let rec: BlockRecord = serde_json::from_str(&text).ok()?;
        Some(rec.blocked_until_ms)
    }

    /// Remaining block duration for `endpoint` at `now`, or `None` if unblocked/expired.
    pub fn remaining_at(&self, endpoint: &JudgeEndpoint, now: SystemTime) -> Option<Duration> {
        let until_ms = self.read_until_ms(endpoint)?;
        let now_ms = system_time_ms(now);
        (until_ms > now_ms).then(|| Duration::from_millis(until_ms - now_ms))
    }

    pub fn remaining(&self, endpoint: &JudgeEndpoint) -> Option<Duration> {
        self.remaining_at(endpoint, SystemTime::now())
    }

    fn acquire_lock_within(lock_file: &std::fs::File, max_wait: Duration) -> std::io::Result<()> {
        let lock_deadline = Instant::now() + max_wait;
        loop {
            match lock_file.try_lock() {
                Ok(()) => return Ok(()),
                Err(std::fs::TryLockError::WouldBlock) => {
                    let now = Instant::now();
                    if now >= lock_deadline {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "timed out waiting for judge block lock",
                        ));
                    }
                    std::thread::sleep(BLOCK_LOCK_POLL.min(lock_deadline - now));
                }
                Err(std::fs::TryLockError::Error(e)) => return Err(e),
            }
        }
    }

    /// Record a 429 block for `endpoint` until `now + retry_after`, waiting at
    /// most `max_wait` to acquire the per-pair lock file. Never deletes `.lock`.
    pub fn record_rate_limit_at_within(
        &self,
        endpoint: &JudgeEndpoint,
        retry_after: Duration,
        now: SystemTime,
        max_wait: Duration,
    ) -> std::io::Result<()> {
        create_private_dir(&self.dir)?;
        let lock_path = self.lock_path(endpoint);
        let mut lock_opts = std::fs::File::options();
        lock_opts.create(true).truncate(false).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            lock_opts.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let lock_file = lock_opts.open(&lock_path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            let meta = lock_file.metadata()?;
            // Lock file may have just been created or already exist; enforce owner + regular file first,
            // then tighten perms to 0600 before locking.
            if !meta.is_file() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "judge block lock path is not a regular file",
                ));
            }
            if meta.uid() != current_euid() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "judge block lock file owned by another user",
                ));
            }
            lock_file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        Self::acquire_lock_within(&lock_file, max_wait)?;

        let now_ms = system_time_ms(now);
        let add_ms = retry_after.as_millis().min(u64::MAX as u128) as u64;
        let new_until = now_ms.saturating_add(add_ms);
        let blocked_until_ms = self
            .read_until_ms(endpoint)
            .map_or(new_until, |existing| existing.max(new_until));
        let rec = BlockRecord {
            provider: endpoint.provider.clone(),
            model: endpoint.model.clone(),
            blocked_until_ms,
        };
        let text = serde_json::to_string(&rec).map_err(std::io::Error::other)?;
        let res = write_atomic(&self.block_path(endpoint), &text);
        drop(lock_file);
        res
    }

    /// Record a 429 block for `endpoint` until `now + retry_after`.
    /// Updates the block file under a bounded per-pair exclusive file lock so
    /// concurrent writers never shorten a longer active block.
    pub fn record_rate_limit_at(
        &self,
        endpoint: &JudgeEndpoint,
        retry_after: Duration,
        now: SystemTime,
    ) -> std::io::Result<()> {
        self.record_rate_limit_at_within(endpoint, retry_after, now, DEFAULT_BLOCK_LOCK_WAIT)
    }

    pub fn record_rate_limit(
        &self,
        endpoint: &JudgeEndpoint,
        retry_after: Duration,
    ) -> std::io::Result<()> {
        self.record_rate_limit_at(endpoint, retry_after, SystemTime::now())
    }
}

fn dispatch_lock_budget(remaining: Duration) -> Duration {
    (remaining / 4).min(DISPATCH_BLOCK_LOCK_WAIT)
}

fn record_block_logged(
    blocks: &BlockStore,
    endpoint: &JudgeEndpoint,
    retry_after: Duration,
    at: SystemTime,
    max_wait: Duration,
) {
    if let Err(err) = blocks.record_rate_limit_at_within(endpoint, retry_after, at, max_wait) {
        eprintln!(
            "agentdeck: failed to persist 429 block for {}/{}: {err}",
            endpoint.provider, endpoint.model
        );
    }
}

/// Outcome of one judge dispatch round (without sleeping).
///
/// Batching consumers (like `jev_queue::lead`) call [`dispatch_once`] in their
/// own loop so newly arriving requests join the retried batch while waiting.
/// Direct consumers call [`dispatch_with_deadline`].
#[derive(Debug)]
pub enum DispatchStep<T, E> {
    Done(T, JudgeChannel),
    /// Wait this long before retrying; already verified to fit before deadline.
    Wait(Duration),
    /// Terminal error (non-fallback error, fallback failed, or block exceeds deadline).
    Fail(E),
}

fn distinct_fallback<'a>(
    primary: &JudgeEndpoint,
    fallback: Option<&'a JudgeEndpoint>,
) -> Option<&'a JudgeEndpoint> {
    fallback.filter(|fb| *fb != primary)
}

/// Execute one dispatch round against `primary` and optional `fallback`,
/// honouring and updating cross-process 429 blocks in `blocks` under a single
/// absolute monotonic `deadline`.
///
/// - Reserves up to `fallback_reserve` of the remaining budget for an unblocked
///   fallback when calling `primary` so normal slow primaries keep their full
///   HTTP timeout while a hanging primary cannot starve the fallback.
/// - Bounds 429 block-lock waits by the caller's remaining budget (capped at
///   [`DEFAULT_BLOCK_LOCK_WAIT`]), skipping persistence on contention timeout
///   so suspended lock holders never stall callers.
/// - Records 429 blocks using `wall_now()` measured at 429 receipt.
/// - After any fallback failure (including 429), records the fallback's block
///   (if 429) and returns `DispatchStep::Fail` without looping back to primary.
#[allow(clippy::too_many_arguments)]
pub async fn dispatch_once<T, E, MonoNow, WallNow, Call, Fut, Classify, RateLimitErr>(
    blocks: &BlockStore,
    primary: &JudgeEndpoint,
    fallback: Option<&JudgeEndpoint>,
    deadline: Instant,
    fallback_reserve: Duration,
    max_rate_limit_wait: Option<Duration>,
    mut mono_now: MonoNow,
    mut wall_now: WallNow,
    mut call: Call,
    classify: Classify,
    rate_limit_err: RateLimitErr,
) -> DispatchStep<T, E>
where
    MonoNow: FnMut() -> Instant,
    WallNow: FnMut() -> SystemTime,
    Call: FnMut(&JudgeEndpoint, Instant) -> Fut,
    Fut: Future<Output = Result<T, E>>,
    Classify: Fn(&E) -> FallbackTrigger,
    RateLimitErr: Fn(&JudgeEndpoint, Duration, Duration) -> E,
{
    let fb = distinct_fallback(primary, fallback);
    let start_m = mono_now();
    let start_w = wall_now();
    let remaining = deadline.saturating_duration_since(start_m);
    let can_wait = |wait: Duration, rem: Duration| -> bool {
        wait < rem && max_rate_limit_wait.is_none_or(|cap| wait <= cap)
    };
    let wait_budget =
        |rem: Duration| -> Duration { max_rate_limit_wait.map_or(rem, |cap| rem.min(cap)) };

    // 1. Primary already blocked: never send to it.
    if let Some(primary_rem) = blocks.remaining_at(primary, start_w) {
        if let Some(fb) = fb {
            if let Some(fb_rem) = blocks.remaining_at(fb, start_w) {
                let wait = primary_rem.min(fb_rem);
                return if can_wait(wait, remaining) {
                    DispatchStep::Wait(wait)
                } else {
                    DispatchStep::Fail(rate_limit_err(primary, wait, wait_budget(remaining)))
                };
            }
            return match call(fb, deadline).await {
                Ok(v) => DispatchStep::Done(v, JudgeChannel::Fallback),
                Err(fb_err) => {
                    if let FallbackTrigger::RateLimited { retry_after } = classify(&fb_err) {
                        let lock_wait =
                            dispatch_lock_budget(deadline.saturating_duration_since(mono_now()));
                        record_block_logged(blocks, fb, retry_after, wall_now(), lock_wait);
                    }
                    // Never wait and loop back to primary after a fallback failure.
                    DispatchStep::Fail(fb_err)
                }
            };
        }
        return if can_wait(primary_rem, remaining) {
            DispatchStep::Wait(primary_rem)
        } else {
            DispatchStep::Fail(rate_limit_err(primary, primary_rem, wait_budget(remaining)))
        };
    }

    // 2. Primary unblocked: send to primary first, reserving time if fallback is available.
    let fb_available = fb.is_some_and(|f| blocks.remaining_at(f, start_w).is_none());
    let reserve = if fb_available {
        fallback_reserve
    } else {
        Duration::ZERO
    };
    let primary_deadline = primary_attempt_deadline(start_m, deadline, reserve);
    match call(primary, primary_deadline).await {
        Ok(v) => DispatchStep::Done(v, JudgeChannel::Primary),
        Err(err) => {
            let received_w = wall_now();
            let received_m = mono_now();
            let remaining_after = deadline.saturating_duration_since(received_m);
            match classify(&err) {
                FallbackTrigger::None => DispatchStep::Fail(err),
                FallbackTrigger::Transient => {
                    let Some(fb) = fb else {
                        return DispatchStep::Fail(err);
                    };
                    if blocks.remaining_at(fb, received_w).is_some() || remaining_after.is_zero() {
                        return DispatchStep::Fail(err);
                    }
                    match call(fb, deadline).await {
                        Ok(v) => DispatchStep::Done(v, JudgeChannel::Fallback),
                        Err(fb_err) => {
                            if let FallbackTrigger::RateLimited { retry_after } = classify(&fb_err)
                            {
                                let lock_wait = dispatch_lock_budget(
                                    deadline.saturating_duration_since(mono_now()),
                                );
                                record_block_logged(blocks, fb, retry_after, wall_now(), lock_wait);
                            }
                            DispatchStep::Fail(fb_err)
                        }
                    }
                }
                FallbackTrigger::RateLimited { retry_after } => {
                    let lock_wait = dispatch_lock_budget(remaining_after);
                    record_block_logged(blocks, primary, retry_after, received_w, lock_wait);
                    let remaining_after = deadline.saturating_duration_since(mono_now());
                    if let Some(fb) = fb {
                        if let Some(fb_rem) = blocks.remaining_at(fb, received_w) {
                            let wait = retry_after.min(fb_rem);
                            return if can_wait(wait, remaining_after) {
                                DispatchStep::Wait(wait)
                            } else {
                                DispatchStep::Fail(rate_limit_err(
                                    primary,
                                    wait,
                                    wait_budget(remaining_after),
                                ))
                            };
                        }
                        if remaining_after.is_zero() {
                            return DispatchStep::Fail(rate_limit_err(
                                primary,
                                retry_after,
                                Duration::ZERO,
                            ));
                        }
                        return match call(fb, deadline).await {
                            Ok(v) => DispatchStep::Done(v, JudgeChannel::Fallback),
                            Err(fb_err) => {
                                if let FallbackTrigger::RateLimited { retry_after: fb_ra } =
                                    classify(&fb_err)
                                {
                                    let fb_lock_wait = dispatch_lock_budget(
                                        deadline.saturating_duration_since(mono_now()),
                                    );
                                    record_block_logged(
                                        blocks,
                                        fb,
                                        fb_ra,
                                        wall_now(),
                                        fb_lock_wait,
                                    );
                                }
                                // Never wait and loop back to primary after a fallback failure.
                                DispatchStep::Fail(fb_err)
                            }
                        };
                    }
                    if can_wait(retry_after, remaining_after) {
                        DispatchStep::Wait(retry_after)
                    } else {
                        DispatchStep::Fail(rate_limit_err(
                            primary,
                            retry_after,
                            wait_budget(remaining_after),
                        ))
                    }
                }
            }
        }
    }
}

/// Run [`dispatch_once`] in a loop up to a single monotonic `budget` deadline,
/// sleeping via caller-provided `sleep` when a rate-limit block fits before the
/// deadline and any `max_rate_limit_wait` cap.
#[allow(clippy::too_many_arguments)]
pub async fn dispatch_with_deadline<
    T,
    E,
    MonoNow,
    WallNow,
    Sleep,
    SleepFut,
    Call,
    Fut,
    Classify,
    RateLimitErr,
>(
    blocks: &BlockStore,
    primary: &JudgeEndpoint,
    fallback: Option<&JudgeEndpoint>,
    budget: DispatchBudget,
    mut mono_now: MonoNow,
    mut wall_now: WallNow,
    mut sleep: Sleep,
    mut call: Call,
    classify: Classify,
    rate_limit_err: RateLimitErr,
) -> Result<(T, JudgeChannel), E>
where
    MonoNow: FnMut() -> Instant,
    WallNow: FnMut() -> SystemTime,
    Sleep: FnMut(Duration) -> SleepFut,
    SleepFut: Future<Output = ()>,
    Call: FnMut(&JudgeEndpoint, Instant) -> Fut,
    Fut: Future<Output = Result<T, E>>,
    Classify: Fn(&E) -> FallbackTrigger,
    RateLimitErr: Fn(&JudgeEndpoint, Duration, Duration) -> E,
{
    let deadline = mono_now() + budget.total;
    let mut remaining_wait_cap = budget.max_rate_limit_wait;
    for attempt in 0..=MAX_RATE_LIMIT_RETRIES {
        match dispatch_once(
            blocks,
            primary,
            fallback,
            deadline,
            budget.fallback_reserve,
            remaining_wait_cap,
            &mut mono_now,
            &mut wall_now,
            &mut call,
            &classify,
            &rate_limit_err,
        )
        .await
        {
            DispatchStep::Done(v, ch) => return Ok((v, ch)),
            DispatchStep::Fail(e) => return Err(e),
            DispatchStep::Wait(wait) => {
                if attempt == MAX_RATE_LIMIT_RETRIES {
                    let rem = deadline.saturating_duration_since(mono_now());
                    let cap = remaining_wait_cap.map_or(rem, |c| rem.min(c));
                    return Err(rate_limit_err(primary, wait, cap));
                }
                if let Some(cap) = &mut remaining_wait_cap {
                    *cap = cap.saturating_sub(wait);
                }
                sleep(wait).await;
            }
        }
    }
    Err(rate_limit_err(primary, Duration::ZERO, Duration::ZERO))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;
    use std::sync::Mutex;

    fn temp_blocks(tag: &str) -> (PathBuf, BlockStore) {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "agentdeck-judge-blocks-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        (dir.clone(), BlockStore::new(dir))
    }

    fn run_block<F: Future>(fut: F) -> F::Output {
        use std::pin::pin;
        use std::task::{Context, Poll, Waker};
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        let mut fut = pin!(fut);
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(v) => v,
            Poll::Pending => panic!("test future should never pend"),
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    struct TestErr {
        trigger: FallbackTrigger,
        msg: String,
    }

    fn primary() -> JudgeEndpoint {
        JudgeEndpoint::new("vercel", "perplexity/pplx-decider-v1.2")
    }

    fn fallback() -> JudgeEndpoint {
        JudgeEndpoint::new("openrouter", "~typesafe/jev-latest")
    }

    #[test]
    fn rate_limit_60s_no_fallback_fails_fast_under_50s_and_waits_under_120s() {
        let (dir, blocks) = temp_blocks("deadline");
        let t0 = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let m0 = Instant::now();
        let wall = Rc::new(Cell::new(t0));
        let mono = Rc::new(Cell::new(m0));
        let slept = Rc::new(Mutex::new(Vec::new()));
        let calls = Rc::new(Mutex::new(0usize));

        // 60s Retry-After > 50s deadline: fails fast with clear error, zero sleep.
        let m1 = mono.clone();
        let w1 = wall.clone();
        let s1 = slept.clone();
        let n1 = calls.clone();
        let err = run_block(dispatch_with_deadline(
            &blocks,
            &primary(),
            None,
            DispatchBudget::new(Duration::from_secs(50), Duration::ZERO),
            move || m1.get(),
            move || w1.get(),
            move |d| {
                s1.lock().unwrap().push(d);
                async {}
            },
            move |_ep, _deadline| {
                *n1.lock().unwrap() += 1;
                async {
                    Err::<&str, TestErr>(TestErr {
                        trigger: FallbackTrigger::RateLimited {
                            retry_after: Duration::from_secs(60),
                        },
                        msg: "HTTP 429".into(),
                    })
                }
            },
            |e| e.trigger,
            |ep, ra, rem| TestErr {
                trigger: FallbackTrigger::None,
                msg: format_rate_limit_exceeded(ep, ra, rem),
            },
        ))
        .unwrap_err();
        assert!(err.msg.contains("60s"), "{err:?}");
        assert!(err.msg.contains("50s"), "{err:?}");
        assert!(slept.lock().unwrap().is_empty(), "must not sleep");
        assert_eq!(*calls.lock().unwrap(), 1);

        // Second request at t0 + 5s with 50s deadline sees block (55s left > 50s):
        // fails fast WITHOUT calling primary again.
        wall.set(t0 + Duration::from_secs(5));
        mono.set(m0 + Duration::from_secs(5));
        let m2 = mono.clone();
        let w2 = wall.clone();
        let n2 = calls.clone();
        let err2 = run_block(dispatch_with_deadline(
            &blocks,
            &primary(),
            None,
            DispatchBudget::new(Duration::from_secs(50), Duration::ZERO),
            move || m2.get(),
            move || w2.get(),
            |_| async {},
            move |_ep, _deadline| {
                *n2.lock().unwrap() += 1;
                async { Ok::<&str, TestErr>("should not run") }
            },
            |e| e.trigger,
            |ep, ra, rem| TestErr {
                trigger: FallbackTrigger::None,
                msg: format_rate_limit_exceeded(ep, ra, rem),
            },
        ))
        .unwrap_err();
        assert!(err2.msg.contains("55s"), "{err2:?}");
        assert_eq!(*calls.lock().unwrap(), 1, "must not hammer blocked primary");

        // Request with 120s deadline waits out remaining 55s (via injected clock) and succeeds!
        let m3 = mono.clone();
        let w3 = wall.clone();
        let m3_sleep = mono.clone();
        let w3_sleep = wall.clone();
        let s3 = slept.clone();
        let n3 = calls.clone();
        let (val, ch) = run_block(dispatch_with_deadline(
            &blocks,
            &primary(),
            None,
            DispatchBudget::new(Duration::from_secs(120), Duration::ZERO),
            move || m3.get(),
            move || w3.get(),
            move |d| {
                s3.lock().unwrap().push(d);
                m3_sleep.set(m3_sleep.get() + d);
                w3_sleep.set(w3_sleep.get() + d);
                async {}
            },
            move |_ep, _deadline| {
                *n3.lock().unwrap() += 1;
                async { Ok::<&str, TestErr>("ok-after-wait") }
            },
            |e| e.trigger,
            |ep, ra, rem| TestErr {
                trigger: FallbackTrigger::None,
                msg: format_rate_limit_exceeded(ep, ra, rem),
            },
        ))
        .unwrap();
        assert_eq!(val, "ok-after-wait");
        assert_eq!(ch, JudgeChannel::Primary);
        assert_eq!(*slept.lock().unwrap(), vec![Duration::from_secs(55)]);
        assert_eq!(*calls.lock().unwrap(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rate_limit_with_fallback_uses_fallback_immediately_and_skips_blocked_primary() {
        let (dir, blocks) = temp_blocks("fallback");
        let t0 = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let m0 = Instant::now();
        let wall = Rc::new(Cell::new(t0));
        let mono = Rc::new(Cell::new(m0));
        let seen = Rc::new(Mutex::new(Vec::<String>::new()));

        // Request 1: primary returns 429 -> immediately answered by fallback.
        let m1 = mono.clone();
        let w1 = wall.clone();
        let s1 = seen.clone();
        let (val, ch) = run_block(dispatch_with_deadline(
            &blocks,
            &primary(),
            Some(&fallback()),
            DispatchBudget::new(Duration::from_secs(10), Duration::from_secs(4)),
            move || m1.get(),
            move || w1.get(),
            |_| async {},
            move |ep, _| {
                s1.lock().unwrap().push(ep.provider.clone());
                let is_primary = ep.provider == "vercel";
                async move {
                    if is_primary {
                        Err(TestErr {
                            trigger: FallbackTrigger::RateLimited {
                                retry_after: Duration::from_secs(60),
                            },
                            msg: "429".into(),
                        })
                    } else {
                        Ok("from-fallback")
                    }
                }
            },
            |e| e.trigger,
            |ep, ra, rem| TestErr {
                trigger: FallbackTrigger::None,
                msg: format_rate_limit_exceeded(ep, ra, rem),
            },
        ))
        .unwrap();
        assert_eq!(val, "from-fallback");
        assert_eq!(ch, JudgeChannel::Fallback);
        assert_eq!(*seen.lock().unwrap(), vec!["vercel", "openrouter"]);

        // Request 2 from a second BlockStore ("second process") at t0 + 10s:
        // skips blocked primary completely and goes straight to fallback.
        wall.set(t0 + Duration::from_secs(10));
        mono.set(m0 + Duration::from_secs(10));
        let second_process_blocks = BlockStore::new(dir.clone());
        let m2 = mono.clone();
        let w2 = wall.clone();
        let s2 = seen.clone();
        let (val2, ch2) = run_block(dispatch_with_deadline(
            &second_process_blocks,
            &primary(),
            Some(&fallback()),
            DispatchBudget::new(Duration::from_secs(10), Duration::from_secs(4)),
            move || m2.get(),
            move || w2.get(),
            |_| async {},
            move |ep, _| {
                s2.lock().unwrap().push(ep.provider.clone());
                async { Ok::<&str, TestErr>("from-fallback-2") }
            },
            |e| e.trigger,
            |ep, ra, rem| TestErr {
                trigger: FallbackTrigger::None,
                msg: format_rate_limit_exceeded(ep, ra, rem),
            },
        ))
        .unwrap();
        assert_eq!(val2, "from-fallback-2");
        assert_eq!(ch2, JudgeChannel::Fallback);
        assert_eq!(
            *seen.lock().unwrap(),
            vec!["vercel", "openrouter", "openrouter"]
        );

        // Request 3 at t0 + 61s (block expired): tries primary again and succeeds.
        wall.set(t0 + Duration::from_secs(61));
        mono.set(m0 + Duration::from_secs(61));
        let m3 = mono.clone();
        let w3 = wall.clone();
        let s3 = seen.clone();
        let (val3, ch3) = run_block(dispatch_with_deadline(
            &second_process_blocks,
            &primary(),
            Some(&fallback()),
            DispatchBudget::new(Duration::from_secs(10), Duration::from_secs(4)),
            move || m3.get(),
            move || w3.get(),
            |_| async {},
            move |ep, _| {
                s3.lock().unwrap().push(ep.provider.clone());
                async { Ok::<&str, TestErr>("from-primary-again") }
            },
            |e| e.trigger,
            |ep, ra, rem| TestErr {
                trigger: FallbackTrigger::None,
                msg: format_rate_limit_exceeded(ep, ra, rem),
            },
        ))
        .unwrap();
        assert_eq!(val3, "from-primary-again");
        assert_eq!(ch3, JudgeChannel::Primary);
        assert_eq!(
            *seen.lock().unwrap(),
            vec!["vercel", "openrouter", "openrouter", "vercel"]
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
            let file_mode = std::fs::metadata(blocks.block_path(&primary()))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(dir_mode, 0o700);
            assert_eq!(file_mode, 0o600);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn transient_triggers_fallback_without_blocking_while_non_fallback_does_not() {
        let (dir, blocks) = temp_blocks("classify");
        let t0 = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let m0 = Instant::now();

        // 5xx / network -> fallback, and primary is NOT blocked.
        let (v, ch) = run_block(dispatch_with_deadline(
            &blocks,
            &primary(),
            Some(&fallback()),
            DispatchBudget::new(Duration::from_secs(10), Duration::from_secs(4)),
            || m0,
            || t0,
            |_| async {},
            |ep, _| {
                let is_primary = ep.provider == "vercel";
                async move {
                    if is_primary {
                        Err(TestErr {
                            trigger: classify_http_status(503, None),
                            msg: "503".into(),
                        })
                    } else {
                        Ok("fallback-on-503")
                    }
                }
            },
            |e| e.trigger,
            |ep, ra, rem| TestErr {
                trigger: FallbackTrigger::None,
                msg: format_rate_limit_exceeded(ep, ra, rem),
            },
        ))
        .unwrap();
        assert_eq!(v, "fallback-on-503");
        assert_eq!(ch, JudgeChannel::Fallback);
        assert!(
            blocks.remaining_at(&primary(), t0).is_none(),
            "5xx must not create a 429 block"
        );

        // 401 / 403 / 400 / parse -> no fallback.
        for status in [401u16, 403, 400, 404] {
            let called_fallback = Rc::new(Cell::new(false));
            let cf = called_fallback.clone();
            let err = run_block(dispatch_with_deadline(
                &blocks,
                &primary(),
                Some(&fallback()),
                DispatchBudget::new(Duration::from_secs(10), Duration::from_secs(4)),
                || m0,
                || t0,
                |_| async {},
                move |ep, _| {
                    let is_primary = ep.provider == "vercel";
                    if !is_primary {
                        cf.set(true);
                    }
                    async move {
                        Err::<&str, TestErr>(TestErr {
                            trigger: classify_http_status(status, None),
                            msg: format!("HTTP {status}"),
                        })
                    }
                },
                |e| e.trigger,
                |ep, ra, rem| TestErr {
                    trigger: FallbackTrigger::None,
                    msg: format_rate_limit_exceeded(ep, ra, rem),
                },
            ))
            .unwrap_err();
            assert_eq!(err.msg, format!("HTTP {status}"));
            assert!(
                !called_fallback.get(),
                "status {status} must not trigger fallback"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_retry_after_defaults_to_30s() {
        assert_eq!(parse_retry_after(None), Duration::from_secs(30));
        assert_eq!(parse_retry_after(Some("")), Duration::from_secs(30));
        assert_eq!(parse_retry_after(Some("bogus")), Duration::from_secs(30));
        assert_eq!(parse_retry_after(Some(" 60 ")), Duration::from_secs(60));
        assert_eq!(parse_retry_after(Some("0")), Duration::from_secs(0));
    }

    #[test]
    fn dispatch_shares_single_monotonic_deadline_and_reserves_budget_for_fallback() {
        let (dir, blocks) = temp_blocks("mono-reserve");
        let t0 = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let m0 = Instant::now();
        let wall = Rc::new(Cell::new(t0));
        let mono = Rc::new(Cell::new(m0));
        let deadlines = Rc::new(Mutex::new(Vec::<(String, Duration)>::new()));

        let m_now = mono.clone();
        let w_now = wall.clone();
        let m_call = mono.clone();
        let dl = deadlines.clone();

        let (val, ch) = run_block(dispatch_with_deadline(
            &blocks,
            &primary(),
            Some(&fallback()),
            DispatchBudget::new(Duration::from_secs(50), Duration::from_secs(15)),
            move || m_now.get(),
            move || w_now.get(),
            |_| async {},
            move |ep, attempt_deadline| {
                let now_m = m_call.get();
                let rem = attempt_deadline.saturating_duration_since(now_m);
                dl.lock().unwrap().push((ep.provider.clone(), rem));
                let is_primary = ep.provider == "vercel";
                if is_primary {
                    // Simulate primary consuming its entire reserved 35s share and timing out.
                    m_call.set(now_m + rem);
                }
                async move {
                    if is_primary {
                        Err(TestErr {
                            trigger: FallbackTrigger::Transient,
                            msg: "timeout".into(),
                        })
                    } else {
                        Ok("fb-ok")
                    }
                }
            },
            |e| e.trigger,
            |ep, ra, rem| TestErr {
                trigger: FallbackTrigger::None,
                msg: format_rate_limit_exceeded(ep, ra, rem),
            },
        ))
        .unwrap();
        assert_eq!(val, "fb-ok");
        assert_eq!(ch, JudgeChannel::Fallback);
        assert_eq!(
            *deadlines.lock().unwrap(),
            vec![
                ("vercel".into(), Duration::from_secs(35)),
                ("openrouter".into(), Duration::from_secs(15)),
            ],
            "primary must reserve fixed fallback_reserve (15s) out of 50s deadline, leaving 35s for primary and 15s for fallback"
        );
        // Tiny budget (10s < 2 * 15s) caps reserve at half (5s) so primary still runs.
        assert_eq!(
            primary_attempt_deadline(m0, m0 + Duration::from_secs(10), Duration::from_secs(15)),
            m0 + Duration::from_secs(5)
        );
        // No fallback (zero reserve) gives primary the full deadline.
        assert_eq!(
            primary_attempt_deadline(m0, m0 + Duration::from_secs(50), Duration::ZERO),
            m0 + Duration::from_secs(50)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rate_limit_block_uses_receipt_time_not_pre_request_time() {
        let (dir, blocks) = temp_blocks("receipt-time");
        let t0 = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let m0 = Instant::now();
        let wall = Rc::new(Cell::new(t0));
        let mono = Rc::new(Cell::new(m0));
        let m_now = mono.clone();
        let w_now = wall.clone();
        let w_call = wall.clone();
        let m_call = mono.clone();

        let _ = run_block(dispatch_with_deadline(
            &blocks,
            &primary(),
            None,
            DispatchBudget::new(Duration::from_secs(20), Duration::ZERO),
            move || m_now.get(),
            move || w_now.get(),
            |_| async {},
            move |_ep, _deadline| {
                // Request took 10s in flight before 429 arrived with Retry-After: 30s.
                w_call.set(w_call.get() + Duration::from_secs(10));
                m_call.set(m_call.get() + Duration::from_secs(10));
                async {
                    Err::<&str, TestErr>(TestErr {
                        trigger: FallbackTrigger::RateLimited {
                            retry_after: Duration::from_secs(30),
                        },
                        msg: "429".into(),
                    })
                }
            },
            |e| e.trigger,
            |ep, ra, rem| TestErr {
                trigger: FallbackTrigger::None,
                msg: format_rate_limit_exceeded(ep, ra, rem),
            },
        ))
        .unwrap_err();

        // Block must expire at t0 + 10s + 30s = t0 + 40s, so at t0 + 35s 5s remain.
        let rem = blocks.remaining_at(&primary(), t0 + Duration::from_secs(35));
        assert_eq!(rem, Some(Duration::from_secs(5)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_record_rate_limit_uses_per_pair_lock_and_keeps_max() {
        let (dir, blocks) = temp_blocks("lock-max");
        let ep = primary();
        let t0 = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        // Seed initial state so the per-pair directory exists.
        blocks
            .record_rate_limit_at(&ep, Duration::from_secs(1), t0)
            .unwrap();
        let lock_path = blocks.lock_path(&ep);
        let lock_file = std::fs::File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .unwrap();
        lock_file.lock().unwrap();

        let blocks_bg = blocks.clone();
        let ep_bg = ep.clone();
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done_bg = done.clone();
        let handle = std::thread::spawn(move || {
            blocks_bg
                .record_rate_limit_at(&ep_bg, Duration::from_secs(5), t0)
                .unwrap();
            done_bg.store(true, Ordering::SeqCst);
        });

        std::thread::sleep(Duration::from_millis(60));
        assert!(
            !done.load(Ordering::SeqCst),
            "record_rate_limit_at must wait on the per-pair lock file"
        );
        // While holding the lock, simulate a longer block written before releasing.
        let long_rec = BlockRecord {
            provider: ep.provider.clone(),
            model: ep.model.clone(),
            blocked_until_ms: system_time_ms(t0) + 60_000,
        };
        write_atomic(
            &blocks.block_path(&ep),
            &serde_json::to_string(&long_rec).unwrap(),
        )
        .unwrap();
        drop(lock_file);
        handle.join().unwrap();

        assert_eq!(
            blocks.remaining_at(&ep, t0),
            Some(Duration::from_secs(60)),
            "shorter concurrent write must never overwrite a longer block"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fallback_429_records_block_and_never_retries_primary() {
        let (dir, blocks) = temp_blocks("fb-429-no-loop");
        let t0 = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let m0 = Instant::now();
        // Primary is blocked for only 1s (well inside 10s deadline).
        blocks
            .record_rate_limit_at(&primary(), Duration::from_secs(1), t0)
            .unwrap();

        let wall = Rc::new(Cell::new(t0));
        let mono = Rc::new(Cell::new(m0));
        let m_now = mono.clone();
        let w_now = wall.clone();
        let m_sleep = mono.clone();
        let w_sleep = wall.clone();
        let seen = Rc::new(Mutex::new(Vec::<String>::new()));
        let s1 = seen.clone();

        let res = run_block(dispatch_with_deadline(
            &blocks,
            &primary(),
            Some(&fallback()),
            DispatchBudget::new(Duration::from_secs(10), Duration::from_secs(4)),
            move || m_now.get(),
            move || w_now.get(),
            move |d| {
                m_sleep.set(m_sleep.get() + d);
                w_sleep.set(w_sleep.get() + d);
                async {}
            },
            move |ep, _deadline| {
                s1.lock().unwrap().push(ep.provider.clone());
                let is_fb = ep.provider == "openrouter";
                async move {
                    if is_fb {
                        Err(TestErr {
                            trigger: FallbackTrigger::RateLimited {
                                retry_after: Duration::from_secs(60),
                            },
                            msg: "fb-429".into(),
                        })
                    } else {
                        Ok("primary-must-not-be-retried")
                    }
                }
            },
            |e| e.trigger,
            |ep, ra, rem| TestErr {
                trigger: FallbackTrigger::None,
                msg: format_rate_limit_exceeded(ep, ra, rem),
            },
        ));
        assert!(
            res.is_err(),
            "fallback 429 must fail, not retry primary: {res:?}"
        );
        assert_eq!(
            *seen.lock().unwrap(),
            vec!["openrouter"],
            "must never loop back to primary after fallback 429"
        );
        assert_eq!(
            blocks.remaining_at(&fallback(), t0),
            Some(Duration::from_secs(60)),
            "fallback 429 block must be persisted"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn block_store_refuses_symlinks_and_loose_permissions() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let (dir, blocks) = temp_blocks("unsafe-perms");
        let ep = primary();
        let t0 = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        blocks
            .record_rate_limit_at(&ep, Duration::from_secs(60), t0)
            .unwrap();

        // 1. Loose file permissions (0644) must not be trusted on read.
        let p = blocks.block_path(&ep);
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            blocks.remaining_at(&ep, t0).is_none(),
            "world/group-readable block file must be refused"
        );
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();

        // 2. Symlinked block file must be refused on read.
        let decoy = dir.join("decoy.json");
        std::fs::copy(&p, &decoy).unwrap();
        std::fs::set_permissions(&decoy, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::remove_file(&p).unwrap();
        symlink(&decoy, &p).unwrap();
        assert!(
            blocks.remaining_at(&ep, t0).is_none(),
            "symlinked block file must be refused on read"
        );
        std::fs::remove_file(&p).unwrap();
        std::fs::rename(&decoy, &p).unwrap();

        // 3. Loose directory permissions (0755) must not be trusted on read.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            blocks.remaining_at(&ep, t0).is_none(),
            "world/group-accessible block dir must be refused"
        );

        // 4. Symlinked block directory must be refused on both read and write.
        let link_dir = dir.with_extension("link");
        let _ = std::fs::remove_file(&link_dir);
        symlink(&dir, &link_dir).unwrap();
        let link_store = BlockStore::new(link_dir.clone());
        assert!(
            link_store
                .record_rate_limit_at(&ep, Duration::from_secs(60), t0)
                .is_err(),
            "symlinked block directory must be refused on write"
        );

        // 5. Foreign UID must be refused by the owner check.
        let meta = std::fs::symlink_metadata(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let private_meta = std::fs::symlink_metadata(&dir).unwrap();
        assert!(verify_owner_and_private_mode(&private_meta, current_euid(), true).is_ok());
        assert!(
            verify_owner_and_private_mode(&private_meta, current_euid().wrapping_add(1), true)
                .is_err()
        );
        let _ = meta;

        let _ = std::fs::remove_file(&link_dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    static ENV_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[cfg(unix)]
    #[test]
    fn default_runtime_without_xdg_uses_per_user_dir() {
        let _guard = ENV_TEST_LOCK.lock().unwrap();
        let prev = std::env::var_os("XDG_RUNTIME_DIR");
        // SAFETY: serialized by ENV_TEST_LOCK across tests in this module.
        unsafe { std::env::remove_var("XDG_RUNTIME_DIR") };
        let store = BlockStore::default_runtime().unwrap();
        let via_base = BlockStore::for_runtime_dir(&default_runtime_base().unwrap());
        if let Some(v) = prev {
            unsafe { std::env::set_var("XDG_RUNTIME_DIR", v) };
        }
        let shared = std::env::temp_dir().join(JUDGE_BLOCKS_DIR_NAME);
        assert_ne!(
            store.dir(),
            shared.as_path(),
            "BlockStore::default_runtime() without XDG_RUNTIME_DIR must use a per-user directory, not shared {}",
            shared.display()
        );
        assert_eq!(store.dir(), via_base.dir());
    }

    #[test]
    fn dispatch_once_does_not_stall_on_held_block_lock_and_continues_with_fallback() {
        let (dir, blocks) = temp_blocks("bounded-flock");
        let ep = primary();
        let t0 = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        blocks
            .record_rate_limit_at(&ep, Duration::from_secs(1), t0)
            .unwrap();
        let lock_path = blocks.lock_path(&ep);
        let lock_file = std::fs::File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .unwrap();
        lock_file.lock().unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        let blocks_bg = blocks.clone();
        let handle = std::thread::spawn(move || {
            let now_w = t0 + Duration::from_secs(5);
            let step = run_block(dispatch_once(
                &blocks_bg,
                &primary(),
                Some(&fallback()),
                Instant::now() + Duration::from_millis(150),
                Duration::from_millis(50),
                None,
                Instant::now,
                move || now_w,
                |ep, _| {
                    let is_primary = ep.provider == "vercel";
                    async move {
                        if is_primary {
                            Err(TestErr {
                                trigger: FallbackTrigger::RateLimited {
                                    retry_after: Duration::from_secs(60),
                                },
                                msg: "429".into(),
                            })
                        } else {
                            Ok("from-fallback")
                        }
                    }
                },
                |e| e.trigger,
                |ep, ra, rem| TestErr {
                    trigger: FallbackTrigger::None,
                    msg: format_rate_limit_exceeded(ep, ra, rem),
                },
            ));
            let _ = tx.send(step);
        });

        let received = rx.recv_timeout(Duration::from_millis(600));
        // Always release lock and join thread before asserting so failure never leaks a hung thread.
        drop(lock_file);
        handle.join().unwrap();

        let step = received
            .expect("dispatch_once must bound flock wait by remaining budget instead of hanging");
        match step {
            DispatchStep::Done(val, ch) => {
                assert_eq!(val, "from-fallback");
                assert_eq!(ch, JudgeChannel::Fallback);
            }
            other => panic!("expected fallback decision to continue in-memory, got {other:?}"),
        }
        assert!(lock_path.exists(), ".lock file must not be deleted on exit");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn default_runtime_base_without_xdg_uses_private_home_dir_not_tmp() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let _guard = ENV_TEST_LOCK.lock().unwrap();
        let fake_home =
            std::env::temp_dir().join(format!("agentdeck-rt-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&fake_home);
        std::fs::create_dir_all(&fake_home).unwrap();

        let prev_xdg = std::env::var_os("XDG_RUNTIME_DIR");
        let prev_home = std::env::var_os("HOME");
        // SAFETY: serialized by ENV_TEST_LOCK across tests in this module.
        unsafe {
            std::env::remove_var("XDG_RUNTIME_DIR");
            std::env::set_var("HOME", &fake_home);
        }
        let base = default_runtime_base().unwrap();
        match prev_xdg {
            Some(v) => unsafe { std::env::set_var("XDG_RUNTIME_DIR", v) },
            None => unsafe { std::env::remove_var("XDG_RUNTIME_DIR") },
        }
        match prev_home {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }

        assert!(
            base.starts_with(&fake_home),
            "without XDG_RUNTIME_DIR, runtime base must live under HOME ({}), got {}",
            fake_home.display(),
            base.display()
        );
        assert_ne!(base, std::env::temp_dir());
        let mode = std::fs::symlink_metadata(&base)
            .expect("runtime base must exist")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700, "runtime base under HOME must be mode 0700");

        // Pre-created symlink at runtime base must be refused.
        let decoy = fake_home.join("decoy");
        std::fs::create_dir_all(&decoy).unwrap();
        std::fs::remove_dir_all(&base).unwrap();
        symlink(&decoy, &base).unwrap();
        assert!(
            runtime_base_from(None, Some(fake_home.as_os_str())).is_err(),
            "symlinked fallback runtime base must be refused"
        );
        let _ = std::fs::remove_dir_all(&fake_home);
    }

    #[cfg(unix)]
    #[test]
    fn runtime_base_requires_existing_absolute_private_xdg_or_falls_back_to_home() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let root =
            std::env::temp_dir().join(format!("agentdeck-xdg-strict-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let fake_home = root.join("home");
        let expected_fallback = fake_home.join(".cache/agentdeck/runtime");
        std::fs::create_dir_all(&fake_home).unwrap();

        // 1. Valid existing absolute 0700 XDG_RUNTIME_DIR keeps its exact path.
        let good_xdg = root.join("xdg-0700");
        std::fs::create_dir_all(&good_xdg).unwrap();
        std::fs::set_permissions(&good_xdg, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            runtime_base_from(Some(good_xdg.as_os_str()), Some(fake_home.as_os_str())).unwrap(),
            good_xdg
        );

        // 2. Group/other accessible modes (0770, 0750, 0755) fall back to $HOME/.cache/agentdeck/runtime.
        for mode in [0o770u32, 0o750, 0o755] {
            let loose_xdg = root.join(format!("xdg-{mode:o}"));
            std::fs::create_dir_all(&loose_xdg).unwrap();
            std::fs::set_permissions(&loose_xdg, std::fs::Permissions::from_mode(mode)).unwrap();
            let resolved =
                runtime_base_from(Some(loose_xdg.as_os_str()), Some(fake_home.as_os_str()))
                    .unwrap();
            assert_eq!(
                resolved, expected_fallback,
                "XDG_RUNTIME_DIR with mode {mode:o} must fall back to HOME runtime base"
            );
        }

        // 3. Non-existent XDG_RUNTIME_DIR falls back.
        let missing_xdg = root.join("does-not-exist");
        assert_eq!(
            runtime_base_from(Some(missing_xdg.as_os_str()), Some(fake_home.as_os_str())).unwrap(),
            expected_fallback,
            "missing XDG_RUNTIME_DIR must fall back to HOME runtime base"
        );

        // 4. Relative XDG_RUNTIME_DIR falls back.
        assert_eq!(
            runtime_base_from(
                Some(OsStr::new("relative/xdg")),
                Some(fake_home.as_os_str())
            )
            .unwrap(),
            expected_fallback,
            "relative XDG_RUNTIME_DIR must fall back to HOME runtime base"
        );

        // 5. Symlink XDG_RUNTIME_DIR falls back.
        let link_xdg = root.join("xdg-symlink");
        symlink(&good_xdg, &link_xdg).unwrap();
        assert_eq!(
            runtime_base_from(Some(link_xdg.as_os_str()), Some(fake_home.as_os_str())).unwrap(),
            expected_fallback,
            "symlink XDG_RUNTIME_DIR must fall back to HOME runtime base"
        );

        // 6. Regular file XDG_RUNTIME_DIR falls back.
        let file_xdg = root.join("xdg-file");
        std::fs::write(&file_xdg, "not a dir").unwrap();
        std::fs::set_permissions(&file_xdg, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            runtime_base_from(Some(file_xdg.as_os_str()), Some(fake_home.as_os_str())).unwrap(),
            expected_fallback,
            "non-directory XDG_RUNTIME_DIR must fall back to HOME runtime base"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn write_atomic_and_read_private_entry_share_size_limit() {
        let dir =
            std::env::temp_dir().join(format!("agentdeck-private-limit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        create_private_dir(&dir).unwrap();
        let path = dir.join("entry.json");

        // Exact cap succeeds on both write and read.
        let at_cap = "x".repeat(MAX_PRIVATE_FILE_BYTES as usize);
        write_atomic(&path, &at_cap).unwrap();
        assert_eq!(
            read_private_entry(&dir, &path).unwrap().unwrap().len(),
            MAX_PRIVATE_FILE_BYTES as usize
        );

        // One byte over cap fails explicitly at write time.
        let over_cap = "x".repeat(MAX_PRIVATE_FILE_BYTES as usize + 1);
        let err = write_atomic(&dir.join("over.json"), &over_cap).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(!dir.join("over.json").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn slow_primary_under_50s_hook_budget_with_fallback_succeeds_on_primary() {
        let (dir, blocks) = temp_blocks("slow-primary");
        let t0 = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let m0 = Instant::now();
        let wall = Rc::new(Cell::new(t0));
        let mono = Rc::new(Cell::new(m0));
        let own_http_timeout = Duration::from_secs(30);

        for simulated_secs in [20u64, 27u64] {
            mono.set(m0);
            wall.set(t0);
            let m_now = mono.clone();
            let w_now = wall.clone();
            let m_call = mono.clone();
            let (val, ch) = run_block(dispatch_with_deadline(
                &blocks,
                &primary(),
                Some(&fallback()),
                DispatchBudget::new(Duration::from_secs(50), Duration::from_secs(15)),
                move || m_now.get(),
                move || w_now.get(),
                |_| async {},
                move |ep, attempt_deadline| {
                    let is_primary = ep.provider == "vercel";
                    let now_m = m_call.get();
                    let eff = attempt_deadline
                        .saturating_duration_since(now_m)
                        .min(own_http_timeout);
                    let needed = Duration::from_secs(simulated_secs);
                    if is_primary {
                        if eff >= needed {
                            m_call.set(now_m + needed);
                        } else {
                            m_call.set(now_m + eff);
                        }
                    }
                    async move {
                        if is_primary {
                            if eff >= needed {
                                Ok("primary-ok")
                            } else {
                                Err(TestErr {
                                    trigger: FallbackTrigger::Transient,
                                    msg: "primary-timeout".into(),
                                })
                            }
                        } else {
                            Ok("fallback-ok")
                        }
                    }
                },
                |e| e.trigger,
                |ep, ra, rem| TestErr {
                    trigger: FallbackTrigger::None,
                    msg: format_rate_limit_exceeded(ep, ra, rem),
                },
            ))
            .unwrap();
            assert_eq!(
                (val, ch),
                ("primary-ok", JudgeChannel::Primary),
                "{simulated_secs}s primary under 50s hook budget with fallback must succeed on primary"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
