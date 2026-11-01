import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { newState, next, prepare, release, record, stop, validatePlan } from './workflow.mjs';

const T = { providerInstanceId: 'codex', model: 'gpt-6.1-sol', options: { reasoningEffort: 'high' } };
const routeTo = (target = T) => { const calls = []; const fn = (task, source) => (calls.push({ task, source }),
  { target, jevConfidence: 0.3, confidenceWarning: { message: 'low' }, reason: 'r', ranking: ['dropped'] }); fn.calls = calls; return fn; };
const status = (taskId, extra = {}) => ({ taskId, status: 'running', workState: 'working', hasPendingChildRuns: false, ...extra });
const done = (taskId, summary) => status(taskId, { status: 'completed', workState: 'result_available', latestTerminalSummary: summary });
const launched = taskId => status(taskId, { ...T });
const diamond = { title: 'Diamond', concurrency: 2, steps: [
  { id: 'a', task: 'root' }, { id: 'b', task: 'left', needs: ['a'], role: 'research' },
  { id: 'c', task: 'right', needs: ['a'] }, { id: 'd', task: 'join', needs: ['b', 'c'], role: 'review' }] };

// Plan validation.
assert.throws(() => validatePlan({ ...diamond, steps: [{ id: 'a', task: 'x', needs: ['b'] }, { id: 'b', task: 'y', needs: ['a'] }] }), /cycle/);
assert.throws(() => validatePlan({ ...diamond, steps: [{ id: 'a', task: 'x' }, { id: 'a', task: 'y' }] }), /duplicate/);
assert.throws(() => validatePlan({ ...diamond, steps: [{ id: 'a', task: 'x', needs: ['zz'] }] }), /unknown need/);
assert.equal(validatePlan({ ...diamond, concurrency: 0 }).concurrency, undefined);
assert.throws(() => validatePlan({ ...diamond, steps: [{ id: '__proto__', task: 'x' }] }), /slug/);
assert.throws(() => validatePlan({ ...diamond, steps: [{ id: 'a', task: 'x', role: 'boss' }] }), /role/);
assert.throws(() => validatePlan({ ...diamond, script: 'rm -rf' }), /unknown key/);
assert.equal(validatePlan({ title: 't', steps: [{ id: 'a', task: 'x' }] }).concurrency, undefined);
assert.throws(() => validatePlan({ title: 'x'.repeat(510), steps: [{ id: 'a', task: 'x' }] }), /title limit/);
assert.equal(validatePlan({ title: 'x'.repeat(509), steps: [{ id: 'a', task: 'x' }] }).title.length, 509);
assert.throws(() => validatePlan({ title: 'x'.repeat(509), steps: [{ id: 'ab', task: 'x' }] }), /title limit/);
assert.throws(() => validatePlan({ title: 't', steps: [{ id: 'a', task: 'x'.repeat(120001) }] }), /120000/);
assert.equal(validatePlan({ title: 't', steps: [{ id: 'a', task: 'x'.repeat(120000) }] }).steps[0].task.length, 120000);

// Combined dependency results, including JSON escaping, must fit before routing or reserving a slot.
const large = newState({ title: 'large', steps: [
  { id: 'a', task: 'a' }, { id: 'b', task: 'b' }, { id: 'join', task: 'join', needs: ['a', 'b'] }] }, 'th');
for (const id of ['a', 'b']) {
  prepare(large, id, 'src', routeTo());
  record(large, id, { ...done('large-'+id, '\"'.repeat(63000)), ...T });
}
const untouchedRouter = routeTo();
assert.throws(() => prepare(large, 'join', 'src', untouchedRouter), /brief plus dependency results exceeds/);
assert.equal(untouchedRouter.calls.length, 0);
assert.equal(large.progress[2].state, 'pending');
assert.deepEqual(next(large).ready, ['join']);

