//! Concurrent `agentdeck route` processes share Jev calls. Each process drops
//! its request into a spool directory; whoever holds the lock sends everything
//! pending for the same judge and pool as one decision through the shared
//! primary/fallback block dispatcher, then writes each answer back. A request
//! alone in the queue is sent unchanged.

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agentdeck_core::judge::{
    BlockStore, DispatchStep, FallbackTrigger, JudgeChannel, JudgeEndpoint, create_private_dir,
    dispatch_once, format_rate_limit_exceeded, read_private_entry, write_atomic,
};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

/// How a terminal dispatch error should be handled across requests in a batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchErrorKind {
    /// Server or channel-wide error (401/403/404, missing API key, channel-level
    /// 400, 5xx, network failure): fail the whole remaining batch at once.
    ChannelWide,
    /// Leader-side timeout: keep local to the leader's own requests so longer
    /// waiters in the same group survive untouched.
    LeaderTimeout,
    /// Ambiguous or request-specific error (ambiguous 400, malformed JSON):
    /// if batched, fall back to one-by-one capped at `batch_size + 1` sends.
    RequestSpecific,
}

fn system_time_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

fn duration_ms(d: Duration) -> u64 {
    d.as_millis().min(u64::MAX as u128) as u64
}

/// Total wait for one answer on the hook path, queue and 429 back-off included:
/// the hook kills `route` after 55 s.
pub const BUDGET: Duration = Duration::from_secs(50);
/// Fixed budget reserved for an unblocked fallback on router judge calls:
/// leaves primary its full 30 s HTTP timeout on the 50 s hook path (`50 - 15 = 35 >= 30`)
/// while guaranteeing at least 15 s for fallback if primary hangs.
pub const FALLBACK_RESERVE: Duration = Duration::from_secs(15);
/// Tasks per batched decision; 7 real briefs measured about 11k input tokens.
pub const MAX_TASKS: usize = 8;
const POLL: Duration = Duration::from_millis(150);
const STALE: Duration = Duration::from_secs(600);
const MAX_WAIT_RETRIES: usize = 8;

/// Internal marker for rate-limit-exceeded errors synthesized by the dispatcher
/// so `lead` always treats them as channel-wide for the batch.
#[derive(Debug)]
struct ChannelWideRateLimit(String);

impl std::fmt::Display for ChannelWideRateLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ChannelWideRateLimit {}

fn next_request_id() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "{}-{nanos}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// HTTP 429 from the decision endpoint, with the server's `Retry-After`.
#[derive(Debug)]
pub struct RateLimited {
    pub retry_after: Duration,
    pub message: String,
}

impl std::fmt::Display for RateLimited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RateLimited {}

/// Answer returned from the shared Jev queue.
#[derive(Debug, Clone)]
pub struct QueueAnswer {
    pub answers: Value,
    pub cost: Option<f64>,
    pub channel: JudgeChannel,
}

/// Asks Jev through the shared queue with the default hook-path budget ([`BUDGET`]).
#[allow(clippy::too_many_arguments)]
pub async fn ask<F, Fut, C, Cb>(
    spool: &Path,
    request: &Value,
    body: &str,
    blocks: &BlockStore,
    primary: &JudgeEndpoint,
    fallback: Option<&JudgeEndpoint>,
    fetch: F,
    classify: C,
    classify_batch: Cb,
) -> anyhow::Result<QueueAnswer>
where
    F: FnMut(JudgeEndpoint, Value, String, Instant) -> Fut,
    Fut: Future<Output = anyhow::Result<(Value, Option<f64>)>>,
    C: Fn(&anyhow::Error) -> FallbackTrigger,
    Cb: Fn(&anyhow::Error) -> BatchErrorKind,
{
    ask_with_deadline(
        spool,
        request,
        body,
        BUDGET,
        blocks,
        primary,
        fallback,
        fetch,
        classify,
        classify_batch,
    )
    .await
}

/// Asks Jev through the shared queue with an explicit `budget` deadline (so
/// callers such as `route_batch` can pass e.g. 120 s).
#[allow(clippy::too_many_arguments)]
pub async fn ask_with_deadline<F, Fut, C, Cb>(
    spool: &Path,
    request: &Value,
    body: &str,
    budget: Duration,
    blocks: &BlockStore,
    primary: &JudgeEndpoint,
    fallback: Option<&JudgeEndpoint>,
    fetch: F,
    classify: C,
    classify_batch: Cb,
) -> anyhow::Result<QueueAnswer>
where
    F: FnMut(JudgeEndpoint, Value, String, Instant) -> Fut,
    Fut: Future<Output = anyhow::Result<(Value, Option<f64>)>>,
    C: Fn(&anyhow::Error) -> FallbackTrigger,
    Cb: Fn(&anyhow::Error) -> BatchErrorKind,
{
    ask_inner(
        spool,
        request,
        body,
        budget,
        blocks,
        primary,
        fallback,
        Instant::now,
        SystemTime::now,
        tokio::time::sleep,
        true,
        fetch,
        classify,
        classify_batch,
    )
    .await
}

