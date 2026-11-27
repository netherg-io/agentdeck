//! Concurrent `agentdeck route` processes share Jev calls. Each process drops
//! its request into a spool directory; whoever holds the lock sends everything
//! pending for the same judge and pool as one decision through the shared
//! primary/fallback block dispatcher, then writes each answer back. A request
//! alone in the queue is sent unchanged.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agentdeck_core::judge::{
    BlockStore, DispatchStep, FallbackTrigger, JudgeChannel, JudgeEndpoint, create_private_dir,
    dispatch_once, format_rate_limit_exceeded, write_atomic,
};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

/// Total wait for one answer on the hook path, queue and 429 back-off included:
/// the hook kills `route` after 55 s.
pub const BUDGET: Duration = Duration::from_secs(50);
/// Tasks per batched decision; 7 real briefs measured about 11k input tokens.
const MAX_TASKS: usize = 8;
const POLL: Duration = Duration::from_millis(150);
const STALE: Duration = Duration::from_secs(600);
const MAX_WAIT_RETRIES: usize = 8;

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
pub async fn ask<F, Fut, C>(
    spool: &Path,
    request: &Value,
    body: &str,
    blocks: &BlockStore,
    primary: &JudgeEndpoint,
    fallback: Option<&JudgeEndpoint>,
    fetch: F,
    classify: C,
) -> anyhow::Result<QueueAnswer>
where
    F: FnMut(JudgeEndpoint, Value, String, Duration) -> Fut,
    Fut: Future<Output = anyhow::Result<(Value, Option<f64>)>>,
    C: Fn(&anyhow::Error) -> FallbackTrigger,
{
    ask_with_deadline(
        spool, request, body, BUDGET, blocks, primary, fallback, fetch, classify,
    )
    .await
}

