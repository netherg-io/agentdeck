import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';

const home = fs.mkdtempSync(path.join(os.tmpdir(), 'model-route-hook-'));
fs.mkdirSync(path.join(home, '.local/state/model-route'), { recursive: true });
fs.copyFileSync(new URL('./catalog.fixture.json', import.meta.url), path.join(home, '.local/state/model-route/catalog.json'));
const hook = new URL('./hook.mjs', import.meta.url).href;
fs.mkdirSync(path.join(home, '.t3/userdata'), { recursive: true });
fs.writeFileSync(path.join(home, '.t3/userdata/settings.json'), JSON.stringify({ providerInstances: {
  codex_second: { driver: 'codex', config: { shadowHomePath: path.join(home, 'second') } },
  claude_second: { driver: 'claudeAgent', config: { homePath: path.join(home, 'claude-second') } },
} }));
const target = { providerInstanceId: 'codex', model: 'gpt-6.1-sol', options: { reasoningEffort: 'high' } };
const route = { target, requestedTarget: target, size: '1: focused', difficulty: '1: bounded', jevConfidence: 0.9,
  reason: 'test', blocked: [], ranking: [], validation: { allowed: true, reason: 'selected winner' } };

function run({ harness = 'codex', args = { task_name: 'nested', message: 'Actual child task' },
  tool = 'spawn_agent', routed = route, inT3 = true, second = false, error = false, malformed = false,
  model = 'gpt-6-astra', wrapMcp = false, mcpServer = 't3-code', source = 'codex_second', sdk = false,
  transcript, session = 'root-session' } = {}) {
  const input = harness === 'agy' ? { toolCall: wrapMcp
    ? { name: 'call_mcp_tool', args: { ServerName: mcpServer, ToolName: tool, Arguments: args } } : { name: tool, args }, conversationId: session }
    : { tool_name: tool, tool_input: args, inT3, model, source, session_id: session,
      ...(transcript && { transcript_path: transcript }) };
  const code = `
    import fs from 'node:fs';
    import os from 'node:os';
    import cp from 'node:child_process';
    import { syncBuiltinESMExports } from 'node:module';
    const actualRead = fs.readFileSync;
    const input = ${JSON.stringify(input)};
    const home = ${JSON.stringify(home)};
    os.homedir = () => home;
    delete process.env.MODEL_ROUTE_HOOK_DEBUG;
    delete process.env.CLAUDE_CONFIG_DIR;
    delete process.env.XDG_STATE_HOME;
    if (${second}) process.env.CODEX_HOME = home + '/second'; else delete process.env.CODEX_HOME;
    if (${second && harness === 'claude'}) process.env.CLAUDE_CONFIG_DIR = home + '/claude-second';
    const cmd = ${JSON.stringify(harness === 'claude' ? sdk ? 'claude\0--input-format\0stream-json\0' : 'claude\0--mcp-config\0{"t3-code":{}}\0'
      : harness === 'agy' ? '/home/u/.t3/tools/antigravity-acp/server\0' : 'codex\0app-server\0')};
    fs.readFileSync = (p, ...a) => {
      if (p === 0) return ${malformed} ? '{' : JSON.stringify(input);
      if (String(p).startsWith('/proc/')) {
        if (String(p).endsWith('/cmdline')) return ${inT3} ?
          (String(p) === '/proc/' + process.ppid + '/cmdline' ? cmd : '/opt/t3/apps/server/dist/bin.mjs\\0') : 'manual-cli\\0';
        if (String(p).endsWith('/stat')) return '0 (process) S ' +
          (String(p) === '/proc/' + process.ppid + '/stat' ? 100 : 1);
      }
      return actualRead(p, ...a);
    };
    let call;
    cp.execFileSync = (file, argv, opts) => {
      call = { argv, brief: opts.input };
      if (${error}) throw new Error('secret stderr must not leak');
      return ${JSON.stringify(JSON.stringify(routed))};
    };
    syncBuiltinESMExports();
    process.argv = ['node', 'hook.mjs', '--harness', ${JSON.stringify(harness)}];
    const output = [];
    console.log = text => output.push(JSON.parse(text));
    await import(${JSON.stringify(hook)});
    process.stdout.write(JSON.stringify({ output, call }));
  `;
  const p = spawnSync(process.execPath, ['--input-type=module', '-e', code], { encoding: 'utf8', timeout: 10_000 });
  assert.equal(p.status, 0, p.stderr);
  const result = JSON.parse(p.stdout);
  const out = result.output[0];
  return { ...result, decision: out?.hookSpecificOutput?.permissionDecision ?? out?.decision ?? 'allow',
    reason: out?.hookSpecificOutput?.permissionDecisionReason ?? out?.hookSpecificOutput?.additionalContext ?? out?.reason,
    updated: out?.hookSpecificOutput?.updatedInput };
}