/// Asks Jev for several requests through the shared spool at once, so the
/// leader batches up to [`MAX_TASKS`] compatible requests per judge call within
/// the shared `budget` deadline. Returns one result per input item in order.
#[allow(clippy::too_many_arguments)]
pub async fn ask_many_with_deadline<F, Fut, C, Cb>(
    spool: &Path,
    items: &[(Value, String)],
    budget: Duration,
    blocks: &BlockStore,
    primary: &JudgeEndpoint,
    fallback: Option<&JudgeEndpoint>,
    fetch: F,
    classify: C,
    classify_batch: Cb,
) -> Vec<anyhow::Result<QueueAnswer>>
where
    F: FnMut(JudgeEndpoint, Value, String, Instant) -> Fut,
    Fut: Future<Output = anyhow::Result<(Value, Option<f64>)>>,
    C: Fn(&anyhow::Error) -> FallbackTrigger,
    Cb: Fn(&anyhow::Error) -> BatchErrorKind,
{
    ask_many_inner(
        spool,
        items,
        budget,
        blocks,
        primary,
        fallback,
        Instant::now,
        SystemTime::now,
        tokio::time::sleep,
        true,
        fetch,
        classify,
        classify_batch,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn ask_inner<MonoNow, WallNow, Sleep, SleepFut, F, Fut, C, Cb>(
    spool: &Path,
    request: &Value,
    body: &str,
    budget: Duration,
    blocks: &BlockStore,
    primary: &JudgeEndpoint,
    fallback: Option<&JudgeEndpoint>,
    mono_now: MonoNow,
    wall_now: WallNow,
    sleep: Sleep,
    add_jitter: bool,
    fetch: F,
    classify: C,
    classify_batch: Cb,
) -> anyhow::Result<QueueAnswer>
where
    MonoNow: FnMut() -> Instant,
    WallNow: FnMut() -> SystemTime,
    Sleep: FnMut(Duration) -> SleepFut,
    SleepFut: Future<Output = ()>,
    F: FnMut(JudgeEndpoint, Value, String, Instant) -> Fut,
    Fut: Future<Output = anyhow::Result<(Value, Option<f64>)>>,
    C: Fn(&anyhow::Error) -> FallbackTrigger,
    Cb: Fn(&anyhow::Error) -> BatchErrorKind,
{
    ask_many_inner(
        spool,
        &[(request.clone(), body.to_string())],
        budget,
        blocks,
        primary,
        fallback,
        mono_now,
        wall_now,
        sleep,
        add_jitter,
        fetch,
        classify,
        classify_batch,
    )
    .await
    .into_iter()
    .next()
    .unwrap_or_else(|| Err(anyhow::anyhow!("Jev queue: empty request")))
}

#[allow(clippy::too_many_arguments)]
async fn ask_many_inner<MonoNow, WallNow, Sleep, SleepFut, F, Fut, C, Cb>(
    spool: &Path,
    items: &[(Value, String)],
    budget: Duration,
    blocks: &BlockStore,
    primary: &JudgeEndpoint,
    fallback: Option<&JudgeEndpoint>,
    mut mono_now: MonoNow,
    mut wall_now: WallNow,
    mut sleep: Sleep,
    add_jitter: bool,
    mut fetch: F,
    classify: C,
    classify_batch: Cb,
) -> Vec<anyhow::Result<QueueAnswer>>
where
    MonoNow: FnMut() -> Instant,
    WallNow: FnMut() -> SystemTime,
    Sleep: FnMut(Duration) -> SleepFut,
    SleepFut: Future<Output = ()>,
    F: FnMut(JudgeEndpoint, Value, String, Instant) -> Fut,
    Fut: Future<Output = anyhow::Result<(Value, Option<f64>)>>,
    C: Fn(&anyhow::Error) -> FallbackTrigger,
    Cb: Fn(&anyhow::Error) -> BatchErrorKind,
{
    if items.is_empty() {
        return Vec::new();
    }
    let deadline = mono_now() + budget;
    let wall_deadline_ms = system_time_ms(wall_now()).saturating_add(duration_ms(budget));
    if let Err(e) = create_private_dir(spool) {
        let msg = e.to_string();
        return (0..items.len())
            .map(|_| Err(anyhow::anyhow!("{msg}")))
            .collect();
    }
    // Enqueue every request before acquiring the per-group lock so the first
    // leader turn sees the whole wave and packs up to MAX_TASKS into one judge request.
    let mut ids = Vec::with_capacity(items.len());
    let mut groups = Vec::with_capacity(items.len());
    let mut req_paths = Vec::with_capacity(items.len());
    let mut done_paths = Vec::with_capacity(items.len());
    let mut results: Vec<Option<anyhow::Result<QueueAnswer>>> =
        (0..items.len()).map(|_| None).collect();
    for (i, (request, body)) in items.iter().enumerate() {
        let id = next_request_id();
        let group = group_of(request, primary, fallback);
        let req_path = spool.join(format!("{id}.req"));
        let done_path = spool.join(format!("{id}.done"));
        let entry = serde_json::json!({
            "group": group,
            "request": request,
            "body": body,
            "deadline_ms": wall_deadline_ms,
        });
        if let Err(e) = write_atomic(&req_path, &entry.to_string()) {
            results[i] = Some(Err(e.into()));
        }
        ids.push(id);
        groups.push(group);
        req_paths.push(req_path);
        done_paths.push(done_path);
    }
    let own_ids: HashSet<&str> = ids.iter().map(String::as_str).collect();
    let mut locks: HashMap<String, std::fs::File> = HashMap::new();
    let mut wait_retries: HashMap<String, usize> = HashMap::new();
    let fail_group = |target_group: &str,
                      msg: &str,
                      spool: &Path,
                      groups: &[String],
                      req_paths: &[PathBuf],
                      done_paths: &[PathBuf],
                      results: &mut [Option<anyhow::Result<QueueAnswer>>]| {
        for (i, slot) in results.iter_mut().enumerate() {
            if slot.is_none() && groups[i] == target_group {
                let _ = std::fs::remove_file(&req_paths[i]);
                *slot = Some(
                    take_done(spool, &done_paths[i])
                        .unwrap_or_else(|| Err(anyhow::anyhow!("{msg}"))),
                );
            }
        }
    };
    loop {
        for (i, slot) in results.iter_mut().enumerate() {
            if slot.is_none()
                && let Some(r) = take_done(spool, &done_paths[i])
            {
                *slot = Some(r);
            }
        }
        let Some(first) = results.iter().position(Option::is_none) else {
            break;
        };
        let remaining = deadline.saturating_duration_since(mono_now());
        if remaining.is_zero() {
            for (i, slot) in results.iter_mut().enumerate() {
                if slot.is_none() {
                    let _ = std::fs::remove_file(&req_paths[i]);
                    *slot = Some(take_done(spool, &done_paths[i]).unwrap_or_else(|| {
                        Err(anyhow::anyhow!(
                            "Jev queue: no answer within {}s (decision rate limit)",
                            budget.as_secs()
                        ))
                    }));
                }
            }
            break;
        }
        let group = groups[first].clone();
        if !locks.contains_key(&group) {
            match open_group_lock(spool, &group) {
                Ok(f) => {
                    locks.insert(group.clone(), f);
                }
                Err(e) => {
                    fail_group(
                        &group,
                        &e.to_string(),
                        spool,
                        &groups,
                        &req_paths,
                        &done_paths,
                        &mut results,
                    );
                    continue;
                }
            }
        }
        let lock = &locks[&group];
        match lock.try_lock() {
            Ok(()) => {
                let outcome = lead_once(
                    spool,
                    &ids[first],
                    &own_ids,
                    deadline,
                    blocks,
                    primary,
                    fallback,
                    &mut mono_now,
                    &mut wall_now,
                    &mut fetch,
                    &classify,
                    &classify_batch,
                )
                .await;
                // Release the per-group lock immediately so we NEVER hold it while sleeping
                // out a 429 block or blocking shorter-deadline waiters in the same group.
                let _ = lock.unlock();
                for (i, slot) in results.iter_mut().enumerate() {
                    if slot.is_none()
                        && let Some(r) = take_done(spool, &done_paths[i])
                    {
                        *slot = Some(r);
                    }
                }
                match outcome {
                    LeadOutcome::Done => {
                        if results[first].is_none() {
                            let rem = deadline.saturating_duration_since(mono_now());
                            if !rem.is_zero() {
                                sleep(POLL.min(rem)).await;
                            }
                        }
                    }
                    LeadOutcome::Wait(wait) => {
                        let rem = deadline.saturating_duration_since(mono_now());
                        let retries = wait_retries.entry(group.clone()).or_insert(0);
                        if *retries >= MAX_WAIT_RETRIES || wait >= rem {
                            let msg = format_rate_limit_exceeded(primary, wait, rem);
                            fail_group(
                                &group,
                                &msg,
                                spool,
                                &groups,
                                &req_paths,
                                &done_paths,
                                &mut results,
                            );
                        } else {
                            *retries += 1;
                            let dur = with_bounded_jitter(wait, rem, add_jitter).min(rem);
                            sleep(dur).await;
                        }
                    }
                    LeadOutcome::LocalError(msg) => {
                        fail_group(
                            &group,
                            &msg,
                            spool,
                            &groups,
                            &req_paths,
                            &done_paths,
                            &mut results,
                        );
                    }
                    LeadOutcome::OwnError(msg) => {
                        let _ = std::fs::remove_file(&req_paths[first]);
                        if results[first].is_none() {
                            results[first] = Some(
                                take_done(spool, &done_paths[first])
                                    .unwrap_or_else(|| Err(anyhow::anyhow!("{msg}"))),
                            );
                        }
                    }
                }
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                sleep(POLL.min(remaining)).await;
            }
            Err(std::fs::TryLockError::Error(e)) => {
                fail_group(
                    &group,
                    &e.to_string(),
                    spool,
                    &groups,
                    &req_paths,
                    &done_paths,
                    &mut results,
                );
            }
        }
    }
    results.into_iter().map(Option::unwrap).collect()
}

fn open_group_lock(spool: &Path, group: &str) -> std::io::Result<std::fs::File> {
    let lock_path = spool.join(format!("lock-{group}"));
    let mut opts = std::fs::File::options();
    opts.create(true).truncate(false).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = opts.open(&lock_path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

enum LeadOutcome {
    Done,
    Wait(Duration),
    /// Caller-local failure (caller-deadline 429 or leader timeout): fail only
    /// this process's items in the group; other processes' `.req` files stay.
    LocalError(String),
    /// This caller's own `.req` file could not be read or parsed.
    OwnError(String),
}

#[derive(Debug)]
struct Pending {
    path: PathBuf,
    id: String,
    request: Value,
    body: String,
    deadline_ms: Option<u64>,
}

impl Pending {
    fn is_expired(&self, now_ms: u64) -> bool {
        self.deadline_ms.is_some_and(|dl| now_ms >= dl)
    }
}

fn with_bounded_jitter(wait: Duration, remaining: Duration, add_jitter: bool) -> Duration {
    if !add_jitter || wait < Duration::from_secs(1) {
        return wait;
    }
    let jitter = Duration::from_millis(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| u64::from(d.subsec_millis() % 500))
            .unwrap_or(0),
    );
    let candidate = wait + jitter;
    if candidate < remaining {
        candidate
    } else {
        wait
    }
}

fn expected_keys_present(p: &Pending, split_answers: &Value) -> bool {
    let Some(ans_obj) = split_answers.as_object() else {
        return false;
    };
    match p.request.get("questions").and_then(Value::as_object) {
        Some(qs) if !qs.is_empty() => qs.keys().all(|k| ans_obj.contains_key(k)),
        _ => !ans_obj.is_empty(),
    }
}

/// Sends this process's request together with every compatible pending one
/// through the shared block/fallback dispatcher without sleeping under the lock.
/// Drops expired peer requests before every send and before writing answers;
/// keeps leader timeouts local to the leader; and on ambiguous request errors
/// falls back one-by-one with a hard cap of `batch_size + 1` sends per batch.
#[allow(clippy::too_many_arguments)]
async fn lead_once<MonoNow, WallNow, F, Fut, C, Cb>(
    spool: &Path,
    own: &str,
    own_ids: &HashSet<&str>,
    deadline: Instant,
    blocks: &BlockStore,
    primary: &JudgeEndpoint,
    fallback: Option<&JudgeEndpoint>,
    mono_now: &mut MonoNow,
    wall_now: &mut WallNow,
    fetch: &mut F,
    classify: &C,
    classify_batch: &Cb,
) -> LeadOutcome
where
    MonoNow: FnMut() -> Instant,
    WallNow: FnMut() -> SystemTime,
    F: FnMut(JudgeEndpoint, Value, String, Instant) -> Fut,
    Fut: Future<Output = anyhow::Result<(Value, Option<f64>)>>,
    C: Fn(&anyhow::Error) -> FallbackTrigger,
    Cb: Fn(&anyhow::Error) -> BatchErrorKind,
{
    let batch = match collect(spool, own, system_time_ms(wall_now())) {
        Ok(b) => b,
        Err(msg) => return LeadOutcome::OwnError(msg),
    };
    if batch.is_empty() || batch[0].id != own {
        return LeadOutcome::Done;
    }
    let max_sends = batch.len() + 1;
    let mut sends = 0usize;
    let mut last_err: Option<String> = None;
    let mut work: VecDeque<Vec<Pending>> = VecDeque::from([batch]);
    while let Some(mut sub) = work.pop_front() {
        if mono_now() >= deadline {
            return LeadOutcome::Done;
        }
        let now_ms = system_time_ms(wall_now());
        sub.retain(|p| {
            own_ids.contains(p.id.as_str()) || (!p.is_expired(now_ms) && p.path.exists())
        });
        if sub.is_empty() {
            continue;
        }
        if sends >= max_sends {
            let err_payload = serde_json::json!({
                "error": last_err
                    .as_deref()
                    .unwrap_or("Jev queue: batch send cap reached"),
            });
            for p in &sub {
                finish_if_live(spool, p, own_ids, &err_payload, now_ms);
            }
            for rest in &work {
                for p in rest {
                    finish_if_live(spool, p, own_ids, &err_payload, now_ms);
                }
            }
            return LeadOutcome::Done;
        }
        sends += 1;
        let (request, body) = if sub.len() == 1 {
            (sub[0].request.clone(), sub[0].body.clone())
        } else {
            let r = combine(&sub);
            let b = crate::route::stringify_like_node(&r);
            (r, b)
        };
        let step = dispatch_once(
            blocks,
            primary,
            fallback,
            deadline,
            FALLBACK_RESERVE,
            None,
            &mut *mono_now,
            &mut *wall_now,
            |ep, attempt_deadline| {
                fetch(ep.clone(), request.clone(), body.clone(), attempt_deadline)
            },
            classify,
            |ep, ra, rem| ChannelWideRateLimit(format_rate_limit_exceeded(ep, ra, rem)).into(),
        )
        .await;
        match step {
            DispatchStep::Done((answers, cost), channel) => {
                let write_now_ms = system_time_ms(wall_now());
                let n = sub.len();
                if n == 1 {
                    finish_if_live(
                        spool,
                        &sub[0],
                        own_ids,
                        &serde_json::json!({
                            "answers": answers,
                            "cost": cost,
                            "channel": channel.as_str(),
                        }),
                        write_now_ms,
                    );
                    continue;
                }
                let mut good = Vec::new();
                let mut bad = Vec::new();
                if answers.is_object() {
                    for (i, p) in sub.into_iter().enumerate() {
                        let a = split(&answers, i);
                        if expected_keys_present(&p, &a) {
                            good.push((p, a));
                        } else {
                            bad.push(p);
                        }
                    }
                } else {
                    bad = sub;
                }
                for (p, a) in good {
                    let per_cost = cost.map(|c| c / n as f64);
                    finish_if_live(
                        spool,
                        &p,
                        own_ids,
                        &serde_json::json!({
                            "answers": a,
                            "cost": per_cost,
                            "channel": channel.as_str(),
                        }),
                        write_now_ms,
                    );
                }
                for p in bad.into_iter().rev() {
                    work.push_front(vec![p]);
                }
            }
            DispatchStep::Wait(wait) => {
                // Already-finished tasks have their .done written; unfinished ones
                // remain as .req files and will be re-collected after sleeping unlocked.
                return LeadOutcome::Wait(wait);
            }
            DispatchStep::Fail(e) => {
                if e.downcast_ref::<ChannelWideRateLimit>().is_some() {
                    // Caller-deadline rate-limit failure: only fail this caller's items
                    // so longer-deadline waiters in the same group survive.
                    return LeadOutcome::LocalError(e.to_string());
                }
                let batch_kind = classify_batch(&e);
                if batch_kind == BatchErrorKind::LeaderTimeout || mono_now() >= deadline {
                    // Leader's own timeout or exhausted deadline stays local to the leader
                    // so longer-deadline waiters in the same group survive untouched.
                    return LeadOutcome::LocalError(e.to_string());
                }
                let channel_wide = batch_kind == BatchErrorKind::ChannelWide
                    || !matches!(classify(&e), FallbackTrigger::None);
                let write_now_ms = system_time_ms(wall_now());
                let err_msg = e.to_string();
                let err_payload = serde_json::json!({"error": &err_msg});
                if channel_wide {
                    // Server/channel-wide failure (401/403, missing key, channel-level 400, 5xx):
                    // fail the whole remaining batch at once instead of N failing calls.
                    for p in &sub {
                        finish_if_live(spool, p, own_ids, &err_payload, write_now_ms);
                    }
                    for rest in &work {
                        for p in rest {
                            finish_if_live(spool, p, own_ids, &err_payload, write_now_ms);
                        }
                    }
                    return LeadOutcome::Done;
                }
                // Ambiguous / request-specific failure (ambiguous 400 / invalid JSON parse error):
                // if solo, attribute the error to this task; if batched, fall back one-by-one
                // (capped at batch_size + 1 total sends for the batch).
                if sub.len() == 1 {
                    finish_if_live(spool, &sub[0], own_ids, &err_payload, write_now_ms);
                } else {
                    last_err = Some(err_msg);
                    for p in sub.into_iter().rev() {
                        work.push_front(vec![p]);
                    }
                }
            }
        }
    }
    LeadOutcome::Done
}

/// Own request first, then the oldest compatible unexpired ones. Requests of
/// exited processes and stale answers are deleted; expired live peers are
/// skipped without touching their files; an unreadable own request returns
/// an explicit error instead of being silently skipped.
fn collect(spool: &Path, own: &str, now_ms: u64) -> Result<Vec<Pending>, String> {
    let mut all: Vec<(SystemTime, Pending, String)> = Vec::new();
    for f in std::fs::read_dir(spool).into_iter().flatten().flatten() {
        let path = f.path();
        let name = f.file_name().to_string_lossy().into_owned();
        let modified = f
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(UNIX_EPOCH);
        if let Some(id) = name.strip_suffix(".req") {
            let alive = id
                .split('-')
                .next()
                .is_some_and(|pid| Path::new("/proc").join(pid).exists());
            if !alive {
                let _ = std::fs::remove_file(&path);
                continue;
            }
            let text = match read_private_entry(spool, &path) {
                Ok(Some(t)) => t,
                Ok(None) => continue,
                Err(e) if id == own => {
                    return Err(format!("Jev queue: own request unreadable: {e}"));
                }
                Err(_) => continue,
            };
            let v = match serde_json::from_str::<Value>(&text) {
                Ok(v) => v,
                Err(e) if id == own => {
                    return Err(format!("Jev queue: own request corrupt: {e}"));
                }
                Err(_) => continue,
            };
            let deadline_ms = v.get("deadline_ms").and_then(Value::as_u64);
            if id != own && deadline_ms.is_some_and(|dl| now_ms >= dl) {
                continue;
            }
            let group = v["group"].as_str().unwrap_or_default().to_string();
            let p = Pending {
                path,
                id: id.to_string(),
                request: v["request"].clone(),
                body: v["body"].as_str().unwrap_or_default().to_string(),
                deadline_ms,
            };
            all.push((modified, p, group));
        } else if (name.ends_with(".done") || name.ends_with(".tmp"))
            && SystemTime::now()
                .duration_since(modified)
                .is_ok_and(|age| age > STALE)
        {
            let _ = std::fs::remove_file(&path);
        }
    }
    let Some(group) = all
        .iter()
        .find(|(_, p, _)| p.id == own)
        .map(|(_, _, g)| g.clone())
    else {
        return Ok(Vec::new());
    };
    all.retain(|(_, _, g)| *g == group);
    all.sort_by_key(|(t, p, _)| (p.id != own, *t));
    Ok(all.into_iter().take(MAX_TASKS).map(|(_, p, _)| p).collect())
}

/// Requests can share a decision only when primary provider/model, fallback
/// provider/model, judge model and candidate profiles all match.
fn group_of(request: &Value, primary: &JudgeEndpoint, fallback: Option<&JudgeEndpoint>) -> String {
    let fb = fallback
        .filter(|f| *f != primary)
        .map(|f| (&f.provider, &f.model));
    let key = serde_json::json!([
        &primary.provider,
        &primary.model,
        fb,
        request["model"],
        request["state"]["models"],
    ]);
    format!("{:x}", Sha256::digest(key.to_string().as_bytes()))
}

/// One decision over several tasks: `state.tasks.tN` holds each brief and
/// every question is renamed `tN__<name>` and pointed at its task.
fn combine(batch: &[Pending]) -> Value {
    let mut tasks = Map::new();
    let mut questions = Map::new();
    for (i, p) in batch.iter().enumerate() {
        let t = format!("t{i}");
        tasks.insert(t.clone(), p.request["state"]["task"].clone());
        for (name, q) in p.request["questions"].as_object().into_iter().flatten() {
            let mut q = q.clone();
            let instructions = q["instructions"]
                .as_str()
                .unwrap_or_default()
                .replace("context.task", &format!("context.tasks.{t}"));
            q["instructions"] =
                Value::from(format!("Task {t} (context.tasks.{t}): {instructions}"));
            questions.insert(format!("{t}__{name}"), q);
        }
    }
    serde_json::json!({
        "model": batch[0].request["model"],
        "state": {"tasks": tasks, "models": batch[0].request["state"]["models"]},
        "questions": questions,
    })
}

fn split(answers: &Value, i: usize) -> Value {
    let prefix = format!("t{i}__");
    Value::Object(
        answers
            .as_object()
            .into_iter()
            .flatten()
            .filter_map(|(k, v)| Some((k.strip_prefix(&prefix)?.to_string(), v.clone())))
            .collect(),
    )
}

fn finish(spool: &Path, p: &Pending, result: &Value) {
    let _ = write_atomic(&spool.join(format!("{}.done", p.id)), &result.to_string());
    let _ = std::fs::remove_file(&p.path);
}

fn finish_if_live(spool: &Path, p: &Pending, own_ids: &HashSet<&str>, result: &Value, now_ms: u64) {
    if !own_ids.contains(p.id.as_str()) && (p.is_expired(now_ms) || !p.path.exists()) {
        return;
    }
    finish(spool, p, result);
}

fn take_done(spool: &Path, path: &Path) -> Option<anyhow::Result<QueueAnswer>> {
    let text = read_private_entry(spool, path).ok()??;
    let v: Value = serde_json::from_str(&text).ok()?;
    let _ = std::fs::remove_file(path);
    Some(match v["error"].as_str() {
        Some(e) => Err(anyhow::anyhow!("{e}")),
        None => {
            let channel = match v["channel"].as_str() {
                Some("fallback") => JudgeChannel::Fallback,
                _ => JudgeChannel::Primary,
            };
            Ok(QueueAnswer {
                answers: v["answers"].clone(),
                cost: v["cost"].as_f64(),
                channel,
            })
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;
    use std::sync::{Arc, Mutex};

    fn request(task: &str) -> Value {
        serde_json::json!({
            "model": "judge",
            "state": {"task": task, "models": {"m": "profile"}},
            "questions": {
                "size": {"type": "score", "instructions": "How big?", "criteria": ["a", "b"]},
                "m": {"type": "score", "instructions": "Rate context.task for context.models.m.", "criteria": ["a", "b"]},
            },
        })
    }

    fn temp_dirs(tag: &str) -> (PathBuf, BlockStore) {
        let base = std::env::temp_dir().join(format!(
            "jev-queue-test-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let spool = base.join("spool");
        std::fs::create_dir_all(&spool).unwrap();
        let blocks = BlockStore::new(base.join("blocks"));
        (spool, blocks)
    }

    fn classify_rl(e: &anyhow::Error) -> FallbackTrigger {
        if let Some(rl) = e.downcast_ref::<RateLimited>() {
            FallbackTrigger::RateLimited {
                retry_after: rl.retry_after,
            }
        } else {
            FallbackTrigger::None
        }
    }

    fn classify_batch_test(e: &anyhow::Error) -> BatchErrorKind {
        let msg = e.to_string();
        if msg.contains("timeout") {
            BatchErrorKind::LeaderTimeout
        } else if msg.contains("401")
            || msg.contains("403")
            || msg.contains("decision key not found")
        {
            BatchErrorKind::ChannelWide
        } else {
            BatchErrorKind::RequestSpecific
        }
    }

    #[test]
    fn combine_points_each_question_at_its_task() {
        let batch: Vec<Pending> = ["one", "two"]
            .iter()
            .enumerate()
            .map(|(i, t)| Pending {
                path: PathBuf::new(),
                id: i.to_string(),
                request: request(t),
                body: String::new(),
                deadline_ms: None,
            })
            .collect();
        let c = combine(&batch);
        assert_eq!(c["state"]["tasks"]["t1"], "two");
        assert_eq!(c["state"]["models"]["m"], "profile");
        assert_eq!(
            c["questions"]["t1__m"]["instructions"],
            "Task t1 (context.tasks.t1): Rate context.tasks.t1 for context.models.m."
        );
        let answers = serde_json::json!({"t0__size": 1, "t1__size": 2, "t1__m": 3});
        assert_eq!(split(&answers, 1), serde_json::json!({"size": 2, "m": 3}));
    }

    /// A 429 that fits the deadline is waited out and newcomers arriving before
    /// the retry are batched into the retried call.
    #[tokio::test(flavor = "current_thread")]
    async fn rate_limit_waits_and_batches_newcomers() {
        let (dir, blocks) = temp_dirs("wait-batch");
        let sent: Arc<Mutex<Vec<Value>>> = Arc::default();
        let primary = JudgeEndpoint::new("vercel", "judge");
        let other = request("queued");
        let other_entry = serde_json::json!({
            "group": group_of(&other, &primary, None),
            "request": other,
            "body": "x"
        });
        write_atomic(
            &dir.join(format!("{}-1.req", std::process::id())),
            &other_entry.to_string(),
        )
        .unwrap();
        let t0 = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let m0 = Instant::now();
        let wall = Rc::new(Cell::new(t0));
        let mono = Rc::new(Cell::new(m0));
        let m_now = mono.clone();
        let w_now = wall.clone();
        let m_sleep = mono.clone();
        let w_sleep = wall.clone();
        let log = sent.clone();
        let answer = ask_inner(
            &dir,
            &request("mine"),
            "body",
            Duration::from_secs(120),
            &blocks,
            &primary,
            None,
            move || m_now.get(),
            move || w_now.get(),
            move |d| {
                m_sleep.set(m_sleep.get() + d);
                w_sleep.set(w_sleep.get() + d);
                async {}
            },
            false,
            move |_ep, r, _, _| {
                let log = log.clone();
                async move {
                    let mut log = log.lock().unwrap();
                    log.push(r.clone());
                    if log.len() == 1 {
                        return Err(RateLimited {
                            retry_after: Duration::from_secs(60),
                            message: "Jev HTTP 429".into(),
                        }
                        .into());
                    }
                    let mut a = Map::new();
                    for k in r["questions"].as_object().unwrap().keys() {
                        a.insert(k.clone(), serde_json::json!({"score": 1}));
                    }
                    Ok((Value::Object(a), Some(0.02)))
                }
            },
            classify_rl,
            classify_batch_test,
        )
        .await
        .unwrap();
        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 2, "one 429, then one retried batch");
        assert_eq!(sent[1]["state"]["tasks"]["t0"], "mine");
        assert_eq!(sent[1]["state"]["tasks"]["t1"], "queued");
        assert_eq!(
            answer.answers,
            serde_json::json!({"size": {"score": 1}, "m": {"score": 1}})
        );
        assert_eq!(answer.cost, Some(0.01));
        assert_eq!(answer.channel, JudgeChannel::Primary);
        let other_done = take_done(&dir, &dir.join(format!("{}-1.done", std::process::id())));
        assert!(other_done.unwrap().is_ok(), "queued request answered too");
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rate_limit_60s_fails_fast_under_50s_without_fallback_and_uses_fallback_when_configured()
     {
        let (dir, blocks) = temp_dirs("60s-fallback");
        let primary = JudgeEndpoint::new("vercel", "perplexity/pplx-decider-v1.2");
        let fallback = JudgeEndpoint::new("openrouter", "~typesafe/jev-latest");
        let t0 = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let m0 = Instant::now();
        let wall = Rc::new(Cell::new(t0));
        let mono = Rc::new(Cell::new(m0));

        // Without fallback, 60s > 50s BUDGET fails fast with clear error and no sleep.
        let m1 = mono.clone();
        let w1 = wall.clone();
        let slept = Rc::new(Cell::new(false));
        let s1 = slept.clone();
        let err = ask_inner(
            &dir,
            &request("mine"),
            "body",
            BUDGET,
            &blocks,
            &primary,
            None,
            move || m1.get(),
            move || w1.get(),
            move |_| {
                s1.set(true);
                async {}
            },
            false,
            |_ep, _r, _, _| async {
                Err::<(Value, Option<f64>), anyhow::Error>(
                    RateLimited {
                        retry_after: Duration::from_secs(60),
                        message: "Jev HTTP 429".into(),
                    }
                    .into(),
                )
            },
            classify_rl,
            classify_batch_test,
        )
        .await
        .unwrap_err();
        assert!(!slept.get());
        assert!(err.to_string().contains("60s"), "{err}");
        assert!(err.to_string().contains("50s"), "{err}");

        // With fallback configured, subsequent request skips blocked primary immediately.
        let m2 = mono.clone();
        let w2 = wall.clone();
        let providers: Arc<Mutex<Vec<String>>> = Arc::default();
        let p2 = providers.clone();
        let ans = ask_inner(
            &dir,
            &request("mine2"),
            "body",
            BUDGET,
            &blocks,
            &primary,
            Some(&fallback),
            move || m2.get(),
            move || w2.get(),
            |_| async {},
            false,
            move |ep, _r, _, _| {
                let p2 = p2.clone();
                async move {
                    p2.lock().unwrap().push(ep.provider);
                    Ok((serde_json::json!({"size": {"score": 1}}), Some(0.01)))
                }
            },
            classify_rl,
            classify_batch_test,
        )
        .await
        .unwrap();
        assert_eq!(ans.channel, JudgeChannel::Fallback);
        assert_eq!(*providers.lock().unwrap(), vec!["openrouter"]);
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn lone_request_is_sent_unchanged() {
        let (dir, blocks) = temp_dirs("lone");
        let r = request("solo");
        let expected = r.clone();
        let primary = JudgeEndpoint::new("openrouter", "judge");
        let got = ask(
            &dir,
            &r,
            "body",
            &blocks,
            &primary,
            None,
            move |_ep, req, body, _| {
                let expected = expected.clone();
                async move {
                    assert_eq!(req, expected);
                    assert_eq!(body, "body");
                    Ok((serde_json::json!({"size": 1}), None))
                }
            },
            classify_rl,
            classify_batch_test,
        )
        .await
        .unwrap();
        assert_eq!(got.answers, serde_json::json!({"size": 1}));
        assert_eq!(got.channel, JudgeChannel::Primary);
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ask_many_batches_up_to_max_tasks_per_call() {
        let (dir, blocks) = temp_dirs("ask-many");
        let primary = JudgeEndpoint::new("vercel", "judge");
        let calls: Arc<Mutex<Vec<usize>>> = Arc::default();

        let make_fetch = |log: Arc<Mutex<Vec<usize>>>| {
            move |_ep: JudgeEndpoint, req: Value, _body: String, _deadline: Instant| {
                let log = log.clone();
                async move {
                    let count = req
                        .pointer("/state/tasks")
                        .and_then(Value::as_object)
                        .map(|m| m.len())
                        .unwrap_or(1);
                    log.lock().unwrap().push(count);
                    let mut answers = Map::new();
                    for (k, _) in req["questions"].as_object().unwrap() {
                        let task_idx = k
                            .strip_prefix('t')
                            .and_then(|s| s.split_once("__"))
                            .and_then(|(n, _)| n.parse::<usize>().ok());
                        let task_text = match task_idx {
                            Some(i) => req["state"]["tasks"][format!("t{i}")]
                                .as_str()
                                .unwrap_or_default(),
                            None => req["state"]["task"].as_str().unwrap_or_default(),
                        };
                        let score = task_text
                            .strip_prefix("task-")
                            .and_then(|n| n.parse::<i64>().ok())
                            .unwrap_or(0);
                        answers.insert(k.clone(), serde_json::json!({"score": score}));
                    }
                    Ok((Value::Object(answers), Some(0.08)))
                }
            }
        };

        // 3 items -> 1 judge call, results in exact input order.
        let three: Vec<(Value, String)> = (0..3)
            .map(|i| {
                let r = request(&format!("task-{i}"));
                let b = r.to_string();
                (r, b)
            })
            .collect();
        let res3 = ask_many_with_deadline(
            &dir,
            &three,
            Duration::from_secs(120),
            &blocks,
            &primary,
            None,
            make_fetch(calls.clone()),
            classify_rl,
            classify_batch_test,
        )
        .await;
        assert_eq!(*calls.lock().unwrap(), vec![3]);
        assert_eq!(res3.len(), 3);
        for (i, r) in res3.iter().enumerate() {
            assert_eq!(r.as_ref().unwrap().answers["size"]["score"], i as i64);
        }

        // 10 items -> 2 judge calls (8 + 2), results in exact input order.
        calls.lock().unwrap().clear();
        let ten: Vec<(Value, String)> = (0..10)
            .map(|i| {
                let r = request(&format!("task-{i}"));
                let b = r.to_string();
                (r, b)
            })
            .collect();
        let res10 = ask_many_with_deadline(
            &dir,
            &ten,
            Duration::from_secs(120),
            &blocks,
            &primary,
            None,
            make_fetch(calls.clone()),
            classify_rl,
            classify_batch_test,
        )
        .await;
        let counts = calls.lock().unwrap().clone();
        assert_eq!(counts, vec![MAX_TASKS, 2], "10 items must batch as 8 + 2");
        assert_eq!(res10.len(), 10);
        for (i, r) in res10.iter().enumerate() {
            assert_eq!(r.as_ref().unwrap().answers["size"]["score"], i as i64);
        }

        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn different_primary_or_fallback_config_never_share_queue_group() {
        let (dir, blocks) = temp_dirs("group-isolation");
        let primary = JudgeEndpoint::new("vercel", "judge");
        let fallback = JudgeEndpoint::new("openrouter", "fb-judge");
        let other = request("no-fallback-task");
        let other_id = format!("{}-999", std::process::id());
        // Pre-seed a request enqueued with fallback = None.
        let other_entry = serde_json::json!({
            "group": group_of(&other, &primary, None),
            "request": other,
            "body": "x"
        });
        write_atomic(
            &dir.join(format!("{other_id}.req")),
            &other_entry.to_string(),
        )
        .unwrap();

        let sent: Arc<Mutex<Vec<Value>>> = Arc::default();
        let s = sent.clone();
        let ans = ask_inner(
            &dir,
            &request("with-fallback-task"),
            "body",
            Duration::from_secs(30),
            &blocks,
            &primary,
            Some(&fallback),
            Instant::now,
            SystemTime::now,
            |_| async {},
            false,
            move |_ep, req, _, _| {
                let s = s.clone();
                async move {
                    s.lock().unwrap().push(req);
                    Ok((serde_json::json!({"size": {"score": 1}}), Some(0.01)))
                }
            },
            classify_rl,
            classify_batch_test,
        )
        .await
        .unwrap();
        assert_eq!(ans.answers["size"]["score"], 1);
        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert!(
            sent[0].pointer("/state/tasks").is_none(),
            "leader with fallback must not batch a request queued under a different fallback config: {:?}",
            sent[0]
        );
        // Also verify distinct primary providers produce distinct groups even for identical model strings.
        let alt_primary = JudgeEndpoint::new("openrouter", "judge");
        assert_ne!(
            group_of(&other, &primary, None),
            group_of(&other, &alt_primary, None)
        );
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn channel_wide_error_fails_whole_batch_in_one_call() {
        let (dir, blocks) = temp_dirs("channel-wide");
        let primary = JudgeEndpoint::new("vercel", "judge");
        let items: Vec<(Value, String)> = (0..8)
            .map(|i| {
                let r = request(&format!("task-{i}"));
                let b = r.to_string();
                (r, b)
            })
            .collect();
        let calls = Arc::new(Mutex::new(0usize));
        let c = calls.clone();
        let outcomes = ask_many_with_deadline(
            &dir,
            &items,
            Duration::from_secs(30),
            &blocks,
            &primary,
            None,
            move |_ep, _req, _body, _deadline| {
                let c = c.clone();
                async move {
                    *c.lock().unwrap() += 1;
                    Err::<(Value, Option<f64>), anyhow::Error>(anyhow::anyhow!(
                        "jev_http_status_401"
                    ))
                }
            },
            classify_rl,
            classify_batch_test,
        )
        .await;
        assert_eq!(outcomes.len(), 8);
        assert!(outcomes.iter().all(Result::is_err));
        assert_eq!(
            *calls.lock().unwrap(),
            1,
            "channel-wide 401 must fail the entire wave of 8 in 1 judge call, not 8"
        );

        // Combined-body 400 (e.g. payload rejected only when combined): falling back
        // one-by-one recovers ALL 3 healthy tasks within batch_size + 1 (4) sends.
        let three: Vec<(Value, String)> = (0..3)
            .map(|i| {
                let r = request(&format!("req-{i}"));
                let b = r.to_string();
                (r, b)
            })
            .collect();
        let req_calls = Arc::new(Mutex::new(0usize));
        let rc = req_calls.clone();
        let res_req = ask_many_with_deadline(
            &dir,
            &three,
            Duration::from_secs(30),
            &blocks,
            &primary,
            None,
            move |_ep, req, _body, _deadline| {
                let rc = rc.clone();
                async move {
                    *rc.lock().unwrap() += 1;
                    if req
                        .pointer("/state/tasks")
                        .and_then(Value::as_object)
                        .is_some_and(|m| m.len() >= 3)
                    {
                        return Err(anyhow::anyhow!("jev_http_status_400"));
                    }
                    let mut answers = Map::new();
                    for (k, _) in req["questions"].as_object().unwrap() {
                        answers.insert(k.clone(), serde_json::json!({"score": 1}));
                    }
                    Ok((Value::Object(answers), Some(0.01)))
                }
            },
            classify_rl,
            classify_batch_test,
        )
        .await;
        assert_eq!(*req_calls.lock().unwrap(), 4);
        assert!(res_req[0].is_ok(), "first request recovered after fallback");
        assert!(
            res_req[1].is_ok(),
            "second request recovered after fallback"
        );
        assert!(res_req[2].is_ok(), "third request recovered after fallback");
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn different_group_is_not_blocked_by_429_sleep_and_short_waiter_fails_fast() {
        let (dir, blocks) = temp_dirs("per-group-lock");
        let group_a = JudgeEndpoint::new("vercel", "judge-a");
        let group_b = JudgeEndpoint::new("openrouter", "judge-b");

        let slept_notify = Arc::new(tokio::sync::Notify::new());
        let a_calls = Arc::new(Mutex::new(0usize));

        // Task A (group A, 5s budget) gets 429 Retry-After: 2s and sleeps out the block.
        let dir_a = dir.clone();
        let blocks_a = blocks.clone();
        let ep_a = group_a.clone();
        let notify_a = slept_notify.clone();
        let calls_a = a_calls.clone();
        let task_a = tokio::spawn(async move {
            ask_with_deadline(
                &dir_a,
                &request("group-a-task"),
                "body-a",
                Duration::from_secs(5),
                &blocks_a,
                &ep_a,
                None,
                move |_ep, _req, _body, _deadline| {
                    let notify = notify_a.clone();
                    let calls = calls_a.clone();
                    async move {
                        let n = {
                            let mut c = calls.lock().unwrap();
                            *c += 1;
                            *c
                        };
                        if n == 1 {
                            notify.notify_one();
                            return Err(RateLimited {
                                retry_after: Duration::from_secs(2),
                                message: "Jev HTTP 429".into(),
                            }
                            .into());
                        }
                        Ok((serde_json::json!({"size": {"score": 1}}), Some(0.01)))
                    }
                },
                classify_rl,
                classify_batch_test,
            )
            .await
        });

        slept_notify.notified().await;
        tokio::time::sleep(Duration::from_millis(80)).await;

        // 1. Different group (group B) must NOT be blocked by group A's 2s 429 sleep.
        let started_b = Instant::now();
        let ans_b = ask_with_deadline(
            &dir,
            &request("group-b-task"),
            "body-b",
            Duration::from_secs(2),
            &blocks,
            &group_b,
            None,
            |_ep, _req, _body, _deadline| async {
                Ok((serde_json::json!({"size": {"score": 2}}), Some(0.01)))
            },
            classify_rl,
            classify_batch_test,
        )
        .await
        .expect("group B must succeed immediately");
        let elapsed_b = started_b.elapsed();
        assert_eq!(ans_b.answers["size"]["score"], 2);
        assert!(
            elapsed_b < Duration::from_millis(600),
            "group B must not wait on group A's 2s 429 sleep; took {elapsed_b:?}"
        );

        // 2. Same group A with shorter budget (500ms < remaining ~1.9s block) must fail fast
        // without waiting out its deadline and without cancelling Task A.
        let started_c = Instant::now();
        let err_c = ask_with_deadline(
            &dir,
            &request("group-a-short-hook"),
            "body-c",
            Duration::from_millis(500),
            &blocks,
            &group_a,
            None,
            |_ep, _req, _body, _deadline| async {
                Ok((serde_json::json!({"size": {"score": 9}}), Some(0.01)))
            },
            classify_rl,
            classify_batch_test,
        )
        .await
        .unwrap_err();
        let elapsed_c = started_c.elapsed();
        assert!(
            err_c.to_string().contains("rate limited (429)"),
            "short waiter should fail fast on recorded 429 block, got: {err_c}"
        );
        assert!(
            elapsed_c < Duration::from_millis(400),
            "short waiter must not hang on leader lock while leader sleeps; took {elapsed_c:?}"
        );

        // Task A must still wake up after its 2s sleep and succeed.
        let ans_a = task_a
            .await
            .unwrap()
            .expect("long-budget Task A must survive short waiter and succeed after 2s block");
        assert_eq!(ans_a.answers["size"]["score"], 1);
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn batch_bisects_on_content_based_400_and_combined_parse_error() {
        let (dir, blocks) = temp_dirs("bisect-400");
        let primary = JudgeEndpoint::new("vercel", "judge");

        // 1. Content-based 400: mock rejects ANY request whose payload contains "BAD-TASK"
        // (positioned at index 2, NOT index 0!). Healthy tasks 0, 1, 3 must all succeed.
        let items: Vec<(Value, String)> = ["ok-0", "ok-1", "BAD-TASK", "ok-3"]
            .into_iter()
            .map(|t| {
                let r = request(t);
                let b = r.to_string();
                (r, b)
            })
            .collect();
        let outcomes = ask_many_with_deadline(
            &dir,
            &items,
            Duration::from_secs(30),
            &blocks,
            &primary,
            None,
            |_ep, req, _body, _deadline| async move {
                let contains_bad = req
                    .pointer("/state/task")
                    .and_then(Value::as_str)
                    .is_some_and(|s| s == "BAD-TASK")
                    || req
                        .pointer("/state/tasks")
                        .and_then(Value::as_object)
                        .is_some_and(|m| m.values().any(|v| v.as_str() == Some("BAD-TASK")));
                if contains_bad {
                    return Err(anyhow::anyhow!("jev_http_status_400"));
                }
                let mut answers = Map::new();
                for (k, _) in req["questions"].as_object().unwrap() {
                    answers.insert(k.clone(), serde_json::json!({"score": 1}));
                }
                Ok((Value::Object(answers), Some(0.01)))
            },
            classify_rl,
            classify_batch_test,
        )
        .await;
        assert_eq!(outcomes.len(), 4);
        assert!(outcomes[0].is_ok(), "ok-0 must succeed: {:?}", outcomes[0]);
        assert!(outcomes[1].is_ok(), "ok-1 must succeed: {:?}", outcomes[1]);
        assert!(
            outcomes[2]
                .as_ref()
                .err()
                .is_some_and(|e| e.to_string().contains("400")),
            "BAD-TASK must fail with 400: {:?}",
            outcomes[2]
        );
        assert!(outcomes[3].is_ok(), "ok-3 must succeed: {:?}", outcomes[3]);

        // 2. Combined-answer parse error: combined request returns malformed/unprefixed JSON
        // while solo requests succeed; bisecting must recover all 3 healthy tasks.
        let three: Vec<(Value, String)> = ["p-0", "p-1", "p-2"]
            .into_iter()
            .map(|t| {
                let r = request(t);
                let b = r.to_string();
                (r, b)
            })
            .collect();
        let parsed_outcomes = ask_many_with_deadline(
            &dir,
            &three,
            Duration::from_secs(30),
            &blocks,
            &primary,
            None,
            |_ep, req, _body, _deadline| async move {
                if req.pointer("/state/tasks").is_some() {
                    // Malformed combined answer (missing tN__ prefixes).
                    return Ok((serde_json::json!({"size": {"score": 1}}), Some(0.01)));
                }
                let mut answers = Map::new();
                for (k, _) in req["questions"].as_object().unwrap() {
                    answers.insert(k.clone(), serde_json::json!({"score": 2}));
                }
                Ok((Value::Object(answers), Some(0.01)))
            },
            classify_rl,
            classify_batch_test,
        )
        .await;
        assert_eq!(parsed_outcomes.len(), 3);
        for (i, res) in parsed_outcomes.iter().enumerate() {
            let ans = res
                .as_ref()
                .unwrap_or_else(|e| panic!("task {i} should succeed after split: {e}"));
            assert_eq!(
                ans.answers["size"]["score"], 2,
                "task {i} missing split answer: {:?}",
                ans.answers
            );
            assert_eq!(ans.answers["m"]["score"], 2);
        }

        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn long_brief_succeeds_and_oversized_req_errors_at_write_time() {
        let (dir, blocks) = temp_dirs("long-brief");
        let primary = JudgeEndpoint::new("vercel", "judge");

        // 1. 100 000-char multibyte brief (~405+ KiB serialized .req, below T3's 120k limit):
        // must NOT hang until deadline; must be read and answered immediately.
        let long_task = "ж".repeat(100_000);
        let long_req = request(&long_task);
        let long_body = crate::route::stringify_like_node(&long_req);
        let calls = Arc::new(Mutex::new(0usize));
        let c = calls.clone();
        let started = Instant::now();
        let ans = ask_with_deadline(
            &dir,
            &long_req,
            &long_body,
            Duration::from_secs(2),
            &blocks,
            &primary,
            None,
            move |_ep, _req, _body, _deadline| {
                let c = c.clone();
                async move {
                    *c.lock().unwrap() += 1;
                    Ok((
                        serde_json::json!({"size": {"score": 1}, "m": {"score": 2}}),
                        Some(0.01),
                    ))
                }
            },
            classify_rl,
            classify_batch_test,
        )
        .await
        .expect("100 000-char brief must succeed without hanging in the queue");
        assert_eq!(ans.answers["size"]["score"], 1);
        assert_eq!(*calls.lock().unwrap(), 1);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "long brief should complete immediately, took {:?}",
            started.elapsed()
        );

        // 2. Request far above the private runtime file cap (> 4 MiB) must fail explicitly
        // at write time without waiting until deadline or leaving a .req file behind.
        let huge_task = "a".repeat(5 * 1024 * 1024);
        let huge_req = request(&huge_task);
        let huge_body = crate::route::stringify_like_node(&huge_req);
        let started_huge = Instant::now();
        let err = ask_with_deadline(
            &dir,
            &huge_req,
            &huge_body,
            Duration::from_secs(3),
            &blocks,
            &primary,
            None,
            |_ep, _req, _body, _deadline| async {
                Ok((serde_json::json!({"size": {"score": 1}}), None))
            },
            classify_rl,
            classify_batch_test,
        )
        .await
        .unwrap_err();
        assert!(
            started_huge.elapsed() < Duration::from_secs(1),
            "oversized request must fail at write time, not wait for deadline: {err} ({:?})",
            started_huge.elapsed()
        );
        assert!(
            err.to_string().contains("limit") || err.to_string().contains("exceeds"),
            "expected explicit size-limit error at write time, got: {err}"
        );
        let leftover_reqs: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".req"))
            .collect();
        assert!(
            leftover_reqs.is_empty(),
            "oversized request must not leave .req files behind: {leftover_reqs:?}"
        );

        // 3. An unreadable own .req on disk must return an explicit error instead of
        // silently skipping one's own request.
        let corrupt_id = format!("{}-corrupt-own", std::process::id());
        write_atomic(&dir.join(format!("{corrupt_id}.req")), "{not-json").unwrap();
        let collect_err =
            collect(&dir, &corrupt_id, system_time_ms(SystemTime::now())).unwrap_err();
        assert!(
            collect_err.contains("own request"),
            "expected explicit own-request error, got: {collect_err}"
        );

        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn short_deadline_leader_timeout_does_not_fail_longer_waiter() {
        let (dir, blocks) = temp_dirs("leader-timeout-local");
        let primary = JudgeEndpoint::new("vercel", "judge");
        let sample = request("long-waiter");
        let group = group_of(&sample, &primary, None);

        // Hold the group lock briefly so the longer waiter enqueues its .req and enters
        // its poll sleep, guaranteeing the short-deadline leader grabs the lock first.
        create_private_dir(&dir).unwrap();
        let test_lock = open_group_lock(&dir, &group).unwrap();
        test_lock.lock().unwrap();

        let dir_w = dir.clone();
        let blocks_w = blocks.clone();
        let ep_w = primary.clone();
        let waiter = tokio::spawn(async move {
            ask_with_deadline(
                &dir_w,
                &request("long-waiter"),
                "body-w",
                Duration::from_secs(3),
                &blocks_w,
                &ep_w,
                None,
                |_ep, _req, _body, attempt_deadline| async move {
                    let needed = Duration::from_millis(400);
                    let rem = attempt_deadline.saturating_duration_since(Instant::now());
                    if rem < needed {
                        tokio::time::sleep(rem).await;
                        return Err(anyhow::anyhow!("jev_error_timeout"));
                    }
                    tokio::time::sleep(needed).await;
                    Ok((
                        serde_json::json!({"size": {"score": 2}, "m": {"score": 2}}),
                        Some(0.01),
                    ))
                },
                |e| {
                    if e.to_string().contains("timeout") {
                        FallbackTrigger::Transient
                    } else {
                        classify_rl(e)
                    }
                },
                classify_batch_test,
            )
            .await
        });

        // Wait until waiter has enqueued its .req and hit WouldBlock on the lock.
        for _ in 0..100 {
            let has_req = std::fs::read_dir(&dir)
                .unwrap()
                .flatten()
                .any(|e| e.file_name().to_string_lossy().ends_with(".req"));
            if has_req {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(test_lock);

        // Short-deadline leader (200ms) grabs lock, batches [leader, waiter], and times out.
        let saw_batch = Arc::new(Mutex::new(false));
        let sb = saw_batch.clone();
        let leader_err = ask_with_deadline(
            &dir,
            &request("short-leader"),
            "body-l",
            Duration::from_millis(200),
            &blocks,
            &primary,
            None,
            move |_ep, req, _body, attempt_deadline| {
                let sb = sb.clone();
                async move {
                    if req
                        .pointer("/state/tasks")
                        .and_then(Value::as_object)
                        .is_some_and(|m| m.len() == 2)
                    {
                        *sb.lock().unwrap() = true;
                    }
                    let rem = attempt_deadline.saturating_duration_since(Instant::now());
                    tokio::time::sleep(rem).await;
                    Err::<(Value, Option<f64>), anyhow::Error>(anyhow::anyhow!("jev_error_timeout"))
                }
            },
            |e| {
                if e.to_string().contains("timeout") {
                    FallbackTrigger::Transient
                } else {
                    classify_rl(e)
                }
            },
            classify_batch_test,
        )
        .await
        .unwrap_err();
        assert!(
            *saw_batch.lock().unwrap(),
            "short leader should have batched both requests"
        );
        assert!(
            leader_err.to_string().contains("timeout")
                || leader_err.to_string().contains("no answer"),
            "unexpected leader error: {leader_err}"
        );

        let waiter_ans = waiter
            .await
            .unwrap()
            .expect("longer-deadline waiter must not be failed by short-deadline leader's timeout");
        assert_eq!(waiter_ans.answers["size"]["score"], 2);
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn expired_peer_not_resent_after_split_and_no_orphaned_done() {
        let (dir, blocks) = temp_dirs("expired-peer");
        let primary = JudgeEndpoint::new("vercel", "judge");
        let t0 = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let t0_ms = t0.duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
        let m0 = Instant::now();

        // 1. Combined request fails with ambiguous 400 after peer's deadline passes:
        // peer must NOT be re-sent and must NOT get an orphaned .done file.
        let peer_id = format!("{}-expired-split", std::process::id());
        let peer_req = request("peer-short");
        let peer_entry = serde_json::json!({
            "group": group_of(&peer_req, &primary, None),
            "request": peer_req,
            "body": "peer",
            "deadline_ms": t0_ms + 500,
        });
        write_atomic(&dir.join(format!("{peer_id}.req")), &peer_entry.to_string()).unwrap();

        let wall = Rc::new(Cell::new(t0));
        let mono = Rc::new(Cell::new(m0));
        let m_now = mono.clone();
        let w_now = wall.clone();
        let m_call = mono.clone();
        let w_call = wall.clone();
        let sent = Arc::new(Mutex::new(Vec::<Value>::new()));
        let sent_cb = sent.clone();

        let ans = ask_inner(
            &dir,
            &request("leader-long"),
            "leader",
            Duration::from_secs(10),
            &blocks,
            &primary,
            None,
            move || m_now.get(),
            move || w_now.get(),
            |_| async {},
            false,
            move |_ep, req, _body, _deadline| {
                let sent_cb = sent_cb.clone();
                let mut log = sent_cb.lock().unwrap();
                log.push(req.clone());
                let call_no = log.len();
                drop(log);
                if call_no == 1 {
                    // Advance past peer's 500ms deadline during the combined call.
                    m_call.set(m_call.get() + Duration::from_millis(700));
                    w_call.set(w_call.get() + Duration::from_millis(700));
                }
                async move {
                    if call_no == 1 {
                        return Err(anyhow::anyhow!("jev_http_status_400"));
                    }
                    let mut a = Map::new();
                    for k in req["questions"].as_object().unwrap().keys() {
                        a.insert(k.clone(), serde_json::json!({"score": 1}));
                    }
                    Ok((Value::Object(a), Some(0.01)))
                }
            },
            classify_rl,
            classify_batch_test,
        )
        .await
        .unwrap();
        assert_eq!(ans.answers["size"]["score"], 1);
        assert_eq!(
            sent.lock().unwrap().len(),
            2,
            "expired peer must be dropped before retry (expected 2 sends: batch + leader solo)"
        );
        assert!(
            !dir.join(format!("{peer_id}.done")).exists(),
            "expired peer must not receive an orphaned .done file"
        );
        let _ = std::fs::remove_file(dir.join(format!("{peer_id}.req")));

        // 2. Combined request succeeds AFTER peer's deadline passes:
        // leader still gets its answer, but expired peer must NOT get an orphaned .done file.
        wall.set(t0);
        mono.set(m0);
        let peer2_id = format!("{}-expired-done", std::process::id());
        let peer2_req = request("peer-late");
        let peer2_entry = serde_json::json!({
            "group": group_of(&peer2_req, &primary, None),
            "request": peer2_req,
            "body": "peer2",
            "deadline_ms": t0_ms + 500,
        });
        write_atomic(
            &dir.join(format!("{peer2_id}.req")),
            &peer2_entry.to_string(),
        )
        .unwrap();

        let m_now2 = mono.clone();
        let w_now2 = wall.clone();
        let m_call2 = mono.clone();
        let w_call2 = wall.clone();
        let ans2 = ask_inner(
            &dir,
            &request("leader-long-2"),
            "leader2",
            Duration::from_secs(10),
            &blocks,
            &primary,
            None,
            move || m_now2.get(),
            move || w_now2.get(),
            |_| async {},
            false,
            move |_ep, req, _body, _deadline| {
                m_call2.set(m_call2.get() + Duration::from_millis(700));
                w_call2.set(w_call2.get() + Duration::from_millis(700));
                async move {
                    let mut a = Map::new();
                    for k in req["questions"].as_object().unwrap().keys() {
                        a.insert(k.clone(), serde_json::json!({"score": 2}));
                    }
                    Ok((Value::Object(a), Some(0.02)))
                }
            },
            classify_rl,
            classify_batch_test,
        )
        .await
        .unwrap();
        assert_eq!(ans2.answers["size"]["score"], 2);
        assert!(
            !dir.join(format!("{peer2_id}.done")).exists(),
            "late response after peer expiry must not leave an orphaned .done file"
        );
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ambiguous_400_falls_back_one_by_one_with_batch_plus_one_send_cap() {
        let (dir, blocks) = temp_dirs("ambig-400-cap");
        let primary = JudgeEndpoint::new("vercel", "judge");
        let items: Vec<(Value, String)> = (0..8)
            .map(|i| {
                let r = request(&format!("task-{i}"));
                let b = r.to_string();
                (r, b)
            })
            .collect();
        let sends = Arc::new(Mutex::new(0usize));
        let s = sends.clone();
        let outcomes = ask_many_with_deadline(
            &dir,
            &items,
            Duration::from_secs(30),
            &blocks,
            &primary,
            None,
            move |_ep, _req, _body, _deadline| {
                let s = s.clone();
                async move {
                    *s.lock().unwrap() += 1;
                    Err::<(Value, Option<f64>), anyhow::Error>(anyhow::anyhow!(
                        "jev_http_status_400"
                    ))
                }
            },
            classify_rl,
            classify_batch_test,
        )
        .await;
        assert_eq!(outcomes.len(), 8);
        assert!(outcomes.iter().all(Result::is_err));
        assert_eq!(
            *sends.lock().unwrap(),
            9,
            "ambiguous 400 on 8 tasks must fall back one-by-one capped at batch_size + 1 (9), not 15 via bisect"
        );
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }
}