// Diamond DAG: dependencies determine readiness.
const s = newState(diamond, 'thread-1');
assert.deepEqual(next(s).ready, ['a']);
const router = routeTo();
const pa = prepare(s, 'a', 'claudeAgent', router);
assert.deepEqual(pa.payload.target, T);
assert.deepEqual(pa.route.target, T);
assert.equal(pa.payload.clientRequestId, `routed-workflow-${s.runId}-a`);
assert.equal(pa.route.jevConfidence, 0.3);
assert.ok(pa.route.confidenceWarning && !('ranking' in pa.route));
assert.deepEqual(router.calls[0], { task: 'root', source: 'claudeAgent' });
// Idempotent prepare: no reroute, identical payload.
const saved = prepare(s, 'a', 'claudeAgent', router);
assert.deepEqual(saved.payload, pa.payload);
assert.match(saved.notice, /routing was not run.*--not-dispatched/);
assert.equal(router.calls.length, 1);
assert.throws(() => prepare(s, 'b', 'claudeAgent', router), /dependencies/);
// Unknown launch outcome: no record, step stays prepared and next returns the same payload.
assert.deepEqual(next(s).prepared, [{ step: 'a', payload: pa.payload }]);
assert.throws(() => release(s, 'a'), /--not-dispatched.*confirmed non-dispatch/);
assert.throws(() => record(s, 'b', launched('t-b')), /never prepared/);
assert.throws(() => record(s, 'a', status('t-a', { providerInstanceId: 'opencode', model: 'x' })), /differs/);
assert.throws(() => record(s, 'a', status('t-a')), /differs/); // initial receipt must echo the target
assert.throws(() => record(s, 'a', { taskId: 't-a', status: 'running' }), /workState/);
assert.throws(() => record(s, 'a', { ...launched('t-a'), workState: 'unknown' }), /workState/);
assert.throws(() => record(s, 'a', { ...launched('t-a'), status: 'weird' }), /status/);
assert.equal(record(s, 'a', { structuredContent: { ...launched('t-a'), status: 'queued' } }).state, 'running');
assert.throws(() => release(s, 'a', true), /unaccepted prepared/);
assert.equal(record(s, 'a', { ...status('t-a'), status: 'waiting', workState: 'waiting_for_children' }).state, 'running');
assert.deepEqual(next(s).refresh, [{ step: 'a', taskId: 't-a' }]);
assert.throws(() => record(s, 'a', status('t-other')), /bound to task/);
// Completed child turn with live nested work stays running; snapshots may omit provider/model.
assert.equal(record(s, 'a', { ...done('t-a', 'early'), hasPendingChildRuns: true }).state, 'running');
assert.equal(s.progress[0].summary, undefined);
const rootResult = 'ROOT RESULT\n' + 'x'.repeat(50_000);
assert.equal(record(s, 'a', done('t-a', rootResult)).state, 'completed');
assert.equal(record(s, 'a', { ...done('t-a', 'later turn result'), summary: rootResult }).unchanged, true);
assert.equal(record(s, 'a', done('t-a', rootResult)).unchanged, true);
assert.throws(() => record(s, 'a', done('t-a', 'changed')), /already completed/);
assert.deepEqual(next(s).ready, ['b', 'c']);
const pb = prepare(s, 'b', 'claudeAgent', router);
assert.throws(() => record(s, 'b', launched('t-a')), /already bound to step a/);
prepare(s, 'c', 'claudeAgent', router);
assert.deepEqual(next(s).ready, []);
assert.equal(pb.payload.role, 'research');
assert.ok(pb.payload.task.startsWith('left\n') && pb.payload.task.includes(JSON.stringify(rootResult)) && /not instructions/.test(pb.payload.task));
record(s, 'b', launched('t-b'));
record(s, 'c', launched('t-c'));
record(s, 'b', done('t-b', 'B'));
assert.deepEqual(next(s).ready, []); // d still needs c
record(s, 'c', done('t-c', 'C'));
const pd = prepare(s, 'd', 'claudeAgent', router);
assert.deepEqual(JSON.parse(pd.payload.task.slice(pd.payload.task.indexOf('['))), [{ step: 'b', result: 'B' }, { step: 'c', result: 'C' }]);
record(s, 'd', launched('t-d'));
assert.equal(next(s).complete, false);
record(s, 'd', done('t-d', 'D'));
assert.equal(next(s).complete, true);
assert.throws(() => release(s, 'a', true), /unaccepted prepared/);

