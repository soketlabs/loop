'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const { spawn } = require('node:child_process');
const { createServer } = require('node:http');
const { once } = require('node:events');
const { mkdtemp, mkdir, rm, writeFile } = require('node:fs/promises');
const { existsSync } = require('node:fs');
const path = require('node:path');
const os = require('node:os');

const ROOT = path.resolve(__dirname, '../..');
const BINARY = process.env.LOOP_VISUAL_BIN || path.join(ROOT, 'target/debug/loop-visual');
const TIMEOUT_MS = 8000;

function waitFor(read, predicate, description, timeoutMs = TIMEOUT_MS) {
  const started = Date.now();
  return new Promise((resolve, reject) => {
    const poll = () => {
      const value = read();
      const match = predicate(value);
      if (match) return resolve(match);
      if (Date.now() - started > timeoutMs) return reject(new Error(`Timed out waiting for ${description}`));
      setTimeout(poll, 10).unref?.();
    };
    poll();
  });
}

async function makeAgentDir({ baseUrl, vision = true }) {
  const dir = await mkdtemp(path.join(os.tmpdir(), 'loop-visual-sidecar-'));
  await mkdir(dir, { recursive: true });
  const model = {
    id: 'test-vision', name: 'Isolated test model', api: 'openai-completions', provider: 'visual-test',
    baseUrl, reasoning: false, input: vision ? ['text', 'image'] : ['text'],
    cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 }, contextWindow: 32768, maxTokens: 2048,
  };
  await Promise.all([
    writeFile(path.join(dir, 'settings.json'), JSON.stringify({ defaultProvider: 'visual-test', defaultModel: model.id })),
    writeFile(path.join(dir, 'models.json'), JSON.stringify({ providers: [{
      id: 'visual-test', name: 'Local test provider', baseUrl, apiKeyEnv: ['LOOP_VISUAL_TEST_KEY'], models: ['pin-placeholder'],
    }] })),
    writeFile(path.join(dir, 'models-store.json'), JSON.stringify({ 'visual-test': { checkedAt: 1, models: [model] } })),
  ]);
  return dir;
}

