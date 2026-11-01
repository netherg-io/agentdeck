#!/usr/bin/env node
// Jev judges capability; local policy preserves the orchestrator's shared quota for routine work.
// Usage: route.mjs [--source <instance>] [--validate-target <provider> <model>] "<task brief>"|- | route.mjs --check
import { createHash } from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { MIN_FIT, NO_REVIEW_BAN, REVIEW_TASK, enabledFavorites, ownsUsage, quotaWindows, reserveActiveQuota, quotaBudget, rankCandidates,
  validAnswer, validateTarget, routerConfig } from './policy.mjs';
import { readActiveAgents } from './load.mjs';
import { saveCatalog, readCatalog, reasoningTarget } from './reasoning.mjs';

const home = os.homedir();
const here = path.dirname(new URL(import.meta.url).pathname);
const expandUser = p => p.startsWith('~/') ? path.join(home, p.slice(2)) : p;
const runtime = process.env.XDG_RUNTIME_DIR ?? os.tmpdir();
const readJson = f => JSON.parse(fs.readFileSync(f, 'utf8'));
const settings = readJson(path.join(home, '.t3/userdata/settings.json'));

const SIZES = ['a quick answer or a few-line edit', 'a focused change or review of a few files', 'a multi-file feature, refactor or review', 'a long autonomous session over a large codebase'];
const DIFFICULTY = ['mechanical work, lookup, summary, rename, formatting or a simple script', 'ordinary bounded implementation, tests, exploration or review with clear requirements', 'hard debugging, ambiguous architecture, security or specialist reasoning', 'exceptionally difficult work requiring the strongest specialist'];
const CAPABILITY = ['Cannot perform the required task or input modality.', 'Significant capability gaps; unreliable for the required task.', 'Adequate capabilities for a good result with normal checks.', 'Specific relevant strengths should produce a substantially better result on THIS task. A merely stronger model with no practical advantage does not qualify.'];
// Explicit source wins; otherwise inspect only executable names in the calling process chain.
function detectSource() {
  for (let pid = process.ppid, i = 0; pid > 1 && i < 12; i++) {
    try {
      const cmd = fs.readFileSync(`/proc/${pid}/cmdline`, 'utf8').split('\0');
      const names = cmd.slice(0, 2).map(c => path.basename(c));
      if (names.includes('codex')) {
        // A second Codex instance differs only by its shadow CODEX_HOME. Only that variable is read.
        let codexHome;
        try {
          codexHome = fs.readFileSync(`/proc/${pid}/environ`, 'utf8').split('\0')
            .find(e => e.startsWith('CODEX_HOME='))?.slice(11);
        } catch {}
        return Object.entries(settings.providerInstances ?? {}).find(([, v]) => codexHome
          && v.driver === 'codex' && v.config?.shadowHomePath === codexHome)?.[0] ?? 'codex';
      }
      if (names.includes('claude')) return Object.entries(settings.providerInstances ?? {}).find(([, v]) =>
        v.driver === 'claudeAgent' && process.env.CLAUDE_CONFIG_DIR && v.config?.homePath === process.env.CLAUDE_CONFIG_DIR)?.[0]
        ?? ((process.env.CLAUDE_CONFIG_DIR ?? '').includes('.claude-openrouter') ? 'claudeAgent_openrouter' : 'claudeAgent');
      if (names.includes('opencode')) return 'opencode';
      if (names.includes('agy') || cmd[0]?.includes('/.t3/tools/antigravity-acp/')) return 'antigravity';
      pid = Number(fs.readFileSync(`/proc/${pid}/stat`, 'utf8').split(') ')[1].split(' ')[1]);
    } catch { break; }
  }
  return null;
}
const USAGE = 'usage: route.mjs [--source <provider instance>] [--validate-target <provider instance> <model>] "<task brief>" | - (brief on stdin) | --check | --catalog (orchestrator_capabilities structuredContent on stdin)';
const argv = process.argv.slice(2);
if (argv.length === 1 && argv[0] === '--catalog') {
  saveCatalog(JSON.parse(fs.readFileSync(0, 'utf8')));
  console.log('model option catalog updated');
  process.exit(0);
}
let source, requested;
while (argv[0]?.startsWith('--') && argv[0] !== '--check') {
  const flag = argv.shift();
  if (flag === '--') break;
  if (flag === '--source' && argv[0]) source = argv.shift();
  else if (flag === '--validate-target' && argv[0] && argv[1]) requested = { providerInstanceId: argv.shift(), model: argv.shift() };
  else { console.error(USAGE); process.exit(2); }
}
source ??= detectSource();
// Pool, account maps and usage path come from the agentdeck router JSON
// (`agentdeck router json` refreshes it); a missing or invalid file fails closed.
let routerJson;
try {
  routerJson = routerConfig();
} catch (e) {
  console.error(e.message);
  process.exit(1);
}
// Unknown instances are routed without shared-quota protection instead of failing the caller's hook.
const sourceWarning = source && !Object.hasOwn(routerJson.ACCOUNT, source)
  ? `unknown source instance "${source}": no shared-quota protection` : undefined;

