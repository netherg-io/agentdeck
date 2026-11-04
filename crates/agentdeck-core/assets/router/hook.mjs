#!/usr/bin/env node
// Inspect the actual tool input before dispatch; SubagentStart cannot block a Codex child.
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { readCatalog, validateReasoning, repairReasoning } from './reasoning.mjs';
import { isDelegatedChild } from './load.mjs';

const harness = process.argv[process.argv.indexOf('--harness') + 1];
const here = path.dirname(new URL(import.meta.url).pathname);
const stateDir = path.join(process.env.XDG_STATE_HOME ?? path.join(os.homedir(), '.local/state'), 'model-route');
let input, args, toolName, instance, route;

function emit(allowed, reason = '') {
  if (harness === 'agy' || harness === 'opencode' || harness === 'shell') {
    console.log(JSON.stringify({ decision: allowed ? 'allow' : 'deny', reason }));
  } else if (reason || !allowed) {
    console.log(JSON.stringify({ hookSpecificOutput: { hookEventName: 'PreToolUse',
      ...(!allowed && { permissionDecision: 'deny', permissionDecisionReason: reason }),
      ...(allowed && reason && { additionalContext: reason }) } }));
  }
}

function record(decision, reason) {
  try {
    fs.mkdirSync(stateDir, { recursive: true });
    fs.appendFileSync(path.join(stateDir, 'decisions.jsonl'), JSON.stringify({
      t: new Date().toISOString(), harness, instance, toolName, taskName: args?.task_name, decision, reason,
      target: route?.target, size: route?.size, confidence: route?.jevConfidence,
      requestedTarget: /delegate_task$/.test(toolName ?? '') ? args?.target : undefined,
      blocked: route?.blocked, ranking: route?.ranking, activeLoad: route?.activeLoad,
    }) + '\n');
  } catch { console.error('model-route: decision log unavailable'); }
}

const USER_CHOICE = /^\[route:user-choice\]/;
const USER_CHOICE_HINT = ' If the user explicitly asked for a specific model, call delegate_task with that exact target'
  + ' and start the task with [route:user-choice]; routing then only warns.';
// Model-id segments that say nothing about which model the user meant.
const NOISE = new Set(['openrouter', 'anthropic', 'google', 'openai', 'meta', 'moonshotai', 'opencode', 'contributor',
  'latest', 'preview', 'low', 'medium', 'high', 'xhigh', 'max', 'ultra']);
const CYRILLIC = { haiku: ['хайку'], sonnet: ['сонет', 'соннет'], opus: ['опус'], fable: ['фейбл', 'фабл'],
  argon: ['аргон'], muse: ['мьюз', 'муз'], gemini: ['гемини', 'джемини'], kimi: ['кими'], gpt: ['гпт'],
  claude: ['клод'], codex: ['кодекс'], astra: ['астра'] };

// Last genuine user messages from the harness transcript; null when unavailable or in an unknown format.
function recentUserTexts(limit = 6) {
  const file = input?.transcript_path ?? input?.transcriptPath;
  if (typeof file !== 'string' || !file) return null;
  let text;
  try {
    const fd = fs.openSync(file, 'r');
    const size = fs.fstatSync(fd).size;
    const length = Math.min(size, 4 * 1024 * 1024);
    const buf = Buffer.alloc(length);
    fs.readSync(fd, buf, 0, length, size - length);
    fs.closeSync(fd);
    text = buf.toString('utf8');
  } catch { return null; }
  const entries = text.split('\n').flatMap(line => { try { return [JSON.parse(line)]; } catch { return []; } });
  // Answers the user picked in an AskUserQuestion dialog come back as that tool's result.
  const asked = new Set(entries.flatMap(e => e?.message?.role === 'assistant' && Array.isArray(e.message.content)
    ? e.message.content.filter(c => c?.type === 'tool_use' && c.name === 'AskUserQuestion').map(c => c.id) : []));
  const answers = c => (typeof c.content === 'string' ? c.content
    : Array.isArray(c.content) ? c.content.map(x => x?.text ?? '').join('\n') : '')
    .match(/="[^"]*"/g)?.map(a => a.slice(2, -1)) ?? [];
  const out = [];
  for (const entry of entries.reverse()) {
    const message = entry?.message ?? entry?.payload ?? entry;
    if (message?.role !== 'user' || entry.isMeta) continue;
    const parts = typeof message.content === 'string' ? [message.content] : Array.isArray(message.content)
      ? message.content.flatMap(c => c?.type === 'text' || c?.type === 'input_text' ? [c.text]
        : c?.type === 'tool_result' && asked.has(c.tool_use_id) ? answers(c) : []) : [];
    const said = parts.join('\n')
      .replace(/<(system-reminder|task-notification|local-command-[a-z]+|command-[a-z]+)>[\s\S]*?<\/\1>/g, '').trim();
    if (!said) continue;
    out.push(said);
    if (out.length >= limit) break;
  }
  return out.length ? out : null;
}

