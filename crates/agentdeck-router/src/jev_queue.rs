//! Concurrent `agentdeck route` processes share Jev calls. The Vercel gateway
//! allows 5 decision requests a minute per model, so a wave of routes must
//! queue, back off on 429 and ask together. Each process drops its request
//! into a spool directory; whoever holds the lock sends everything pending for
//! the same judge and pool as one decision, then writes each answer back.
//! A request alone in the queue is sent unchanged.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

/// Total wait for one answer, queue and 429 back-off included: the hook kills
/// `route` after 55 s.
pub const BUDGET: Duration = Duration::from_secs(50);
/// Tasks per batched decision; 7 real briefs measured about 11k input tokens.
const MAX_TASKS: usize = 8;
const POLL: Duration = Duration::from_millis(150);
const STALE: Duration = Duration::from_secs(600);

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

type Answer = (Value, Option<f64>);

/// Asks Jev through the shared queue. `fetch` sends one decision request
/// (request value, serialized body) and is only called while this process
/// holds the queue lock.
pub async fn ask<F, Fut>(
    spool: &Path,
    request: &Value,
    body: &str,
    mut fetch: F,
) -> anyhow::Result<Answer>
where
    F: FnMut(Value, String) -> Fut,
    Fut: Future<Output = anyhow::Result<Answer>>,
{
    let deadline = Instant::now() + BUDGET;
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
        if Instant::now() >= deadline {
            let _ = std::fs::remove_file(&req_path);
            return take_done(&done_path).unwrap_or_else(|| {
                Err(anyhow::anyhow!(
                    "Jev queue: no answer within {}s (decision rate limit)",
                    BUDGET.as_secs()
                ))
            });
        }
        match lock.try_lock() {
            Ok(()) => {
                let led = lead(spool, &id, deadline, &mut fetch).await;
                let _ = lock.unlock();
                led?;
            }
            Err(std::fs::TryLockError::WouldBlock) => tokio::time::sleep(POLL).await,
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

/// Sends this process's request together with every compatible pending one,
/// sleeping out 429s (newcomers join the retried batch) until `deadline`.
async fn lead<F, Fut>(
    spool: &Path,
    own: &str,
    deadline: Instant,
    fetch: &mut F,
) -> anyhow::Result<()>
where
    F: FnMut(Value, String) -> Fut,
    Fut: Future<Output = anyhow::Result<Answer>>,
{
    loop {
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
        match fetch(request, body).await {
            Ok((answers, cost)) => {
                let n = batch.len();
                for (i, p) in batch.iter().enumerate() {
                    let a = if n == 1 {
                        answers.clone()
                    } else {
                        split(&answers, i)
                    };
                    let cost = cost.map(|c| c / n as f64);
                    finish(spool, p, &serde_json::json!({"answers": a, "cost": cost}));
                }
                return Ok(());
            }
            Err(e) => {
                if let Some(rl) = e.downcast_ref::<RateLimited>() {
                    let jitter = Duration::from_millis(
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map(|d| u64::from(d.subsec_millis()))
                            .unwrap_or(0),
                    );
                    let wait = rl.retry_after + jitter;
                    if Instant::now() + wait < deadline {
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                }
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
        } else if name.ends_with(".done")
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

fn take_done(path: &Path) -> Option<anyhow::Result<Answer>> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let _ = std::fs::remove_file(path);
    Some(match v["error"].as_str() {
        Some(e) => Err(anyhow::anyhow!("{e}")),
        None => Ok((v["answers"].clone(), v["cost"].as_f64())),
    })
}

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

/// Briefs stay private: files are 0600 and appear only complete.
fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = PathBuf::from(format!("{}.tmp", path.display()));
    let mut f = std::fs::File::options()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(text.as_bytes())?;
    std::fs::rename(tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
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

    fn spool() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "jev-queue-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
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

    /// A lone request goes out unchanged; a 429 is waited out and requests
    /// queued meanwhile are answered by the same retried call.
    #[tokio::test(flavor = "current_thread")]
    async fn rate_limit_waits_and_batches_newcomers() {
        let dir = spool();
        let sent: Arc<Mutex<Vec<Value>>> = Arc::default();
        // A second process's request is already queued for the same pool.
        let other = request("queued");
        let other_entry =
            serde_json::json!({"group": group_of(&other), "request": other, "body": "x"});
        write_atomic(
            &dir.join(format!("{}-1.req", std::process::id())),
            &other_entry.to_string(),
        )
        .unwrap();
        let log = sent.clone();
        let answer = ask(&dir, &request("mine"), "body", move |r, _| {
            let log = log.clone();
            async move {
                let mut log = log.lock().unwrap();
                log.push(r.clone());
                if log.len() == 1 {
                    return Err(RateLimited {
                        retry_after: Duration::ZERO,
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
        })
        .await
        .unwrap();
        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 2, "one 429, then one retried batch");
        assert_eq!(sent[1]["state"]["tasks"]["t0"], "mine");
        assert_eq!(sent[1]["state"]["tasks"]["t1"], "queued");
        assert_eq!(
            answer.0,
            serde_json::json!({"size": {"score": 1}, "m": {"score": 1}})
        );
        assert_eq!(answer.1, Some(0.01));
        let other_done = take_done(&dir.join(format!("{}-1.done", std::process::id())));
        assert!(other_done.unwrap().is_ok(), "queued request answered too");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn lone_request_is_sent_unchanged() {
        let dir = spool();
        let r = request("solo");
        let expected = r.clone();
        let got = ask(&dir, &r, "body", move |req, body| {
            let expected = expected.clone();
            async move {
                assert_eq!(req, expected);
                assert_eq!(body, "body");
                Ok((serde_json::json!({"size": 1}), None))
            }
        })
        .await
        .unwrap();
        assert_eq!(got.0, serde_json::json!({"size": 1}));
        let _ = std::fs::remove_dir_all(dir);
    }
}
