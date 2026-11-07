//! Port of `load.mjs`: live T3 agent metadata from the state SQLite database.

use rusqlite::OpenFlags;

use crate::policy::ActiveAgent;

const ACTIVE_AGENTS_SQL: &str = "
      WITH active_turns AS (
        SELECT DISTINCT p.provider_thread_id AS id, p.provider_instance_id AS provider,
          COALESCE(json_extract(n.payload_json, '$.model'), s.model) AS model
        FROM orchestration_v2_projection_provider_turns v
        JOIN orchestration_v2_projection_provider_threads p USING (provider_thread_id)
        LEFT JOIN orchestration_v2_projection_provider_sessions s USING (provider_session_id)
        LEFT JOIN orchestration_v2_projection_subagents n
          ON n.provider_thread_id = p.provider_thread_id AND n.origin = 'provider_native' AND n.status = 'running'
        WHERE v.status = 'running' AND p.status = 'active' AND (s.status IS NULL OR s.status = 'ready')
      )
      SELECT id, provider, model FROM active_turns
      UNION ALL
      -- Claude/agy native children have no provider thread or turn of their own. The model stays
      -- NULL unless the child payload names it (agent types may override the parent's model), so
      -- callers reserve the maximum burn; a stopped parent session marks a leftover 'running' row.
      SELECT n.subagent_id AS id,
        COALESCE(n.provider_instance_id, json_extract(n.payload_json, '$.providerInstanceId')) AS provider,
        COALESCE(json_extract(n.payload_json, '$.model'), json_extract(n.payload_json, '$.modelSelection.model')) AS model
      FROM orchestration_v2_projection_subagents n
      JOIN orchestration_v2_projection_runs r USING (run_id)
      LEFT JOIN orchestration_v2_projection_provider_threads p ON p.provider_thread_id = r.provider_thread_id
      LEFT JOIN orchestration_v2_projection_provider_sessions s ON s.provider_session_id = p.provider_session_id
      WHERE n.origin = 'provider_native' AND n.status = 'running' AND n.provider_thread_id IS NULL
        AND (s.status IS NULL OR s.status = 'ready')
      UNION ALL
      -- A held queued run (queueHeld) waits for a manual release and may never start: no slot.
      SELECT r.run_id AS id, r.provider_instance_id AS provider,
        CASE WHEN r.status = 'waiting'
          THEN COALESCE(s.model, json_extract(r.payload_json, '$.modelSelection.model'))
          ELSE COALESCE(json_extract(r.payload_json, '$.modelSelection.model'), s.model) END AS model
      FROM orchestration_v2_projection_runs r
      LEFT JOIN orchestration_v2_projection_provider_threads p USING (provider_thread_id)
      LEFT JOIN orchestration_v2_projection_provider_sessions s USING (provider_session_id)
      WHERE r.status IN ('queued', 'preparing', 'starting', 'waiting', 'running')
        AND NOT (r.status = 'queued' AND json_extract(r.payload_json, '$.queueHeld') IS 1)
        AND NOT (r.status = 'running' AND EXISTS (SELECT 1 FROM active_turns live WHERE live.id = r.provider_thread_id))
        AND NOT EXISTS (
          SELECT 1 FROM orchestration_v2_projection_provider_turns v
          JOIN orchestration_v2_projection_run_attempts a ON a.attempt_id = v.run_attempt_id
          JOIN active_turns live ON live.id = v.provider_thread_id
          WHERE a.run_id = r.run_id AND v.status = 'running'
        )
    ";

pub struct AgentLoad {
    pub known: bool,
    pub agents: Vec<ActiveAgent>,
    pub note: Option<String>,
}

pub fn read_active_agents(file: &std::path::Path) -> AgentLoad {
    let unknown = || AgentLoad {
        known: false,
        agents: vec![],
        note: Some("live T3 agent metadata unavailable".into()),
    };
    let Ok(db) = rusqlite::Connection::open_with_flags(file, OpenFlags::SQLITE_OPEN_READ_ONLY)
    else {
        return unknown();
    };
    if db.execute_batch("PRAGMA busy_timeout=1000").is_err() {
        return unknown();
    }
    let agents = (|| {
        let mut stmt = db.prepare(ACTIVE_AGENTS_SQL)?;
        let rows = stmt.query_map([], |row| {
            Ok(ActiveAgent {
                id: row.get::<_, String>(0)?,
                provider: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                model: row.get::<_, Option<String>>(2)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
    })();
    match agents {
        Ok(agents) => AgentLoad {
            known: true,
            agents,
            note: None,
        },
        Err(_) => unknown(),
    }
}

/// Hook helper (part 2 uses it): whether a native session/thread id belongs to
/// an app-owned delegated child.
pub fn is_delegated_child(
    native_id: Option<&str>,
    thread_id: Option<&str>,
    file: &std::path::Path,
) -> bool {
    if native_id.is_none() && thread_id.is_none() {
        return false;
    }
    let Ok(db) = rusqlite::Connection::open_with_flags(file, OpenFlags::SQLITE_OPEN_READ_ONLY)
    else {
        return false;
    };
    if db.execute_batch("PRAGMA busy_timeout=1000").is_err() {
        return false;
    }
    db.query_row(
        "SELECT 1 FROM orchestration_v2_projection_subagents
      WHERE child_thread_id = ?1 OR child_thread_id IN (
        SELECT thread_id FROM orchestration_v2_projection_provider_threads
        WHERE json_extract(payload_json, '$.nativeThreadRef.nativeId') = ?2)
      LIMIT 1",
        rusqlite::params![thread_id, native_id],
        |_| Ok(()),
    )
    .is_ok()
}
