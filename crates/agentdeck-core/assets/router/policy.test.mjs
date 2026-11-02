import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { MIN_FIT, NO_REVIEW_BAN, REVIEW_TASK, enabledFavorites, ownsUsage, quotaWindows, reserveActiveQuota, quotaBudget, rankCandidates, validAnswer, validateTarget } from './policy.mjs';

// The pool and account maps moved from T3 favorites and hardcoded maps into the
// agentdeck router JSON; every direct and spawned use below reads this fixture.
const routerHome = fs.mkdtempSync(path.join(os.tmpdir(), 'model-route-router-json-'));
const routerFile = path.join(routerHome, 'router.json');
const writeRouterJson = ({ pool, accounts, usageStatePath }) => fs.writeFileSync(routerFile, JSON.stringify({
  version: 1, pool, accounts, ...(usageStatePath && { usageStatePath }),
}));
process.env.MODEL_ROUTE_ROUTER_JSON = routerFile;
const allAccounts = {
  claude: { instances: ['claudeAgent'], usageProvider: 'claude', usageId: 'claude-main' },
  claude_second: { instances: ['claude_second'], usageProvider: 'claude', usageId: 'claude-second' },
  claude_third: { instances: ['claude_third'], usageProvider: 'claude', usageId: 'claude-third' },
  claude_fourth: { instances: ['claude_fourth'], usageProvider: 'claude', usageId: 'claude-fourth' },
  codex: { instances: ['codex'], usageProvider: 'codex', usageId: 'codex-main' },
  codex_second: { instances: ['codex_second'], usageProvider: 'codex', usageId: 'codex-second' },
  antigravity: { instances: ['antigravity'], usageProvider: 'antigravity' },
  openrouter: { instances: ['opencode', 'claudeAgent_openrouter'], usageProvider: 'openrouter' },
  'opencode-go': { instances: ['opencode'], usageProvider: 'opencode-go', modelPrefix: 'opencode-go/' },
};
const allPool = Object.keys(allAccounts).filter(a => a !== 'opencode-go')
  .flatMap(a => allAccounts[a].instances.map(providerInstanceId => ({ providerInstanceId, model: providerInstanceId, enabled: true })));
writeRouterJson({ pool: allPool, accounts: allAccounts });

const weights = JSON.parse(fs.readFileSync(new URL('./roster.json', import.meta.url), 'utf8')).routing;
const model = (provider, overrides = {}) => ({ provider, model: provider, fit: 0.95,
  quality: 0.8, allowed: true, ...overrides });
const sonnet = model('claudeAgent', { quality: 0.82 });
const flash = model('antigravity', { fit: 0.85 });
const gpt = model('codex');
const pick = (models, difficulty = 1, source = 'claudeAgent') => rankCandidates(models, difficulty, source, weights).find(r => r.eligible);

// A small quality edge is not enough to spend the orchestrator's shared quota.
assert.notEqual(pick([sonnet, flash, gpt]).provider, 'claudeAgent');
assert.equal(pick([{ ...sonnet, allowed: false }, flash]).provider, 'antigravity');
// A substantial quality gain outweighs the shared-quota penalty.
assert.equal(pick([{ ...sonnet, quality: 0.98 }, { ...flash, quality: 0.7 }]).provider, 'claudeAgent');
assert.equal(pick([{ ...sonnet, quality: 0.9 }, flash], 2).provider, 'claudeAgent');
assert.equal(pick([sonnet, { ...flash, fit: 0.69 }]).provider, 'claudeAgent');
assert.equal(pick([sonnet, { ...flash, allowed: false }]).provider, 'claudeAgent');
assert.equal(pick([{ ...sonnet, allowed: false }, { ...flash, fit: 0.2 }]), undefined);
assert.equal(pick([flash, gpt], 1, 'codex').provider, 'antigravity');
// Different harnesses can still debit the same credit balance.
assert.equal(pick([model('opencode'), flash], 1, 'claudeAgent_openrouter').provider, 'antigravity');
// Equal quality and no quota penalties: the better fit wins.
assert.equal(pick([flash, gpt], 1, null).provider, 'codex');
// Low remaining quota is not penalized: once the task fits, the better separate-account model wins.
assert.equal(pick([flash, { ...gpt, quality: 0.84, taskCost: 0.9 }]).provider, 'codex');
// Paid OpenRouter credits carry no penalty (user 2026-10-18): a small quality edge picks paid Muse...
const muse = model('claudeAgent_openrouter', { model: 'meta/muse-spark', quality: 0.823 });
for (const source of ['codex', 'claudeAgent', 'antigravity', null]) {
  for (const difficulty of [1, 2]) assert.equal(pick([muse, flash], difficulty, source).model, 'meta/muse-spark');
}
// ...unless the caller already spends that OpenRouter balance: the shared-quota penalty still applies.
for (const difficulty of [1, 2]) assert.equal(pick([muse, flash], difficulty, 'claudeAgent_openrouter').provider, 'antigravity');
assert.equal(pick([{ ...muse, quality: 0.997 }, { ...flash, quality: 0.7 }], 2, 'codex').model, 'meta/muse-spark');
assert.equal(pick([muse, { ...flash, fit: 0.6 }], 1, 'codex').model, 'meta/muse-spark');

