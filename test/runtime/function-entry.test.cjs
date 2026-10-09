'use strict';

// Tests for the IPC shell src/function_entry.cjs. Run with: node --test test/runtime/*.test.cjs
//
// The shell is started the way the Rust runner starts a worker: `node function-entry.cjs`
// with cwd = a runtime directory, framed requests on stdin, framed responses on stdout.

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawn } = require('node:child_process');

const SRC = path.resolve(__dirname, '../../src');

const FAKE_ROUTING = `
exports.resolveRoutes = async ({ url, pathnames }) =>
  pathnames.includes(url.pathname) ? { resolvedPathname: url.pathname } : {};
`;

const HANDLERS = {
  log: `module.exports = async (req, res) => {
  console.log('console.log from handler');
  console.info('console.info from handler');
  console.debug('console.debug from handler');
  process.stdout.write('raw stdout from handler\\n');
  res.setHeader('set-cookie', ['a=1', 'b=2']);
  res.end('logged');
};`,
  echo: `module.exports = async (req, res) => {
  const chunks = [];
  for await (const chunk of req) chunks.push(chunk);
  res.end(JSON.stringify({ url: req.url, cookie: req.headers.cookie, body: Buffer.concat(chunks).toString() }));
};`,
  'fail-after-head': `module.exports = async (req, res) => {
  res.writeHead(200);
  await new Promise((resolve) => res.write('partial', resolve));
  throw new Error('late failure');
};`,
};

function makeRuntime({ metadata = true } = {}) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'meshscale-function-entry-test-'));
  fs.copyFileSync(path.join(SRC, 'function_entry.cjs'), path.join(dir, 'function-entry.cjs'));
  fs.copyFileSync(path.join(SRC, 'runtime.cjs'), path.join(dir, 'runtime.cjs'));
  const routing = path.join(dir, 'node_modules', '@next', 'routing');
  fs.mkdirSync(routing, { recursive: true });
  fs.writeFileSync(path.join(routing, 'index.js'), FAKE_ROUTING);
  fs.mkdirSync(path.join(dir, '.next'));
  for (const [name, source] of Object.entries(HANDLERS)) {
    fs.writeFileSync(path.join(dir, `${name}.cjs`), source);
  }
  if (metadata) {
    fs.writeFileSync(path.join(dir, '.next', 'meshscale-adapter.json'), JSON.stringify({
      version: 1,
      buildId: 'test',
      config: { basePath: '' },
      routing: {},
      outputs: {
        appRoutes: Object.keys(HANDLERS).map((name) => ({ id: name, pathname: `/${name}`, filePath: `${name}.cjs` })),
      },
    }));
  }
  return dir;
}

function encodeFrame(header, body = Buffer.alloc(0)) {
  const json = Buffer.from(JSON.stringify({ ...header, length: body.length }));
  const prefix = Buffer.alloc(8);
  prefix.writeUInt32BE(json.length, 0);
  prefix.writeUInt32BE(body.length, 4);
  return Buffer.concat([prefix, json, body]);
}

function startWorker(dir) {
  const child = spawn(process.execPath, ['function-entry.cjs'], { cwd: dir, stdio: ['pipe', 'pipe', 'pipe'] });
  let buffer = Buffer.alloc(0);
  let stderr = '';
  const frames = [];
  const waiters = [];
  child.stderr.on('data', (chunk) => {
    stderr += chunk;
  });
  child.stdout.on('data', (chunk) => {
    buffer = Buffer.concat([buffer, chunk]);
    while (buffer.length >= 8) {
      const headerLength = buffer.readUInt32BE(0);
      const bodyLength = buffer.readUInt32BE(4);
      assert.ok(headerLength < 1024 * 1024, `corrupt frame stream: header length ${headerLength}`);
      if (buffer.length < 8 + headerLength + bodyLength) break;
      const header = JSON.parse(buffer.subarray(8, 8 + headerLength).toString('utf8'));
      const body = buffer.subarray(8 + headerLength, 8 + headerLength + bodyLength);
      buffer = buffer.subarray(8 + headerLength + bodyLength);
      frames.push({ header, body });
      waiters.splice(0).forEach((wake) => wake());
    }
  });
  const exited = new Promise((resolve) => child.on('exit', (code) => resolve(code)));
  async function until(predicate) {
    while (!predicate(frames)) {
      await Promise.race([new Promise((resolve) => waiters.push(resolve)), exited]);
      if (child.exitCode !== null && !predicate(frames)) throw new Error(`worker exited: ${stderr}`);
    }
  }
  return { child, frames, until, exited, stderr: () => stderr };
}

// Windows cannot remove a running process's cwd: stop the worker first.
async function stopWorker(worker, dir) {
  if (worker.child.exitCode === null) worker.child.kill();
  await worker.exited;
  fs.rmSync(dir, { recursive: true, force: true });
}

async function call(worker, id, uri, { headers = [], body = Buffer.alloc(0) } = {}) {
  worker.child.stdin.write(encodeFrame({ kind: 'request', id, method: 'POST', uri, headers }, body));
  await worker.until((frames) => frames.some((f) => f.header.id === id && ['end', 'error'].includes(f.header.kind)));
  return worker.frames.filter((f) => f.header.id === id);
}

test('application stdout writes go to stderr and never corrupt the frame stream', async (t) => {
  const dir = makeRuntime();
  const worker = startWorker(dir);
  t.after(() => stopWorker(worker, dir));
  await worker.until((frames) => frames.length > 0);
  assert.equal(worker.frames[0].header.kind, 'ready');

  const frames = await call(worker, 1, '/log');
  assert.deepEqual(frames.map((f) => f.header.kind), ['headers', 'chunk', 'end']);
  assert.equal(frames[0].header.status, 200);
  assert.deepEqual(frames[0].header.headers, [['set-cookie', 'a=1'], ['set-cookie', 'b=2']]);
  assert.equal(frames[1].body.toString(), 'logged');

  // The worker keeps serving after application logging.
  const echo = await call(worker, 2, '/echo?a=1', { headers: [['cookie', 'x=1'], ['cookie', 'y=2']], body: Buffer.from('payload') });
  assert.deepEqual(JSON.parse(echo[1].body.toString()), { url: '/echo?a=1', cookie: 'x=1; y=2', body: 'payload' });

  for (const line of ['console.log from handler', 'console.info from handler', 'console.debug from handler', 'raw stdout from handler']) {
    assert.ok(worker.stderr().includes(line), `${line} missing from stderr`);
  }
});

test('a failure after the head is reported as an error frame', async (t) => {
  const dir = makeRuntime();
  const worker = startWorker(dir);
  t.after(() => stopWorker(worker, dir));
  const frames = await call(worker, 7, '/fail-after-head');
  assert.deepEqual(frames.map((f) => f.header.kind), ['headers', 'chunk', 'error']);
  assert.equal(frames[1].body.toString(), 'partial');
  assert.match(frames[2].header.error, /late failure/);

  // Unknown routes are a normal 404 response.
  const missing = await call(worker, 8, '/nowhere');
  assert.equal(missing[0].header.status, 404);
  assert.equal(missing[1].body.toString(), 'Not Found');
});

test('startup failure exits non-zero before the ready frame', async (t) => {
  const dir = makeRuntime({ metadata: false });
  const worker = startWorker(dir);
  t.after(() => stopWorker(worker, dir));
  const code = await worker.exited;
  assert.notEqual(code, 0);
  assert.equal(worker.frames.length, 0);
  assert.match(worker.stderr(), /failed to start/);
});