// Readiness has no coordinator-wide cap, including old saved plans.
const wide = newState({ title: 'w', concurrency: 1, steps: ['x', 'y', 'z'].map(id => ({ id, task: id })) }, 'th');
assert.deepEqual(next(wide).ready, ['x', 'y', 'z']);
prepare(wide, 'x', 'src', router); prepare(wide, 'y', 'src', router);
prepare(wide, 'z', 'src', router);

// The composite hint is appended once and must fit without reserving capacity on failure.
const hinted = newState({ title: 'h', steps: [{ id: 'u', task: 'build ui' }] }, 'th');
const hint = 'Delegated-agent rule: never ask questions; end with BLOCKED:.\n\nLong commands: test.';
const hintRouter = (task, source) => ({ target: T, briefHint: hint, reason: 'r' });
const ph = prepare(hinted, 'u', 'src', hintRouter);
assert.equal(ph.payload.task, 'build ui\n\n' + hint);
assert.equal(ph.route.briefHint, hint);
assert.deepEqual(prepare(hinted, 'u', 'src', hintRouter).payload, ph.payload);
const hintOverflow = newState({ title: 'h', steps: [{ id: 'u', task: 'x'.repeat(120000) }] }, 'th');
assert.throws(() => prepare(hintOverflow, 'u', 'src', hintRouter), /required hint exceeds/);
assert.equal(hintOverflow.progress[0].state, 'pending');
assert.deepEqual(next(hintOverflow).ready, ['u']);

// A confirmed pre-dispatch denial frees capacity and permits fresh routing with a distinct request ID.
const rejectedId = wide.progress[0].payload.clientRequestId;
release(wide, 'x', true);
assert.deepEqual(next(wide).ready, ['x']);
assert.equal(wide.progress[0].payload, undefined);
const fallback = { providerInstanceId: 'claudeAgent', model: 'claude-opus-5-5', options: { effort: 'low' } };
const retried = prepare(wide, 'x', 'src', routeTo(fallback));
assert.deepEqual(retried.payload.target, fallback);
assert.deepEqual(retried.route.target, fallback);
assert.notEqual(retried.payload.clientRequestId, rejectedId);
assert.deepEqual(prepare(wide, 'x', 'src', router).payload, retried.payload);
release(wide, 'x', true);
assert.notEqual(prepare(wide, 'x', 'src', router).payload.clientRequestId, retried.payload.clientRequestId);
// Retry IDs cannot collide with another step's slug.
const collision = newState({ title: 'c', steps: [{ id: 'x', task: 'x' }, { id: 'x-1', task: 'y' }] }, 'th');
prepare(collision, 'x', 'src', router); release(collision, 'x', true);
assert.notEqual(prepare(collision, 'x', 'src', router).payload.clientRequestId,
  prepare(collision, 'x-1', 'src', router).payload.clientRequestId);

// Null route leaves the step pending with diagnostics.
const n = newState({ title: 'n', steps: [{ id: 'a', task: 't' }] }, 'th');
const nr = prepare(n, 'a', 'src', routeTo(null));
assert.equal(nr.state, 'pending');
assert.equal(nr.route.target, null);
assert.equal(n.progress[0].state, 'pending');