// A disabled provider's surviving model ids are not active favorites; explicit instances win over legacy settings.
const favorites = ['opencode', 'claudeAgent_openrouter', 'antigravity'].map(provider => ({ provider, model: provider }));
assert.deepEqual(enabledFavorites(favorites, { providerInstances: { opencode: { enabled: false } } }), favorites.slice(1));
assert.deepEqual(enabledFavorites(favorites, { providers: { opencode: { enabled: false } } }), favorites.slice(1));
assert.deepEqual(enabledFavorites(favorites, { providers: { opencode: { enabled: false } },
  providerInstances: { opencode: { enabled: true } } }), favorites);

// Antigravity pools follow stable bucket keys even when ai-usage's active model switches their scope to null.
const agyWindows = [{ key: '3p-weekly', scope: null, remaining_percent: 0, duration_mins: 10080 },
  { key: 'gemini-weekly', scope: 'Gemini', remaining_percent: 100, duration_mins: 10080 }];
assert.deepEqual(quotaWindows('antigravity', 'gemini-3.8-flash-high', agyWindows), [agyWindows[1]]);
assert.deepEqual(quotaWindows('antigravity', 'claude-opus-5-5', agyWindows), [agyWindows[0]]);
const switched = agyWindows.map((w, i) => ({ ...w, scope: i ? null : 'Сторонние модели' }));
assert.deepEqual(quotaWindows('antigravity', 'gemini-3.8-flash-high', switched), [switched[1]]);
assert.deepEqual(quotaWindows('antigravity', 'claude-opus-5-5', switched), [switched[0]]);
const argonWindows = [...agyWindows, { key: 'gemini-argon-wtu-weekly', scope: null, remaining_percent: 100, duration_mins: 10080 }];
assert.deepEqual(quotaWindows('antigravity', 'gemini-3.8-flash-high', argonWindows), [argonWindows[1]]);
assert.deepEqual(quotaWindows('antigravity', 'gemini-4-argon-high', argonWindows), [argonWindows[2]]);
const claudeWindows = [{ key: 'weekly_all', scope: null }, { key: 'weekly_scoped:fable', scope: 'Fable' }];
assert.deepEqual(quotaWindows('claudeAgent', 'claude-fable-5-1', claudeWindows), claudeWindows);
assert.deepEqual(quotaWindows('claudeAgent', 'claude-opus-5-5', claudeWindows), [claudeWindows[0]]);

// The final weekly reserve cannot fund delegation, including hard work or costs measured as zero.
for (const provider of ['claudeAgent', 'codex', 'antigravity']) {
  const budget = quotaBudget([{ remaining_percent: 5, duration_mins: 10080 }], [], [0, 1, 2, 5], 1, 5);
  const low = { ...model(provider, { quality: 1 }), allowed: budget.allowed(0) };
  assert.equal(pick([low, model('claudeAgent_openrouter')], 3).provider, 'claudeAgent_openrouter');
}
const budget = (windows, weekly = [0.5, 2, 6, 15], burn = 1, balances = []) => quotaBudget(windows, balances, weekly, burn, 5);
assert.equal(budget([{ remaining_percent: 7, duration_mins: 10080 }]).allowed(1), false);
assert.equal(budget([{ remaining_percent: 8, duration_mins: 10080 }]).allowed(1), true);
assert.equal(budget([{ remaining_percent: 5, duration_mins: 300 }]).allowed(0), true);
assert.equal(budget(quotaWindows('antigravity', 'gemini-3.8-flash-high', agyWindows)).allowed(3), true);
assert.equal(budget([], undefined, 1, [3.8]).allowed(3), true);
assert.equal(budget([], undefined, 3, [3.8]).allowed(3), false);
assert.equal(budget([]).cost(0), null);

// Near reset, release reserve without inventing quota.
const now = 1_000;
const timed = (remaining, hours) => quotaBudget([{ remaining_percent: remaining, duration_mins: 10080,
  resets_at: now + hours * 3600 }], [], [0.5, 2, 6, 15], 1, 5, now);
assert.equal(timed(5, 30).allowed(1), false);
assert.equal(timed(5, 1).allowed(1), true);
assert.equal(timed(1, 1).allowed(1), false);
assert.equal(timed(5, 0).allowed(1), false); // An expired snapshot does not imply a fresh budget.
assert.equal(timed(5, -1).allowed(1), false);
const fableBudget = remaining => quotaBudget([{ remaining_percent: remaining, duration_mins: 10080 },
  { remaining_percent: 91, duration_mins: 10080 }], [], [1.5, 6, 18, 45], 3, 5, now);
assert.equal(fableBudget(5).allowed(0), false);
const refilled = fableBudget(80);
assert.equal(pick([model('claudeAgent', { model: 'claude-fable-5-1', quality: 0.997,
  allowed: refilled.allowed(1) }), flash], 3).model, 'claude-fable-5-1');