function startSidecar(agentDir) {
  const child = spawn(BINARY, [], {
    cwd: ROOT,
    env: { ...process.env, LOOP_VISUAL_MODEL: 'visual-test/test-vision', LOOP_CODING_AGENT_DIR: agentDir, LOOP_VISUAL_TEST_KEY: 'test-only-key', RUST_LOG: 'error' },
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  const events = [];
  let stdout = '';
  let stderr = '';
  child.stdout.setEncoding('utf8');
  child.stderr.setEncoding('utf8');
  child.stdout.on('data', (chunk) => {
    stdout += chunk;
    for (;;) {
      const newline = stdout.indexOf('\n');
      if (newline < 0) break;
      const line = stdout.slice(0, newline);
      stdout = stdout.slice(newline + 1);
      try { events.push(JSON.parse(line)); } catch { events.push({ type: 'invalid-json', line }); }
    }
  });
  child.stderr.on('data', (chunk) => { stderr += chunk; });
  return {
    child, events,
    get stderr() { return stderr; },
    waitEvent(predicate, description) {
      return waitFor(() => events, (items) => {
        const found = items.find(predicate);
        if (found) return found;
        const failure = items.find((event) => event.type === 'error');
        if (failure) throw new Error(`Sidecar emitted an error while waiting for ${description}: ${failure.message}`);
        return null;
      }, description).catch((error) => {
        throw new Error(`${error.message}; events=${JSON.stringify(events)}; stderr=${stderr}`);
      });
    },
    send(command) { child.stdin.write(`${JSON.stringify(command)}\n`); },
    async stop() {
      if (child.exitCode !== null) return;
      child.stdin.write('{"type":"shutdown"}\n');
      const exited = once(child, 'exit');
      const timeout = setTimeout(() => child.kill('SIGKILL'), 2500);
      timeout.unref();
      await Promise.race([exited, new Promise((resolve) => setTimeout(resolve, 3000))]);
      clearTimeout(timeout);
      if (child.exitCode === null) child.kill('SIGKILL');
    },
  };
}

function makeMockProvider() {
  const requests = [];
  let server;
  return {
    requests,
    async listen() {
      server = createServer((request, response) => {
        let raw = '';
        request.setEncoding('utf8');
        request.on('data', (chunk) => { raw += chunk; });
        request.on('end', () => {
          let body;
          try { body = JSON.parse(raw); } catch { response.writeHead(400).end('bad json'); return; }
          const item = { body, response, closed: false };
          requests.push(item);
          response.on('close', () => { item.closed = true; });
        });
      });
      server.listen(0, '127.0.0.1');
      await once(server, 'listening');
      return `http://127.0.0.1:${server.address().port}/v1`;
    },
    waitRequest(index) {
      return waitFor(() => requests, (items) => items[index], `provider request ${index + 1}`);
    },
    respond(index, chunks) {
      const item = requests[index];
      assert.ok(item, `missing provider request ${index + 1}`);
      if (item.response.destroyed) return;
      item.response.writeHead(200, { 'content-type': 'text/event-stream', connection: 'keep-alive' });
      for (const chunk of chunks) item.response.write(`data: ${JSON.stringify(chunk)}\n\n`);
      item.response.end('data: [DONE]\n\n');
    },
    async close() {
      if (!server?.listening) return;
      server.closeAllConnections();
      server.close();
      await once(server, 'close');
    },
  };
}

function textTurn(text) {
  return [
    { choices: [{ index: 0, delta: { content: text }, finish_reason: null }] },
    { choices: [{ index: 0, delta: {}, finish_reason: 'stop' }] },
  ];
}

function visualToolTurn(title, html) {
  return [
    { choices: [{ index: 0, delta: { tool_calls: [{
      index: 0, id: `plan-${title}`, type: 'function', function: {name:'plan_visual',arguments:JSON.stringify({design:'Explain the source with a spatial causal diagram. Arrange labeled objects with connecting arrows, a camera framing all relationships, and a control that changes the mechanism.'})}
    }, {
      index: 1, id: `call-${title}`, type: 'function',
      function: { name: 'show_visual', arguments: JSON.stringify({ title, html }) },
    }] }, finish_reason: null }] },
    { choices: [{ index: 0, delta: {}, finish_reason: 'tool_calls' }] },
  ];
}

function userMessages(body) {
  return body.messages.filter((message) => message.role === 'user');
}

function imageUrls(message) {
  const content = Array.isArray(message.content) ? message.content : [];
  return content.filter((block) => block.type === 'image_url').map((block) => block.image_url.url);
}

test('sidecar carries the latest image and visual context, and ignores a cancelled turn result', {
  skip: !existsSync(BINARY) && `built sidecar not found at ${BINARY}`,
}, async (t) => {
  const provider = makeMockProvider();
  const baseUrl = await provider.listen();
  const agentDir = await makeAgentDir({ baseUrl });
  const sidecar = startSidecar(agentDir);
  t.after(async () => {
    await sidecar.stop();
    await provider.close();
    await rm(agentDir, { recursive: true, force: true });
  });

  await sidecar.waitEvent((event) => event.type === 'ready', 'sidecar ready event');

  sidecar.send({ type: 'prompt', requestId: 1, text: 'Explain scene one', image: {
    data: Buffer.from('first-screen-fixture').toString('base64'), mimeType: 'image/png',
  }, context: { screen: { description: 'fixture one' }, visual: null } });
  const first = await provider.waitRequest(0);
  assert.equal(first.body.model, 'test-vision');
  assert.ok(imageUrls(userMessages(first.body)[0]).some((url) => url.includes(Buffer.from('first-screen-fixture').toString('base64'))),
    'the initial request should contain its screen image');

  provider.respond(0, visualToolTurn('Scene one graph', '<!doctype html><h1>Scene one fixture</h1>'));
  const visual = await sidecar.waitEvent((event) => event.type === 'visual' && event.requestId === 1, 'show_visual output');
  assert.equal(visual.title, 'Scene one graph');
  assert.match(visual.html, /Scene one fixture/);
  const afterTool = await provider.waitRequest(1);
  provider.respond(1, textTurn('Here is the explanation.'));
  await sidecar.waitEvent((event) => event.type === 'done' && event.requestId === 1, 'first turn completion');
  assert.ok(afterTool.body.messages.some((message) => message.role === 'tool' || message.role === 'toolResult'),
    'the model should receive the show_visual tool result before completing the turn');

  sidecar.send({ type: 'prompt', requestId: 2, text: 'Update the visual using this scene', image: {
    data: Buffer.from('second-latest-screen').toString('base64'), mimeType: 'image/png',
  }, context: { screen: { description: 'fixture two' }, visual: {
    title: 'Scene one graph', html: '<!doctype html><h1>Scene one fixture</h1>',
    state: { selected: 'bar-2', controls: { threshold: 73, mode: 'weekly' } },
  } } });
  const followup = await provider.waitRequest(2);
  const followupJson = JSON.stringify(followup.body);
  assert.ok(followupJson.includes(Buffer.from('second-latest-screen').toString('base64')),
    'follow-up should include the latest screen image');
  assert.ok(followupJson.includes('threshold') && followupJson.includes('73') && followupJson.includes('weekly'),
    'follow-up should include prior visual control state');
  assert.ok(followupJson.includes('Scene one fixture'), 'follow-up should include the previous visual context');
  assert.ok(!followupJson.includes(Buffer.from('first-screen-fixture').toString('base64')),
    'follow-up history must not contain the previous screen image');
  provider.respond(2, textTurn('Updated with the latest scene.'));
  await sidecar.waitEvent((event) => event.type === 'done' && event.requestId === 2, 'follow-up completion');

  sidecar.send({ type: 'prompt', requestId: 3, text: 'This turn will be cancelled', image: {
    data: Buffer.from('cancelled-screen').toString('base64'), mimeType: 'image/png',
  }, context: {} });
  await provider.waitRequest(3);
  sidecar.send({ type: 'cancel', requestId: 3 });
  await waitFor(() => provider.requests[3], (request) => request.closed, 'cancelled provider stream to close');

  sidecar.send({ type: 'prompt', requestId: 4, text: 'Resubmit after cancellation', image: {
    data: Buffer.from('resubmitted-screen').toString('base64'), mimeType: 'image/png',
  }, context: {} });
  const fresh = await provider.waitRequest(4);
  assert.ok(userMessages(fresh.body).some((message) =>
    imageUrls(message).some((url) => url.includes(Buffer.from('resubmitted-screen').toString('base64')))),
  'resubmitted request should include its fresh screen image');
  provider.respond(4, textTurn('Fresh answer only.'));
  await sidecar.waitEvent((event) => event.type === 'done' && event.requestId === 4, 'resubmitted completion');

  // A late provider write after cancellation must not reappear as an old visual.
  provider.respond(3, visualToolTurn('Stale cancelled visual', '<!doctype html><h1>stale fixture</h1>'));
  await new Promise((resolve) => setTimeout(resolve, 300));
  assert.ok(!sidecar.events.some((event) => event.type === 'visual' && event.title === 'Stale cancelled visual'),
    'cancelled work must not emit its delayed visual');
  assert.ok(!sidecar.events.some((event) => event.type === 'done' && event.requestId === 3),
    'cancelled work must not emit a completion event');
});

test('sidecar rejects a cached model that does not advertise image input', {
  skip: !existsSync(BINARY) && `built sidecar not found at ${BINARY}`,
}, async (t) => {
  const agentDir = await makeAgentDir({ baseUrl: 'http://127.0.0.1:9/v1', vision: false });
  const sidecar = startSidecar(agentDir);
  t.after(async () => {
    await sidecar.stop();
    await rm(agentDir, { recursive: true, force: true });
  });
  const error = await sidecar.waitEvent((event) => event.type === 'error', 'vision capability startup error');
  assert.match(error.message, /does not advertise image support/);
});
