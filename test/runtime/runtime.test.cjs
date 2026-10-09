'use strict';

// Tests for src/runtime.cjs. Run with: node --test test/runtime/*.test.cjs
//
// Each test builds a runtime directory like the generated artifact's runtime/: runtime.cjs,
// .next/meshscale-adapter.json, a stub @next/routing and stub adapter output handlers.
// No Next.js is involved.

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const SOURCE = path.resolve(__dirname, '../../src/runtime.cjs');

const FAKE_ROUTING = `
'use strict';
exports.resolveRoutes = async ({ url, pathnames, routes, headers }) => {
  globalThis.routingCalls = (globalThis.routingCalls || 0) + 1;
  globalThis.lastRoutingHost = headers.get('host');
  const redirect = routes.redirects && routes.redirects[url.pathname];
  if (redirect) return { redirect: { status: 308, url: new URL(redirect, url) } };
  if (url.pathname === '/k6') {
    return {
      resolvedPathname: '/echo',
      resolvedHeaders: new Headers({ 'x-from-routing': '1' }),
      resolvedQuery: { rewritten: '1' },
      invocationTarget: { pathname: '/echo', query: { rewritten: '1' } },
      status: 418,
    };
  }
  return pathnames.includes(url.pathname) ? { resolvedPathname: url.pathname } : {};
};
`;

const HANDLERS = {
  'echo.cjs': `
module.exports = async (req, res) => {
  const chunks = [];
  for await (const chunk of req) chunks.push(chunk);
  res.setHeader('Content-Type', 'application/json');
  res.setHeader('Set-Cookie', ['a=1; Path=/', 'b=2; Path=/']);
  res.end(JSON.stringify({
    method: req.method,
    url: req.url,
    headers: req.headers,
    rawHeaders: req.rawHeaders,
    httpVersion: req.httpVersion,
    remoteAddress: req.socket.remoteAddress,
    body: Buffer.concat(chunks).toString('base64'),
  }));
};`,
  'stream.cjs': `
module.exports = async (req, res) => {
  res.writeHead(201, { 'x-step': 'head' });
  await globalThis.streamGate;
  res.write('one,');
  res.write(Buffer.from('two,'));
  res.write('');
  res.end('three');
};`,
  'throw-before-head.cjs': `
module.exports = async (req, res) => {
  res.setHeader('x-before', 'kept');
  throw new Error('boom before head');
};`,
  'throw-after-head.cjs': `
module.exports = async (req, res) => {
  res.writeHead(200, { 'content-type': 'text/plain' });
  await new Promise((resolve) => res.write('partial', resolve));
  throw new Error('boom after head');
};`,
  'destroy-before-head.cjs': `
module.exports = async (req, res) => { res.destroy(); };`,
  'no-end.cjs': `
module.exports = async (req, res) => { res.statusCode = 204; };`,
  'wait-until.cjs': `
module.exports = async (req, res, ctx) => {
  const delay = Number(new URL(req.url, 'http://x').searchParams.get('ms') || 50);
  ctx.waitUntil(new Promise((resolve) => setTimeout(() => {
    globalThis.waitUntilDone = (globalThis.waitUntilDone || 0) + 1;
    resolve();
  }, delay)));
  ctx.waitUntil(Promise.reject(new Error('background failure')));
  res.end('ok:' + ctx.requestMeta.hostname);
};`,
  'big.cjs': `
module.exports = async (req, res) => {
  for (let i = 0; i < 64; i += 1) {
    if (!res.write(Buffer.alloc(64 * 1024, i))) await new Promise((resolve) => res.once('drain', resolve));
  }
  globalThis.bigFinished = true;
  res.end();
};`,
  'abandoned.cjs': `
module.exports = async (req, res) => {
  res.writeHead(200);
  res.on('close', () => { globalThis.abandonedClosed = true; });
  for (let i = 0; i < 1000 && !res.destroyed; i += 1) {
    await new Promise((resolve) => res.write(Buffer.alloc(1024), resolve));
  }
};`,
  'counted.cjs': `
globalThis.countedLoads = (globalThis.countedLoads || 0) + 1;
exports.handler = async (req, res) => res.end(String(globalThis.countedLoads));`,
  'not-a-function.cjs': `module.exports = { nope: true };`,
};

const ROUTES = Object.keys(HANDLERS).map((file) => '/' + file.replace(/\.cjs$/, ''));