// Active work reserves its model's cost on every shared window, but only its own scoped pool.
const costsOf = (_p, m) => m.includes('fable') ? { weekly: [1.5, 6, 18, 45], burn: 3 }
  : { weekly: [0.5, 2, 6, 15], burn: 1 };
const account = { provider: 'claude', balances: [], windows: [
  { key: 'weekly_all', scope: null, remaining_percent: 14, duration_mins: 10080 },
  { key: 'weekly_scoped:fable', scope: 'Fable', remaining_percent: 91, duration_mins: 10080 }] };
const reserved = reserveActiveQuota(account, [{ provider: 'claudeAgent', model: 'claude-opus-5-5' },
  { provider: 'claudeAgent', model: 'claude-fable-5-1' }, { provider: 'codex', model: 'gpt-6.1-sol' }], costsOf);
assert.equal(reserved.activeAgents, 2);
assert.equal(reserved.windows[0].remaining_percent, 6);
assert.equal(reserved.windows[1].remaining_percent, 85);
assert.equal(budget(reserved.windows).allowed(1), false);
assert.equal(account.windows[0].remaining_percent, 14); // The live snapshot is never mutated.
const unknown = reserveActiveQuota(account, [{ provider: 'claudeAgent', model: null }],
  () => ({ weekly: [1.5, 6, 18, 45], burn: 3 }));
assert.equal(unknown.windows[0].remaining_percent, 8);
assert.equal(unknown.windows[1].remaining_percent, 85);
const money = reserveActiveQuota({ provider: 'openrouter', windows: [], balances: [{ total: '1.00' }] }, [
  { provider: 'claudeAgent_openrouter', model: 'muse-spark' }, { provider: 'opencode', model: 'muse-spark' }], costsOf);
assert.equal(money.activeAgents, 2);
assert.equal(money.balances[0], 0.4);
// OpenCode Go models on the opencode instance use the subscription, not OpenRouter credits.
const goAgent = { provider: 'opencode', model: 'opencode-go/kimi-k3' };
assert.equal(ownsUsage({ provider: 'opencode-go' }, goAgent.provider, goAgent.model), true);
assert.equal(ownsUsage({ provider: 'openrouter' }, goAgent.provider, goAgent.model), false);
assert.equal(reserveActiveQuota({ provider: 'openrouter', windows: [], balances: [{ total: '1.00' }] }, [goAgent], costsOf)
  .activeAgents, 0);
const [goRank] = rankCandidates([{ ...goAgent, quality: 0.8, fit: 1, allowed: true, taskCost: 0 }], 1, 'opencode',
  { sharedQuotaPenalty: 0.15, paidCreditPenalty: 0.25, cheapModelBonus: 0.1 });
assert.deepEqual([goRank.account, goRank.sharedQuota, goRank.paidCreditPenalty], ['opencode-go', false, 0]);
const google = reserveActiveQuota({ provider: 'antigravity', balances: [], windows: agyWindows }, [
  { provider: 'antigravity', model: 'claude-opus-5-5' }], costsOf);
assert.equal(google.windows[1].remaining_percent, 100);

// Both Codex logins report provider "codex"; each instance reads and reserves only its own account.
const codexMain = { id: 'codex-main', provider: 'codex', balances: [], windows: [] };
const codexSecond = { id: 'codex-second', provider: 'codex', balances: [],
  windows: [{ remaining_percent: 0, duration_mins: 300 }] };
assert.equal(ownsUsage(codexMain, 'codex'), true);
assert.equal(ownsUsage(codexSecond, 'codex'), false);
assert.equal(ownsUsage(codexSecond, 'codex_second'), true);
assert.equal(ownsUsage(codexMain, 'codex_second'), false);
assert.equal(reserveActiveQuota(codexSecond, [{ provider: 'codex', model: 'gpt-6.1-sol' },
  { provider: 'codex_second', model: 'gpt-6.1-sol' }], costsOf).activeAgents, 1);
assert.equal(budget(codexSecond.windows).allowed(0), false);
// Older ai-usage snapshots without ids still match the main Codex login by provider.
assert.equal(ownsUsage({ provider: 'codex' }, 'codex'), true);
assert.equal(ownsUsage({ provider: 'codex' }, 'codex_second'), false);

// One model on two accounts shares one pooled verdict, so parallel work goes to the idler account.
const sol = (provider, quality, activeAgents, taskCost) => model(provider, { model: 'gpt-6.1-sol', quality, activeAgents, taskCost });
const spread = rankCandidates([sol('codex', 0.86, 1, 0.5), sol('codex_second', 0.84, 0, 0.6), model('antigravity', { quality: 0.849 })],
  3, 'claudeAgent', weights);
