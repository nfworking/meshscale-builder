'use strict';

// Tests for src/lambda_local_host.cjs. Run with: node --test test/runtime/*.test.cjs
//
// The host is started exactly the way the Rust runner starts a worker: as a child
// process whose cwd is a runtime directory, speaking framed requests on stdin and
// framed responses on stdout. A fake lambda-entry.cjs stands in for the real one.

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawn } = require('node:child_process');

const HOST = path.resolve(__dirname, '../../src/lambda_local_host.cjs');

const FAKE_ENTRY = `
'use strict';
exports.handler = async (event, context) => {
  const path = event.rawPath;
  if (path === '/echo') {
    return {
      statusCode: 200,
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ event, hasContext: typeof context.getRemainingTimeInMillis() === 'number', env: process.env.AWS_LAMBDA_FUNCTION_NAME }),
    };
  }
  if (path === '/cookies') {
    return { statusCode: 200, headers: { 'content-type': 'text/plain' }, cookies: ['a=1; Path=/; HttpOnly', 'b=2; Path=/'], body: 'ok' };
  }
  if (path === '/binary') {
    return { statusCode: 200, headers: { 'content-type': 'application/octet-stream' }, isBase64Encoded: true, body: Buffer.from([0, 255, 1, 254]).toString('base64') };
  }
  if (path === '/log') {
    console.log('this must not reach the frame channel');
    process.stdout.write('neither must this\\n');
    return { statusCode: 200, body: 'logged' };
  }
  if (path === '/redirect') {
    return { statusCode: 307, headers: { location: '/target' } };
  }
  if (path === '/no-content') {
    return { statusCode: 204, body: 'must be dropped' };
  }
  if (path === '/big') {
    return { statusCode: 200, headers: { 'content-type': 'text/plain' }, body: 'x'.repeat(7 * 1024 * 1024) };
  }
  if (path === '/throw') {
    throw new Error('boom');
  }
  if (path === '/no-status') {
    return { body: 'missing statusCode' };
  }
  if (path === '/bad-header') {
    return { statusCode: 200, headers: { 'x-count': 5 } };
  }
  if (path === '/slow') {
    await new Promise((resolve) => setTimeout(resolve, 150));
    return { statusCode: 200, body: String(Date.now()) };
  }
  if (path === '/content-length') {
    return { statusCode: 200, headers: { 'content-length': '999', 'content-type': 'text/plain' }, body: 'short' };
  }
  return { statusCode: 404, body: 'not found' };
};
`;

function encodeFrame(header, body = Buffer.alloc(0)) {
  const json = Buffer.from(JSON.stringify({ ...header, length: body.length }));
  const prefix = Buffer.alloc(8);
  prefix.writeUInt32BE(json.length, 0);
  prefix.writeUInt32BE(body.length, 4);
  return Buffer.concat([prefix, json, body]);
}

class Host {
  constructor(entrySource = FAKE_ENTRY, env = {}) {
    this.dir = fs.mkdtempSync(path.join(os.tmpdir(), 'lambda-local-host-'));
    fs.writeFileSync(path.join(this.dir, 'lambda-entry.cjs'), entrySource);
    this.child = spawn(process.execPath, [HOST], {
      cwd: this.dir,
      env: { ...process.env, ...env },
      stdio: ['pipe', 'pipe', 'pipe'],
    });
    this.stderr = '';
    this.child.stderr.on('data', (chunk) => { this.stderr += chunk; });
    this.pending = new Map();
    this.nextId = 1;
    this.buffer = Buffer.alloc(0);
    this.ready = new Promise((resolve, reject) => {
      this.resolveReady = resolve;
      this.child.once('exit', (code) => reject(new Error(`host exited early (${code}): ${this.stderr}`)));
    });
    this.child.stdout.on('data', (chunk) => this.onData(chunk));
  }

  onData(chunk) {
    this.buffer = Buffer.concat([this.buffer, chunk]);
    while (this.buffer.length >= 8) {
      const headerLength = this.buffer.readUInt32BE(0);
      const bodyLength = this.buffer.readUInt32BE(4);
      const total = 8 + headerLength + bodyLength;
      if (this.buffer.length < total) return;
      const header = JSON.parse(this.buffer.subarray(8, 8 + headerLength).toString('utf8'));
      const body = Buffer.from(this.buffer.subarray(8 + headerLength, total));
      this.buffer = this.buffer.subarray(total);
      assert.equal(header.length, body.length, 'frame header length must equal body length');
      if (header.kind === 'ready') {
        this.resolveReady();
        continue;
      }
      const entry = this.pending.get(header.id);
      assert.ok(entry, `frame for unknown request ${header.id}`);
      if (header.kind === 'headers') {
        entry.status = header.status;
        entry.headers = header.headers;
      } else if (header.kind === 'chunk') {
        entry.chunks.push(body);
      } else if (header.kind === 'end') {
        this.pending.delete(header.id);
        entry.resolve({
          status: entry.status,
          headers: entry.headers,
          body: Buffer.concat(entry.chunks),
        });
      } else {
        assert.fail(`unexpected frame kind ${header.kind}`);
      }
    }
  }

