//! Shared LLM judge channel selection, fallback trigger classification, and
//! cross-process 429 block state reused by router, approver and coordinator.
//!
//! No fixed RPM cap: HTTP 429 + `Retry-After` (default 30s) is the only rate-limit
//! signal. When a `(provider, model)` pair receives 429, a private atomic block
//! file is written under `$XDG_RUNTIME_DIR/agentdeck-judge-blocks/<sha>.json` so
//! no consumer process hammers the blocked pair until the block expires.

use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Default `Retry-After` when a 429 response omits the header or sends a non-integer.
pub const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(30);
/// Subdirectory under `$XDG_RUNTIME_DIR` (or temp dir) storing per-pair 429 blocks.
pub const JUDGE_BLOCKS_DIR_NAME: &str = "agentdeck-judge-blocks";
/// Upper bound on rate-limit wait loops within one deadline so a broken server
/// replying `Retry-After: 0` cannot spin forever.
const MAX_RATE_LIMIT_RETRIES: usize = 8;

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

/// Ensure `dir` exists with private `0700` permissions.
pub fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Atomically write `text` to `path` with private `0600` permissions via a
/// unique sibling `.tmp` file so concurrent processes cannot clobber each other.
pub fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let tmp = PathBuf::from(format!(
        "{}.{}.{}.tmp",
        path.display(),
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let mut opts = std::fs::File::options();
    opts.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    f.write_all(text.as_bytes())?;
    drop(f);
    std::fs::rename(&tmp, path)
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

/// Cross-process 429 block store backed by private JSON files per `(provider, model)`.
#[derive(Debug, Clone)]
pub struct BlockStore {
    dir: PathBuf,
}

impl BlockStore {
    pub fn default_runtime() -> Self {
        let base = std::env::var_os("XDG_RUNTIME_DIR")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        Self {
            dir: base.join(JUDGE_BLOCKS_DIR_NAME),
        }
    }

    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn block_path(&self, endpoint: &JudgeEndpoint) -> PathBuf {
        let mut h = Sha256::new();
        h.update(endpoint.provider.as_bytes());
        h.update([0u8]);
        h.update(endpoint.model.as_bytes());
        self.dir.join(format!("{:x}.json", h.finalize()))
    }

    fn read_until_ms(&self, endpoint: &JudgeEndpoint) -> Option<u64> {
        let text = std::fs::read_to_string(self.block_path(endpoint)).ok()?;
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

    /// Record a 429 block for `endpoint` until `now + retry_after`.
    /// Never shortens an already-longer active block.
    pub fn record_rate_limit_at(
        &self,
        endpoint: &JudgeEndpoint,
        retry_after: Duration,
        now: SystemTime,
    ) -> std::io::Result<()> {
        let now_ms = system_time_ms(now);
        let add_ms = retry_after.as_millis().min(u64::MAX as u128) as u64;
        let new_until = now_ms.saturating_add(add_ms);
        let blocked_until_ms = self
            .read_until_ms(endpoint)
            .map_or(new_until, |existing| existing.max(new_until));
        create_private_dir(&self.dir)?;
        let rec = BlockRecord {
            provider: endpoint.provider.clone(),
            model: endpoint.model.clone(),
            blocked_until_ms,
        };
        let text = serde_json::to_string(&rec).map_err(std::io::Error::other)?;
        write_atomic(&self.block_path(endpoint), &text)
    }

    pub fn record_rate_limit(
        &self,
        endpoint: &JudgeEndpoint,
        retry_after: Duration,
    ) -> std::io::Result<()> {
        self.record_rate_limit_at(endpoint, retry_after, SystemTime::now())
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
/// honouring and updating cross-process 429 blocks in `blocks`.
#[allow(clippy::too_many_arguments)]
pub async fn dispatch_once<T, E, Call, Fut, Classify, RateLimitErr>(
    blocks: &BlockStore,
    primary: &JudgeEndpoint,
    fallback: Option<&JudgeEndpoint>,
    now: SystemTime,
    remaining_budget: Duration,
    mut call: Call,
    classify: Classify,
    rate_limit_err: RateLimitErr,
) -> DispatchStep<T, E>
where
    Call: FnMut(&JudgeEndpoint) -> Fut,
    Fut: Future<Output = Result<T, E>>,
    Classify: Fn(&E) -> FallbackTrigger,
    RateLimitErr: Fn(&JudgeEndpoint, Duration, Duration) -> E,
{
    let fb = distinct_fallback(primary, fallback);

    // 1. Primary already blocked: never send to it.
    if let Some(primary_rem) = blocks.remaining_at(primary, now) {
        if let Some(fb) = fb {
            if let Some(fb_rem) = blocks.remaining_at(fb, now) {
                let wait = primary_rem.min(fb_rem);
                return if wait < remaining_budget {
                    DispatchStep::Wait(wait)
                } else {
                    DispatchStep::Fail(rate_limit_err(primary, wait, remaining_budget))
                };
            }
            return match call(fb).await {
                Ok(v) => DispatchStep::Done(v, JudgeChannel::Fallback),
                Err(fb_err) => {
                    if let FallbackTrigger::RateLimited { retry_after } = classify(&fb_err) {
                        let _ = blocks.record_rate_limit_at(fb, retry_after, now);
                        let wait = primary_rem.min(retry_after);
                        if wait < remaining_budget {
                            return DispatchStep::Wait(wait);
                        }
                        return DispatchStep::Fail(rate_limit_err(fb, wait, remaining_budget));
                    }
                    DispatchStep::Fail(fb_err)
                }
            };
        }
        return if primary_rem < remaining_budget {
            DispatchStep::Wait(primary_rem)
        } else {
            DispatchStep::Fail(rate_limit_err(primary, primary_rem, remaining_budget))
        };
    }

    // 2. Primary unblocked: send to primary first.
    match call(primary).await {
        Ok(v) => DispatchStep::Done(v, JudgeChannel::Primary),
        Err(err) => match classify(&err) {
            FallbackTrigger::None => DispatchStep::Fail(err),
            FallbackTrigger::Transient => {
                let Some(fb) = fb else {
                    return DispatchStep::Fail(err);
                };
                if blocks.remaining_at(fb, now).is_some() {
                    return DispatchStep::Fail(err);
                }
                match call(fb).await {
                    Ok(v) => DispatchStep::Done(v, JudgeChannel::Fallback),
                    Err(fb_err) => {
                        if let FallbackTrigger::RateLimited { retry_after } = classify(&fb_err) {
                            let _ = blocks.record_rate_limit_at(fb, retry_after, now);
                        }
                        DispatchStep::Fail(fb_err)
                    }
                }
            }
            FallbackTrigger::RateLimited { retry_after } => {
                let _ = blocks.record_rate_limit_at(primary, retry_after, now);
                if let Some(fb) = fb {
                    if let Some(fb_rem) = blocks.remaining_at(fb, now) {
                        let wait = retry_after.min(fb_rem);
                        return if wait < remaining_budget {
                            DispatchStep::Wait(wait)
                        } else {
                            DispatchStep::Fail(rate_limit_err(primary, wait, remaining_budget))
                        };
                    }
                    return match call(fb).await {
                        Ok(v) => DispatchStep::Done(v, JudgeChannel::Fallback),
                        Err(fb_err) => {
                            if let FallbackTrigger::RateLimited { retry_after: fb_ra } =
                                classify(&fb_err)
                            {
                                let _ = blocks.record_rate_limit_at(fb, fb_ra, now);
                                let wait = retry_after.min(fb_ra);
                                if wait < remaining_budget {
                                    return DispatchStep::Wait(wait);
                                }
                                return DispatchStep::Fail(rate_limit_err(
                                    fb,
                                    wait,
                                    remaining_budget,
                                ));
                            }
                            DispatchStep::Fail(fb_err)
                        }
                    };
                }
                if retry_after < remaining_budget {
                    DispatchStep::Wait(retry_after)
                } else {
                    DispatchStep::Fail(rate_limit_err(primary, retry_after, remaining_budget))
                }
            }
        },
    }
}

/// Run [`dispatch_once`] in a loop up to `deadline`, sleeping via caller-provided
/// `sleep` when a rate-limit block fits before the deadline.
#[allow(clippy::too_many_arguments)]
pub async fn dispatch_with_deadline<T, E, Now, Sleep, SleepFut, Call, Fut, Classify, RateLimitErr>(
    blocks: &BlockStore,
    primary: &JudgeEndpoint,
    fallback: Option<&JudgeEndpoint>,
    deadline: Duration,
    mut now: Now,
    mut sleep: Sleep,
    mut call: Call,
    classify: Classify,
    rate_limit_err: RateLimitErr,
) -> Result<(T, JudgeChannel), E>
where
    Now: FnMut() -> SystemTime,
    Sleep: FnMut(Duration) -> SleepFut,
    SleepFut: Future<Output = ()>,
    Call: FnMut(&JudgeEndpoint, Duration) -> Fut,
    Fut: Future<Output = Result<T, E>>,
    Classify: Fn(&E) -> FallbackTrigger,
    RateLimitErr: Fn(&JudgeEndpoint, Duration, Duration) -> E,
{
    let start = now();
    for attempt in 0..=MAX_RATE_LIMIT_RETRIES {
        let cur = now();
        let elapsed = cur.duration_since(start).unwrap_or(Duration::ZERO);
        let remaining = deadline.saturating_sub(elapsed);
        match dispatch_once(
            blocks,
            primary,
            fallback,
            cur,
            remaining,
            |ep| call(ep, remaining),
            &classify,
            &rate_limit_err,
        )
        .await
        {
            DispatchStep::Done(v, ch) => return Ok((v, ch)),
            DispatchStep::Fail(e) => return Err(e),
            DispatchStep::Wait(wait) => {
                if attempt == MAX_RATE_LIMIT_RETRIES {
                    return Err(rate_limit_err(primary, wait, remaining));
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
        let clock = Rc::new(Cell::new(t0));
        let slept = Rc::new(Mutex::new(Vec::new()));
        let calls = Rc::new(Mutex::new(0usize));

        // 60s Retry-After > 50s deadline: fails fast with clear error, zero sleep.
        let c1 = clock.clone();
        let s1 = slept.clone();
        let n1 = calls.clone();
        let err = run_block(dispatch_with_deadline(
            &blocks,
            &primary(),
            None,
            Duration::from_secs(50),
            move || c1.get(),
            move |d| {
                s1.lock().unwrap().push(d);
                async {}
            },
            move |_ep, _rem| {
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
        clock.set(t0 + Duration::from_secs(5));
        let c2 = clock.clone();
        let n2 = calls.clone();
        let err2 = run_block(dispatch_with_deadline(
            &blocks,
            &primary(),
            None,
            Duration::from_secs(50),
            move || c2.get(),
            |_| async {},
            move |_ep, _rem| {
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
        let c3 = clock.clone();
        let c3_sleep = clock.clone();
        let s3 = slept.clone();
        let n3 = calls.clone();
        let (val, ch) = run_block(dispatch_with_deadline(
            &blocks,
            &primary(),
            None,
            Duration::from_secs(120),
            move || c3.get(),
            move |d| {
                s3.lock().unwrap().push(d);
                c3_sleep.set(c3_sleep.get() + d);
                async {}
            },
            move |_ep, _rem| {
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
        let clock = Rc::new(Cell::new(t0));
        let seen = Rc::new(Mutex::new(Vec::<String>::new()));

        // Request 1: primary returns 429 -> immediately answered by fallback.
        let c1 = clock.clone();
        let s1 = seen.clone();
        let (val, ch) = run_block(dispatch_with_deadline(
            &blocks,
            &primary(),
            Some(&fallback()),
            Duration::from_secs(10),
            move || c1.get(),
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
        clock.set(t0 + Duration::from_secs(10));
        let second_process_blocks = BlockStore::new(dir.clone());
        let c2 = clock.clone();
        let s2 = seen.clone();
        let (val2, ch2) = run_block(dispatch_with_deadline(
            &second_process_blocks,
            &primary(),
            Some(&fallback()),
            Duration::from_secs(10),
            move || c2.get(),
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
        clock.set(t0 + Duration::from_secs(61));
        let c3 = clock.clone();
        let s3 = seen.clone();
        let (val3, ch3) = run_block(dispatch_with_deadline(
            &second_process_blocks,
            &primary(),
            Some(&fallback()),
            Duration::from_secs(10),
            move || c3.get(),
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

        // 5xx / network -> fallback, and primary is NOT blocked.
        let (v, ch) = run_block(dispatch_with_deadline(
            &blocks,
            &primary(),
            Some(&fallback()),
            Duration::from_secs(10),
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
                Duration::from_secs(10),
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
}