assert.equal(spread[0].provider, 'codex_second');
assert.equal(spread[0].quality, spread[1].quality);
assert.equal(spread[2].quality, 0.849);
// Equal load: the cheaper task still breaks the tie; a pooled copy never beats a genuinely better model.
assert.equal(pick([sol('codex', 0.86, 0, 0.5), sol('codex_second', 0.84, 0, 0.6)], 3).provider, 'codex');
assert.equal(pick([sol('codex', 0.84, 0, 0.5), sol('codex_second', 0.84, 0, 0.6), model('antigravity', { quality: 0.9 })], 3).provider,
  'antigravity');

// Task cost only breaks exact ties; any quality edge wins once the task fits.
const paced = (weekly, burn) => quotaBudget([{ remaining_percent: 20, duration_mins: 300, resets_at: now + 2 * 3600 },
  { remaining_percent: 60, duration_mins: 10080, resets_at: now + 6.5 * 86400 }], [], weekly, burn, 5, now);
const opusBudget = paced([0.5, 2, 6, 15], 1), haikuBudget = paced([0.1, 0.4, 1.2, 3], 0.2);
assert(opusBudget.cost(0) > 4 * haikuBudget.cost(0));
const asCandidate = (m, b, quality) => ({ provider: 'claudeAgent', model: m, fit: 0.95, quality, allowed: b.allowed(0),
  taskCost: b.cost(0) });
assert.equal(pick([asCandidate('opus', opusBudget, 0.8), asCandidate('haiku', haikuBudget, 0.8)], 0).model, 'haiku');
assert.equal(pick([asCandidate('opus', opusBudget, 0.803), asCandidate('haiku', haikuBudget, 0.8)], 0).model, 'opus');
// A cheap model wins a small quality gap, not a full Jev level; hard work halves the bonus.
const haiku = model('claude_third', { model: 'claude-haiku-5-5', burn: 0.2, quality: 0.72 });
const sonnet55 = model('claude_third', { model: 'claude-sonnet-5-5', burn: 0.6, quality: 0.75 });
assert.equal(pick([sonnet55, haiku], 0).model, 'claude-haiku-5-5');
assert.equal(pick([sonnet55, haiku], 2).model, 'claude-sonnet-5-5');
assert.equal(pick([{ ...sonnet55, quality: 1 }, { ...haiku, quality: 0.667 }], 0).model, 'claude-sonnet-5-5');
assert.equal(rankCandidates([model('codex', { burn: 3 })], 1, null, weights)[0].cheapModelBonus, 0);
// Eligibility uses the raw fit, never a rounded one.
assert.equal(rankCandidates([model('codex', { fit: MIN_FIT - 0.0004 })], 1, null, weights)[0].eligible, false);
assert.equal(rankCandidates([model('codex', { profiled: false })], 1, null, weights)[0].eligible, true);
assert.equal(rankCandidates([model('codex', { profiled: false, fit: 0.1 })], 1, null, weights)[0].eligible, false);

// Jev answers must be bounded and coherent before they may make anything eligible.
const answer = (score, probabilities = { 0: 0, 1: 0.1, 2: 0.6, 3: 0.3 }, confidence = 0.8) => ({ score, confidence, probabilities });
assert.equal(validAnswer(answer(2.2)), true);
assert.equal(validAnswer(answer(9, { 0: 0, 1: 0, 2: 1, 3: 1 })), false); // fit 2, quality 3
assert.equal(validAnswer(answer(-0.1)), false);
assert.equal(validAnswer(answer(2, undefined, 1.2)), false);
assert.equal(validAnswer(answer(2, { 1: 0.1, 2: 0.6, 3: 0.3 })), false);
assert.equal(validAnswer(answer(2, { 0: 0, 1: 0.1, 2: 0.2, 3: 0.2 })), false);
assert.equal(validAnswer(answer(2, { 0: -0.1, 1: 0.2, 2: 0.6, 3: 0.3 })), false);
assert.equal(validAnswer({ score: 1, confidence: 0.5 }, false), true);
assert.equal(validAnswer({ score: 1 }, false), false);

// A requested delegate must be routable now and within the margin of the selected best.
const ranked = rankCandidates([model('codex', { quality: 0.8 }), model('antigravity', { quality: 0.79 }),
  model('claudeAgent', { quality: 0.7 }), model('opencode', { allowed: false }), model('codex_second', { fit: 0.6 })], 1, 'claudeAgent', weights);
const check = (provider, m = provider) => validateTarget(ranked, { providerInstanceId: provider, model: m });
assert.deepEqual(check('codex'), { allowed: true, reason: 'selected winner' });
assert.equal(check('antigravity').allowed, true);
assert.equal(check('claudeAgent').allowed, false);
assert.match(check('opencode').reason, /quota/);
assert.match(check('codex_second').reason, /fit/);
assert.match(check('codex', 'gpt-other').reason, /exact active favorite/);

// A near-equal score cannot undo the cheaper selection on a size-0 typo task.
const reviewWindows = [{ remaining_percent: 55, duration_mins: 300, resets_at: now + 3 * 3600 },
  { remaining_percent: 85, duration_mins: 10080, resets_at: now + 6 * 86400 }];