// true/false when the transcript can tell; null when it cannot (the prefix is then trusted).
function userNamedModel(model) {
  const texts = recentUserTexts();
  if (!texts) return null;
  const words = model.toLowerCase().split(/[^a-z]+/).filter(w => w.length > 2 && !NOISE.has(w));
  const names = words.flatMap(w => [w, ...(CYRILLIC[w] ?? [])]);
  return texts.some(t => names.some(n => new RegExp('(^|[^\\p{L}\\p{N}])' + n, 'iu').test(t)));
}

function ancestors() {
  const out = [];
  for (let pid = process.ppid, i = 0; pid > 1 && i < 12; i++) {
    try {
      out.push(fs.readFileSync('/proc/' + pid + '/cmdline', 'utf8').split('\0'));
      pid = Number(fs.readFileSync('/proc/' + pid + '/stat', 'utf8').split(') ')[1].split(' ')[1]);
    } catch { break; }
  }
  return out;
}

function run() {
  input = JSON.parse(fs.readFileSync(0, 'utf8') || '{}');
  args = harness === 'agy' ? (input.toolCall?.args?.Arguments ?? input.toolCall?.args ?? input.tool_input ?? {}) : input.tool_input ?? {};
  toolName = harness === 'agy' ? input.toolCall?.name ?? input.tool_name : input.tool_name;
  if (harness === 'agy' && toolName === 'call_mcp_tool'
    && ['t3-code', 't3_code'].includes(input.toolCall?.args?.ServerName))
    toolName = input.toolCall.args.ToolName;
  const native = /^(Agent|(?:collaboration\.?)?spawn_agent|invoke_subagent|start_subagent|subagent)$/.test(toolName ?? '');
  const delegate = /^(?:mcp__t3[_-]code__)?delegate_task$/.test(toolName ?? '');
  const workflow = toolName === 'Workflow';
  if (!native && !delegate && !workflow) return emit(true);

  const chain = ancestors();
  // Older T3 ran a Node server; current T3 is a native `t3 serve` binary.
  const server = cmd => cmd.some(a => a.endsWith('/apps/server/dist/bin.mjs'))
    || (/(^|\/)t3$/.test(cmd[0] ?? '') && cmd[1] === 'serve');
  const underServer = predicate => { const i = chain.findIndex(predicate); return i >= 0 && chain.slice(i).some(server); };
  const inT3 = {
    claude: () => chain.some(cmd => cmd.includes('--mcp-config') && cmd.some(a => a.includes('"t3-code"')))
      || underServer(cmd => cmd.includes('--input-format') && cmd.includes('stream-json')),
    codex: () => underServer(cmd => cmd.includes('app-server')),
    agy: () => underServer(cmd => cmd[0]?.includes('/.t3/tools/antigravity-acp/')),
    opencode: () => input.inT3 === true,
    shell: () => input.inT3 === true,
  }[harness];
  // delegate_task exists only in T3, so it is always routed: a missed T3 detection must not skip routing.
  if (!delegate && !inT3?.()) return emit(true);

  // Children told to delegate via routing gave up and did the work anyway; deny nesting outright instead.
  if (isDelegatedChild({ nativeId: harness === 'agy' ? input.conversationId : input.session_id, threadId: input.threadId })) {
    const reason = 'model-route: you are a delegated child agent; nested delegation is disabled. Do this task yourself '
      + 'with your own tools. If it is too large, complete what fits and report the remainder in your final response.';
    record('deny:nested', reason);
    return emit(false, reason);
  }

  instance = { claude: (process.env.CLAUDE_CONFIG_DIR ?? '').includes('.claude-openrouter')
    ? 'claudeAgent_openrouter' : 'claudeAgent', codex: 'codex', agy: 'antigravity', opencode: 'opencode' }[harness];
  if (harness === 'claude' && process.env.CLAUDE_CONFIG_DIR) {
    const settings = JSON.parse(fs.readFileSync(path.join(os.homedir(), '.t3/userdata/settings.json'), 'utf8'));
    instance = Object.entries(settings.providerInstances ?? {}).find(([, v]) => v.driver === 'claudeAgent'
      && v.config?.homePath === process.env.CLAUDE_CONFIG_DIR)?.[0] ?? instance;
  }
  if (harness === 'shell') {
    instance = input.source;
    if (typeof instance !== 'string' || !readCatalog().providers.some(p => p.providerInstanceId === instance)) {
      const reason = 'model-route: shell source must be a provider from live scoped orchestrator_capabilities.';
      record('deny:shell-source', reason);
      return emit(false, reason);
    }
  }
  if (harness === 'codex' && process.env.CODEX_HOME) {
    const settings = JSON.parse(fs.readFileSync(path.join(os.homedir(), '.t3/userdata/settings.json'), 'utf8'));
    instance = Object.entries(settings.providerInstances ?? {}).find(([, v]) => v.driver === 'codex'
      && v.config?.shadowHomePath === process.env.CODEX_HOME)?.[0] ?? instance;
  }
  if (process.env.MODEL_ROUTE_HOOK_DEBUG || fs.existsSync(path.join(stateDir, 'debug'))) {
    fs.mkdirSync(stateDir, { recursive: true });
    fs.appendFileSync(path.join(stateDir, 'hook-input.jsonl'), JSON.stringify({ harness, t: new Date().toISOString(), input }) + '\n');
  }
  if (workflow) {
    // Workflow's script can spawn many agents without invoking Agent; one route cannot validate them.
    const reason = 'model-route: Workflow can launch or resume agents with unchecked inherited models. '
      + 'Route each child task separately with route.mjs and use T3 delegate_task with its exact target and no runtimeMode. '
      + 'Same-model inheritance does not bypass routing. Do not retry Workflow to evade this check.';
    record('deny:workflow', reason);
    return emit(false, reason);
  }
  // Native subagent calls have no bypass: they are always routed and handed back to delegate_task.
  const prompt = args.message ?? args.prompt ?? args.instructions ?? args.task ?? '';
  if (delegate && (typeof args.task !== 'string' || !args.task.trim()
    || typeof args.target?.providerInstanceId !== 'string' || !args.target.providerInstanceId
    || typeof args.target?.model !== 'string' || !args.target.model)) {
    const reason = 'model-route: delegate_task requires a task and an explicit exact providerInstanceId/model target.';
    record('deny:invalid-delegate', reason);
    return emit(false, reason);
  }
  // Claude and agy reach the T3 shell bridge, so `route dispatch` replaces their direct delegate_task;
  // routed workflow payloads and explicit user choices still validate.
  if (delegate && ['claude', 'agy'].includes(harness) && !USER_CHOICE.test(args.task)
    && !String(args.clientRequestId ?? '').startsWith('routed-workflow-')) {
    const reason = 'model-route: the router delegates tasks itself. Write the complete task to a file and run `agentdeck route dispatch FILE --title TITLE [--isolation worktree] [--role implementation|research|review|design|test|general]`: it picks the model, delegates and prints a dispatch ID; its watcher cancels and re-dispatches the child after a usage limit. When T3 reports the child ended, run `agentdeck route dispatch --status ID` (running means a replacement is already working). Do not call delegate_task directly.' + USER_CHOICE_HINT;
    record('deny:use-dispatch', reason);
    return emit(false, reason);
  }
  const command = ['--source', instance];
  if (delegate) command.push('--validate-target', args.target.providerInstanceId, args.target.model);
  command.push('-');
  // Stdin preserves the whole brief without the OS argument-length limit or shell interpretation.
  route = JSON.parse(execFileSync(path.join(here, 'route.mjs'), command, {
    input: delegate ? args.task : typeof prompt === 'string' && prompt.trim()
      ? prompt : 'Subagent call from ' + harness + ': ' + JSON.stringify(args),
    encoding: 'utf8', timeout: 55_000, maxBuffer: 2 * 1024 * 1024, stdio: ['pipe', 'pipe', 'pipe'],
  }));
  const warning = route.jevConfidence < 0.4
    ? ' Confidence is low (' + route.jevConfidence + '); compare expected quality and quota penalties: ' + JSON.stringify(route.confidenceWarning?.topTwo ?? []) + '.'
    : '';
  if (delegate) {
    if (typeof route.validation?.allowed !== 'boolean') throw new Error('invalid validation');
    // A review role counts as a review even when the brief never says so.
    if (args.role === 'review' && route.requestedNoReview)
      route.validation = { allowed: false, reason: 'never for audits or reviews (user rule 2026-10-17)' };
    // The user may pick any catalog model: routing then only warns. The prefix states that intent;
    // where the harness exposes its transcript, a recent user message must actually name the model.
    const userChoice = !route.validation.allowed && USER_CHOICE.test(args.task);
    let choiceWarning = '';
    if (userChoice) {
      const named = userNamedModel(args.target.model);
      if (named === false) {
        const reason = 'model-route: [route:user-choice] needs a recent user message that names '
          + args.target.model + '. Do not use it for models the user did not ask for.';
        record('deny:user-choice', reason);
        return emit(false, reason);
      }
      choiceWarning = ' User-requested target despite routing (' + route.validation.reason + ')'
        + (route.target ? '; the router would pick ' + route.target.providerInstanceId + '/' + route.target.model : '')
        + '. Tell the user if this model is a poor fit.';
      route.validation = { allowed: true, reason: 'user-requested target' };
    }
    const workflowRetry = /^routed-workflow-/.test(args.clientRequestId ?? '')
      ? 'This workflow call was denied before dispatch. Only if this step has no accepted or uncertain earlier launch, release its prepared step with --not-dispatched, check success, then prepare again. Do not change the saved payload or dispatch a different target directly.' : '';
    if (route.validation.allowed) {
      const catalog = readCatalog();
      const invalid = validateReasoning(catalog, args.target);
      if (invalid) {
        const reason = 'model-route: ' + invalid + '. ' + (workflowRetry || 'Use target '
          + JSON.stringify(repairReasoning(catalog, args.target, route.requestedTarget ?? route.target))
          + ' with the same task and no runtimeMode.');
        record('deny:reasoning', reason);
        return emit(false, reason);
      }
      // Require the shared delegation policy and any family hint verbatim in every harness.
      const hint = route.requestedBriefHint;
      if (typeof hint === 'string' && hint && !String(args.task).includes(hint)) {
        const reason = 'model-route: ' + args.target.providerInstanceId + '/' + args.target.model
          + ' needs this hint appended to the task verbatim: ' + JSON.stringify(hint)
          + (workflowRetry ? ' ' + workflowRetry : ' Resend the same call with the hint at the end of the task.');
        record('deny:hint', reason);
        return emit(false, reason);
      }
    }
    const retry = workflowRetry || (route.target
      ? 'Use target ' + JSON.stringify(route.target) + ' with the same task and no runtimeMode.' : '');
    const reason = 'model-route: ' + route.validation.reason + '.' + warning + choiceWarning
      + (!route.validation.allowed && retry ? ' ' + retry + USER_CHOICE_HINT : '');
    record(userChoice ? 'override:user-choice' : route.validation.allowed ? 'allow:delegate' : 'deny:delegate', reason);
    return emit(route.validation.allowed, reason);
  }
  const t = route.target;
  if (!t) {
    const errors = (route.ranking ?? []).filter(r => r.reasoningError).map(r => r.reasoningError).join('; ');
    const reason = 'model-route: no adequate model with enough quota and usable reasoning controls (blocked: '
      + ((route.blocked ?? []).join(', ') || 'none') + '). ' + (errors ? errors + '. ' : '')
      + 'Split the task or do it yourself.' + warning;
    record('deny:no-target', reason);
    return emit(false, reason);
  }
  // Native role configs can override even a successful model rewrite; T3 applies the exact model target.
  const reason = 'model-route picked ' + t.providerInstanceId + '/' + t.model + ' (' + route.reason + '; ' + route.size + '; ' + route.difficulty + ').' + warning + ' '
    + 'Call the T3 MCP tool delegate_task with target ' + JSON.stringify(t) + ', the complete instructions as task'
    + (route.briefHint ? ' followed verbatim by ' + JSON.stringify(route.briefHint) : '') + ', and no runtimeMode.'
    + USER_CHOICE_HINT;
  record('deny:delegate ' + t.providerInstanceId + '/' + t.model, reason);
  emit(false, reason);
}

try { run(); } catch {
  const reason = 'model-route: routing failed or timed out; delegation was blocked. Retry the router or do the work yourself.';
  record('deny:router-error', reason);
  emit(false, reason);
}