const roster = readJson(path.join(here, 'roster.json'));
const familyOf = model => roster.families.find(r => new RegExp(r.match, 'i').test(model));

// Codex logs quota percent with every token count, so its per-session cost is measured per model and raw
// plan_type: sizes 0..3 map to the p25/p50/p75/p90 session. Other providers fall back to roster.plans x burn.
// Sessions of both Codex accounts share one log directory, so each event belongs to a stream: plan_type plus
// the weekly window's resets_at, which jitters by seconds but jumps on reset. Plus logs its 5-hour window as
// primary, so only the 10080-minute window counts. Concurrent sessions make per-session deltas unattributable,
// and sessions that never overlap are mostly short ones, so a percent-per-token rate is learned per plan+model
// (from whole clean streams, else from quiet intervals below) and applied to every session's tokens.
// Quiet intervals are those where the previous event on the stream was the same session.
// Parallel sessions publish quota snapshots with a delay, so such an interval is also skipped when another
// session logged on the stream within QUIET_MS before it. Tokens are deltas of the cumulative total_token_usage:
// about 8% of token_count events repeat the previous total with a non-zero last_token_usage.
// ponytail: attribution is approximate (a delayed snapshot older than QUIET_MS still leaks in) and one rate per
// plan+model ignores cache-hit mix; weight token kinds if measured costs drift from quota.
const COST_VERSION = 7;
const RESET_JITTER = 600;
const QUIET_MS = 2 * 60e3;
const MIN_RATE_PERCENT = 10;
// Some sessions report stale snapshots (after an in-window partial reset the log interleaves old and new
// levels), so a jump larger than one API call can plausibly cost is noise, not consumption.
const MAX_STEP_PERCENT = 2;
function codexCosts() {
  const cache = path.join(runtime, `model-route-codex-costs-v${COST_VERSION}.json`);
  if (fs.existsSync(cache) && Date.now() - fs.statSync(cache).mtimeMs < 6 * 3600e3) {
    try { const c = readJson(cache); if (c.version === COST_VERSION) return c; } catch {}
  }
  const root = path.join(home, '.codex/sessions');
  const events = [], fiveHour = {};
  for (const rel of fs.existsSync(root) ? fs.globSync('*/*/*/*.jsonl', { cwd: root }) : []) {
    const file = path.join(root, rel);
    let text;
    try {
      if (Date.now() - fs.statSync(file).mtimeMs > 21 * 86400e3) continue;
      text = fs.readFileSync(file, 'utf8');
    } catch { continue; } // An unreadable/oversized log must not disable routing for every task.
    let model, total = 0, short;
    for (const line of text.split('\n')) {
      if (!line.includes('"turn_context"') && !line.includes('"token_count"')) continue;
      let o;
      try { o = JSON.parse(line); } catch { continue; }
      if (o.type === 'turn_context') { model = o.payload?.model; continue; }
      const rl = o.payload?.rate_limits;
      const week = [rl?.primary, rl?.secondary].find(w => w?.window_minutes === 10080);
      // Within one session, 5-hour and weekly deltas share every call, so their ratio needs no attribution.
      const hour5 = [rl?.primary, rl?.secondary].find(w => w?.window_minutes === 300);
      if (week && hour5 && rl.plan_type) {
        const dw = week.used_percent - short?.week, dh = hour5.used_percent - short?.hour5;
        if (short?.resets === week.resets_at && Math.abs(short.hour5Resets - hour5.resets_at) <= RESET_JITTER
          && dw >= 0 && dh >= 0 && dw <= MAX_STEP_PERCENT && dh <= 5 * MAX_STEP_PERCENT) {
          const f = fiveHour[String(rl.plan_type).toLowerCase()] ??= { week: 0, hour5: 0 };
          f.week += dw; f.hour5 += dh;
        }
        short = { week: week.used_percent, hour5: hour5.used_percent, resets: week.resets_at, hour5Resets: hour5.resets_at };
      }
      const t = Date.parse(o.timestamp);
      const cumulative = o.payload?.info?.total_token_usage?.total_tokens;
      let tokens = Number(o.payload?.info?.last_token_usage?.total_tokens) || 0;
      if (Number.isFinite(cumulative)) { tokens = Math.max(0, cumulative - total); total = cumulative; }
      if (o.payload?.type === 'token_count' && week && model && rl.plan_type && Number.isFinite(t)
        && Number.isFinite(week.used_percent) && Number.isFinite(week.resets_at))
        events.push({ t, file, model, plan: String(rl.plan_type).toLowerCase(), used: week.used_percent, resets: week.resets_at,
          tokens });
    }
  }
  events.sort((a, b) => a.t - b.t);
  const windows = [];
  for (const e of events) {
    let w = windows.find(w => w.plan === e.plan && Math.abs(w.resets - e.resets) <= RESET_JITTER);
    if (!w) windows.push(w = { plan: e.plan, events: [] });
    w.resets = e.resets;
    w.events.push(e);
  }
  const sessions = {}, rates = {};
  for (const w of windows) w.events.forEach((e, i) => {
    // Time of the latest other-session event before this session's current run on the stream.
    if (w.events[i - 1]?.file !== e.file) w.foreignT = w.events[i - 1]?.t ?? -Infinity;
    const s = sessions[e.file] ??= { keys: new Set(), tokens: 0 };
    const key = `${e.plan}|${e.model}`.toLowerCase();
    s.keys.add(key);
    s.tokens += e.tokens;
    const prev = w.events[i - 1];
    if (prev?.file === e.file && prev.model === e.model && prev.t - w.foreignT > QUIET_MS && e.used >= prev.used && e.used - prev.used <= MAX_STEP_PERCENT) {
      const r = rates[key] ??= { used: 0, tokens: 0 };
      r.used += e.used - prev.used;
      r.tokens += e.tokens;
    }
  });
  // Quiet intervals miss or misattribute consumption around parallel sessions' delayed snapshots: on Plus they
  // understated the rate about 8% against whole weeks (2026-10-14). A stream with one model and no drop below its running maximum is one
  // account, so its whole consumption over all its tokens is exact. Merged accounts (both Plus logins share
  // reset times) or stale partial-reset snapshots make a stream unclean; then the quiet-interval rate is used.
  const streamRates = {};
  for (const w of windows) {
    const models = new Set(w.events.map(e => e.model));
    let hi = w.events[0].used, used = 0, tokens = 0;
    for (const e of w.events.slice(1)) {
      if (e.used < hi - MAX_STEP_PERCENT) { used = NaN; break; }
      const d = Math.max(0, e.used - hi);
      // Larger jumps are usage outside the logs (other machines, gaps); neither side of it is attributable.
      if (d <= MAX_STEP_PERCENT) { used += d; tokens += e.tokens; }
      hi = Math.max(hi, e.used);
    }
    if (models.size !== 1 || !Number.isFinite(used)) continue;
    const r = streamRates[`${w.plan}|${[...models][0]}`.toLowerCase()] ??= { used: 0, tokens: 0 };
    r.used += used; r.tokens += tokens;
  }
  for (const [key, r] of Object.entries(streamRates)) if (r.used >= MIN_RATE_PERCENT && r.tokens) rates[key] = r;
  const groups = {};
  for (const s of Object.values(sessions)) if (s.keys.size === 1) (groups[[...s.keys][0]] ??= []).push(s.tokens);
  const costs = {};
  for (const [key, v] of Object.entries(groups)) {
    const r = rates[key];
    // Percent resolution is 1 point: require enough attributable consumption for a stable rate.
    if (v.length < 10 || !r || r.used < MIN_RATE_PERCENT || !r.tokens) continue;
    v.sort((x, y) => x - y);
    costs[key] = { n: v.length, ratePercent: r.used, pct: [0.25, 0.5, 0.75, 0.9]
      .map(q => +(v[Math.floor(v.length * q)] * r.used / r.tokens).toFixed(2)) };
  }
  // 5-hour percent per weekly percent, per plan; the 5x fallback underestimated Plus at about 6.5x.
  const shortFactor = Object.fromEntries(Object.entries(fiveHour).filter(([, f]) => f.week >= MIN_RATE_PERCENT)
    .map(([plan, f]) => [plan, +(f.hour5 / f.week).toFixed(2)]));
  const result = { version: COST_VERSION, costs, shortFactor };
  fs.writeFileSync(cache, JSON.stringify(result));
  return result;
}