const reviewOpus = quotaBudget(reviewWindows, [], [0.5, 2, 6, 15], 1, 5, now);
const reviewHaiku = quotaBudget(reviewWindows, [], [0.1, 0.4, 1.2, 3], 0.2, 5, now);
const nearTie = rankCandidates([asCandidate('opus', reviewOpus, 0.9),
  asCandidate('haiku', reviewHaiku, 0.9)], 0, 'codex', weights);
assert.equal(nearTie[0].model, 'haiku');
assert(nearTie[0].score - nearTie[1].score < 0.02);
assert.match(validateTarget(nearTie, { providerInstanceId: 'claudeAgent', model: 'opus' }).reason, /costs more/);
const cheaperAlternative = rankCandidates([asCandidate('opus', reviewOpus, 0.91),
  asCandidate('haiku', reviewHaiku, 0.9)], 0, 'codex', weights);
assert.equal(cheaperAlternative[0].model, 'opus');
assert.equal(validateTarget(cheaperAlternative, { providerInstanceId: 'claudeAgent', model: 'haiku' }).allowed, true);

const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'model-route-check-test-'));
try {
  // Exercise route.mjs itself: a failed refresh must not bypass retained account limits.
  const models = ['claude-opus-5-5', 'claude-fable-5-1'];
  for (const rel of ['.t3/userdata', '.local/state/ai-usage']) fs.mkdirSync(path.join(dir, rel), { recursive: true });
  writeRouterJson({ pool: models.map(model => ({ providerInstanceId: 'claudeAgent', model, enabled: true })),
    accounts: { claude: { instances: ['claudeAgent'], usageProvider: 'claude', usageId: 'claude-main' } },
    usageStatePath: path.join(dir, '.local/state/ai-usage/state.json') });
  fs.writeFileSync(path.join(dir, '.t3/userdata/settings.json'), '{}');
  fs.writeFileSync(path.join(dir, 'model-route-codex-costs-v4.json'), '{"version":4,"costs":{}}');
  const cachedAccount = { provider: 'claude', plan: 'Max 5x', status: 'stale', error: 'quota HTTP 429',
    updated_at: Date.now() / 1000 - 2400, balances: [], windows: [
      { key: 'weekly_all', label: 'Week', remaining_percent: 0.05, duration_mins: 10080,
        resets_at: Date.now() / 1000 + 30 * 3600 },
      { key: 'weekly_scoped:fable', label: 'Fable', scope: 'Fable', remaining_percent: 91, duration_mins: 10080 }] };
  const checkRoute = account => {
    fs.writeFileSync(path.join(dir, '.local/state/ai-usage/state.json'), JSON.stringify({ accounts: [account] }));
    return execFileSync(process.execPath, ['--input-type=module', '-e', `
      import os from 'node:os';
      os.homedir = () => process.cwd();
      process.argv = [process.execPath, 'route.mjs', '--check'];
      await import(${JSON.stringify(new URL('./route.mjs', import.meta.url).href)});
    `], { cwd: dir, env: { ...process.env, XDG_RUNTIME_DIR: dir, XDG_STATE_HOME: path.join(dir, 'test-state'),
      MODEL_ROUTE_ROUTER_JSON: routerFile }, encoding: 'utf8' });
  };
  for (const status of ['ok', 'stale', 'error']) {
    const output = checkRoute({ ...cachedAccount, status });
    for (const model of models) assert.match(output.split(`claudeAgent/${model}\n`)[1], / 0% left[^]*?allowed sizes: none/);
    if (status !== 'ok') assert.match(output, /cached quota/);
  }
  const expired = { ...cachedAccount, windows: cachedAccount.windows.map(w => ({ ...w, resets_at: 1 })) };
  assert.match(checkRoute(expired), / 0% left[^]*?allowed sizes: none/);
  assert.match(checkRoute({ ...cachedAccount, windows: [], balances: [] }), /quota unknown[^]*?allowed sizes: 0,1,2,3/);
  writeRouterJson({ pool: [{ providerInstanceId: 'claudeAgent_openrouter', model: 'meta/muse-spark-1.3-contributor[1m]', enabled: true }],
    accounts: { openrouter: { instances: ['claudeAgent_openrouter'], usageProvider: 'openrouter' } },
    usageStatePath: path.join(dir, '.local/state/ai-usage/state.json') });
  assert.match(checkRoute({ ...cachedAccount, provider: 'openrouter', windows: [], balances: [{ total: '0' }] }),
    /\$0\.00 left[^]*?allowed sizes: none/);
} finally { fs.rmSync(dir, { recursive: true }); }
// Actual CLI with isolated home, mocked Jev fetch and no paid calls.
const cli = (dir, args, jev, input) => {
  // Each case supplies a different remote answer; cache reuse has its own integration suite.
  fs.rmSync(path.join(dir, 'model-route-jev-cache.json'), { force: true });
  fs.rmSync(path.join(dir, 'model-route-jev-cache'), { recursive: true, force: true });
  return spawnSync(process.execPath, ['--input-type=module', '-e', `
  import fs from 'node:fs';
  import os from 'node:os';
  os.homedir = () => process.cwd();
  globalThis.fetch = async (_url, init) => {
    fs.writeFileSync('jev-request.json', init.body);
    return { ok: true, json: async () => JSON.parse(process.env.MOCK_JEV) };
  };
  process.argv = [process.execPath, 'route.mjs', ...JSON.parse(process.env.ROUTE_ARGS)];
  await import(${JSON.stringify(new URL('./route.mjs', import.meta.url).href)});
`], { cwd: dir, timeout: 10_000, input, encoding: 'utf8', env: { ...process.env, XDG_RUNTIME_DIR: dir, XDG_STATE_HOME: path.join(dir, 'test-state'), JEV_OPENROUTER_API_KEY: 'test',
  MOCK_JEV: JSON.stringify(jev), ROUTE_ARGS: JSON.stringify(args), MODEL_ROUTE_ROUTER_JSON: routerFile } });
};
const routeDir = fs.mkdtempSync(path.join(os.tmpdir(), 'model-route-cli-test-'));
try {
  for (const rel of ['.t3/userdata', '.local/state/ai-usage']) fs.mkdirSync(path.join(routeDir, rel), { recursive: true });
  fs.mkdirSync(path.join(routeDir, 'test-state/model-route'), { recursive: true });
  fs.copyFileSync(new URL('./catalog.fixture.json', import.meta.url), path.join(routeDir, 'test-state/model-route/catalog.json'));
  const setPool = entries => writeRouterJson({ pool: entries.map(([provider, model]) => ({ providerInstanceId: provider, model, enabled: true })),
    accounts: allAccounts, usageStatePath: path.join(routeDir, '.local/state/ai-usage/state.json') });
  setPool([['claudeAgent', 'claude-opus-5-5'], ['claudeAgent', 'claude-haiku-5-5']]);
  fs.writeFileSync(path.join(routeDir, '.t3/userdata/settings.json'), '{}');
  const t = Date.now() / 1000;
  fs.writeFileSync(path.join(routeDir, '.local/state/ai-usage/state.json'), JSON.stringify({ accounts: [{
    provider: 'claude', plan: 'Max 5x', status: 'ok', updated_at: t, balances: [], windows: [
      { key: 'session', label: '5h', remaining_percent: 20, duration_mins: 300, resets_at: t + 2 * 3600 },
      { key: 'weekly_all', label: 'Week', remaining_percent: 60, duration_mins: 10080, resets_at: t + 6.5 * 86400 }] }] }));
  const level = (score, fit = 0.95) => ({ score, confidence: 0.9, probabilities: { 0: 0, 1: 1 - fit, 2: fit - 0.2, 3: 0.2 } });
  const jev = (opus, haiku, size = { score: 0, confidence: 0.9 }) => ({ answers: { size, difficulty: { score: 0, confidence: 0.9 },
    claudeagent_claude_opus_5_5: opus, claudeagent_claude_haiku_5_5: haiku } });
  const run = (args, answers, input) => {
    const r = cli(routeDir, args, answers, input);
    assert.equal(r.status, 0, r.stderr);
    return JSON.parse(r.stdout);
  };
  const fixture = JSON.parse(fs.readFileSync(new URL('./catalog.fixture.json', import.meta.url), 'utf8'));
  const installed = cli(routeDir, ['--catalog'], {}, JSON.stringify(fixture));
  assert.equal(installed.status, 0, installed.stderr);
  const catalogFile = path.join(routeDir, 'test-state/model-route/catalog.json');
  assert.deepEqual(JSON.parse(fs.readFileSync(catalogFile, 'utf8')).providers, fixture.providers);
  assert.equal(fs.statSync(catalogFile).mode & 0o777, 0o600);
  const savedCatalog = fs.readFileSync(catalogFile, 'utf8');
  assert.notEqual(cli(routeDir, ['--catalog'], {}, '{}').status, 0);
  assert.equal(fs.readFileSync(catalogFile, 'utf8'), savedCatalog);
  const typo = jev(level(2.4), level(2.4));
  const brief = 'Fix the typo "recieve" in README.md';
  let out = run(['--source', 'claudeAgent', brief], typo);
  assert.equal(out.target.model, 'claude-haiku-5-5');
  assert.equal(out.validation, undefined);
  assert.equal(run(['--source', 'claudeAgent', brief], jev(level(2.9), level(2.4))).target.model, 'claude-opus-5-5');
  out = run(['--source', 'claudeAgent', '--validate-target', 'claudeAgent', 'claude-opus-5-5', brief], typo);
  assert.deepEqual(out.requestedTarget, { providerInstanceId: 'claudeAgent', model: 'claude-opus-5-5', options: { effort: 'low' } });
  assert.equal(out.validation.allowed, false);
  assert.equal(out.target.model, 'claude-haiku-5-5');
  out = run(['--validate-target', 'claudeAgent', 'claude-haiku-5-5', '--source', 'claudeAgent', brief], typo);
  assert.deepEqual(out.validation, { allowed: true, reason: 'selected winner' });
  assert.match(run(['--source', 'claudeAgent', '--validate-target', 'claudeAgent', 'claude-sonnet-5-5', brief], typo)
    .validation.reason, /exact active favorite/);
  // Rounded fit 0.700 is still below the threshold.
  out = run(['--source', 'claudeAgent', brief], jev(level(2.409), level(2.4, 0.6996)));
  assert.equal(out.target.model, 'claude-opus-5-5');
  assert.equal(out.ranking.find(r => r.model === 'claude-haiku-5-5').eligible, false);
  // Size confidence counts toward low confidence; the warning names the top two eligible candidates.
  out = run(['--source', 'claudeAgent', brief], jev(level(2.4), level(2.4), { score: 0, confidence: 0.3 }));
  assert.equal(out.jevConfidence, 0.3);
  assert.deepEqual(out.confidenceWarning.topTwo.map(r => r.model), ['claude-haiku-5-5', 'claude-opus-5-5']);
  assert.equal(run(['--source', 'claudeAgent', brief], typo).confidenceWarning, undefined);
  // Unknown instances route without protection; codex_second is its own account.
  out = run(['--source', 'some_new_instance', brief], typo);
  assert.equal(out.sourceAccount, null);
  assert.match(out.sourceWarning, /no shared-quota protection/);
  assert.equal(run(['--source', 'codex_second', brief], typo).sourceAccount, 'codex_second');
  // The full brief reaches Jev, including from stdin.
  const long = `${'Inspect every module and report findings. '.repeat(400)}END`;
  run(['--source', 'claudeAgent', '-'], typo, long);
  assert.equal(JSON.parse(fs.readFileSync(path.join(routeDir, 'jev-request.json'), 'utf8')).state.task, long);
  // Malformed answers and missing briefs fail loudly.
  for (const bad of [jev(level(2.4), { ...level(2.4), probabilities: { 0: 0, 1: 0, 2: 1, 3: 1 } }),
    jev(level(2.4), { ...level(2.4), score: 3.5 }), jev(level(2.4), { ...level(2.4), confidence: 1.5 }),
    jev(level(2.4), level(2.4), { score: 0 })]) {
    const r = cli(routeDir, ['--source', 'claudeAgent', brief], bad);
    assert.equal(r.status, 1);
    assert.equal(r.stdout, '');
  }
  for (const args of [['--source', 'claudeAgent'], ['--validate-target', 'claudeAgent'], ['--bogus', brief]])
    assert.equal(cli(routeDir, args, typo).status, 2);

  // Quota behavior is covered above; option cases must remain eligible at every size.
  fs.writeFileSync(path.join(routeDir, '.local/state/ai-usage/state.json'), JSON.stringify({ accounts: [] }));
  // Effort follows difficulty, independently of size, and uses the target harness's option id.
  for (const [provider, model, key, levels] of [
    ['codex', 'gpt-6.1-sol', 'reasoningEffort', ['low', 'medium', 'high', 'xhigh']],
    ['codex_second', 'gpt-6.1-sol', 'reasoningEffort', ['low', 'medium', 'high', 'xhigh']],
    ['claudeAgent', 'claude-opus-5-5', 'effort', ['low', 'medium', 'high', 'xhigh']],
    ['opencode', 'anthropic/claude-opus-5-5', 'variant', ['low', 'low', 'high', 'xhigh']],
    ['claudeAgent', 'claude-opus-4-5', 'effort', ['low', 'medium', 'high', 'high']],
    ['claudeAgent_openrouter', 'claude-opus-5-5', 'effort', ['low', 'medium', 'high', 'xhigh']],
  ]) {
    setPool([[provider, model]]);
    for (const [difficulty, effort] of levels.entries()) {
      const answers = { answers: { size: { score: 3 - difficulty, confidence: 0.9 },
        difficulty: { score: difficulty, confidence: 0.9 },
        [`${provider}/${model}`.replace(/[^a-zA-Z0-9]+/g, '_').toLowerCase()]: level(2.9) } };
      const args = ['--source', 'codex', '--validate-target', provider, model, brief];
      out = run(args, answers);
      const expected = { providerInstanceId: provider, model, options: { [key]: effort } };
      assert.deepEqual(out.target, expected, `${provider}: difficulty ${difficulty}`);
      assert.deepEqual(out.requestedTarget, expected);
      assert.equal(out.validation.allowed, true);
    }
  }
  const agy = { providerInstanceId: 'antigravity', model: 'gemini-3.8-flash-high' };
  setPool([['antigravity', agy.model], ['codex_second', 'gpt-6.1-sol']]);
  const crossProvider = { answers: { size: { score: 0, confidence: 0.9 }, difficulty: { score: 3, confidence: 0.9 },
    antigravity_gemini_3_8_flash_high: level(2.9), codex_second_gpt_6_1_sol: level(2.1) } };
  out = run(['--source', 'codex', '--validate-target', 'codex_second', 'gpt-6.1-sol', brief], crossProvider);
  assert.deepEqual(out.target, agy);
  assert.deepEqual(out.requestedTarget, { providerInstanceId: 'codex_second', model: 'gpt-6.1-sol', options: { reasoningEffort: 'xhigh' } });
  assert.equal(out.validation.allowed, false);
  out = run(['--source', 'codex', '--validate-target', agy.providerInstanceId, agy.model, brief], crossProvider);
  assert.deepEqual(out.target, agy);
  assert.deepEqual(out.requestedTarget, agy);
  assert.equal(out.validation.allowed, true);
  // A favorite whose reasoning cannot be applied must not prevent a viable fallback or crash validation.
  const custom = 'meta/muse-spark-1.3-contributor[1m]';
  const catalogPath = path.join(routeDir, 'test-state/model-route/catalog.json');
  const customCatalog = JSON.parse(fs.readFileSync(catalogPath, 'utf8'));
  customCatalog.providers.find(p => p.providerInstanceId === 'claudeAgent_openrouter').models.push({
    id: custom, options: [{ id: 'effort', type: 'select', options: [{ id: 'high' }] }] });
  fs.writeFileSync(catalogPath, JSON.stringify(customCatalog));
  setPool([['claudeAgent_openrouter', custom], ['codex_second', 'gpt-6.1-sol']]);
  const unsuitable = { answers: { size: { score: 0, confidence: 0.9 }, difficulty: { score: 2, confidence: 0.9 },
    claudeagent_openrouter_meta_muse_spark_1_3_contributor_1m_: level(3), codex_second_gpt_6_1_sol: level(2) } };
  out = run(['--source', 'codex', '--validate-target', 'claudeAgent_openrouter', custom, brief], unsuitable);
  assert.equal(out.target.providerInstanceId, 'codex_second');
  assert.equal(out.validation.allowed, false);
  assert.match(out.validation.reason, /cannot apply/);
  assert.equal(out.ranking.find(r => r.model === custom).eligible, false);
  setPool([['claudeAgent_openrouter', custom]]);
  out = run(['--source', 'codex', '--validate-target', 'claudeAgent_openrouter', custom, brief], unsuitable);
  assert.equal(out.target, null);
  assert.match(out.validation.reason, /cannot apply/);

  const opaqueCatalog = JSON.parse(fs.readFileSync(catalogPath, 'utf8'));
  opaqueCatalog.providers.find(p => p.providerInstanceId === 'opencode').models[0].options = [
    { id: 'variant', type: 'select', options: [{ id: 'quick' }, { id: 'deep' }] }];
  fs.writeFileSync(catalogPath, JSON.stringify(opaqueCatalog));
  setPool([['opencode', 'anthropic/claude-opus-5-5'], ['codex_second', 'gpt-6.1-sol']]);
  const unmappable = { answers: { size: { score: 0, confidence: 0.9 }, difficulty: { score: 2, confidence: 0.9 },
    opencode_anthropic_claude_opus_5_5: level(3), codex_second_gpt_6_1_sol: level(2) } };
  out = run(['--source', 'codex', '--validate-target', 'opencode', 'anthropic/claude-opus-5-5', brief], unmappable);
  assert.equal(out.target.providerInstanceId, 'codex_second');
  assert.match(out.validation.reason, /cannot map/);
} finally { fs.rmSync(routeDir, { recursive: true }); }
console.log('routing policy checks passed');