  request({ method = 'GET', uri = '/', headers = [], body = Buffer.alloc(0) } = {}) {
    const id = this.nextId++;
    const result = new Promise((resolve) => {
      this.pending.set(id, { resolve, chunks: [] });
    });
    this.child.stdin.write(encodeFrame({ kind: 'request', id, method, uri, headers }, body));
    return result;
  }

  async json(options) {
    const response = await this.request(options);
    assert.equal(response.status, 200, response.body.toString());
    return JSON.parse(response.body.toString('utf8'));
  }

  async close() {
    this.child.stdin.end();
    await new Promise((resolve) => this.child.once('exit', resolve)).catch(() => {});
    fs.rmSync(this.dir, { recursive: true, force: true });
  }
}

async function withHost(fn, entry, env) {
  const host = new Host(entry, env);
  try {
    await host.ready;
    await fn(host);
  } finally {
    await host.close();
  }
}

function headerValues(response, name) {
  return response.headers.filter(([key]) => key === name).map(([, value]) => value);
}

test('builds a payload v2 event with raw path, raw query and forwarded host', () => withHost(async (host) => {
  const { event, hasContext, env } = await host.json({
    method: 'GET',
    uri: '/echo?ref=a&ref=b&x=%20y',
    headers: [
      ['Host', 'app.localhost:3000'],
      ['Accept', 'text/html'],
      ['Accept', 'application/json'],
      ['User-Agent', 'test-agent'],
    ],
  });
  assert.equal(event.version, '2.0');
  assert.equal(event.routeKey, '$default');
  assert.equal(event.rawPath, '/echo');
  assert.equal(event.rawQueryString, 'ref=a&ref=b&x=%20y');
  assert.equal(event.requestContext.http.method, 'GET');
  assert.equal(event.requestContext.http.path, '/echo');
  assert.equal(event.requestContext.http.userAgent, 'test-agent');
  assert.equal(event.headers.accept, 'text/html,application/json');
  assert.equal(event.headers['x-forwarded-host'], 'app.localhost:3000');
  assert.match(event.headers.host, /\.lambda-url\.[a-z0-9-]+\.on\.aws$/);
  assert.equal(event.headers['x-forwarded-proto'], 'https');
  assert.equal(event.queryStringParameters.ref, 'a,b');
  assert.equal(event.isBase64Encoded, false);
  assert.equal(event.body, undefined);
  assert.equal(event.cookies, undefined);
  assert.equal(hasContext, true);
  assert.equal(env, 'meshscale-local');
}));

test('keeps the raw path percent-encoded and does not decode twice', () => withHost(async (host) => {
  const { event } = await host.json({ uri: '/echo/%2e%2e/a%2Fb%20c' });
  assert.equal(event.rawPath, '/echo/%2e%2e/a%2Fb%20c');
}, FAKE_ENTRY.replace("event.rawPath;", "event.rawPath.startsWith('/echo') ? '/echo' : event.rawPath;")));

test('moves the cookie header into event.cookies and strips it from headers', () => withHost(async (host) => {
  const { event } = await host.json({
    uri: '/echo',
    headers: [['Cookie', 'a=1; b=2'], ['Cookie', 'c=3']],
  });
  assert.deepEqual(event.cookies, ['a=1', 'b=2', 'c=3']);
  assert.equal(event.headers.cookie, undefined);
}));

test('text bodies stay strings and binary bodies are base64 encoded', () => withHost(async (host) => {
  const text = await host.json({
    method: 'POST', uri: '/echo',
    headers: [['Content-Type', 'application/json']],
    body: Buffer.from('{"a":"é"}'),
  });
  assert.equal(text.event.isBase64Encoded, false);
  assert.equal(text.event.body, '{"a":"é"}');
  assert.equal(text.event.headers['content-length'], String(Buffer.byteLength('{"a":"é"}')));

  const bytes = Buffer.from([0, 1, 2, 255, 254]);
  const binary = await host.json({
    method: 'POST', uri: '/echo',
    headers: [['Content-Type', 'application/octet-stream']],
    body: bytes,
  });
  assert.equal(binary.event.isBase64Encoded, true);
  assert.deepEqual(Buffer.from(binary.event.body, 'base64'), bytes);

  const invalidUtf8 = await host.json({
    method: 'POST', uri: '/echo',
    headers: [['Content-Type', 'text/plain']],
    body: Buffer.from([0xff, 0xfe, 0x41]),
  });
  assert.equal(invalidUtf8.event.isBase64Encoded, true);
}));

