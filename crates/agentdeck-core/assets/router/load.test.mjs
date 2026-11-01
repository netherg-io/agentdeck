import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { DatabaseSync } from 'node:sqlite';
import { readActiveAgents } from './load.mjs';

// Real statev2 projection DDL (2026-10-09); Claude/agy native children carry NULL provider_thread_id.
const schema = `
  CREATE TABLE orchestration_v2_projection_subagents (subagent_id TEXT PRIMARY KEY, thread_id TEXT NOT NULL,
    run_id TEXT, parent_node_id TEXT NOT NULL, provider TEXT NOT NULL, provider_thread_id TEXT, child_thread_id TEXT,
    origin TEXT NOT NULL, status TEXT NOT NULL, started_at TEXT, completed_at TEXT, updated_at TEXT NOT NULL,
    payload_json TEXT NOT NULL, driver TEXT, provider_instance_id TEXT);
  CREATE TABLE orchestration_v2_projection_provider_threads (provider_thread_id TEXT PRIMARY KEY, thread_id TEXT,
    owner_node_id TEXT, provider TEXT NOT NULL, provider_session_id TEXT, status TEXT NOT NULL, first_run_ordinal INTEGER,
    last_run_ordinal INTEGER, updated_at TEXT NOT NULL, payload_json TEXT NOT NULL, driver TEXT, provider_instance_id TEXT);
  CREATE TABLE orchestration_v2_projection_provider_sessions (provider_session_id TEXT PRIMARY KEY, thread_id TEXT,
    provider TEXT NOT NULL, status TEXT NOT NULL, model TEXT, updated_at TEXT NOT NULL, payload_json TEXT NOT NULL,
    driver TEXT, provider_instance_id TEXT);
  CREATE TABLE orchestration_v2_projection_provider_turns (provider_turn_id TEXT PRIMARY KEY, thread_id TEXT NOT NULL,
    provider_thread_id TEXT NOT NULL, node_id TEXT NOT NULL, run_attempt_id TEXT, ordinal INTEGER NOT NULL,
    status TEXT NOT NULL, started_at TEXT, completed_at TEXT, payload_json TEXT NOT NULL);
  CREATE TABLE orchestration_v2_projection_runs (run_id TEXT PRIMARY KEY, thread_id TEXT NOT NULL, ordinal INTEGER NOT NULL,
    provider TEXT NOT NULL, provider_thread_id TEXT, status TEXT NOT NULL, requested_at TEXT NOT NULL, completed_at TEXT,
    payload_json TEXT NOT NULL, provider_instance_id TEXT);
  CREATE TABLE orchestration_v2_projection_run_attempts (attempt_id TEXT PRIMARY KEY, thread_id TEXT NOT NULL,
    run_id TEXT NOT NULL, attempt_ordinal INTEGER NOT NULL, root_node_id TEXT NOT NULL, provider TEXT NOT NULL,
    provider_thread_id TEXT NOT NULL, provider_turn_id TEXT, status TEXT NOT NULL, payload_json TEXT NOT NULL,
    provider_instance_id TEXT);`;

