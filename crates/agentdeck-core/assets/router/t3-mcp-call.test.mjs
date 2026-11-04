import assert from 'node:assert/strict';
import { EventEmitter } from 'node:events';
import { discoverConnection, connectionFromConfig, callTool, validatePayload, checkRoute, readPayload, scopedConnection } from './t3-mcp-call.mjs';

const connection = { endpoint: 'http://127.0.0.1:3773/mcp', authorization: 'Bearer fixture-secret' };
const config = { mcpServers: { 't3-code': { type: 'http', url: connection.endpoint,
  headers: { Authorization: connection.authorization } } } };
const payload = { task: 'Exact unicode task: броня `touch /tmp/nope` $(false)\n' + 'x'.repeat(100000),
  target: { providerInstanceId: 'opencode', model: 'opencode-go/muse-spark-1.3-contributor', options: { variant: 'high' } },
  mode: 'async', clientRequestId: 'routed-workflow-fixture-step' };
const capabilities = { parentThreadId: 'parent-fixture', inheritedProviderInstanceId: 'claude_second',
  providers: [{ providerInstanceId: 'claude_second' }], features: { appOwnedSubagents: true } };
const launched = { taskId: 'actual-server-task-id', childThreadId: 'actual-child-thread-id',
  providerInstanceId: payload.target.providerInstanceId, model: payload.target.model, status: 'running' };

assert.deepEqual(connectionFromConfig(config), connection);
assert.equal(connectionFromConfig({ mcpServers: { unrelated: config.mcpServers['t3-code'] } }), null);
assert.throws(() => connectionFromConfig({ mcpServers: { 't3-code': { url: 'https://evil.test/mcp',
  headers: { Authorization: connection.authorization } } } }), /loopback/);
assert.throws(() => connectionFromConfig({ mcpServers: { 't3-code': { url: connection.endpoint + '?token=secret',
  headers: { Authorization: connection.authorization } } } }), /loopback/);
assert.deepEqual(discoverConnection({ env: { T3_ACP_MCP_ENDPOINT: connection.endpoint,
  T3_ACP_MCP_AUTHORIZATION: connection.authorization }, read: () => assert.fail('ACP must not read /proc') }), connection);
assert.throws(() => discoverConnection({ env: { T3_ACP_MCP_ENDPOINT: connection.endpoint } }), /authorization/);

for (const fileConfig of [false, true]) {
  const reads = [];
  const read = name => {
    reads.push(name);
    const data = { '/proc/20/cmdline': 'bash\0-c\0command\0', '/proc/20/stat': '20 (bash child) S 10 rest',
      '/proc/10/cmdline': 'claude\0--mcp-config\0' + (fileConfig ? '/fixture/config.json' : JSON.stringify(config)) + '\0',
      '/proc/10/stat': '10 (claude) S 1 rest', '/fixture/config.json': JSON.stringify(config) };
    if (!(name in data)) throw new Error('Must not inspect another process: ' + name);
    return data[name];
  };
  assert.deepEqual(discoverConnection({ env: {}, pid: 20, read }), connection);
  assert.ok(reads.every(n => n.startsWith('/proc/20/') || n.startsWith('/proc/10/') || n === '/fixture/config.json'));
}
assert.throws(() => discoverConnection({ env: {}, pid: 20, read: () => { throw new Error('private'); } }), /No inherited/);
for (const response of [JSON.stringify(connection) + '\n', '{"error":"not_configured"}\n', 'invalid fixture-secret']) {
  let destroyed = false;
  const connect = args => {
    assert.deepEqual(args, { path: '/runtime/own-session.sock' });
    const socket = new EventEmitter();
    socket.setTimeout = milliseconds => assert.equal(milliseconds, 5000);
    socket.destroy = () => { destroyed = true; };
    queueMicrotask(() => {
      socket.emit('data', Buffer.from(response.slice(0, 9)));
      socket.emit('data', Buffer.from(response.slice(9)));
      socket.emit('end');
    });
    return socket;
  };
  const result = scopedConnection({ env: { T3_SHELL_MCP_SOCKET: '/runtime/own-session.sock' }, connect,
    read: () => assert.fail('Socket sessions must not inspect /proc') });
  if (response.startsWith('{"endpoint"')) assert.deepEqual(await result, connection);
  else await assert.rejects(result, error => !error.message.includes('fixture-secret'));
  assert.ok(destroyed);
}
validatePayload('delegate_task', payload);
for (const bad of [{ ...payload, mode: 'wait' }, { ...payload, clientRequestId: '' },
  { ...payload, runtimeMode: 'full-access' }, { ...payload, runtimeMode: null },
  { ...payload, task: 'x'.repeat(120001) }, { ...payload, target: {} },
  { ...payload, role: 'implement' }, { ...payload, role: null }])
  assert.throws(() => validatePayload('delegate_task', bad));
assert.throws(() => validatePayload('t3_thread_launch', {}), /Supported tools/);
assert.deepEqual(readPayload('/fixture/prepared.json', () => JSON.stringify({ payload, route: { target: payload.target } })), payload);
assert.deepEqual(readPayload('-', name => { assert.equal(name, 0); return JSON.stringify(payload); }), payload);
assert.throws(() => readPayload('/fixture/bad.json', () => '{'), /valid JSON/);

checkRoute(payload, capabilities, (exe, args, options) => {
  assert.deepEqual(args.slice(-2), ['--harness', 'shell']);
  assert.equal(JSON.parse(options.input).source, 'claude_second');
  assert.deepEqual(JSON.parse(options.input).tool_input, payload);
  return { status: 0, stdout: '{"decision":"allow"}' };
});
assert.throws(() => checkRoute(payload, capabilities, () => ({ status: 0,
  stdout: '{"decision":"deny","reason":"quota denied before dispatch"}' })), /quota denied/);
