import { DatabaseSync } from 'node:sqlite';
import os from 'node:os';
import path from 'node:path';

export function readActiveAgents(file = path.join(os.homedir(), '.t3/userdata/statev2.sqlite')) {
  let db;
  try {
    db = new DatabaseSync(file, { readOnly: true });
    db.exec('PRAGMA busy_timeout=1000');
    const agents = db.prepare(`
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
      SELECT r.run_id AS id, r.provider_instance_id AS provider,
        CASE WHEN r.status = 'waiting'
          THEN COALESCE(s.model, json_extract(r.payload_json, '$.modelSelection.model'))
          ELSE COALESCE(json_extract(r.payload_json, '$.modelSelection.model'), s.model) END AS model
      FROM orchestration_v2_projection_runs r
      LEFT JOIN orchestration_v2_projection_provider_threads p USING (provider_thread_id)
      LEFT JOIN orchestration_v2_projection_provider_sessions s USING (provider_session_id)
      WHERE r.status IN ('queued', 'preparing', 'starting', 'waiting', 'running')
        AND NOT (r.status = 'running' AND EXISTS (SELECT 1 FROM active_turns live WHERE live.id = r.provider_thread_id))
        AND NOT EXISTS (
          SELECT 1 FROM orchestration_v2_projection_provider_turns v
          JOIN orchestration_v2_projection_run_attempts a ON a.attempt_id = v.run_attempt_id
          JOIN active_turns live ON live.id = v.provider_thread_id
          WHERE a.run_id = r.run_id AND v.status = 'running'
        )
    `).all();
    return { known: true, agents };
  } catch {
    return { known: false, agents: [], note: 'live T3 agent metadata unavailable' };
  } finally {
    db?.close();
  }
}

// Delegated children are app-owned subagent child threads. Hooks see the provider's native session id
// (Claude session_id, Codex session_id, agy conversationId, OpenCode sessionID); the shell bridge sees
// the T3 thread id. Unreadable metadata answers false: routing still applies and the brief forbids nesting.
export function isDelegatedChild({ nativeId, threadId }, file = path.join(os.homedir(), '.t3/userdata/statev2.sqlite')) {
  if (!nativeId && !threadId) return false;
  let db;
  try {
    db = new DatabaseSync(file, { readOnly: true });
    db.exec('PRAGMA busy_timeout=1000');
    return Boolean(db.prepare(`
      SELECT 1 FROM orchestration_v2_projection_subagents
      WHERE child_thread_id = ? OR child_thread_id IN (
        SELECT thread_id FROM orchestration_v2_projection_provider_threads
        WHERE json_extract(payload_json, '$.nativeThreadRef.nativeId') = ?)
      LIMIT 1`).get(threadId ?? null, nativeId ?? null));
  } catch {
    return false;
  } finally {
    db?.close();
  }
}
