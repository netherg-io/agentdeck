import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

const productionRoster = JSON.parse(fs.readFileSync(new URL('./roster.json', import.meta.url), 'utf8'));
const museHint = productionRoster.families.find(f => f.match === 'muse-spark').briefHint;
assert.match(museHint, /^Long commands:/);
assert.match(museHint, /PARTIAL:/);
for (const hint of [productionRoster.briefHint, ...productionRoster.families.map(f => f.briefHint)].filter(Boolean))
  assert.doesNotMatch(hint, /\bskills?\b|скилл/iu);

// Jev answer cache through the actual CLI: isolated home and runtime dir, mocked fetch, no paid calls.
// Router files are copied so the roster profile can change without touching the real roster.
const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'model-route-jev-cache-test-'));
try {
  fs.mkdirSync(path.join(dir, '.local/state/model-route'), { recursive: true });
  fs.copyFileSync(new URL('./catalog.fixture.json', import.meta.url), path.join(dir, '.local/state/model-route/catalog.json'));
  const code = path.join(dir, 'router');
  fs.mkdirSync(code);
  for (const f of ['route.mjs', 'policy.mjs', 'load.mjs', 'reasoning.mjs', 'roster.json'])
    fs.copyFileSync(new URL(f, import.meta.url), path.join(code, f));
  for (const rel of ['.t3/userdata', '.local/state/ai-usage']) fs.mkdirSync(path.join(dir, rel), { recursive: true });
  fs.writeFileSync(path.join(dir, '.t3/userdata/settings.json'), '{}');
  const favorites = models => fs.writeFileSync(path.join(code, 'router.json'), JSON.stringify({ version: 1,
    pool: models.map(([provider, model]) => ({ providerInstanceId: provider, model, enabled: true })),
    accounts: {
      claude: { instances: ['claudeAgent'], usageProvider: 'claude', usageId: 'claude-main' },
      codex: { instances: ['codex'], usageProvider: 'codex', usageId: 'codex-main' } },
    usageStatePath: path.join(dir, '.local/state/ai-usage/state.json') }));
  favorites([['claudeAgent', 'claude-opus-5-5'], ['codex', 'gpt-6.1-sol']]);
  const quota = claudeLeft => {
    const t = Date.now() / 1000;
    const account = (provider, plan, left) => ({ provider, plan, status: 'ok', updated_at: t, balances: [],
      windows: [{ key: 'weekly', label: 'Week', remaining_percent: left, duration_mins: 10080, resets_at: t + 3 * 86400 }] });
    fs.writeFileSync(path.join(dir, '.local/state/ai-usage/state.json'), JSON.stringify({ accounts: [
      account('claude', 'Max 5x', claudeLeft), account('codex', 'Pro 5x', 80)] }));
  };
  quota(80);

  const fetches = path.join(dir, 'fetches');
  const level = score => ({ score, confidence: 0.9, probabilities: { 0: 0, 1: 0.05, 2: 0.75, 3: 0.2 } });
  const jev = (opus, sol, difficulty = 0) => ({ answers: { size: { score: 0, confidence: 0.9 }, difficulty: { score: difficulty, confidence: 0.9 },
    claudeagent_claude_opus_5_5: level(opus), codex_gpt_6_1_sol: level(sol) }, usage: { cost: 0.01 } });
  const cli = (args, answers, vanish = '') => spawnSync(process.execPath, ['--input-type=module', '-e', `
    import fs from 'node:fs';
    import os from 'node:os';
    os.homedir = () => process.cwd();
    const readdir = fs.readdirSync;
    fs.readdirSync = (file, ...args) => {
      const entries = readdir(file, ...args);
      if (process.env.PRUNE_VANISH && String(file).endsWith('/model-route-jev-cache'))
        fs.rmSync(String(file) + '/' + process.env.PRUNE_VANISH, { force: true });
      return entries;
    };
    globalThis.fetch = async () => {
      fs.appendFileSync('fetches', 'x');
      return { ok: true, json: async () => JSON.parse(process.env.MOCK_JEV) };
    };
    process.argv = [process.execPath, 'route.mjs', ...JSON.parse(process.env.ROUTE_ARGS)];
    await import(${JSON.stringify(path.join(code, 'route.mjs'))});
  `], { cwd: dir, timeout: 10_000, encoding: 'utf8', env: { ...process.env, XDG_RUNTIME_DIR: dir, XDG_STATE_HOME: path.join(dir, '.local/state'), JEV_OPENROUTER_API_KEY: 'test',
    MOCK_JEV: JSON.stringify(answers), ROUTE_ARGS: JSON.stringify(args), PRUNE_VANISH: vanish } });
  const count = () => fs.existsSync(fetches) ? fs.readFileSync(fetches, 'utf8').length : 0;
  const run = (args, answers, fetched, vanish = '') => {
    const before = count();
    const r = cli(args, answers, vanish);
    assert.equal(r.status, 0, r.stderr);
    assert.equal(count() - before, fetched ? 1 : 0, `fetch for ${JSON.stringify(args)}`);
    const out = JSON.parse(r.stdout);
    assert.equal(out.jevCache, fetched ? 'miss' : 'hit');
    return out;
  };
  const task = 'Implement the secret-sauce feature in parser.c';
  const prefersOpus = jev(2.9, 2.0), prefersSol = jev(2.0, 2.9, 3);
  const validate = model => ['--source', 'claudeAgent', '--validate-target', model.split('/')[0], model.split('/')[1], task];

  let out = run(['--source', 'claudeAgent', task], prefersOpus, true);
  assert.equal(out.target.model, 'claude-opus-5-5');
  const commonHint = JSON.parse(fs.readFileSync(path.join(code, 'roster.json'), 'utf8')).briefHint;
  assert.match(commonHint, /never ask questions/i);
  assert.match(commonHint, /never spawn, delegate to or launch other agents/);
  assert.match(commonHint, /BLOCKED:/);
  assert.equal(out.briefHint, commonHint);
  const opusTarget = { providerInstanceId: 'claudeAgent', model: 'claude-opus-5-5', options: { effort: 'low' } };
  const solTarget = { providerInstanceId: 'codex', model: 'gpt-6.1-sol', options: { reasoningEffort: 'low' } };
  assert.deepEqual(out.target, opusTarget);
  // A fresh Jev sample would now prefer Sol; validation reuses the cached answers instead of flipping.
  out = run(validate('claudeAgent/claude-opus-5-5'), prefersSol, false);
  assert.equal(out.validation.allowed, true);
  assert.deepEqual(out.target, opusTarget);
  assert.deepEqual(out.requestedTarget, opusTarget);
  assert.equal(out.cost, 0);
  out = run(validate('codex/gpt-6.1-sol'), prefersSol, false);
  assert.deepEqual(out.target, opusTarget);
  assert.deepEqual(out.requestedTarget, solTarget);
  assert.equal(out.requestedBriefHint, commonHint);
  // Validation sees the prepared payload: the brief plus the appended hint. It is the same work and must hit.
  out = run(validate('claudeAgent/claude-opus-5-5').map(a => a === task ? `${task}\n\n${commonHint}` : a), prefersSol, false);
  assert.equal(out.validation.allowed, true);
  // Quota is re-read on a hit: an exhausted winner is blocked, still without a new Jev request.
  quota(0);
  out = run(validate('claudeAgent/claude-opus-5-5'), prefersSol, false);
  assert.equal(out.validation.allowed, false);
  assert.equal(out.target.model, 'gpt-6.1-sol');
  assert.deepEqual(out.target, solTarget);
  assert.deepEqual(out.requestedTarget, opusTarget);
  quota(80);

  // Capacity returns: the old fallback stays affordable, but the same cached judgment prefers Opus again.
  out = run(validate('codex/gpt-6.1-sol'), prefersSol, false);
  assert.equal(out.validation.allowed, false);
  assert.deepEqual(out.target, opusTarget);
  assert.deepEqual(out.requestedTarget, solTarget);

  const cacheDir = path.join(dir, 'model-route-jev-cache');
  const files = () => fs.readdirSync(cacheDir).filter(f => f.endsWith('.json'));
  assert.equal(files().length, 1);
  const [file] = files().map(f => path.join(cacheDir, f));
  assert.match(path.basename(file), /^[0-9a-f]{64}\.json$/);
  assert.equal(fs.statSync(file).mode & 0o777, 0o600);
  assert.equal(fs.statSync(cacheDir).mode & 0o777, 0o700);
  assert.doesNotMatch(fs.readFileSync(file, 'utf8'), /secret-sauce/);

  // Task, source, favorite and profile changes are different requests.
  run(['--source', 'claudeAgent', task + '!'], prefersOpus, true);
  run(['--source', 'codex', task], prefersOpus, true);
  favorites([['claudeAgent', 'claude-opus-5-5']]);
  run(['--source', 'claudeAgent', task], prefersOpus, true);
  favorites([['claudeAgent', 'claude-opus-5-5'], ['codex', 'gpt-6.1-sol']]);
  run(['--source', 'claudeAgent', task], prefersOpus, false);
  const rosterFile = path.join(code, 'roster.json');
  const roster = JSON.parse(fs.readFileSync(rosterFile, 'utf8'));
  roster.families.find(f => f.match === 'claude-opus').profile += ' Changed.';
  roster.families.find(f => f.match === 'claude-opus').briefHint = 'Long commands: fixture.';
  fs.writeFileSync(rosterFile, JSON.stringify(roster));
  out = run(['--source', 'claudeAgent', task], prefersOpus, true);
  assert.equal(out.briefHint, commonHint + '\n\nLong commands: fixture.');
  out = run(validate('codex/gpt-6.1-sol'), prefersOpus, false);
  assert.equal(out.briefHint, commonHint + '\n\nLong commands: fixture.');
  assert.equal(out.requestedBriefHint, commonHint);
  out = run(validate('claudeAgent/claude-opus-5-5'), prefersOpus, false);
  assert.equal(out.requestedBriefHint, out.briefHint);

  // Corrupt or invalid cache entries are refetched; invalid remote answers fail and are not cached.
  const entry = path.join(cacheDir, files().find(f => fs.statSync(path.join(cacheDir, f)).mtimeMs
    === Math.max(...files().map(g => fs.statSync(path.join(cacheDir, g)).mtimeMs))));
  for (const bad of ['{', 'null', JSON.stringify({ t: Date.now(), answers: { size: { score: 9, confidence: 1 } } }),
    JSON.stringify({ t: Date.now() + 3600e3, answers: prefersOpus.answers })]) {
    fs.writeFileSync(entry, bad);
    run(['--source', 'claudeAgent', task], prefersOpus, true);
  }
  const before = count();
  const broken = cli(['--source', 'claudeAgent', 'another task'], { answers: { size: { score: 0, confidence: 0.9 } } });
  assert.equal(broken.status, 1);
  assert.equal(count() - before, 1);
  run(['--source', 'claudeAgent', 'another task'], prefersOpus, true);

  // Cached difficulty carries both winner and requested candidate's distinct option IDs on validation.
  const hardTask = task + ' difficult';
  out = run(['--source', 'claudeAgent', hardTask], jev(2.9, 2.0, 3), true);
  assert.deepEqual(out.target.options, { effort: 'xhigh' });
  out = run(['--source', 'claudeAgent', '--validate-target', 'codex', 'gpt-6.1-sol', hardTask], prefersOpus, false);
  assert.deepEqual(out.target.options, { effort: 'xhigh' });
  assert.deepEqual(out.requestedTarget.options, { reasoningEffort: 'xhigh' });
  run(['--source', 'claudeAgent', task], prefersOpus, false);

  // Expired entries are refetched and pruned on the next write; foreign files are left alone.
  fs.writeFileSync(entry, JSON.stringify({ t: Date.now() - 11 * 60e3, answers: prefersOpus.answers }));
  const stale = path.join(cacheDir, 'f'.repeat(64) + '.json');
  fs.writeFileSync(stale, '{}');
  fs.writeFileSync(path.join(cacheDir, 'notes.txt'), 'keep');
  const old = new Date(Date.now() - 11 * 60e3);
  fs.utimesSync(stale, old, old);
  run(['--source', 'claudeAgent', task], prefersOpus, true);
  assert.equal(fs.existsSync(stale), false);
  assert.equal(fs.existsSync(path.join(cacheDir, 'notes.txt')), true);
  // Another route can remove an entry after readdir; pruning must still write this task's answer.
  const vanishing = 'e'.repeat(64) + '.json';
  fs.writeFileSync(path.join(cacheDir, vanishing), '{}');
  const concurrent = ['--source', 'claudeAgent', task + ' concurrent'];
  run(concurrent, prefersOpus, true, vanishing);
  run(concurrent, prefersSol, false);
} finally { fs.rmSync(dir, { recursive: true }); }
console.log('jev cache checks passed');