const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'model-route-load-'));
try {
  const file = path.join(dir, 'state.sqlite');
  const db = new DatabaseSync(file);
  db.exec(schema);
  const t = '2026-10-09T00:00:00Z';
  const session = (id, provider, status, model) => db.prepare(`INSERT INTO orchestration_v2_projection_provider_sessions
    VALUES (?, 'th', ?, ?, ?, '${t}', '{}', ?, ?)`).run(id, provider, status, model, provider, provider);
  const pthread = (id, provider, sessionId, status) => db.prepare(`INSERT INTO orchestration_v2_projection_provider_threads
    VALUES (?, 'th', 'n', ?, ?, ?, 1, 1, '${t}', '{}', ?, ?)`).run(id, provider, sessionId, status, provider, provider);
  const run = (id, provider, threadId, status, model) => db.prepare(`INSERT INTO orchestration_v2_projection_runs
    VALUES (?, 'th', 1, ?, ?, ?, '${t}', NULL, ?, ?)`)
    .run(id, provider, threadId, status, JSON.stringify(model ? { modelSelection: { model } } : {}), provider);
  const turn = (id, threadId, attempt, status) => db.prepare(`INSERT INTO orchestration_v2_projection_provider_turns
    VALUES (?, 'th', ?, 'n', ?, 1, ?, '${t}', NULL, '{}')`).run(id, threadId, attempt, status);
  const attempt = (id, runId, threadId) => db.prepare(`INSERT INTO orchestration_v2_projection_run_attempts
    VALUES (?, 'th', ?, 1, 'n', 'x', ?, NULL, 'running', '{}', 'x')`).run(id, runId, threadId);
  const child = (id, provider, runId, threadId, status, model) => db.prepare(`INSERT INTO orchestration_v2_projection_subagents
    VALUES (?, 'th', ?, 'n', ?, ?, 'child', 'provider_native', ?, NULL, NULL, '${t}', ?, ?, ?)`)
    .run(id, runId, provider, threadId, status,
      JSON.stringify({ providerInstanceId: provider, providerThreadId: threadId, model, status }), provider, provider);

  // Claude parent with NULL-thread native children; one queued next turn and a waiting run already live.
  session('sc', 'claudeAgent', 'ready', 'claude-opus-5-5');
  pthread('pt-claude', 'claudeAgent', 'sc', 'active');
  run('run-c', 'claudeAgent', 'pt-claude', 'running', 'claude-opus-5-5');
  run('run-c-wait', 'claudeAgent', 'pt-claude', 'waiting', 'claude-opus-5-5');
  run('run-c-next', 'claudeAgent', 'pt-claude', 'queued', 'claude-sonnet-5-5');
  attempt('att-c', 'run-c-wait', 'pt-claude');
  turn('turn-c', 'pt-claude', 'att-c', 'running');
  child('kid-sonnet', 'claudeAgent', 'run-c', null, 'running', 'claude-sonnet-5-5');
  child('kid-unknown', 'claudeAgent', 'run-c', null, 'running', null);
  child('kid-done', 'claudeAgent', 'run-c', null, 'completed', 'claude-opus-5-5');
  child('kid-cancel', 'claudeAgent', 'run-c', null, 'cancelled', 'claude-opus-5-5');

  // agy: an idle (finished) child and a running one with no model in payload.
  session('sa', 'antigravity', 'ready', 'gemini-3.8-pro');
  pthread('pt-agy', 'antigravity', 'sa', 'idle');
  run('run-a', 'antigravity', 'pt-agy', 'completed', 'gemini-3.8-pro');
  child('agy-idle', 'antigravity', 'run-a', null, 'idle', null);
  child('agy-running', 'antigravity', 'run-a', null, 'running', null);

  // Leftover 'running' child whose parent session stopped.
  session('ss', 'claudeAgent', 'stopped', 'claude-opus-5-5');
  pthread('pt-stale', 'claudeAgent', 'ss', 'idle');
  run('run-s', 'claudeAgent', 'pt-stale', 'completed', 'claude-opus-5-5');
  child('kid-stale', 'claudeAgent', 'run-s', null, 'running', 'claude-opus-5-5');

  // Codex parent and native child with its own thread and turn: counted once via its turn.
  session('sx', 'codex', 'ready', 'gpt-6.1-sol');
  pthread('pt-codex', 'codex', 'sx', 'active');
  pthread('pt-codex-kid', 'codex', 'sx', 'active');
  run('run-x', 'codex', 'pt-codex', 'running', 'gpt-6.1-sol');
  turn('turn-x', 'pt-codex', null, 'running');
  turn('turn-x-kid', 'pt-codex-kid', null, 'running');
  child('codex-kid', 'codex', 'run-x', 'pt-codex-kid', 'running', 'gpt-6-astra');
  // Running is projected before the provider's first turn: the branch still occupies a slot.
  pthread('pt-warm', 'codex', 'sx', 'active');
  run('run-warm', 'codex', 'pt-warm', 'running', 'gpt-6.1-sol');
  db.close();

  const before = fs.readFileSync(file);
  const load = readActiveAgents(file);
  assert.equal(load.known, true);
  const byId = Object.fromEntries(load.agents.map(a => [a.id, { ...a }]));
  assert.deepEqual(byId, {
    'pt-claude': { id: 'pt-claude', provider: 'claudeAgent', model: 'claude-opus-5-5' },
    'kid-sonnet': { id: 'kid-sonnet', provider: 'claudeAgent', model: 'claude-sonnet-5-5' },
    'kid-unknown': { id: 'kid-unknown', provider: 'claudeAgent', model: null },
    'agy-running': { id: 'agy-running', provider: 'antigravity', model: null },
    'run-c-next': { id: 'run-c-next', provider: 'claudeAgent', model: 'claude-sonnet-5-5' },
    'pt-codex': { id: 'pt-codex', provider: 'codex', model: 'gpt-6.1-sol' },
    'pt-codex-kid': { id: 'pt-codex-kid', provider: 'codex', model: 'gpt-6-astra' },
    'run-warm': { id: 'run-warm', provider: 'codex', model: 'gpt-6.1-sol' },
  });
  assert.equal(load.agents.length, 8);
  assert.deepEqual(fs.readFileSync(file), before);

  const missing = path.join(dir, 'missing.sqlite');
  assert.equal(readActiveAgents(missing).known, false);
  assert.equal(fs.existsSync(missing), false);
  console.log('load tests passed');
} finally {
  fs.rmSync(dir, { recursive: true, force: true });
}