/// Asks Jev through the shared queue with an explicit `budget` deadline (so
/// callers such as a future `route_batch` can pass e.g. 120 s).
#[allow(clippy::too_many_arguments)]
pub async fn ask_with_deadline<F, Fut, C>(
    spool: &Path,
    request: &Value,
    body: &str,
    budget: Duration,
    blocks: &BlockStore,
    primary: &JudgeEndpoint,
    fallback: Option<&JudgeEndpoint>,
    fetch: F,
    classify: C,
) -> anyhow::Result<QueueAnswer>
where
    F: FnMut(JudgeEndpoint, Value, String, Duration) -> Fut,
    Fut: Future<Output = anyhow::Result<(Value, Option<f64>)>>,
    C: Fn(&anyhow::Error) -> FallbackTrigger,
{
    ask_inner(
        spool,
        request,
        body,
        budget,
        blocks,
        primary,
        fallback,
        SystemTime::now,
        tokio::time::sleep,
        true,
        fetch,
        classify,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn ask_inner<Now, Sleep, SleepFut, F, Fut, C>(
    spool: &Path,
    request: &Value,
    body: &str,
    budget: Duration,
    blocks: &BlockStore,
    primary: &JudgeEndpoint,
    fallback: Option<&JudgeEndpoint>,
    mut now: Now,
    mut sleep: Sleep,
    add_jitter: bool,
    mut fetch: F,
    classify: C,
) -> anyhow::Result<QueueAnswer>
where
    Now: FnMut() -> SystemTime,
    Sleep: FnMut(Duration) -> SleepFut,
    SleepFut: Future<Output = ()>,
    F: FnMut(JudgeEndpoint, Value, String, Duration) -> Fut,
    Fut: Future<Output = anyhow::Result<(Value, Option<f64>)>>,
    C: Fn(&anyhow::Error) -> FallbackTrigger,
{
    let started = now();
    create_private_dir(spool)?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let id = format!("{}-{nanos}", std::process::id());
    let req_path = spool.join(format!("{id}.req"));
    let done_path = spool.join(format!("{id}.done"));
    let entry = serde_json::json!({"group": group_of(request), "request": request, "body": body});
    write_atomic(&req_path, &entry.to_string())?;
    let lock = std::fs::File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(spool.join("lock"))?;
    loop {
        if let Some(r) = take_done(&done_path) {
            return r;
        }
        let elapsed = now().duration_since(started).unwrap_or(Duration::ZERO);
        if elapsed >= budget {
            let _ = std::fs::remove_file(&req_path);
            return take_done(&done_path).unwrap_or_else(|| {
                Err(anyhow::anyhow!(
                    "Jev queue: no answer within {}s (decision rate limit)",
                    budget.as_secs()
                ))
            });
        }
        match lock.try_lock() {
            Ok(()) => {
                let led = lead(
                    spool, &id, started, budget, blocks, primary, fallback, &mut now, &mut sleep,
                    add_jitter, &mut fetch, &classify,
                )
                .await;
                let _ = lock.unlock();
                led?;
                if let Some(r) = take_done(&done_path) {
                    return r;
                }
                // Not answered (own request unreadable): don't spin on the lock.
                sleep(POLL).await;
            }
            Err(std::fs::TryLockError::WouldBlock) => sleep(POLL).await,
            Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
        }
    }
}

struct Pending {
    path: PathBuf,
    id: String,
    request: Value,
    body: String,
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

/// Sends this process's request together with every compatible pending one
/// through the shared block/fallback dispatcher. When waiting out a 429 block
/// (no fallback, block ends before deadline), loops back so newcomers join the
/// retried batch.
#[allow(clippy::too_many_arguments)]
async fn lead<Now, Sleep, SleepFut, F, Fut, C>(
    spool: &Path,
    own: &str,
    started: SystemTime,
    budget: Duration,
    blocks: &BlockStore,
    primary: &JudgeEndpoint,
    fallback: Option<&JudgeEndpoint>,
    now: &mut Now,
    sleep: &mut Sleep,
    add_jitter: bool,
    fetch: &mut F,
    classify: &C,
) -> anyhow::Result<()>
where
    Now: FnMut() -> SystemTime,
    Sleep: FnMut(Duration) -> SleepFut,
    SleepFut: Future<Output = ()>,
    F: FnMut(JudgeEndpoint, Value, String, Duration) -> Fut,
    Fut: Future<Output = anyhow::Result<(Value, Option<f64>)>>,
    C: Fn(&anyhow::Error) -> FallbackTrigger,
{
    for attempt in 0..=MAX_WAIT_RETRIES {
        let batch = collect(spool, own);
        if batch.is_empty() || batch[0].id != own {
            return Ok(());
        }
        let (request, body) = if batch.len() == 1 {
            (batch[0].request.clone(), batch[0].body.clone())
        } else {
            let r = combine(&batch);
            let b = crate::route::stringify_like_node(&r);
            (r, b)
        };
        let cur = now();
        let elapsed = cur.duration_since(started).unwrap_or(Duration::ZERO);
        let remaining = budget.saturating_sub(elapsed);
        let step = dispatch_once(
            blocks,
            primary,
            fallback,
            cur,
            remaining,
            |ep| fetch(ep.clone(), request.clone(), body.clone(), remaining),
            classify,
            |ep, ra, rem| anyhow::anyhow!("{}", format_rate_limit_exceeded(ep, ra, rem)),
        )
        .await;
        match step {
            DispatchStep::Done((answers, cost), channel) => {
                let n = batch.len();
                for (i, p) in batch.iter().enumerate() {
                    let a = if n == 1 {
                        answers.clone()
                    } else {
                        split(&answers, i)
                    };
                    let cost = cost.map(|c| c / n as f64);
                    finish(
                        spool,
                        p,
                        &serde_json::json!({
                            "answers": a,
                            "cost": cost,
                            "channel": channel.as_str(),
                        }),
                    );
                }
                return Ok(());
            }
            DispatchStep::Wait(wait) => {
                if attempt == MAX_WAIT_RETRIES {
                    finish(
                        spool,
                        &batch[0],
                        &serde_json::json!({
                            "error": format_rate_limit_exceeded(primary, wait, remaining),
                        }),
                    );
                    return Ok(());
                }
                sleep(with_bounded_jitter(wait, remaining, add_jitter)).await;
            }
            DispatchStep::Fail(e) => {
                // Only this request fails: a combined body may be what the
                // judge rejected, and others still have their own budget.
                finish(
                    spool,
                    &batch[0],
                    &serde_json::json!({"error": e.to_string()}),
                );
                return Ok(());
            }
        }
    }
    Ok(())
}

/// Own request first, then the oldest compatible ones. Requests of exited
/// processes and stale answers are deleted.
fn collect(spool: &Path, own: &str) -> Vec<Pending> {
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
            let Some(v) = std::fs::read_to_string(&path)
                .ok()
                .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            else {
                continue;
            };
            let group = v["group"].as_str().unwrap_or_default().to_string();
            let p = Pending {
                path,
                id: id.to_string(),
                request: v["request"].clone(),
                body: v["body"].as_str().unwrap_or_default().to_string(),
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
        return Vec::new();
    };
    all.retain(|(_, _, g)| *g == group);
    all.sort_by_key(|(t, p, _)| (p.id != own, *t));
    all.into_iter().take(MAX_TASKS).map(|(_, p, _)| p).collect()
}

/// Requests can share a decision when judge model and candidate profiles match.
fn group_of(request: &Value) -> String {
    let key = serde_json::json!([request["model"], request["state"]["models"]]);
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

fn take_done(path: &Path) -> Option<anyhow::Result<QueueAnswer>> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
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
        let other = request("queued");
        let other_entry =
            serde_json::json!({"group": group_of(&other), "request": other, "body": "x"});
        write_atomic(
            &dir.join(format!("{}-1.req", std::process::id())),
            &other_entry.to_string(),
        )
        .unwrap();
        let primary = JudgeEndpoint::new("vercel", "judge");
        let t0 = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let clock = Rc::new(Cell::new(t0));
        let c_now = clock.clone();
        let c_sleep = clock.clone();
        let log = sent.clone();
        let answer = ask_inner(
            &dir,
            &request("mine"),
            "body",
            Duration::from_secs(120),
            &blocks,
            &primary,
            None,
            move || c_now.get(),
            move |d| {
                c_sleep.set(c_sleep.get() + d);
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
        let other_done = take_done(&dir.join(format!("{}-1.done", std::process::id())));
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
        let clock = Rc::new(Cell::new(t0));

        // Without fallback, 60s > 50s BUDGET fails fast with clear error and no sleep.
        let c1 = clock.clone();
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
            move || c1.get(),
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
        )
        .await
        .unwrap_err();
        assert!(!slept.get());
        assert!(err.to_string().contains("60s"), "{err}");
        assert!(err.to_string().contains("50s"), "{err}");

        // With fallback configured, subsequent request skips blocked primary immediately.
        let c2 = clock.clone();
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
            move || c2.get(),
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
        )
        .await
        .unwrap();
        assert_eq!(got.answers, serde_json::json!({"size": 1}));
        assert_eq!(got.channel, JudgeChannel::Primary);
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }
}