let usage = readJson(expandUser(routerJson.usageStatePath ?? '~/.local/state/ai-usage/state.json'));
let activeLoad = readActiveAgents();
const measured = codexCosts();

function modelCosts(acc, provider, model) {
  const normalized = /^(opus|sonnet|haiku|fable)$/.test(model ?? '') ? `claude-${model}` : model ?? '';
  const burn = familyOf(normalized)?.burn ?? Math.max(1, ...roster.families.map(f => f.burn));
  const plan = acc.provider === 'codex' ? roster.codexPlanTypes[acc.plan] : undefined;
  const m = plan ? measured.costs[`${plan}|${model}`.toLowerCase()] : undefined;
  // ai-usage labels plans inconsistently ("pro" vs "Pro"); match the roster case-insensitively.
  const plans = roster.plans[acc.provider] ?? {};
  const planCost = plans[Object.keys(plans).find(k => k.toLowerCase() === String(acc.plan).toLowerCase())];
  const weekly = m ? m.pct : (planCost ?? roster.plans['*']).map(c => c * burn);
  const basis = m ? `measured over ${m.n} sessions on ${acc.plan}` : planCost ? `${acc.plan} estimate × burn ${burn}` : `unknown plan "${acc.plan}", conservative estimate`;
  return { burn, weekly, basis };
}

