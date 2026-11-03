import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { catalogFile, saveCatalog, readCatalog, reasoningTarget, validateReasoning, repairReasoning } from './reasoning.mjs';

const select = (id, values, extra = {}) => ({ id, type: 'select', options: values.map(id => ({ id })), ...extra });
const codex = select('reasoningEffort', ['low', 'medium', 'high', 'xhigh', 'max', 'ultra']);
const claude = select('effort', ['low', 'medium', 'high', 'xhigh', 'max', 'ultracode', 'ultrathink']);
const model = (id, ...options) => ({ id, options });
const catalog = { providers: [
  { providerInstanceId: 'codex', driverKind: 'codex', models: [model('sol', codex), model('luna', select('reasoningEffort', ['low', 'medium', 'high', 'xhigh', 'max']))] },
  { providerInstanceId: 'codex_second', driverKind: 'codex', models: [model('sol', codex)] },
  { providerInstanceId: 'claudeAgent', driverKind: 'claudeAgent', models: [model('claude-opus', claude), model('claude-old', select('effort', ['low', 'medium', 'high', 'max'])),
    model('claude-haiku', { id: 'thinking', type: 'boolean' })] },
  { providerInstanceId: 'claudeAgent_openrouter', driverKind: 'claudeAgent', models: [model('claude-opus', claude)] },
  { providerInstanceId: 'opencode', driverKind: 'opencode', models: [model('ordinary', select('variant', ['low', 'high', 'xhigh'])),
    model('opaque', select('variant', ['quick', 'deep'], { currentValue: 'deep' })),
    model('unknown', select('variant', ['quick', 'deep'])), model('none'), model('other-only', select('serviceTier', ['default', 'priority']))] },
  { providerInstanceId: 'antigravity', driverKind: 'antigravity', models: [model('gemini-high')] },
  { providerInstanceId: 'custom-claude', driverKind: 'claudeAgent', models: [model('meta/muse', claude)] },
] };

// Delegates never run on low: difficulty 0 maps to medium, 1 to high, 2-3 to xhigh.
for (const [difficulty, value] of ['medium', 'high', 'xhigh', 'xhigh'].entries()) {
  for (const provider of ['codex', 'codex_second'])
    assert.deepEqual(reasoningTarget(catalog, provider, 'sol', difficulty).options, { reasoningEffort: value });
  for (const provider of ['claudeAgent', 'claudeAgent_openrouter'])
    assert.deepEqual(reasoningTarget(catalog, provider, 'claude-opus', difficulty).options, { effort: value });
}
assert.deepEqual(reasoningTarget(catalog, 'claudeAgent', 'claude-old', 3).options, { effort: 'high' });

// roster.json minEffort: Muse Spark never runs below high, whatever the difficulty or an explicit request.
const museVariant = select('variant', ['minimal', 'low', 'medium', 'high', 'xhigh', 'max']);
const museCatalog = { providers: [{ providerInstanceId: 'opencode', driverKind: 'opencode',
  models: [model('openrouter/meta/muse-spark-1.3-contributor', museVariant), model('openrouter/other/model', museVariant)] }] };
const muse = { providerInstanceId: 'opencode', model: 'openrouter/meta/muse-spark-1.3-contributor' };
for (const [difficulty, value] of ['high', 'high', 'xhigh', 'xhigh'].entries())
  assert.deepEqual(reasoningTarget(museCatalog, muse.providerInstanceId, muse.model, difficulty).options, { variant: value });
