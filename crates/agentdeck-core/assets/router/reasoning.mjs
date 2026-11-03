import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

export const catalogFile = () => path.join(process.env.XDG_STATE_HOME ?? path.join(os.homedir(), '.local/state'),
  'model-route/catalog.json');
const IDS = ['reasoningEffort', 'effort', 'variant', 'thinking'];
const LEVELS = ['low', 'medium', 'high', 'xhigh'];
// Full ordinary order across harnesses, used only to compare a value against a family's minEffort floor.
const RANK = ['none', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max'];
const roster = JSON.parse(fs.readFileSync(new URL('./roster.json', import.meta.url), 'utf8'));
const minEffort = model => roster.families.find(f => new RegExp(f.match, 'i').test(model ?? ''))?.minEffort;
const validDescriptor = d => d && typeof d.id === 'string' && (d.type === 'boolean'
  || d.type === 'select' && Array.isArray(d.options) && d.options.every(o => o && typeof o.id === 'string'));

export function saveCatalog(data) {
  data = data?.structuredContent ?? data;
  if (!data?.providers && Array.isArray(data?.content))
    data = JSON.parse(data.content.find(c => c.type === 'text')?.text ?? '{}');
  // T3 now sends {id, values, default}; map it onto the {type, options} descriptor shape.
  const normalize = d => d && Array.isArray(d.values) && !d.type
    ? d.values.every(v => typeof v === 'boolean') ? { id: d.id, type: 'boolean' }
      : { id: d.id, type: 'select', options: d.values.map(v => ({ id: String(v), isDefault: v === d.default })) }
    : d;
  for (const p of data?.providers ?? [])
    for (const m of p?.models ?? []) if (Array.isArray(m?.options)) m.options = m.options.map(normalize);
  const providers = data?.providers;
  if (!Array.isArray(providers) || !providers.length || providers.some(p =>
    typeof p.providerInstanceId !== 'string' || typeof p.driverKind !== 'string' || !Array.isArray(p.models)
    || p.models.some(m => typeof m.id !== 'string' || m.options != null
      && (!Array.isArray(m.options) || !m.options.every(validDescriptor)))))
    throw new Error('expected orchestrator_capabilities structuredContent with model option descriptors');
  const file = catalogFile();
  fs.mkdirSync(path.dirname(file), { recursive: true, mode: 0o700 });
  const tmp = `${file}.${process.pid}.tmp`;
  fs.writeFileSync(tmp, JSON.stringify({ providers: providers.map(p => ({ providerInstanceId: p.providerInstanceId, driverKind: p.driverKind,
    models: p.models.map(m => ({ id: m.id, options: m.options ?? [] })) })), updatedAt: new Date().toISOString() }), { mode: 0o600 });
  fs.renameSync(tmp, file);
}

export function readCatalog() {
  try {
    const data = JSON.parse(fs.readFileSync(catalogFile(), 'utf8'));
    if (!Array.isArray(data.providers) || data.providers.some(p => typeof p.driverKind !== 'string')) throw new Error('invalid catalog');
    return data;
  }
  catch { throw new Error('refresh model options: pass orchestrator_capabilities structuredContent to route.mjs --catalog on stdin'); }
}

export function reasoningDescriptor(catalog, provider, model) {
  const instance = catalog.providers.find(p => p.providerInstanceId === provider);
  const entry = instance?.models.find(m => m.id === model);
  if (!entry) throw new Error(`refresh orchestrator_capabilities catalog: no model ${provider}/${model}`);
  const d = entry.options.find(d => IDS.includes(d.id));
  // T3's Claude compiler applies effort only to its native catalog, despite advertising custom knobs.
  if (d && instance.driverKind === 'claudeAgent' && !model.startsWith('claude-'))
    throw new Error(`T3 custom Claude model ${provider}/${model} cannot apply its advertised reasoning option; use another harness`);
  return d;
}

export function reasoningTarget(catalog, provider, model, difficulty) {
  const target = { providerInstanceId: provider, model };
  const d = reasoningDescriptor(catalog, provider, model);
  if (!d) return target;
  // Delegates never run on low or an implicit default: difficulty 0 medium, 1 high, 2-3 xhigh.
  difficulty = Math.min(Math.max(difficulty, 0) + 1, 3);
  if (d.type === 'boolean') return { ...target, options: { [d.id]: true } };
  const supported = d.options.map(o => o.id);
  const floor = LEVELS.indexOf(minEffort(model));
  if (floor > difficulty) difficulty = floor;
  // Special modes can enable orchestration; ordinary difficulty must never select them implicitly.
  // Highest supported level up to the wanted one but not below medium, else the next one above.
  const value = LEVELS.slice(1, difficulty + 1).reverse().find(v => supported.includes(v))
    ?? LEVELS.slice(difficulty + 1).find(v => supported.includes(v))
    ?? LEVELS.find(v => supported.includes(v))
    ?? d.options.find(o => o.id === d.currentValue || o.isDefault)?.id;
  if (value === undefined || ['ultracode', 'ultrathink'].includes(value))
    throw new Error(`cannot map difficulty to ${provider}/${model} ${d.id}; choose a model with ordinary effort levels`);
  return { ...target, options: { [d.id]: value } };
}

export function validateReasoning(catalog, target) {
  let d;
  try { d = reasoningDescriptor(catalog, target.providerInstanceId, target.model); }
  catch (e) { return e.message; }
  const entries = Array.isArray(target.options) ? target.options
    : target.options && typeof target.options === 'object'
      ? Object.entries(target.options).map(([id, value]) => ({ id, value })) : [];
  if (entries.some(e => !e || typeof e.id !== 'string') || new Set(entries.map(e => e.id)).size !== entries.length)
    return 'invalid or duplicate target option IDs';
  if (entries.some(e => IDS.includes(e.id) && e.id !== d?.id)) return 'reasoning option belongs to another harness';
  if (!d) return null;
  const e = entries.find(e => e.id === d.id);
  if (!e) return `explicit target.options.${d.id} is required; omitted options can lose reasoning across providers/models`;
  if (d.type === 'boolean' ? typeof e.value !== 'boolean'
    : typeof e.value !== 'string' || !d.options.some(o => o.id === e.value))
    return `unsupported target.options.${d.id} for this model`;
  const floor = minEffort(target.model);
  if (floor && RANK.includes(e.value) && RANK.indexOf(e.value) < RANK.indexOf(floor))
    return `target.options.${d.id} ${e.value} is below this model family's minimum ${floor}`;
  if (d.type !== 'boolean' && d.options.some(o => LEVELS.slice(1).includes(o.id)) && RANK.includes(e.value)
    && RANK.indexOf(e.value) < RANK.indexOf('medium'))
    return `target.options.${d.id} ${e.value} is below the router minimum medium`;
  if (d.type === 'boolean' && e.value !== true) return `target.options.${d.id} must be true`;
  return null;
}

export function repairReasoning(catalog, target, recommended) {
  const d = reasoningDescriptor(catalog, target.providerInstanceId, target.model);
  const entries = Array.isArray(target.options) ? target.options.filter(e => e && typeof e.id === 'string')
    : target.options && typeof target.options === 'object'
      ? Object.entries(target.options).map(([id, value]) => ({ id, value })) : [];
  const unrelated = entries.filter(e => !IDS.includes(e.id));
  if (!d) return { ...target, options: Array.isArray(target.options) ? unrelated
    : Object.fromEntries(unrelated.map(e => [e.id, e.value])) };
  const explicit = entries.filter(e => e.id === d.id);
  const value = explicit.length === 1 && validateReasoning(catalog,
    { ...target, options: { [d.id]: explicit[0].value } }) === null ? explicit[0].value : recommended.options[d.id];
  const corrected = [...unrelated, { id: d.id, value }];
  return { ...target, options: Array.isArray(target.options) ? corrected
    : Object.fromEntries(corrected.map(e => [e.id, e.value])) };
}