test('transfer-encoding is dropped and content-length reflects the buffered body', () => withHost(async (host) => {
  const { event } = await host.json({
    method: 'POST', uri: '/echo',
    headers: [['Transfer-Encoding', 'chunked'], ['Content-Type', 'text/plain']],
    body: Buffer.from('hello'),
  });
  assert.equal(event.headers['transfer-encoding'], undefined);
  assert.equal(event.headers['content-length'], '5');
}));

test('multiple cookies come back as separate Set-Cookie headers', () => withHost(async (host) => {
  const response = await host.request({ uri: '/cookies' });
  assert.equal(response.status, 200);
  assert.deepEqual(headerValues(response, 'set-cookie'), ['a=1; Path=/; HttpOnly', 'b=2; Path=/']);
  assert.equal(response.body.toString(), 'ok');
}));

test('base64 response bodies are decoded to bytes', () => withHost(async (host) => {
  const response = await host.request({ uri: '/binary' });
  assert.deepEqual(response.body, Buffer.from([0, 255, 1, 254]));
}));

test('application stdout writes cannot corrupt the frame stream', () => withHost(async (host) => {
  const response = await host.request({ uri: '/log' });
  assert.equal(response.status, 200);
  assert.equal(response.body.toString(), 'logged');
  assert.match(host.stderr, /this must not reach the frame channel/);
  const after = await host.request({ uri: '/cookies' });
  assert.equal(after.status, 200);
}));

test('redirects keep status and location', () => withHost(async (host) => {
  const response = await host.request({ uri: '/redirect' });
  assert.equal(response.status, 307);
  assert.deepEqual(headerValues(response, 'location'), ['/target']);
}));

test('204 and HEAD responses carry no body', () => withHost(async (host) => {
  const noContent = await host.request({ uri: '/no-content' });
  assert.equal(noContent.status, 204);
  assert.equal(noContent.body.length, 0);
  const head = await host.request({ method: 'HEAD', uri: '/cookies' });
  assert.equal(head.status, 200);
  assert.equal(head.body.length, 0);
}));

test('content-length from the handler is dropped except for HEAD', () => withHost(async (host) => {
  const get = await host.request({ uri: '/content-length' });
  assert.deepEqual(headerValues(get, 'content-length'), []);
  const head = await host.request({ method: 'HEAD', uri: '/content-length' });
  assert.deepEqual(headerValues(head, 'content-length'), ['999']);
}));

test('failures map to 502 and the host keeps serving', () => withHost(async (host) => {
  for (const uri of ['/throw', '/no-status', '/bad-header']) {
    const response = await host.request({ uri });
    assert.equal(response.status, 502, uri);
    assert.deepEqual(headerValues(response, 'x-meshscale-lambda-local-error'), ['1']);
  }
  const ok = await host.request({ uri: '/cookies' });
  assert.equal(ok.status, 200);
}));

test('responses above the Lambda payload limit are rejected', () => withHost(async (host) => {
  const response = await host.request({ uri: '/big' });
  assert.equal(response.status, 502);
  assert.match(response.body.toString(), /payload limit/);
}));

test('requests above the Lambda payload limit are rejected with 413', () => withHost(async (host) => {
  const response = await host.request({
    method: 'POST', uri: '/echo',
    headers: [['Content-Type', 'application/octet-stream']],
    body: Buffer.alloc(7 * 1024 * 1024, 1),
  });
  assert.equal(response.status, 413);
}));

test('handler timeouts return 502', () => withHost(async (host) => {
  const response = await host.request({ uri: '/slow' });
  assert.equal(response.status, 502);
  assert.match(response.body.toString(), /timed out/);
}, FAKE_ENTRY, { MESHSCALE_LAMBDA_LOCAL_TIMEOUT_MS: '50' }));

test('invocations are serialized by default and parallel when configured', async () => {
  const run = async (env) => {
    const host = new Host(FAKE_ENTRY, env);
    try {
      await host.ready;
      const started = Date.now();
      await Promise.all([host.request({ uri: '/slow' }), host.request({ uri: '/slow' }), host.request({ uri: '/slow' })]);
      return Date.now() - started;
    } finally {
      await host.close();
    }
  };
  const serial = await run({});
  const parallel = await run({ MESHSCALE_LAMBDA_LOCAL_CONCURRENCY: '3' });
  assert.ok(serial >= 400, `serial run took ${serial} ms`);
  assert.ok(parallel < 400, `parallel run took ${parallel} ms`);
});

test('a handler module that throws on load stops the host before it is ready', async () => {
  const host = new Host("throw new Error('init failed');");
  await assert.rejects(host.ready, /host exited early/);
  assert.match(host.stderr, /init failed/);
  fs.rmSync(host.dir, { recursive: true, force: true });
});

test('a module without a handler export is rejected', async () => {
  const host = new Host('exports.nothing = 1;');
  await assert.rejects(host.ready, /host exited early/);
  assert.match(host.stderr, /does not export a handler/);
  fs.rmSync(host.dir, { recursive: true, force: true });
});
