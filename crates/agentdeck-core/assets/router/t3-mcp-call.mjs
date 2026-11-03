#!/usr/bin/env node
// Reuse only this shell's inherited MCP session; never search other threads for credentials.
import fs from 'node:fs';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { createConnection } from 'node:net';

const TOOLS = ['orchestrator_capabilities', 'delegate_task', 'task_status', 'task_cancel', 't3_thread_organize'];
const USAGE = 'usage: t3-mcp-call TOOL JSON_FILE|- (orchestrator_capabilities needs no file)';
const fail = message => { throw new Error(message); };
const object = v => v !== null && typeof v === 'object' && !Array.isArray(v);

export function connectionFromConfig(config) {
  const server = config?.mcpServers?.['t3-code'];
  if (!server) return null;
  const authorization = Object.entries(server.headers ?? {}).find(([k]) => k.toLowerCase() === 'authorization')?.[1];
  return checkConnection({ endpoint: server.url, authorization });
}

function checkConnection(connection) {
  let url;
  try { url = new URL(connection.endpoint); } catch { fail('Invalid scoped T3 MCP endpoint.'); }
  if (url.protocol !== 'http:' || !['127.0.0.1', 'localhost', '[::1]'].includes(url.hostname)
    || url.username || url.password || url.search || url.hash
    || typeof connection.authorization !== 'string' || !connection.authorization.trim())
    fail('Expected a loopback T3 MCP endpoint with session authorization.');
  return connection;
}

export async function scopedConnection({ env = process.env, connect = createConnection, ...options } = {}) {
  if (!env.T3_SHELL_MCP_SOCKET) return discoverConnection({ env, ...options });
  const connection = await new Promise((resolve, reject) => {
    const socket = connect({ path: env.T3_SHELL_MCP_SOCKET });
    let data = '', settled = false;
    const finish = () => {
      if (settled) return;
      settled = true;
      socket.destroy();
      try {
        const value = JSON.parse(data);
        if (!object(value) || value.error) fail('Claude session MCP bridge is not configured yet.');
        resolve(value);
      } catch { reject(new Error('Claude session MCP bridge returned no valid connection.')); }
    };
    socket.setTimeout(5000);
    socket.on('data', chunk => {
      data += chunk.toString('utf8');
      if (data.length > 16384) { socket.destroy(new Error('Invalid session bridge response.')); return; }
      if (data.includes('\n')) finish();
    });
    socket.once('end', finish);
    socket.once('timeout', () => socket.destroy(new Error('Session bridge timed out.')));
    socket.once('error', () => reject(new Error('Cannot connect to this Claude session MCP bridge.')));
  });
  return checkConnection(connection);
}

export function discoverConnection({ env = process.env, pid = process.ppid, read = fs.readFileSync } = {}) {
  if (env.T3_ACP_MCP_ENDPOINT || env.T3_ACP_MCP_AUTHORIZATION)
    return checkConnection({ endpoint: env.T3_ACP_MCP_ENDPOINT, authorization: env.T3_ACP_MCP_AUTHORIZATION });
  for (let i = 0; pid > 1 && i < 16; i++) {
    let args, parent;
    try {
      args = read(`/proc/${pid}/cmdline`, 'utf8').split('\0');
      const stat = read(`/proc/${pid}/stat`, 'utf8');
      parent = Number(stat.slice(stat.lastIndexOf(') ') + 2).split(' ')[1]);
    } catch { break; }
    const index = args.indexOf('--mcp-config');
    const value = index >= 0 ? args[index + 1] : args.find(a => a.startsWith('--mcp-config='))?.slice(13);
    if (value) {
      let config;
      try { config = JSON.parse(value.trim().startsWith('{') ? value : read(value, 'utf8')); }
      catch { fail('Cannot read this session\'s MCP configuration.'); }
      const connection = connectionFromConfig(config);
      if (connection) return connection;
    }
    pid = parent;
  }
  fail('No inherited T3 MCP connection. Run inside a T3 Claude/ACP session; use native MCP in Codex.');
}

// MCP streamable HTTP can return one JSON envelope or an SSE stream.
export async function rpc(connection, fetchImpl = fetch) {
  let session, protocol, id = 0;
  return async (method, params, notification = false) => {
    const requestId = notification ? undefined : ++id;
    const response = await fetchImpl(connection.endpoint, {
      method: 'POST', redirect: 'error', signal: AbortSignal.timeout(30_000),
      headers: { 'content-type': 'application/json', accept: 'application/json, text/event-stream',
        authorization: connection.authorization,
        ...(session && { 'mcp-session-id': session }), ...(protocol && { 'mcp-protocol-version': protocol }) },
      body: JSON.stringify({ jsonrpc: '2.0', ...(requestId && { id: requestId }), method, ...(params && { params }) }),
    });
    if (!response.ok) fail(`T3 MCP HTTP ${response.status}.`);
    session = response.headers.get('mcp-session-id') ?? session;
    if (notification) { await response.body?.cancel(); return; }
    let envelope;
    if (response.headers.get('content-type')?.includes('text/event-stream')) {
      const reader = response.body.pipeThrough(new TextDecoderStream()).getReader();
      let buffer = '';
      try {
        while (!envelope) {
          const chunk = await reader.read();
          buffer += chunk.value ?? '';
          let match;
          while ((match = /\r?\n\r?\n/.exec(buffer))) {
            const event = buffer.slice(0, match.index); buffer = buffer.slice(match.index + match[0].length);
            const data = event.split(/\r?\n/).filter(l => l.startsWith('data:')).map(l => l.slice(5).replace(/^ /, '')).join('\n');
            if (data) {
              let candidate;
              try { candidate = JSON.parse(data); } catch { fail('Invalid T3 MCP SSE envelope.'); }
              if (object(candidate) && candidate.id === requestId) { envelope = candidate; break; }
            }
          }
          if (chunk.done) break;
        }
      } finally { await reader.cancel(); }
    } else {
      try { envelope = await response.json(); } catch { fail('Invalid T3 MCP JSON envelope.'); }
    }
    if (!envelope || envelope.id !== requestId || envelope.error || !object(envelope.result))
      fail('T3 MCP rejected the request or returned an invalid response.');
    protocol = envelope.result.protocolVersion ?? protocol;
    return envelope.result;
  };
}

