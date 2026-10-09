'use strict';

// Smoke test for the generated Lambda entrypoint. Run with: node --test test/runtime/*.test.cjs
//
// Lays out a runtime directory the way artifact.rs generates it (hyphenated file names,
// stub @next/routing, stub adapter outputs) outside the source checkout, then loads
// lambda-entry.cjs in a separate Node process and invokes it with event fixtures. As on Lambda
// (cwd /var/task), the cwd is the runtime directory: Next resolves .next/ against process.cwd().
// The real-artifact version of this check runs in `cargo test builds_fixture_app -- --ignored`.

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { execFileSync } = require('node:child_process');

const SRC = path.resolve(__dirname, '../../src');
const EVENTS = path.resolve(__dirname, '../fixtures/events');
const GENERATED = {
  'runtime.cjs': 'runtime.cjs',
  'lambda-adapter.cjs': 'lambda_adapter.cjs',
  'lambda-entry.cjs': 'lambda_entry.cjs',
};

const HANDLER = `
module.exports = async (req, res, ctx) => {
  const chunks = [];
  for await (const chunk of req) chunks.push(chunk);
  ctx.waitUntil(new Promise((resolve) => setTimeout(() => { globalThis.background = 'done'; resolve(); }, 30)));
  res.setHeader('content-type', req.method === 'POST' && req.headers['content-type'] === 'application/octet-stream'
    ? 'application/octet-stream' : 'application/json');
  res.setHeader('set-cookie', ['one=1', 'two=2']);
  if (res.getHeader('content-type') === 'application/octet-stream') {
    res.end(Buffer.concat(chunks));
    return;
  }
  res.end(JSON.stringify({ method: req.method, url: req.url, host: req.headers.host, cookie: req.headers.cookie || null }));
};`;

function makeRuntime() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'meshscale-lambda-entry-test-'));
  for (const [generated, source] of Object.entries(GENERATED)) {
    fs.copyFileSync(path.join(SRC, source), path.join(dir, generated));
  }
  const routing = path.join(dir, 'node_modules', '@next', 'routing');
  fs.mkdirSync(routing, { recursive: true });
  fs.writeFileSync(path.join(routing, 'index.js'),
    'exports.resolveRoutes = async ({ url }) => ({ resolvedPathname: url.pathname === "/missing" ? undefined : "/any" });');
  fs.mkdirSync(path.join(dir, '.next'));
  fs.writeFileSync(path.join(dir, 'handler.cjs'), HANDLER);
  fs.writeFileSync(path.join(dir, '.next', 'meshscale-adapter.json'), JSON.stringify({
    version: 1,
    buildId: 'test',
    config: { basePath: '' },
    routing: {},
    outputs: { appRoutes: [{ id: 'any', pathname: '/any', filePath: 'handler.cjs' }] },
  }));
  return dir;
}

// Loads lambda-entry.cjs in a fresh Node process and returns the handler results for the
// named event fixtures.
function invokeResults(dir, names) {
  const events = names.map((name) => JSON.parse(fs.readFileSync(path.join(EVENTS, `${name}.json`), 'utf8')));
  const script = path.join(dir, 'run.cjs');
  fs.writeFileSync(script, `
const { handler } = require('./lambda-entry.cjs');
(async () => {
  const results = [];
  for (const event of ${JSON.stringify(events)}) {
    results.push({ result: await handler(event, {}), background: globalThis.background || null });
  }
  require('node:fs').writeFileSync(process.argv[2], JSON.stringify(results));
})().catch((error) => { console.error(error); process.exitCode = 1; });`);
  const out = path.join(dir, 'results.json');
  execFileSync(process.execPath, [script, out], { cwd: dir, stdio: ['ignore', 'ignore', 'pipe'] });
  return JSON.parse(fs.readFileSync(out, 'utf8'));
}

test('generated lambda-entry.cjs loads from a runtime directory and answers events', (t) => {
  const dir = makeRuntime();
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  const [probe, get, binary, head] = invokeResults(dir, ['probe', 'get-cookies', 'post-binary-base64', 'head']);

  assert.equal(probe.result.statusCode, 200);
  assert.deepEqual(JSON.parse(probe.result.body), { status: 'ok', probe: 'init' });

  assert.equal(get.result.statusCode, 200);
  assert.deepEqual(get.result.cookies, ['one=1', 'two=2']);
  assert.deepEqual(JSON.parse(get.result.body), {
    method: 'GET', url: '/api/echo', host: 'app.example.test', cookie: 'a=1; b=2',
  });
  // waitUntil work finished before the handler returned.
  assert.equal(get.background, 'done');

  const sent = JSON.parse(fs.readFileSync(path.join(EVENTS, 'post-binary-base64.json'), 'utf8')).body;
  assert.equal(binary.result.isBase64Encoded, true);
  assert.equal(binary.result.body, sent);

  assert.equal(head.result.statusCode, 200);
  assert.equal(head.result.body, '');
});