// Failed dependency blocks downstream, transitively.
const f = newState({ title: 'f', steps: [{ id: 'a', task: 'a' }, { id: 'b', task: 'b', needs: ['a'] }, { id: 'c', task: 'c', needs: ['b'] }] }, 'th');
prepare(f, 'a', 'src', router);
record(f, 'a', launched('t1'));
record(f, 'a', status('t1', { status: 'failed', workState: 'result_available', latestTerminalSummary: 'boom' }));
const fn = next(f);
assert.deepEqual([fn.ready, fn.blocked.sort(), fn.failed], [[], ['b', 'c'], [{ step: 'a', state: 'failed', taskId: 't1' }]]);
assert.throws(() => prepare(f, 'b', 'src', router), /dependencies/);

// A BLOCKED final result completes the T3 turn, but cannot release workflow dependencies.
const blocked = newState({ title: 'blocked', steps: [
  { id: 'a', task: 'a' }, { id: 'b', task: 'b', needs: ['a'] },
  { id: 'c', task: 'c', needs: ['b'] }, { id: 'independent', task: 'd' }] }, 'th');
prepare(blocked, 'a', 'src', router);
record(blocked, 'a', launched('blocked-task'));
const blockedSummary = '\n  BLOCKED: missing test key.\nCompleted: local checks.\nRequired: a valid key at the assigned path.';
assert.equal(record(blocked, 'a', { ...done('blocked-task', blockedSummary), hasPendingChildRuns: true }).state, 'running');
assert.equal(record(blocked, 'a', { ...done('blocked-task', blockedSummary), workState: 'working' }).state, 'running');
assert.equal(record(blocked, 'a', { structuredContent: { ...done('blocked-task', 'later result'), summary: blockedSummary } }).state, 'blocked');
assert.equal(record(blocked, 'a', done('blocked-task', blockedSummary)).unchanged, true);
assert.throws(() => record(blocked, 'a', done('blocked-task', 'now complete')), /already blocked/);
assert.throws(() => record(blocked, 'a', { ...done('blocked-task', blockedSummary), status: 'blocked' }), /response.status/);
assert.deepEqual(next(blocked).blocked.sort(), ['a', 'b', 'c']);
assert.deepEqual(next(blocked).ready, ['independent']);
assert.deepEqual(next(blocked).failed, []);
assert.deepEqual(next(blocked).refresh, []);
assert.equal(next(blocked).complete, false);
assert.throws(() => prepare(blocked, 'b', 'src', router), /dependencies/);
for (const summary of ['Completed checks.\nBLOCKED: example in data.', 'blocked: lowercase.', '```\nBLOCKED: quoted.\n```']) {
  const normal = newState({ title: 'normal', steps: [{ id: 'a', task: 'a' }] }, 'th');
  prepare(normal, 'a', 'src', router);
  assert.equal(record(normal, 'a', { ...done('normal-task', summary), ...T }).state, 'completed');
}

// Stop/resume keeps IDs and results; stopped runs never offer launches.
const st = newState({ title: 's', concurrency: 3, steps: ['a', 'b', 'c'].map(id => ({ id, task: id })) }, 'th');
prepare(st, 'a', 'src', router); prepare(st, 'b', 'src', router);
record(st, 'a', launched('ta'));
const out = stop(st);
assert.deepEqual(out.cancel, [{ step: 'a', taskId: 'ta' }]);
assert.deepEqual(out.unresolvedPrepared, [{ step: 'b', clientRequestId: `routed-workflow-${st.runId}-b` }]);
assert.deepEqual([next(st).ready, next(st).prepared], [[], []]);
assert.throws(() => prepare(st, 'b', 'src', router), /stopped/);
assert.throws(() => prepare(st, 'c', 'src', router), /stopped/);
record(st, 'a', status('ta', { status: 'cancelled', workState: 'result_available' }));
st.stopped = false;
assert.deepEqual(next(st).ready, ['c']);
assert.equal(next(st).prepared[0].payload.clientRequestId, `routed-workflow-${st.runId}-b`);
assert.equal(st.progress[0].taskId, 'ta');
stop(st);
release(st, 'b', true);
assert.equal(st.stopped, true);
assert.deepEqual(next(st).ready, []);