function makeRoot() {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'meshscale-runtime-test-'));
  fs.copyFileSync(SOURCE, path.join(root, 'runtime.cjs'));
  const routing = path.join(root, 'node_modules', '@next', 'routing');
  fs.mkdirSync(routing, { recursive: true });
  fs.writeFileSync(path.join(routing, 'package.json'), JSON.stringify({ name: '@next/routing', main: 'index.js' }));
  fs.writeFileSync(path.join(routing, 'index.js'), FAKE_ROUTING);
  fs.mkdirSync(path.join(root, 'handlers'));
  for (const [file, source] of Object.entries(HANDLERS)) {
    fs.writeFileSync(path.join(root, 'handlers', file), source);
  }
  fs.mkdirSync(path.join(root, '.next'));
  const outputs = Object.keys(HANDLERS).map((file) => ({
    id: file,
    pathname: '/' + file.replace(/\.cjs$/, ''),
    filePath: 'handlers/' + file,
  }));
  fs.writeFileSync(path.join(root, '.next', 'meshscale-adapter.json'), JSON.stringify({
    version: 1,
    buildId: 'test',
    config: { basePath: '', i18n: null },
    routing: { redirects: { '/old': '/echo' } },
    outputs: { pages: outputs.slice(0, 3), pagesApi: outputs.slice(3), appPages: [], appRoutes: [] },
  }));
  return root;
}

const root = makeRoot();
test.after(() => fs.rmSync(root, { recursive: true, force: true }));
const { createRuntime } = require(path.join(root, 'runtime.cjs'));
const runtimePromise = createRuntime({ root });

function request(url, { method = 'GET', headers = [], body } = {}) {
  return { method, url, headers, body: body || Buffer.alloc(0) };
}

async function collect(response) {
  const chunks = [];
  for await (const chunk of response.body) chunks.push(chunk);
  await response.done;
  return Buffer.concat(chunks);
}

test('runtime.cjs stays transport-neutral', () => {
  const source = fs.readFileSync(SOURCE, 'utf8').replace(/^\s*\/\/.*$/gm, '');
  for (const forbidden of ['process.stdout', 'process.stdin', 'process.exit', 'console.log', /lambda/i, /frame/i]) {
    assert.doesNotMatch(source, forbidden instanceof RegExp ? forbidden : new RegExp(forbidden.replace('.', '\\.')));
  }
  assert.equal(ROUTES.length, Object.keys(HANDLERS).length);
});

test('builds a Node-like request: url as received, headers, cookie join, body bytes', async () => {
  const runtime = await runtimePromise;
  const body = Buffer.from([0, 1, 2, 250, 255]);
  const response = await runtime.invoke(request('/echo?x=1&x=2&enc=a%20b', {
    method: 'POST',
    headers: [
      ['Host', 'app.example.test'],
      ['Cookie', 'a=1'],
      ['cookie', 'b=2'],
      ['X-Multi', 'one'],
      ['x-multi', 'two'],
      ['set-cookie', 'ignored=1'],
      ['set-cookie', 'ignored=2'],
    ],
    body,
  }));
  assert.equal(response.status, 200);
  const echoed = JSON.parse((await collect(response)).toString('utf8'));
  assert.equal(echoed.method, 'POST');
  assert.equal(echoed.url, '/echo?x=1&x=2&enc=a%20b');
  assert.equal(echoed.headers.host, 'app.example.test');
  assert.equal(echoed.headers.cookie, 'a=1; b=2');
  assert.equal(echoed.headers['x-multi'], 'one, two');
  assert.deepEqual(echoed.headers['set-cookie'], ['ignored=1', 'ignored=2']);
  assert.deepEqual(echoed.rawHeaders.slice(0, 4), ['Host', 'app.example.test', 'Cookie', 'a=1']);
  assert.equal(echoed.httpVersion, '1.1');
  assert.equal(echoed.remoteAddress, '127.0.0.1');
  assert.deepEqual(Buffer.from(echoed.body, 'base64'), body);
  assert.equal(globalThis.lastRoutingHost, 'app.example.test');
});

test('response headers are lowercase pairs and set-cookie is never merged', async () => {
  const runtime = await runtimePromise;
  const response = await runtime.invoke(request('/echo'));
  assert.deepEqual(response.headers, [
    ['content-type', 'application/json'],
    ['set-cookie', 'a=1; Path=/'],
    ['set-cookie', 'b=2; Path=/'],
  ]);
  await collect(response);
});

test('invoke resolves at the head, before the body is produced', async () => {
  const runtime = await runtimePromise;
  let open;
  globalThis.streamGate = new Promise((resolve) => {
    open = resolve;
  });
  const response = await runtime.invoke(request('/stream'));
  assert.equal(response.status, 201);
  assert.deepEqual(response.headers, [['x-step', 'head']]);
  open();
  assert.equal((await collect(response)).toString(), 'one,two,three');
});

test('a failure before the head becomes a controlled 500', async () => {
  const runtime = await runtimePromise;
  const response = await runtime.invoke(request('/throw-before-head'));
  assert.equal(response.status, 500);
  assert.deepEqual(response.headers, [['x-before', 'kept']]);
  assert.equal((await collect(response)).toString(), 'Internal Server Error');

  const destroyed = await runtime.invoke(request('/destroy-before-head'));
  assert.equal(destroyed.status, 500);
  assert.equal((await collect(destroyed)).toString(), 'Internal Server Error');

  const invalid = await runtime.invoke(request('/not-a-function'));
  assert.equal(invalid.status, 500);
  assert.equal((await collect(invalid)).toString(), 'Internal Server Error');
});