export function validatePayload(tool, payload) {
  if (!TOOLS.includes(tool)) fail('Supported tools: ' + TOOLS.join(', '));
  if (!object(payload)) fail('Arguments must be a JSON object.');
  if (tool === 'delegate_task') {
    if (typeof payload.task !== 'string' || !payload.task.trim() || payload.task.length > 120000
      || typeof payload.target?.providerInstanceId !== 'string' || !payload.target.providerInstanceId
      || typeof payload.target?.model !== 'string' || !payload.target.model)
      fail('delegate_task needs a complete task (at most 120000 characters) and exact target.');
    if (payload.mode !== 'async' || typeof payload.clientRequestId !== 'string' || !payload.clientRequestId.trim())
      fail('Shell delegation requires mode=async and a stable clientRequestId.');
    if ('runtimeMode' in payload) fail('Omit runtimeMode to inherit the parent permissions.');
  }
}

export function checkRoute(payload, capabilities, run = spawnSync) {
  const result = run(process.execPath, [fileURLToPath(new URL('./hook.mjs', import.meta.url)), '--harness', 'shell'], {
    input: JSON.stringify({ tool_name: 'delegate_task', tool_input: payload, inT3: true, threadId: capabilities.parentThreadId,
      source: capabilities.inheritedProviderInstanceId }), encoding: 'utf8', timeout: 58_000, maxBuffer: 2 * 1024 * 1024,
  });
  let output;
  try { output = JSON.parse(result.stdout); } catch { fail('model-route failed; nothing dispatched.'); }
  if (result.status !== 0 || !['allow', 'deny'].includes(output.decision)) fail('model-route failed; nothing dispatched.');
  if (output.decision !== 'allow') fail(output.reason || 'model-route denied delegation before dispatch.');
  if (output.reason) process.stderr.write(output.reason + '\n');
}

export async function callTool(tool, payload, connection, { fetchImpl = fetch, route = checkRoute, catalog } = {}) {
  validatePayload(tool, payload);
  if (tool === 'delegate_task' && typeof catalog !== 'function') fail('Catalog refresh is required; nothing dispatched.');
  const send = await rpc(connection, fetchImpl);
  await send('initialize', { protocolVersion: '2025-06-18', capabilities: {},
    clientInfo: { name: 't3-mcp-call', version: '1' } });
  await send('notifications/initialized', undefined, true);
  if (tool === 'delegate_task') {
    const result = await send('tools/call', { name: 'orchestrator_capabilities', arguments: {} });
    const capabilities = result.structuredContent;
    if (result.isError || !object(capabilities) || !capabilities.parentThreadId
      || typeof capabilities.inheritedProviderInstanceId !== 'string'
      || !capabilities.providers?.some(p => p.providerInstanceId === capabilities.inheritedProviderInstanceId)
      || !capabilities.features?.appOwnedSubagents) fail('T3 delegation unavailable; nothing dispatched.');
    catalog(capabilities);
    route(payload, capabilities);
  }
  try { return await send('tools/call', { name: tool, arguments: payload }); }
  catch { fail(tool === 'delegate_task'
    ? 'T3 tool-call outcome unknown. Do not retry delegation: recover its taskId from parent activity first.'
    : 'T3 MCP tool call failed.'); }
}

export function readPayload(file, read = fs.readFileSync) {
  let parsed;
  try { parsed = file ? JSON.parse(read(file === '-' ? 0 : file, 'utf8')) : {}; }
  catch { fail('Cannot read valid JSON arguments.'); }
  // workflow prepare emits a wrapper; send its payload exactly as saved.
  return parsed?.payload ?? parsed;
}

async function main() {
  const [tool, file, ...extra] = process.argv.slice(2);
  if (tool === '--help') { console.log(USAGE); return; }
  if (extra.length || !tool || (!file && tool !== 'orchestrator_capabilities')) fail(USAGE);
  const payload = readPayload(file);
  validatePayload(tool, payload);
  const result = await callTool(tool, payload, await scopedConnection(), {
    catalog: cap => {
      const result = spawnSync(process.execPath, [fileURLToPath(new URL('./route.mjs', import.meta.url)), '--catalog'], {
        input: JSON.stringify(cap), encoding: 'utf8', timeout: 10_000,
      });
      if (result.status !== 0) fail('Cannot refresh the model catalog; nothing dispatched.');
    },
  });
  console.log(JSON.stringify(result));
  if (result.isError) process.exitCode = 1;
}

if (process.argv[1] && fs.realpathSync(process.argv[1]) === fileURLToPath(import.meta.url))
  main().catch(error => { console.error(error.message); process.exitCode = 1; });