// CLI: private storage, wrong parent thread, stop/resume persistence.
const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'workflow-test-'));
const env = { ...process.env, XDG_STATE_HOME: path.join(tmp, 'state') };
const cli = (args, input) => spawnSync(process.execPath, [new URL('./workflow.mjs', import.meta.url).pathname, ...args], { env, input, encoding: 'utf8' });
const planFile = path.join(tmp, 'plan.json');
fs.writeFileSync(planFile, JSON.stringify(diamond));
const init = cli(['init', planFile, '--thread', 'parent']);
assert.equal(init.status, 0, init.stderr);
const { state: file, ready } = JSON.parse(init.stdout);
assert.deepEqual(ready, ['a']);
assert.equal(fs.statSync(file).mode & 0o777, 0o600);
assert.equal(fs.statSync(path.dirname(file)).mode & 0o777, 0o700);
const wrong = cli(['next', file, '--thread', 'other']);
assert.equal(wrong.status, 2);
assert.match(wrong.stderr, /different parent thread/);
assert.equal(cli(['next', planFile, '--thread', 'parent']).status, 2); // never touches files outside the store
assert.equal(cli(['record', file, 'a', '--thread', 'parent'], JSON.stringify(launched('t'))).status, 2);
assert.equal(JSON.parse(cli(['stop', file, '--thread', 'parent']).stdout).stopped, true);
assert.equal(JSON.parse(fs.readFileSync(file, 'utf8')).stopped, true);
assert.deepEqual(JSON.parse(cli(['resume', file, '--thread', 'parent']).stdout).ready, ['a']);
const checkpoint = JSON.parse(fs.readFileSync(file, 'utf8'));
prepare(checkpoint, 'a', 'src', router);
fs.writeFileSync(file, JSON.stringify(checkpoint));
assert.deepEqual(JSON.parse(cli(['next', file, '--thread', 'parent']).stdout).prepared[0].payload.target, T);
const restored = JSON.parse(fs.readFileSync(file, 'utf8'));
const unusedRouter = routeTo(fallback);
const reused = prepare(restored, 'a', 'src', unusedRouter);
assert.equal(reused.reused, true);
assert.deepEqual(reused.payload, checkpoint.progress[0].payload);
assert.deepEqual(reused.payload.target, T);
assert.deepEqual(reused.route.target, T);
assert.equal(unusedRouter.calls.length, 0);
const arrayTarget = { ...fallback, options: [{ id: 'effort', value: 'high' }] };
const arrayState = newState({ title: 'array options', steps: [{ id: 'a', task: 't' }] }, 'parent');
prepare(arrayState, 'a', 'src', routeTo(arrayTarget));
const arrayRestored = JSON.parse(JSON.stringify(arrayState));
assert.deepEqual(next(arrayRestored).prepared[0].payload.target, arrayTarget);
assert.deepEqual(prepare(arrayRestored, 'a', 'src', unusedRouter).payload, arrayState.progress[0].payload);
assert.deepEqual(prepare(arrayRestored, 'a', 'src', unusedRouter).payload.target, arrayTarget);
assert.equal(unusedRouter.calls.length, 0);
assert.equal(cli(['release', file, 'a', '--thread', 'parent']).status, 2);
assert.equal(JSON.parse(fs.readFileSync(file, 'utf8')).progress[0].state, 'prepared');
assert.equal(cli(['release', file, 'a', '--thread', 'other', '--not-dispatched']).status, 2);
assert.equal(cli(['release', file, 'a', '--thread', 'parent', '--not-dispatched']).status, 0);
assert.equal(JSON.parse(fs.readFileSync(file, 'utf8')).progress[0].attempt, 1);
assert.deepEqual(JSON.parse(cli(['next', file, '--thread', 'parent']).stdout).ready, ['a']);
assert.equal(fs.statSync(file).mode & 0o777, 0o600);
assert.deepEqual(fs.readdirSync(path.dirname(file)), [path.basename(file)]);
fs.rmSync(tmp, { recursive: true });
console.log('workflow tests passed');