// Returns allowed(size) and a human note; an unknown account is allowed but flagged.
function quota(provider, model) {
  let acc = usage.accounts.find(a => ownsUsage(a, provider, model));
  const factor = acc?.provider === 'codex' && measured.shortFactor?.[roster.codexPlanTypes[acc.plan]];
  if (factor) acc = { ...acc, windows: acc.windows.map(w => w.duration_mins === 300 ? { ...w, costFactor: factor } : w) };
  // A failed refresh cannot replenish the last known quota or balance.
  if (!acc || !(acc.windows.length || acc.balances.length)) return { allowed: () => true, cost: () => null,
    note: `quota unknown (${acc?.error ?? 'no ai-usage account'})` };
  const { burn, weekly, basis } = modelCosts(acc, provider, model);
  const reservePercent = familyOf(model)?.spendToZero ? 0 : roster.routing.weeklyReservePercent;
  const reserved = reserveActiveQuota(acc, activeLoad.agents, (p, m) => modelCosts(acc, p, m));
  const windows = quotaWindows(provider, model, reserved.windows);
  const budget = quotaBudget(windows, reserved.balances, weekly, burn, reservePercent);
  const staleMin = Math.round((Date.now() / 1000 - acc.updated_at) / 60);
  return {
    ...(windows.length && { weekly, basis }),
    ...budget, activeAgents: reserved.activeAgents,
    note: [...windows.map(w => `${w.label} ${Math.round(w.remaining_percent)}% left`
      + (w.pending ? ` after ~${w.pending.toFixed(1)}% active-work reserve` : '')
      + (Number.isFinite(w.resets_at) ? `; resets in ${Math.max(0, (w.resets_at - Date.now() / 1000) / 3600).toFixed(1)}h` : '')),
      ...reserved.balances.map(b => `$${b.toFixed(2)} left`
        + (reserved.pendingDollars ? ` after ~$${reserved.pendingDollars.toFixed(2)} active-work reserve` : ''))].join(', ')
      + ` (${reserved.activeAgents} active agents${activeLoad.known ? '' : '; count unknown'})`
      + (reservePercent && windows.some(w => w.duration_mins >= 10080) ? ` (up to ${reservePercent}% weekly reserve)` : '')
      + (acc.status !== 'ok' ? ` (cached quota; ${acc.error ?? acc.status})` : '')
      + (acc.updated_at > Date.now() / 1000 ? ' (quota timestamp in future)' : '')
      + (staleMin > 30 ? ` (ai-usage stale ${staleMin} min)` : ''),
  };
}

