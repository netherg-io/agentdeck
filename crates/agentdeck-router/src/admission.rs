//! Shared launch reservations bridge the pre-tool hook and T3's run projection.

use std::collections::HashMap;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use rusqlite::{Connection, OpenFlags, Transaction, TransactionBehavior, params};

use crate::load::{AgentLoad, read_active_agents};

// A failed tool dispatch has no completion callback. Its reservation expires;
// accepted launches hand over to T3 as soon as their subagent row is visible.
const HANDOFF_SECS: f64 = 120.0;

fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

fn connect(state: &Path) -> anyhow::Result<Connection> {
    let dir = state.join("model-route");
    std::fs::create_dir_all(&dir)?;
    let file = dir.join("admissions.sqlite");
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&file)?;
    let db = Connection::open(file)?;
    db.busy_timeout(std::time::Duration::from_secs(2))?;
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS pending (
        id INTEGER PRIMARY KEY, provider TEXT NOT NULL, task TEXT NOT NULL, created REAL NOT NULL);
        CREATE TABLE IF NOT EXISTS seen (id TEXT PRIMARY KEY, observed REAL NOT NULL);",
    )?;
    Ok(db)
}

fn refresh(tx: &Transaction<'_>, home: &Path, time: f64) -> anyhow::Result<AgentLoad> {
    tx.execute(
        "DELETE FROM pending WHERE created < ?1",
        [time - HANDOFF_SECS],
    )?;
    tx.execute(
        "DELETE FROM seen WHERE observed < ?1",
        [time - HANDOFF_SECS * 2.0],
    )?;
    let file = home.join(".t3/userdata/statev2.sqlite");
    let t3 = Connection::open_with_flags(&file, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    t3.busy_timeout(std::time::Duration::from_secs(1))?;
    let load = read_active_agents(&file);
    anyhow::ensure!(
        load.known,
        "live T3 agent metadata unavailable; cannot check instance limit"
    );
    let live_ids = serde_json::to_string(&load.agents.iter().map(|a| &a.id).collect::<Vec<_>>())?;
    let mut stmt = t3.prepare(
        "SELECT n.subagent_id, n.provider_instance_id, json_extract(n.payload_json, '$.prompt'),
        CASE WHEN n.status IN ('completed', 'failed', 'cancelled', 'interrupted')
          OR n.provider_thread_id IN (SELECT value FROM json_each(?2))
          OR EXISTS (SELECT 1 FROM orchestration_v2_projection_runs r
            WHERE r.thread_id = n.child_thread_id AND (r.run_id IN (SELECT value FROM json_each(?2))
              OR r.provider_thread_id IN (SELECT value FROM json_each(?2))))
        THEN 1 ELSE 0 END
        FROM orchestration_v2_projection_subagents n
        WHERE (julianday(n.started_at) - 2440587.5) * 86400 >= ?1
        ORDER BY n.started_at, n.subagent_id",
    )?;
    let launches = stmt.query_map(params![time - HANDOFF_SECS, live_ids], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, bool>(3)?,
        ))
    })?;
    for launch in launches {
        let (id, provider, prompt, projected) = launch?;
        // A subagent record can precede its run. Keep its claim until the
        // load snapshot counts that exact child, or the child has finished.
        let pending: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM pending WHERE provider = ?1 AND instr(?2, task) > 0)",
            params![provider, prompt],
            |r| r.get(0),
        )?;
        if pending && !projected {
            continue;
        }
        let fresh = tx.execute(
            "INSERT OR IGNORE INTO seen VALUES (?1, ?2)",
            params![id, time],
        )?;
        if fresh > 0 {
            // T3 prefixes the task with its role instruction. Match the complete
            // original brief, and consume each launch once even for identical tasks.
            tx.execute(
                "DELETE FROM pending WHERE id = (SELECT id FROM pending
                WHERE provider = ?1 AND instr(?2, task) > 0 ORDER BY id LIMIT 1)",
                params![provider, prompt],
            )?;
        }
    }
    Ok(load)
}

fn counts(tx: &Transaction<'_>) -> anyhow::Result<HashMap<String, usize>> {
    let mut stmt = tx.prepare("SELECT provider, count(*) FROM pending GROUP BY provider")?;
    Ok(stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?)
}

pub fn snapshot(home: &Path, state: &Path) -> anyhow::Result<(AgentLoad, HashMap<String, usize>)> {
    let mut db = connect(state)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let load = refresh(&tx, home, now())?;
    let pending = counts(&tx)?;
    tx.commit()?;
    Ok((load, pending))
}