test('a failure after the head errors the body and rejects done', async () => {
  const runtime = await runtimePromise;
  const response = await runtime.invoke(request('/throw-after-head'));
  assert.equal(response.status, 200);
  const chunks = [];
  await assert.rejects(async () => {
    for await (const chunk of response.body) chunks.push(chunk);
  }, /boom after head/);
  assert.equal(Buffer.concat(chunks).toString(), 'partial');
  await assert.rejects(response.done, /boom after head/);
});

test('not found, routing redirects and handlers that never end', async () => {
  const runtime = await runtimePromise;
  const missing = await runtime.invoke(request('/nowhere'));
  assert.equal(missing.status, 404);
  assert.equal((await collect(missing)).toString(), 'Not Found');

  const redirect = await runtime.invoke(request('/old', { headers: [['host', 'app.test']] }));
  assert.equal(redirect.status, 308);
  assert.deepEqual(redirect.headers, [['location', 'http://app.test/echo']]);
  assert.equal((await collect(redirect)).length, 0);

  const noEnd = await runtime.invoke(request('/no-end'));
  assert.equal(noEnd.status, 204);
  assert.equal((await collect(noEnd)).length, 0);
});

// K6 characterization (current behavior, not necessarily correct): only `redirect` and
// `resolvedPathname` from resolveRoutes are used. Resolved headers, status, rewritten query
// and invocation target are ignored and the handler sees the original URL.
test('K6: resolveRoutes headers, status and rewritten query are ignored', async () => {
  const runtime = await runtimePromise;
  const response = await runtime.invoke(request('/k6?original=1'));
  assert.equal(response.status, 200);
  assert.ok(!response.headers.some(([name]) => name === 'x-from-routing'));
  const echoed = JSON.parse((await collect(response)).toString('utf8'));
  assert.equal(echoed.url, '/k6?original=1');
});

test('handlers are loaded once and cached', async () => {
  const runtime = await runtimePromise;
  for (let i = 0; i < 3; i += 1) {
    assert.equal((await collect(await runtime.invoke(request('/counted')))).toString(), '1');
  }
  assert.equal(globalThis.countedLoads, 1);
});

test('body is pulled with backpressure and a stopped consumer releases the handler', async () => {
  const runtime = await runtimePromise;
  const big = await runtime.invoke(request('/big'));
  await new Promise((resolve) => setTimeout(resolve, 50));
  assert.notEqual(globalThis.bigFinished, true, 'handler finished without a consumer');
  assert.equal((await collect(big)).length, 64 * 64 * 1024);
  assert.equal(globalThis.bigFinished, true);

  const abandoned = await runtime.invoke(request('/abandoned'));
  for await (const chunk of abandoned.body) {
    assert.equal(chunk.length, 1024);
    break;
  }
  await assert.rejects(abandoned.done);
  await new Promise((resolve) => setTimeout(resolve, 20));
  assert.equal(globalThis.abandonedClosed, true);
});

test('settle waits for waitUntil work, logs failures and honours the timeout', async () => {
  const runtime = await runtimePromise;
  globalThis.waitUntilDone = 0;
  const errors = [];
  const originalError = console.error;
  console.error = (...args) => errors.push(args.map(String).join(' '));
  try {
    const response = await runtime.invoke(request('/wait-until?ms=80', { headers: [['host', 'h.test']] }));
    assert.equal((await collect(response)).toString(), 'ok:h.test');
    assert.equal(globalThis.waitUntilDone, 0);
    assert.deepEqual(await runtime.settle(5000), { pending: 0, timedOut: false });
    assert.equal(globalThis.waitUntilDone, 1);
    assert.ok(errors.some((line) => line.includes('waitUntil task failed') && line.includes('background failure')));

    await collect(await runtime.invoke(request('/wait-until?ms=500')));
    const started = Date.now();
    const result = await runtime.settle(50);
    assert.equal(result.timedOut, true);
    assert.equal(result.pending, 1);
    assert.ok(Date.now() - started < 400);
    assert.deepEqual(await runtime.settle(5000), { pending: 0, timedOut: false });
    assert.deepEqual(await runtime.settle(0), { pending: 0, timedOut: false });
  } finally {
    console.error = originalError;
  }
});

test('createRuntime fails when adapter metadata is missing', async () => {
  const empty = fs.mkdtempSync(path.join(os.tmpdir(), 'meshscale-runtime-empty-'));
  try {
    await assert.rejects(createRuntime({ root: empty }), /meshscale-adapter\.json/);
  } finally {
    fs.rmSync(empty, { recursive: true, force: true });
  }
});