assert.deepEqual(reasoningTarget(museCatalog, 'opencode', 'openrouter/other/model', 0).options, { variant: 'medium' });
for (const low of ['minimal', 'low', 'medium'])
  assert.match(validateReasoning(museCatalog, { ...muse, options: { variant: low } }), /below this model family's minimum high/);
for (const ok of ['high', 'xhigh', 'max'])
  assert.equal(validateReasoning(museCatalog, { ...muse, options: { variant: ok } }), null);
assert.match(validateReasoning(museCatalog, { providerInstanceId: 'opencode', model: 'openrouter/other/model', options: { variant: 'low' } }), /router minimum medium/);
assert.deepEqual(repairReasoning(museCatalog, { ...muse, options: { variant: 'medium' } },
  reasoningTarget(museCatalog, muse.providerInstanceId, muse.model, 1)).options, { variant: 'high' });
assert.deepEqual(reasoningTarget(catalog, 'claudeAgent', 'claude-haiku', 0).options, { thinking: true });
assert.match(validateReasoning(catalog, { providerInstanceId: 'claudeAgent', model: 'claude-haiku', options: { thinking: false } }), /must be true/);
assert.deepEqual(reasoningTarget(catalog, 'claudeAgent', 'claude-haiku', 2).options, { thinking: true });
assert.deepEqual(reasoningTarget(catalog, 'opencode', 'ordinary', 1).options, { variant: 'high' });
// medium is not offered: the next supported level above it, never low.
assert.deepEqual(reasoningTarget(catalog, 'opencode', 'ordinary', 0).options, { variant: 'high' });
assert.deepEqual(reasoningTarget(catalog, 'opencode', 'opaque', 2).options, { variant: 'deep' });
assert.throws(() => reasoningTarget(catalog, 'opencode', 'unknown', 2), /cannot map/);
assert.equal(reasoningTarget(catalog, 'opencode', 'none', 2).options, undefined);
assert.deepEqual(reasoningTarget(catalog, 'antigravity', 'gemini-high', 2), { providerInstanceId: 'antigravity', model: 'gemini-high' });
assert.throws(() => reasoningTarget(catalog, 'missing', 'sol', 2), /refresh/);
assert.throws(() => reasoningTarget(catalog, 'custom-claude', 'meta/muse', 2), /cannot apply/);
assert.match(validateReasoning(catalog, { providerInstanceId: 'custom-claude', model: 'meta/muse', options: { effort: 'high' } }), /cannot apply/);

const target = { providerInstanceId: 'codex_second', model: 'sol' };
for (const options of [undefined, {}, [], { serviceTier: 'priority' }])
  assert.match(validateReasoning(catalog, { ...target, options }), /explicit.*required/);
assert.match(validateReasoning(catalog, { ...target, options: { reasoningEffort: 'low' } }), /router minimum medium/);
for (const value of ['medium', 'ultra']) {
  assert.equal(validateReasoning(catalog, { ...target, options: { reasoningEffort: value, serviceTier: 'priority' } }), null);
  assert.equal(validateReasoning(catalog, { ...target, options: [{ id: 'reasoningEffort', value }] }), null);
}
assert.match(validateReasoning(catalog, { ...target, options: [{ id: 'reasoningEffort', value: 'high' }, { id: 'reasoningEffort', value: 'low' }] }), /duplicate/);
for (const options of [{ effort: 'high' }, { reasoningEffort: 'high', effort: 'high' }])
  assert.match(validateReasoning(catalog, { ...target, options }), /another harness/);
for (const value of ['', false, 2, 'invented'])
  assert.match(validateReasoning(catalog, { ...target, options: { reasoningEffort: value } }), /unsupported/);
assert.match(validateReasoning(catalog, { providerInstanceId: 'codex', model: 'luna', options: { reasoningEffort: 'ultra' } }), /unsupported/);
assert.equal(validateReasoning(catalog, { providerInstanceId: 'claudeAgent', model: 'claude-haiku', options: { thinking: true } }), null);
assert.match(validateReasoning(catalog, { providerInstanceId: 'antigravity', model: 'gemini-high', options: { reasoningEffort: 'high' } }), /another harness/);
const recommended = reasoningTarget(catalog, 'codex_second', 'sol', 1);
const partial = { ...target, options: { serviceTier: 'priority' } };
assert.deepEqual(repairReasoning(catalog, partial, recommended).options, { serviceTier: 'priority', reasoningEffort: 'high' });
assert.deepEqual(partial.options, { serviceTier: 'priority' });
assert.deepEqual(repairReasoning(catalog, { ...target, options: { serviceTier: 'priority', reasoningEffort: 'ultra', effort: 'high' } }, recommended).options,
  { serviceTier: 'priority', reasoningEffort: 'ultra' });
assert.deepEqual(repairReasoning(catalog, { ...target, options: [{ id: 'serviceTier', value: 'priority' }, { id: 'effort', value: 'high' }] }, recommended).options,
  [{ id: 'serviceTier', value: 'priority' }, { id: 'reasoningEffort', value: 'high' }]);
assert.deepEqual(repairReasoning(catalog, { providerInstanceId: 'opencode', model: 'other-only', options: { serviceTier: 'priority', effort: 'high' } },
  { providerInstanceId: 'opencode', model: 'other-only' }).options, { serviceTier: 'priority' });

const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'model-route-reasoning-'));
const previousState = process.env.XDG_STATE_HOME;
try {
  process.env.XDG_STATE_HOME = dir;
  assert.throws(readCatalog, /refresh/);
  saveCatalog(catalog);
  assert.deepEqual(readCatalog().providers, catalog.providers);
  saveCatalog({ structuredContent: catalog });
  assert.deepEqual(readCatalog().providers, catalog.providers);
  saveCatalog({ content: [{ type: 'text', text: JSON.stringify(catalog) }] });
  assert.deepEqual(readCatalog().providers, catalog.providers);
  assert.equal(fs.statSync(catalogFile()).mode & 0o777, 0o600);
  const saved = fs.readFileSync(catalogFile(), 'utf8');
  for (const bad of [{}, { providers: [] }, { providers: [{ providerInstanceId: 'x', models: [] }] },
    { providers: [{ providerInstanceId: 'x', driverKind: 'codex', models: [model('bad', { id: 'effort', type: 'select' })] }] }])
    assert.throws(() => saveCatalog(bad), /expected/);
  assert.equal(fs.readFileSync(catalogFile(), 'utf8'), saved);
} finally {
  if (previousState === undefined) delete process.env.XDG_STATE_HOME; else process.env.XDG_STATE_HOME = previousState;
  fs.rmSync(dir, { recursive: true, force: true });
}
console.log('reasoning checks passed');