try {
  const actual = 'Nested child task ' + 'x'.repeat(50_000) + ' END_OF_FULL_BRIEF';
  let r = run({ args: { task_name: 'child', message: actual } });
  assert.equal(r.decision, 'deny');
  assert.equal(r.call.brief, actual);
  assert.deepEqual(r.call.argv, ['--source', 'codex', '-']);
  assert.match(r.reason, /delegate_task/);
  assert.ok(r.reason.includes(JSON.stringify(target)));
  for (const tool of ['collaborationspawn_agent', 'collaboration.spawn_agent']) {
    assert.equal(run({ tool }).decision, 'deny');
    assert.ok(run({ tool }).call);
  }

  r = run({ args: { task_name: 'child', message: 'Do the work', fork_turns: 'none', model: 'gpt-6-astra' } });
  assert.equal(r.decision, 'deny');
  assert.equal(r.updated, undefined);
  assert.equal(run({ model: target.model }).decision, 'deny');
  assert.equal(run({ model: target.model, args: { model: 'gpt-6-astra', message: 'Do work' } }).decision, 'deny');
  for (const fork_turns of ['none', 'all']) {
    r = run({ model: target.model, args: { agent_type: 'fixture', fork_turns, message: 'Role config overrides model' } });
    assert.equal(r.decision, 'deny');
    assert.match(r.reason, /delegate_task/);
  }
  assert.equal(run({ second: true }).call.argv[1], 'codex_second');
  for (const sdk of [false, true]) {
    assert.equal(run({ harness: 'claude', sdk, second: true }).call.argv[1], 'claude_second');
    assert.equal(run({ harness: 'claude', sdk, inT3: false }).call, undefined);
  }
  r = run({ harness: 'shell', tool: 'delegate_task', args: { task: actual, target } });
  assert.equal(r.decision, 'allow');
  assert.deepEqual(r.call.argv, ['--source', 'codex_second', '--validate-target', 'codex', 'gpt-6.1-sol', '-']);
  assert.equal(run({ harness: 'shell', tool: 'delegate_task', args: { task: actual, target },
    routed: { ...route, validation: { allowed: false, reason: 'quota denied' } } }).decision, 'deny');
  for (const source of [null, '', 'spoofed']) {
    r = run({ harness: 'shell', tool: 'delegate_task', args: { task: actual, target }, source });
    assert.equal(r.decision, 'deny');
    assert.equal(r.call, undefined);
  }

  // Native calls have no bypass prefix any more: they are routed and handed back to delegate_task.
  for (const args of [{ task_name: 'route_native_probe', message: 'Explicit override' },
    { message: '[route:native] Explicit override' }]) {
    r = run({ args });
    assert.equal(r.decision, 'deny');
    assert.ok(r.call);
    assert.match(r.reason, /delegate_task/);
    assert.match(r.reason, /\[route:user-choice\]/);
  }
  assert.equal(run({ inT3: false }).call, undefined);
  assert.equal(run({ tool: 'update_plan' }).call, undefined);

  for (const args of [
    { script: 'export default async () => agent("Child task")' },
    { name: 'understand' },
    { scriptPath: '/not/read/by/the/hook.mjs' },
    { scriptPath: '/not/read/by/the/hook.mjs', resumeFromRunId: 'wf_previous-run' },
    { script: '[route:native] agent("Child task")', task_name: 'route_native_workflow' },
  ]) {
    r = run({ harness: 'claude', tool: 'Workflow', args });
    assert.equal(r.decision, 'deny');
    assert.equal(r.call, undefined); // A script is not one child task that Jev can validate.
    assert.match(r.reason, /each child task separately/);
    assert.equal(run({ harness: 'claude', tool: 'Workflow', args, inT3: false }).decision, 'allow');
  }
  for (const tool of ['TaskOutput', 'TaskStop', 'SendMessage']) {
    assert.equal(run({ harness: 'claude', tool, args: { task_id: 'existing-workflow' } }).decision, 'allow');
  }

  for (const harness of ['claude', 'agy', 'opencode']) {
    r = run({ harness, tool: harness === 'claude' ? 'Agent' : harness === 'agy' ? 'start_subagent' : 'subagent',
      routed: { ...route, target: { providerInstanceId: harness === 'claude' ? 'claudeAgent' : harness === 'agy' ? 'antigravity' : 'opencode', model: 'exact-model' } } });
    assert.equal(r.decision, 'deny'); // No unverified model alias is reported as the chosen model.
  }

  const delegate = { task: actual, target };
  const wf = { ...delegate, clientRequestId: 'routed-workflow-run-step' };
  for (const harness of ['claude', 'agy']) {
    assert.equal(run({ harness, tool: 'delegate_task', args: delegate, wrapMcp: harness === 'agy' }).decision, 'allow');
  }
  assert.equal(run({ harness: 'codex', tool: 'delegate_task', args: delegate }).decision, 'allow');
  assert.equal(run({ harness: 'agy', tool: 'delegate_task', args: { ...delegate, target: { providerInstanceId: 'codex', model: target.model } },
    wrapMcp: true }).decision, 'deny');
  assert.equal(run({ harness: 'agy', tool: 'delegate_task', args: wf, wrapMcp: true,
    routed: { ...route, validation: { allowed: false, reason: 'quota' } } }).decision, 'deny');
  assert.equal(run({ harness: 'agy', tool: 'delegate_task', args: wf, wrapMcp: true, mcpServer: 'unrelated' }).call, undefined);
  for (const tool of ['mcp__t3_code__delegate_task', 'mcp__t3-code__delegate_task', 'delegate_task']) {
    r = run({ tool, args: delegate });
    assert.equal(r.decision, 'allow');
    assert.equal(r.call.brief, actual);
    assert.deepEqual(r.call.argv, ['--source', 'codex', '--validate-target', 'codex', target.model, '-']);
    assert.equal(r.output[0].hookSpecificOutput.permissionDecision, undefined);
  }
  assert.equal(run({ harness: 'claude', tool: 'delegate_task', args: wf })
    .output[0].hookSpecificOutput.permissionDecision, undefined);
  assert.equal(run({ tool: 'delegate_task', args: { task: 'Missing target' } }).decision, 'deny');
  // The shared policy and family hint must both reach the child, unchanged.
  const commonHint = 'Delegated-agent rule: never ask questions; end with BLOCKED:.';
  const familyHint = 'Long commands: test.';
  const hint = commonHint + '\n\n' + familyHint;
  const hinted = { ...route, requestedBriefHint: hint };
  for (const harness of ['codex', 'opencode', 'shell']) {
    r = run({ harness, tool: 'delegate_task', args: delegate, routed: hinted });
    assert.equal(r.decision, 'deny');
    assert.ok(r.reason.includes('needs this hint appended to the task verbatim: ' + JSON.stringify(hint)));
    assert.match(r.reason, /Resend the same call/);
    for (const part of [commonHint, familyHint])
      assert.equal(run({ harness, tool: 'delegate_task', args: { ...delegate, task: actual + '\n\n' + part }, routed: hinted }).decision, 'deny');
    assert.equal(run({ harness, tool: 'delegate_task', args: { ...delegate, task: actual + '\n\n' + hint }, routed: hinted }).decision, 'allow');
    r = run({ harness, tool: 'delegate_task', args: { ...delegate, clientRequestId: 'routed-workflow-run-step' }, routed: hinted });
    assert.match(r.reason, /--not-dispatched/);
  }
  r = run({ routed: { ...route, briefHint: hint } });
  assert.equal(r.decision, 'deny');
  assert.ok(r.reason.includes('complete instructions as task followed verbatim by ' + JSON.stringify(hint)));
  assert.doesNotMatch(run({}).reason, /followed verbatim/);
  assert.equal(run({ tool: 'delegate_task', args: delegate,
    routed: { ...route, validation: { allowed: false, reason: 'not an exact active favorite' } } }).decision, 'deny');
  for (const harness of ['codex', 'claude', 'agy', 'opencode']) {
    r = run({ harness, tool: 'delegate_task', args: { ...delegate, clientRequestId: 'routed-workflow-run-step' },
      routed: { ...route, validation: { allowed: false, reason: 'current ranking changed' } } });
    assert.equal(r.decision, 'deny');
    assert.match(r.reason, /denied before dispatch.*--not-dispatched.*check success.*prepare again/);
    assert.match(r.reason, /no accepted or uncertain earlier launch/);
    assert.match(r.reason, /Do not change the saved payload/);
    assert.doesNotMatch(r.reason, /Use target/);
    if (!['claude', 'agy'].includes(harness)) assert.match(run({ harness, tool: 'delegate_task', args: delegate,
      routed: { ...route, validation: { allowed: false, reason: 'current ranking changed' } } }).reason, /Use target/);
    r = run({ harness, tool: 'delegate_task', args: { ...delegate,
      clientRequestId: 'routed-workflow-run-step', target: { ...target, options: {} } } });
    assert.equal(r.decision, 'deny');
    assert.match(r.reason, /--not-dispatched/);
    assert.doesNotMatch(r.reason, /Use target/);
    assert.doesNotMatch(run({ harness, tool: 'delegate_task', args: { ...delegate, clientRequestId: 'routed-workflow-run-step' } }).reason,
      /release its prepared step/);
  }
  assert.equal(run({ tool: 'delegate_task', args: delegate, routed: { ...route, validation: {} } }).decision, 'deny');
  assert.equal(run({ tool: 'delegate_task', args: { ...delegate, task: '[route:native] MCP cannot escape' },
    routed: { ...route, validation: { allowed: false, reason: 'quota' } } }).decision, 'deny');
  // The calibrate prefix is gone; a user-chosen model passes routing with a warning.
  const lower = { ...route, target: { providerInstanceId: 'claudeAgent', model: 'claude-sonnet-5-5', options: { effort: 'high' } },
    validation: { allowed: false, reason: 'fit 0.460 below 0.7' } };
  r = run({ tool: 'delegate_task', args: { ...delegate, task: '[route:calibrate] Measure cost' }, routed: lower });
  assert.equal(r.decision, 'deny');
  assert.match(r.reason, /\[route:user-choice\]/);
  r = run({ tool: 'delegate_task', args: { ...delegate, task: '[route:user-choice] Audit' }, routed: lower });
  assert.equal(r.decision, 'allow'); // no transcript in this harness: the prefix is trusted
  assert.match(r.reason, /User-requested target despite routing \(fit 0\.460 below 0\.7\); the router would pick claudeAgent\/claude-sonnet-5-5/);
  const transcript = (...userTexts) => {
    const file = path.join(home, 'transcript-' + Math.random().toString(36).slice(2) + '.jsonl');
    fs.writeFileSync(file, [
      ...userTexts.map(text => JSON.stringify({ type: 'user', message: { role: 'user', content: text } })),
      JSON.stringify({ type: 'user', message: { role: 'user', content: [{ type: 'tool_result', content: 'gpt-6.1-sol' }] } }),
      JSON.stringify({ type: 'user', isMeta: true, message: { role: 'user', content: 'use gpt-6.1-sol' } }),
      JSON.stringify({ type: 'user', message: { role: 'user', content: '<task-notification>sol finished</task-notification>' } }),
      JSON.stringify({ type: 'assistant', message: { role: 'assistant', content: 'Shall I use Sol?' } }),
    ].join('\n') + '\n');
    return file;
  };
  for (const said of ['Поручи это Sol, она быстрее', 'use gpt for the audit', 'Отдай GPT-6.1-sol']) {
    assert.equal(run({ tool: 'delegate_task', args: { ...delegate, task: '[route:user-choice] Audit' }, routed: lower,
      transcript: transcript('Earlier note', said) }).decision, 'allow', said);
  }
  // Mentions only in tool results, meta entries, notifications or the agent's own words do not count.
  r = run({ tool: 'delegate_task', args: { ...delegate, task: '[route:user-choice] Audit' }, routed: lower,
    transcript: transcript('Да', 'Сделай аудит подешевле') });
  assert.equal(r.decision, 'deny');
  assert.match(r.reason, /needs a recent user message that names gpt-6\.1-sol/);
  // An answer picked in an AskUserQuestion dialog counts; the question text and other tool results do not.
  const dialog = answer => {
    const file = path.join(home, 'dialog-' + Math.random().toString(36).slice(2) + '.jsonl');
    fs.writeFileSync(file, [
      { type: 'user', message: { role: 'user', content: 'Fix the audit findings' } },
      { type: 'assistant', message: { role: 'assistant', content: [{ type: 'tool_use', id: 'q1', name: 'AskUserQuestion', input: {} }] } },
      { type: 'user', message: { role: 'user', content: [{ type: 'tool_result', tool_use_id: 'q1',
        content: 'Your questions have been answered: "Which model: Sol or Fable?"="' + answer + '". Continue.' }] } },
      { type: 'assistant', message: { role: 'assistant', content: [{ type: 'tool_use', id: 'b1', name: 'Bash', input: {} }] } },
      { type: 'user', message: { role: 'user', content: [{ type: 'tool_result', tool_use_id: 'b1', content: 'model="gpt-6.1-sol"' }] } },
    ].map(e => JSON.stringify(e)).join('\n') + '\n');
    return file;
  };
  assert.equal(run({ tool: 'delegate_task', args: { ...delegate, task: '[route:user-choice] Fix' }, routed: lower,
    transcript: dialog('Sol (recommended)') }).decision, 'allow');
  assert.equal(run({ tool: 'delegate_task', args: { ...delegate, task: '[route:user-choice] Fix' }, routed: lower,
    transcript: dialog('Fable') }).decision, 'deny');
  // Cyrillic spellings of the model name count.
  const argon = { providerInstanceId: 'antigravity', model: 'gemini-4-argon-medium' };
  r = run({ tool: 'delegate_task', args: { task: '[route:user-choice] Audit', target: argon }, routed: { ...lower, requestedTarget: argon },
    transcript: transcript('Так же аргон тоже может подойти') });
  assert.notEqual(r.reason ?? '', '');
  assert.doesNotMatch(r.reason, /needs a recent user message/);
  // The noReview ban (Argon) stops agents, but the user may still ask: allowed with the rule in the warning.
  // The hook acts on the router's verdict, so any catalog model stands in for the banned family here.
  const flash = { providerInstanceId: 'antigravity', model: 'gemini-3.8-flash-high' };
  const banned = { ...lower, requestedTarget: flash, validation: { allowed: false, reason: 'never for audits or reviews (user rule 2026-10-17)' } };
  r = run({ tool: 'delegate_task', args: { task: 'Audit', target: flash }, routed: banned });
  assert.equal(r.decision, 'deny');
  assert.match(r.reason, /never for audits or reviews/);
  r = run({ tool: 'delegate_task', args: { task: '[route:user-choice] Audit', target: flash }, routed: banned,
    transcript: transcript('Поручи аудит гемини флеш') });
  assert.equal(r.decision, 'allow', r.reason);
  assert.match(r.reason, /User-requested target despite routing \(never for audits or reviews/);
  // A review role counts as a review even when the brief does not say so; only the user can lift it.
  r = run({ tool: 'delegate_task', args: { task: 'Check screens', target: flash, role: 'review' },
    routed: { ...lower, requestedTarget: flash, requestedNoReview: true, validation: { allowed: true, reason: 'selected winner' } } });
  assert.equal(r.decision, 'deny');
  assert.match(r.reason, /never for audits or reviews/);
  r = run({ tool: 'delegate_task', args: { task: '[route:user-choice] Check screens', target: flash, role: 'review' },
    routed: { ...lower, requestedTarget: flash, requestedNoReview: true }, transcript: transcript('use gemini flash') });
  assert.equal(r.decision, 'allow', r.reason);
  // Without the prefix a denied target stays denied, even when the user named it.
  assert.equal(run({ tool: 'delegate_task', args: { ...delegate, task: 'Audit' }, routed: lower,
    transcript: transcript('use sol') }).decision, 'deny');
  // A user choice still needs valid reasoning options for that exact model.
  assert.equal(run({ tool: 'delegate_task', args: { task: '[route:user-choice] Audit', target: { ...target, options: {} } },
    routed: lower }).decision, 'deny');

  // Validate the requested candidate's effort key even when another harness wins the ranking.
  for (const [providerInstanceId, model, key, foreign, levels] of [
    ['codex', 'gpt-6.1-sol', 'reasoningEffort', 'effort', ['low', 'medium', 'high', 'xhigh', 'max', 'ultra']],
    ['codex_second', 'gpt-6.1-sol', 'reasoningEffort', 'effort', ['low', 'medium', 'high', 'xhigh', 'max', 'ultra']],
    ['claudeAgent', 'claude-opus-5-5', 'effort', 'reasoningEffort', ['low', 'medium', 'high', 'xhigh', 'max', 'ultracode', 'ultrathink']],
    ['opencode', 'anthropic/claude-opus-5-5', 'variant', 'reasoningEffort', ['low', 'high', 'xhigh']],
    ['claudeAgent_openrouter', 'claude-opus-5-5', 'effort', 'reasoningEffort', ['low', 'medium', 'high', 'xhigh', 'max', 'ultracode', 'ultrathink']],
  ]) {
    const requestedTarget = { providerInstanceId, model, options: { [key]: 'high' } };
    const winner = key === 'effort' ? target
      : { providerInstanceId: 'claudeAgent', model: 'claude-opus-5-5', options: { effort: 'high' } };
    const routed = { ...route, target: winner, requestedTarget };
    for (const harness of ['codex', 'claude']) {
      for (const low of ['low'])
        assert.equal(run({ harness, tool: 'delegate_task', args: { task: 'Below the router minimum',
          target: { providerInstanceId, model, options: { [key]: low } } }, routed }).decision, 'deny');
      for (const level of levels.filter(l => l !== 'low')) {
        for (const options of [{ [key]: level }, [{ id: key, value: level }]]) {
          const args = { task: 'Explicit supported effort', target: { providerInstanceId, model, options } };
          r = run({ harness, tool: 'delegate_task', args, routed });
          assert.equal(r.decision, 'allow', `${harness}: ${providerInstanceId} ${JSON.stringify(options)}`);
          assert.equal(r.updated, undefined); // Supported explicit levels must survive without a rewrite.
          assert.deepEqual(r.call.argv, ['--source', harness === 'claude' ? 'claudeAgent' : 'codex',
            '--validate-target', providerInstanceId, model, '-']);
        }
      }
      for (const options of [undefined, null, {}, [], { [key]: '' }, [{ id: key, value: '' }],
        { [foreign]: 'high' }, [{ id: foreign, value: 'high' }],
        { [key]: 'high', [foreign]: 'high' }, [{ id: key, value: 'high' }, { id: foreign, value: 'high' }],
        [{ id: key, value: 'high' }, { id: key, value: 'high' }],
        { [key]: 'unsupported' }, [{ id: key, value: 'unsupported' }]]) {
        r = run({ harness, tool: 'delegate_task', args: { task: 'Invalid effort',
          target: { providerInstanceId, model, options } }, routed });
        assert.equal(r.decision, 'deny', `${harness}: ${providerInstanceId} ${JSON.stringify(options)}`);
      }
      assert.equal(run({ harness, tool: 'delegate_task', args: { task: 'Quota still applies', target: requestedTarget },
        routed: { ...routed, validation: { allowed: false, reason: 'quota' } } }).decision, 'deny');
    }
  }

  // Catalog identity, rather than the ranking winner or requested candidate's echoed options, validates overrides.
  for (const harness of ['codex', 'claude', 'agy', 'opencode']) {
    const wrongEcho = { ...target, options: { effort: 'high' } };
    assert.equal(run({ harness, tool: 'delegate_task', args: delegate,
      routed: { ...route, requestedTarget: wrongEcho } }).decision, 'allow');
    assert.equal(run({ harness, tool: 'delegate_task', args: { ...delegate, target: wrongEcho },
      routed: { ...route, requestedTarget: wrongEcho } }).decision, 'deny');
    const agy = { providerInstanceId: 'antigravity', model: 'gemini-3.8-flash-high' };
    assert.equal(run({ harness, tool: 'delegate_task', args: { task: 'Exact optionless model', target: agy },
      routed: { ...route, requestedTarget: agy } }).decision, 'allow');
    for (const key of ['reasoningEffort', 'effort', 'variant', 'thinking'])
      assert.equal(run({ harness, tool: 'delegate_task', args: { task: 'Foreign option', target: { ...agy, options: { [key]: 'high' } } },
        routed: { ...route, requestedTarget: agy } }).decision, 'deny');
  }
  for (const [providerInstanceId, model, options, expected] of [
    ['codex', 'gpt-6-luna', { reasoningEffort: 'ultra' }, 'deny'],
    ['claudeAgent', 'claude-opus-4-5', { effort: 'xhigh' }, 'deny'],
    ['claudeAgent', 'claude-haiku-4-5', { thinking: true }, 'allow'],
    ['claudeAgent', 'claude-haiku-4-5', { thinking: false }, 'deny'],
    ['claudeAgent', 'claude-haiku-4-5', { thinking: 'high' }, 'deny'],
    ['claudeAgent', 'claude-haiku-4-5', {}, 'deny'],
  ]) assert.equal(run({ tool: 'delegate_task', args: { task: 'Exact profile', target: { providerInstanceId, model, options } } }).decision, expected);

  r = run({ tool: 'delegate_task', args: delegate, routed: { ...route, jevConfidence: 0.02,
    confidenceWarning: { topTwo: [{ model: target.model }, { model: 'runner-up' }] } } });
  assert.equal(r.decision, 'allow');
  assert.match(r.reason, /Confidence is low.*runner-up/);
  for (const harness of ['codex', 'claude', 'agy', 'opencode']) {
    const tool = harness === 'claude' ? 'Agent' : harness === 'agy' ? 'invoke_subagent' : harness === 'opencode' ? 'subagent' : 'spawn_agent';
    r = run({ harness, tool, error: true });
    assert.equal(r.decision, 'deny');
    assert.doesNotMatch(r.reason, /secret stderr/);
  }
  // A delegated child (its native session maps to an app-owned subagent's child thread) cannot nest.
  const { DatabaseSync } = await import('node:sqlite');
  const db = new DatabaseSync(path.join(home, '.t3/userdata/statev2.sqlite'));
  db.exec(`CREATE TABLE orchestration_v2_projection_subagents (subagent_id TEXT, child_thread_id TEXT);
    CREATE TABLE orchestration_v2_projection_provider_threads (provider_thread_id TEXT, thread_id TEXT, payload_json TEXT);
    INSERT INTO orchestration_v2_projection_subagents VALUES ('node:child', 'thread:child');
    INSERT INTO orchestration_v2_projection_provider_threads VALUES ('pt', 'thread:child', '{"nativeThreadRef":{"nativeId":"child-session"}}');`);
  db.close();
  for (const harness of ['codex', 'claude', 'agy', 'opencode']) {
    const tool = harness === 'claude' ? 'Agent' : harness === 'agy' ? 'invoke_subagent' : harness === 'opencode' ? 'subagent' : 'spawn_agent';
    r = run({ harness, tool, session: 'child-session' });
    assert.equal(r.decision, 'deny');
    assert.match(r.reason, /nested delegation is disabled/);
    assert.equal(r.call, undefined);
  }
  r = run({ tool: 'mcp__t3_code__delegate_task', args: delegate, session: 'child-session' });
  assert.match(r.reason, /nested delegation is disabled/);
  assert.notEqual(run({ session: 'root-session' }).reason ?? '', '');
  assert.doesNotMatch(run({ session: 'root-session' }).reason ?? '', /nested delegation/);
  assert.equal(run({ malformed: true }).decision, 'deny');
  const records = fs.readFileSync(path.join(home, '.local/state/model-route/decisions.jsonl'), 'utf8').trim().split('\n').map(JSON.parse);
  assert.ok(records.some(r => r.decision === 'deny:router-error'));
  assert.ok(records.some(r => r.decision === 'deny:workflow'));
  assert.ok(records.some(r => r.instance === 'codex_second'));
  assert.ok(records.every(r => !JSON.stringify(r).includes('END_OF_FULL_BRIEF')));
  console.log('hook tests passed');
} finally { fs.rmSync(home, { recursive: true, force: true }); }