// The pool is agentdeck's own; T3 favorites stay UI-only and are never read.
const configured = enabledFavorites(routerJson.pool
  .filter(e => e.enabled !== false)
  .map(e => ({ provider: e.providerInstanceId, model: e.model, ...(e.options && { options: e.options }) })), settings);
// Not `opencode models`: on a cold OpenCode location it prints nothing, then a list taken before
// enabled_providers/plugins settle (Zen, no opencode-go). T3's saved catalog is what delegate_task accepts.
const opencodeModels = new Set(configured.some(f => f.provider === 'opencode')
  ? readCatalog().providers.find(p => p.providerInstanceId === 'opencode')?.models.map(m => m.id) : []);
const favorites = configured
  .filter(f => f.provider !== 'opencode' || opencodeModels.has(f.model))
  .map(f => ({ ...f, profile: familyOf(f.model)?.profile, quota: quota(f.provider, f.model) }));
const unprofiled = favorites.filter(f => !f.profile).map(f => `${f.provider}/${f.model}`);

if (argv[0] === '--check') {
  console.log(`source: ${source ?? 'unknown (use --source to preserve orchestrator quota)'}`);
  if (!activeLoad.known) console.log(activeLoad.note);
  for (const f of favorites) {
    const q = f.quota;
    console.log(`${f.profile ? 'ok' : 'UNPROFILED'}  ${f.provider}/${f.model}\n    left: ${q.note}`
      + (q.weekly ? `\n    weekly % per size 0..3: ${q.weekly.map(c => +c.toFixed(2)).join(' / ')} (${q.basis})` : '')
      + `\n    allowed sizes: ${[0, 1, 2, 3].filter(s => q.allowed(s)).join(',') || 'none'}`);
  }
  process.exit(unprofiled.length ? 1 : 0);
}

const hintOf = model => [roster.briefHint, familyOf(model ?? '')?.briefHint].filter(Boolean).join('\n\n');
// `prepare` routes the bare brief and only then appends the winner's hint; target validation receives the brief
// with it. Judging the hint too missed the Jev cache, and a fresh sample could resize the same work and deny
// the target the router had just chosen. Strip any fixed hint suffix so both see identical work.
const knownHints = [...new Set(roster.families.map(f => [roster.briefHint, f.briefHint].filter(Boolean).join('\n\n'))
  .concat(roster.briefHint ?? []))].filter(Boolean).sort((a, b) => b.length - a.length);
let task = (argv.length === 1 && argv[0] === '-' ? fs.readFileSync(0, 'utf8') : argv.join(' ')).trim();
const hintSuffix = knownHints.find(h => task.endsWith(h) && task.length > h.length);
if (hintSuffix) task = task.slice(0, -hintSuffix.length).trim();
if (!task) { console.error(USAGE); process.exit(2); }

const keyOf = f => `${f.provider}/${f.model}`.replace(/[^a-zA-Z0-9]+/g, '_').toLowerCase();
const criteria = Object.fromEntries(favorites.map(f => [keyOf(f), `${f.model}: ${f.profile ?? 'unprofiled; capability unknown'}`]));
const questions = {
  size: { type: 'score', instructions: 'How much work will the actual delegated task take? Scope is not difficulty. Do not assume a large task from a long prompt, repository name or safeguards.', criteria: SIZES },
  difficulty: { type: 'score', instructions: 'How difficult is the actual task, independently of its volume? Many files, a large repository, long inputs or a request to use Sonnet/Opus do not by themselves make work difficult.', criteria: DIFFICULTY },
};
for (const f of favorites) questions[keyOf(f)] = {
  type: 'score',
  instructions: `Using the capability profile in context.models.${keyOf(f)}, rate expected result quality for context.task independently of other models. Distinguish adequate capability from a substantial practical quality advantage for this task. Routine file search, summaries and bounded edits do not require a frontier leader; a missing specialty or short track record alone is not a capability gap. All candidates have file and shell tools. Ignore the native tool's model field and prices.`,
  criteria: CAPABILITY,
};
const request = { model: routerJson.decisionModel || '~typesafe/jev-latest', state: { task, models: criteria }, questions };
const complete = a => validAnswer(a?.size, false) && validAnswer(a?.difficulty, false)
  && favorites.every(f => validAnswer(a[keyOf(f)]));