assert.throws(() => checkRoute(payload, capabilities, () => ({ status: 1, stdout: 'private error' })), /nothing dispatched/);

function fixture({ sse = false, lost = false, denied = false, capabilityData = capabilities,
  httpError = false, invalidJson = false, rpcError = false, invalidParams = false, mcpError = false } = {}) {
  const calls = [], events = [];
  const fetchImpl = async (url, options) => {
    assert.equal(url, connection.endpoint);
    assert.equal(options.headers.authorization, connection.authorization);
    assert.equal(options.redirect, 'error');
    const message = JSON.parse(options.body); calls.push(message);
    if (message.method === 'initialize') return new Response(JSON.stringify({ jsonrpc: '2.0', id: message.id,
      result: { protocolVersion: '2025-06-18' } }), { headers: { 'content-type': 'application/json', 'mcp-session-id': 'fixture-session' } });
    assert.equal(options.headers['mcp-session-id'], 'fixture-session');
    assert.equal(options.headers['mcp-protocol-version'], '2025-06-18');
    if (message.method === 'notifications/initialized') return new Response(null, { status: 202 });
    if (message.params.name === 'orchestrator_capabilities') return new Response(JSON.stringify({ jsonrpc: '2.0', id: message.id,
      result: { structuredContent: capabilityData } }), { headers: { 'content-type': 'application/json' } });
    assert.deepEqual(message.params.arguments, payload);
    assert.deepEqual(events, ['catalog', 'route']);
    if (lost) throw new Error('Socket lost; fixture-secret must not leak');
    if (httpError) return new Response('fixture-secret must not leak', { status: 401 });
    if (invalidJson) return new Response('fixture-secret must not leak', { headers: { 'content-type': 'application/json' } });
    const envelope = { jsonrpc: '2.0', id: message.id, ...(invalidParams ? { error: { code: -32602, message: 'Invalid parameters: role must be implementation' } }
      : rpcError ? { error: { code: -32603, message: 'private' } }
      : { result: mcpError ? { isError: true, content: [{ type: 'text', text: 'Rejected input' }] } : { structuredContent: launched } }) };
    if (!sse) return new Response(JSON.stringify(envelope), { headers: { 'content-type': 'application/json' } });
    const source = ': comment\r\ndata: null\r\n\r\nevent: message\r\ndata: {"jsonrpc":"2.0","method":"notifications/progress"}\r\n\r\n'
      + 'event: message\r\ndata: ' + JSON.stringify(envelope) + '\r\n\r\n';
    const bytes = new TextEncoder().encode(source);
    return new Response(new ReadableStream({ start(controller) {
      for (let i = 0; i < bytes.length; i += 7) controller.enqueue(bytes.slice(i, i + 7));
      // Keep the SSE stream open: the client must return after the matching envelope.
    } }), { headers: { 'content-type': 'text/event-stream' } });
  };
  return { calls, options: { fetchImpl, catalog: cap => { assert.deepEqual(cap, capabilities); events.push('catalog'); },
    route: (value, cap) => { assert.deepEqual(value, payload); assert.deepEqual(cap, capabilityData); events.push('route');
      if (denied) throw new Error('model-route denied before dispatch'); } } };
}

for (const sse of [false, true]) {
  const f = fixture({ sse });
  const result = await callTool('delegate_task', payload, connection, f.options);
  assert.deepEqual(result.structuredContent, launched);
  assert.equal(f.calls.filter(c => c.params?.name === 'delegate_task').length, 1);
}
for (const sse of [false, true]) {
  const f = fixture({ sse, invalidParams: true });
  await assert.rejects(callTool('delegate_task', payload, connection, f.options), error => {
    assert.match(error.message, /-32602.*role must be implementation/);
    assert.ok(!error.message.includes('outcome unknown'));
    return true;
  });
  assert.equal(f.calls.filter(c => c.params?.name === 'delegate_task').length, 1);
}
const denied = fixture({ denied: true });
await assert.rejects(callTool('delegate_task', payload, connection, denied.options), /denied before dispatch/);
assert.equal(denied.calls.filter(c => c.params?.name === 'delegate_task').length, 0);
for (const options of [{ lost: true }, { httpError: true }, { invalidJson: true }, { sse: true, rpcError: true }]) {
  const f = fixture(options);
  await assert.rejects(callTool('delegate_task', payload, connection, f.options), error => {
    assert.match(error.message, /outcome unknown.*Do not retry/);
    assert.ok(!error.message.includes('fixture-secret'));
    return true;
  });
  assert.equal(f.calls.filter(c => c.params?.name === 'delegate_task').length, 1);
}
for (const capabilityData of [null, { ...capabilities, features: { appOwnedSubagents: false } },
  { ...capabilities, inheritedProviderInstanceId: 'unknown' }]) {
  const f = fixture({ capabilityData });
  await assert.rejects(callTool('delegate_task', payload, connection, f.options), /unavailable/);
  assert.equal(f.calls.filter(c => c.params?.name === 'delegate_task').length, 0);
}
const rejected = fixture({ mcpError: true });
assert.equal((await callTool('delegate_task', payload, connection, rejected.options)).isError, true);
await assert.rejects(callTool('delegate_task', payload, connection, {}), /Catalog refresh/);
console.log('t3 shell MCP tests passed');