// Each Claude login is its own ai-usage account; the shared provider name must not match the wrong one.
const claudeMain = { provider: 'claude', id: 'claude-main' }, claudeThird = { provider: 'claude', id: 'claude-third' };
assert.equal(ownsUsage(claudeMain, 'claudeAgent'), true);
assert.equal(ownsUsage(claudeThird, 'claudeAgent'), false);
assert.equal(ownsUsage(claudeThird, 'claude_third'), true);
assert.equal(ownsUsage(claudeMain, 'claude_third'), false);

// User rule 2026-10-17: noReview families never take audits or reviews, not even as a user choice.
for (const brief of ['Read-only design and UI/UX audit', 'Code review of the diff', 'Reviewer: check this',
  'Аудит дизайну mini-app', 'зроби рев\'ю коду', 'сделай ревью'])
  assert.ok(REVIEW_TASK.test(brief), brief);
for (const brief of ['Preview the landing page', 'Implement the cart', 'Перевір вёрстку'])
  assert.ok(!REVIEW_TASK.test(brief), brief);
const banned = rankCandidates([
  { provider: 'antigravity', model: 'gemini-4-argon-medium', quality: 1, fit: 1, allowed: true, policyBan: NO_REVIEW_BAN },
  { provider: 'claude', model: 'claude-sonnet-5-5', quality: 0.5, fit: 1, allowed: true },
], 1, undefined, weights);
assert.equal(banned.find(c => c.model === 'gemini-4-argon-medium').eligible, false);
assert.equal(banned.find(c => c.eligible).model, 'claude-sonnet-5-5');
assert.deepEqual(validateTarget(banned, { providerInstanceId: 'antigravity', model: 'gemini-4-argon-medium' }),
  { allowed: false, reason: NO_REVIEW_BAN });