// A delegate validation re-asks Jev about the same task; a fresh sample can reverse an A->B redirect and
// doubles requests. Valid answers are reused for 10 minutes under a hash of source and the exact request;
// quota and load are still re-read below. One file per key keeps parallel routes from overwriting each other;
// files hold scores only, never the brief.
const JEV_TTL = 10 * 60e3;
const cacheDir = path.join(runtime, 'model-route-jev-cache');
const cacheFile = path.join(cacheDir, createHash('sha256').update(JSON.stringify([source ?? null, routerJson.version, request])).digest('hex') + '.json');
const fresh = t => Date.now() - t >= 0 && Date.now() - t < JEV_TTL;
let cached;
try { const e = readJson(cacheFile); if (fresh(e.t) && complete(e.answers)) cached = e.answers; } catch {}
let answers = cached, cost = { cost: 0 };
if (!cached) {
  const vercel = routerJson.decisionProvider === 'vercel';
  if (vercel && !routerJson.decisionModel) { console.error('decision provider vercel needs [router] decision_model'); process.exit(1); }
  const [keyEnv, keyName] = vercel ? ['AI_GATEWAY_API_KEY', 'vercel.key'] : ['JEV_OPENROUTER_API_KEY', 'openrouter.key'];
  const keyFile = path.join(home, '.config/model-route', keyName);
  const jevKey = process.env[keyEnv] || (fs.existsSync(keyFile) ? fs.readFileSync(keyFile, 'utf8').trim() : '');
  if (!jevKey) { console.error(`decision key not found: set ${keyEnv} or write ${keyFile}`); process.exit(1); }
  // Vercel AI Gateway has no decisions endpoint: ask the same questions as one JSON chat completion.
  const system = 'You answer routing questions for an AI agent router. Reply with one JSON object and nothing else. '
    + 'For every key of `questions` return {"score": n, "confidence": c, "probabilities": {"0": p0, "1": p1, "2": p2, "3": p3}}: '
    + "n is the index (0-3) of the best-matching entry in that question's `criteria`, c in [0,1] is your confidence, "
    + "and the probabilities over the four criteria sum to 1. Follow each question's `instructions`; `state` is the context.";
  const body = vercel
    ? { model: request.model, messages: [{ role: 'system', content: system }, { role: 'user', content: JSON.stringify({ state: request.state, questions: request.questions }) }], response_format: { type: 'json_object' }, temperature: 0 }
    : request;
  const res = await fetch(vercel ? 'https://ai-gateway.vercel.sh/v1/chat/completions' : 'https://openrouter.ai/api/alpha/decisions', {
    method: 'POST', signal: AbortSignal.timeout(30_000),
    headers: { Authorization: `Bearer ${jevKey}`, 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
  });
  if (!res.ok) { console.error(`Jev HTTP ${res.status}: ${(await res.text()).slice(0, 300)}`); process.exit(1); }
  const out = await res.json();
  if (vercel) {
    try { answers = JSON.parse(String(out.choices?.[0]?.message?.content ?? '').trim().replace(/^```(?:json)?|```$/g, '').trim()); }
    catch (e) { console.error(`gateway answer is not JSON: ${e.message}`); process.exit(1); }
    cost = {};
  } else ({ answers, usage: cost = {} } = out);
  if (!complete(answers)) { console.error('Jev returned incomplete or out-of-range routing answers'); process.exit(1); }
  try {
    fs.mkdirSync(cacheDir, { recursive: true, mode: 0o700 });
    for (const f of fs.readdirSync(cacheDir)) {
      const file = path.join(cacheDir, f);
      if (/^[0-9a-f]{64}\.json$/.test(f) && !fresh(fs.statSync(file, { throwIfNoEntry: false })?.mtimeMs ?? 0))
        fs.rmSync(file, { force: true });
    }
    const tmp = `${cacheFile}.${process.pid}.tmp`;
    fs.writeFileSync(tmp, JSON.stringify({ t: Date.now(), answers }), { mode: 0o600 });
    fs.renameSync(tmp, cacheFile);
  } catch {}
}
const size = Math.max(0, Math.min(3, Math.round(answers.size.score)));
const difficulty = Math.max(0, Math.min(3, Math.round(answers.difficulty.score)));
const catalog = readCatalog();
const targetOf = (provider, model) => reasoningTarget(catalog, provider, model, difficulty);
// Work can start while Jev is judging; refresh both snapshots before committing the ranking.
usage = readJson(expandUser(routerJson.usageStatePath ?? '~/.local/state/ai-usage/state.json'));
activeLoad = readActiveAgents();
for (const f of favorites) f.quota = quota(f.provider, f.model);
// Thresholds and scores use raw values; rounding is display only.
const ranking = rankCandidates(favorites.map(f => {
  const a = answers[keyOf(f)];
  let target, reasoningError;
  try { target = targetOf(f.provider, f.model); } catch (e) { reasoningError = e.message; }
  return { provider: f.provider, model: f.model, profiled: Boolean(f.profile), burn: familyOf(f.model)?.burn,
    target, ...(reasoningError && { reasoningError }),
    ...(familyOf(f.model)?.noReview && { noReview: true, ...(REVIEW_TASK.test(task) && { policyBan: NO_REVIEW_BAN }) }),
    fit: f.profile ? a.probabilities['2'] + a.probabilities['3'] : 0,
    quality: a.score / (CAPABILITY.length - 1), confidence: a.confidence,
    // spendToZero families never lose a cost tie-break.
    allowed: f.quota.allowed(size), taskCost: familyOf(f.model)?.spendToZero ? 0 : f.quota.cost(size),
    quota: f.quota.note, activeAgents: f.quota.activeAgents };
}), difficulty, source, roster.routing);
const best = ranking.find(r => r.eligible);
const blocked = ranking.filter(r => !r.allowed && r.fit >= MIN_FIT).map(r => `${r.provider}/${r.model}`);
const confidence = { size: answers.size.confidence, difficulty: answers.difficulty.confidence, selected: best?.confidence ?? 0 };
const jevConfidence = Math.min(...Object.values(confidence));
const round = r => Object.fromEntries(Object.entries(r).map(([k, v]) => [k, typeof v === 'number' && !Number.isInteger(v) ? +v.toFixed(3) : v]));
const brief = r => ({ provider: r.provider, model: r.model, quality: +r.quality.toFixed(3), score: +r.score.toFixed(3),
  sharedQuotaPenalty: +r.sharedQuotaPenalty.toFixed(3), paidCreditPenalty: +r.paidCreditPenalty.toFixed(3),
  cheapModelBonus: +r.cheapModelBonus.toFixed(3) });
console.log(JSON.stringify({
  target: best?.target ?? null,
  ...(best && hintOf(best.model) && { briefHint: hintOf(best.model) }),
  source: source ?? null, sourceAccount: routerJson.ACCOUNT[source] ?? null, ...(sourceWarning && { sourceWarning }),
  reason: !best ? 'no adequate model with enough quota' : 'highest estimated quality with enough quota, after shared-quota and paid-credit penalties and cheap-model bonus',
  size: `${size}: ${SIZES[size]}`, difficulty: `${difficulty}: ${DIFFICULTY[difficulty]}`,
  jevConfidence: +jevConfidence.toFixed(3), confidence: round(confidence),
  ...(jevConfidence < 0.4 && { confidenceWarning: {
    message: 'Jev confidence below 0.4: compare expected quality and quota penalties of the top two eligible candidates',
    topTwo: ranking.filter(r => r.eligible).slice(0, 2).map(brief) } }),
  ...(requested && hintOf(requested.model) && { requestedBriefHint: hintOf(requested.model) }),
  ...(requested && { requestedTarget: ranking.find(r => r.provider === requested.providerInstanceId && r.model === requested.model)?.target
    ?? requested, requestedNoReview: Boolean(familyOf(requested.model)?.noReview),
    // The ban covers non-favorite variants too; only an explicit user choice lifts it (hook).
    validation: familyOf(requested.model)?.noReview && REVIEW_TASK.test(task)
      ? { allowed: false, reason: NO_REVIEW_BAN } : validateTarget(ranking, requested) }),
  coefficients: roster.routing,
  blocked, ranking: ranking.map(round), unprofiled, activeLoad, cost: cost.cost, jevCache: cached ? 'hit' : 'miss',
}, null, 2));