pub fn reason(load: &AgentLoad, pending: usize, provider: &str, limit: u32) -> Option<String> {
    if !load.known {
        return Some("live T3 agent metadata unavailable; cannot check instance limit".into());
    }
    let active = load
        .agents
        .iter()
        .filter(|a| a.provider == provider)
        .count();
    (active + pending >= limit as usize).then(|| format!(
        "instance parallel limit reached for {provider}: {active} active + {pending} launching / {limit}"
    ))
}

pub fn reserve(
    home: &Path,
    state: &Path,
    provider: &str,
    limit: u32,
    task: &str,
) -> anyhow::Result<()> {
    let mut db = connect(state)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let time = now();
    let load = refresh(&tx, home, time)?;
    let pending = counts(&tx)?.get(provider).copied().unwrap_or_default();
    if let Some(reason) = reason(&load, pending, provider, limit) {
        anyhow::bail!(reason);
    }
    tx.execute(
        "INSERT INTO pending (provider, task, created) VALUES (?1, ?2, ?3)",
        params![provider, task, time],
    )?;
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    struct Fixture(std::path::PathBuf);
    impl Fixture {
        fn new() -> Self {
            // Parallel tests can share pid and timestamp; the counter keeps dirs apart.
            static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "agentdeck-admission-{}-{}-{}",
                std::process::id(),
                now(),
                N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            ));
            std::fs::create_dir_all(dir.join(".t3/userdata")).unwrap();
            let fx = Self(dir);
            fx.t3().execute_batch("CREATE TABLE orchestration_v2_projection_subagents (
                subagent_id TEXT PRIMARY KEY, provider_instance_id TEXT, payload_json TEXT, started_at TEXT,
                origin TEXT, status TEXT, provider_thread_id TEXT, run_id TEXT, child_thread_id TEXT);
                CREATE TABLE orchestration_v2_projection_provider_threads (
                provider_thread_id TEXT, provider_instance_id TEXT, provider_session_id TEXT, status TEXT);
                CREATE TABLE orchestration_v2_projection_provider_sessions (provider_session_id TEXT, model TEXT, status TEXT);
                CREATE TABLE orchestration_v2_projection_provider_turns (provider_thread_id TEXT, run_attempt_id TEXT, status TEXT);
                CREATE TABLE orchestration_v2_projection_runs (run_id TEXT, provider_instance_id TEXT, provider_thread_id TEXT, payload_json TEXT, status TEXT, thread_id TEXT);
                CREATE TABLE orchestration_v2_projection_run_attempts (attempt_id TEXT, run_id TEXT);").unwrap();
            fx
        }
        fn t3(&self) -> Connection {
            Connection::open(self.0.join(".t3/userdata/statev2.sqlite")).unwrap()
        }
        fn launch(&self, id: &str, provider: &str, task: &str, status: &str) {
            let db = self.t3();
            let payload = serde_json::json!({"prompt": format!("Act as implementation agent.\n\n{task}"), "modelSelection": {"model": "different-model"}}).to_string();
            db.execute(
                "INSERT INTO orchestration_v2_projection_subagents
                VALUES (?1, ?2, ?3, strftime('%Y-%m-%dT%H:%M:%fZ','now'), 'app', ?4, NULL, ?1, ?1)",
                params![id, provider, payload, status],
            )
            .unwrap();
            db.execute(
                "INSERT INTO orchestration_v2_projection_runs VALUES (?1, ?2, NULL, ?3, ?4, ?1)",
                params![id, provider, payload, status],
            )
            .unwrap();
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn concurrent_coordinators_share_instance_slots_and_failed_dispatch_expires() {
        let fx = Fixture::new();
        let barrier = Arc::new(Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let home = fx.0.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    reserve(&home, &home, "codex", 3, "same task").is_ok()
                })
            })
            .collect();
        assert_eq!(
            handles
                .into_iter()
                .filter_map(|h| h.join().ok())
                .filter(|ok| *ok)
                .count(),
            3
        );
        assert!(reserve(&fx.0, &fx.0, "codex_second", 1, "same task").is_ok());
        let (load, pending) = snapshot(&fx.0, &fx.0).unwrap();
        assert_eq!(pending["codex"], 3);
        assert!(
            reason(&load, 3, "codex", 3)
                .unwrap()
                .contains("parallel limit")
        );
        connect(&fx.0)
            .unwrap()
            .execute("UPDATE pending SET created = 0", [])
            .unwrap();
        assert!(reserve(&fx.0, &fx.0, "codex", 3, "retry").is_ok());
    }

    #[test]
    fn handoff_counts_queued_waiting_and_all_models_once_then_frees_on_completion() {
        let fx = Fixture::new();
        // A previous identical task must not consume the new reservation.
        fx.launch("old", "codex", "same", "completed");
        reserve(&fx.0, &fx.0, "codex", 2, "same").unwrap();
        reserve(&fx.0, &fx.0, "codex", 2, "same").unwrap();
        fx.launch("a", "codex", "same", "queued");
        let (load, pending) = snapshot(&fx.0, &fx.0).unwrap();
        assert_eq!(load.agents.len(), 1);
        assert_eq!(pending["codex"], 1);
        assert!(reserve(&fx.0, &fx.0, "codex", 2, "other model").is_err());
        fx.launch("b", "codex", "same", "waiting");
        let (load, pending) = snapshot(&fx.0, &fx.0).unwrap();
        assert_eq!(load.agents.len(), 2);
        assert!(!pending.contains_key("codex"));
        assert!(reason(&load, 0, "codex", 2).is_some());
        assert!(reason(&load, 0, "codex_second", 1).is_none());
        fx.t3()
            .execute(
                "UPDATE orchestration_v2_projection_runs SET status = 'completed'",
                [],
            )
            .unwrap();
        reserve(&fx.0, &fx.0, "codex", 2, "after completion").unwrap();
    }

    #[test]
    fn reservation_stays_until_its_run_is_projected() {
        let fx = Fixture::new();
        reserve(&fx.0, &fx.0, "codex", 1, "task").unwrap();
        fx.launch("child", "codex", "task", "queued");
        fx.t3()
            .execute("DELETE FROM orchestration_v2_projection_runs", [])
            .unwrap();
        let (_, pending) = snapshot(&fx.0, &fx.0).unwrap();
        assert_eq!(pending.get("codex"), Some(&1));
        assert!(reserve(&fx.0, &fx.0, "codex", 1, "racing task").is_err());
        fx.t3().execute_batch("INSERT INTO orchestration_v2_projection_runs VALUES ('child', 'codex', NULL, '{}', 'queued', 'child')").unwrap();
        let (load, pending) = snapshot(&fx.0, &fx.0).unwrap();
        assert!(!pending.contains_key("codex"));
        assert_eq!(load.agents.len(), 1);
        fx.t3()
            .execute(
                "UPDATE orchestration_v2_projection_runs SET status = 'running'",
                [],
            )
            .unwrap();
        let (load, _) = snapshot(&fx.0, &fx.0).unwrap();
        assert_eq!(load.agents.len(), 1);
        assert!(reserve(&fx.0, &fx.0, "codex", 1, "before first turn").is_err());
    }

    #[test]
    fn unreadable_t3_fails_closed() {
        let fx = Fixture::new();
        fx.t3()
            .execute_batch("DROP TABLE orchestration_v2_projection_runs")
            .unwrap();
        assert!(reserve(&fx.0, &fx.0, "codex", 1, "task").is_err());
    }

    #[test]
    fn ordinary_threads_occupy_slots_and_native_child_threads_are_counted_once() {
        let fx = Fixture::new();
        let db = fx.t3();
        db.execute_batch("INSERT INTO orchestration_v2_projection_provider_sessions VALUES ('s', 'gpt-6.1-sol', 'ready');
            INSERT INTO orchestration_v2_projection_provider_threads VALUES ('parent', 'codex', 's', 'active');
            INSERT INTO orchestration_v2_projection_provider_turns VALUES ('parent', NULL, 'running');
            INSERT INTO orchestration_v2_projection_runs VALUES ('r', 'codex', 'parent', '{}', 'running', 'root');").unwrap();
        let (load, pending) = snapshot(&fx.0, &fx.0).unwrap();
        assert_eq!(load.agents.len(), 1);
        assert!(reason(&load, 0, "codex", 1).is_some());
        assert!(pending.is_empty());
        assert!(reserve(&fx.0, &fx.0, "codex", 1, "child").is_err());
        db.execute_batch("INSERT INTO orchestration_v2_projection_provider_threads VALUES ('child', 'codex', 's', 'active');
            INSERT INTO orchestration_v2_projection_provider_turns VALUES ('child', NULL, 'running');
            INSERT INTO orchestration_v2_projection_subagents VALUES ('n', 'codex', '{\"model\":\"gpt-6-astra\"}', NULL, 'provider_native', 'running', 'child', 'r', NULL);").unwrap();
        let (load, _) = snapshot(&fx.0, &fx.0).unwrap();
        assert_eq!(load.agents.len(), 2);
        assert_eq!(load.agents.iter().filter(|a| a.id == "child").count(), 1);
        db.execute("UPDATE orchestration_v2_projection_provider_turns SET status = 'completed' WHERE provider_thread_id = 'parent'", []).unwrap();
        db.execute(
            "UPDATE orchestration_v2_projection_runs SET status = 'completed'",
            [],
        )
        .unwrap();
        let (load, _) = snapshot(&fx.0, &fx.0).unwrap();
        assert_eq!(load.agents.len(), 1);
        assert!(reason(&load, 0, "codex", 2).is_none());
    }
}
