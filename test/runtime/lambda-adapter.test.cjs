'use strict';

// Tests for src/lambda_adapter.cjs. Run with: node --test test/runtime/*.test.cjs
// A fake runtime is injected through createHandler; no Next.js and no AWS are involved.

const test = require('node:test');
const assert = require('node:assert/strict');
const crypto = require('node:crypto');
const fs = require('node:fs');
const path = require('node:path');

const {
  createHandler,
  fromLambdaEvent,
  toLambdaResponse,
  InvalidEventError,
  ResponseTooLargeError,
  LAMBDA_PAYLOAD_LIMIT,
} = require('../../src/lambda_adapter.cjs');

const EVENTS = path.resolve(__dirname, '../fixtures/events');
const event = (name) => JSON.parse(fs.readFileSync(path.join(EVENTS, `${name}.json`), 'utf8'));
const binary = Buffer.from(Array.from({ length: 4096 }, (_, i) => (i * 37 + 11) % 256));

// A runtime response built from parts, like runtime.cjs produces.
function response(status, headers, chunks = [], { failAfter } = {}) {
  const state = { returned: false };
  const body = {
    [Symbol.asyncIterator]() {
      let index = 0;
      return {
        async next() {
          if (failAfter !== undefined && index === failAfter) throw new Error('late failure');
          if (index < chunks.length) return { value: Buffer.from(chunks[index++]), done: false };
          return { value: undefined, done: true };
        },
        async return() {
          state.returned = true;
          return { value: undefined, done: true };
        },
      };
    },
  };
  const done = failAfter !== undefined ? Promise.reject(new Error('late failure')) : Promise.resolve();
  done.catch(() => {});
  return { status, headers, body, done, state };
}

function fakeRuntime(reply, calls = []) {
  return {
    calls,
    async invoke(request) {
      calls.push({ kind: 'invoke', request });
      return typeof reply === 'function' ? reply(request) : reply;
    },
    async settle(timeoutMs) {
      await new Promise((resolve) => setTimeout(resolve, 10));
      calls.push({ kind: 'settle', timeoutMs });
      return { pending: 0, timedOut: false };
    },
  };
}

function quietly(fn) {
  return async () => {
    const lines = [];
    const original = console.log;
    console.log = (...args) => lines.push(args.join(' '));
    try {
      return await fn(lines);
    } finally {
      console.log = original;
    }
  };
}

// ---------------------------------------------------------------- fromLambdaEvent

test('fixtures are marked as hand-written until replaced by captured events', () => {
  for (const file of fs.readdirSync(EVENTS)) {
    assert.ok(JSON.parse(fs.readFileSync(path.join(EVENTS, file), 'utf8'))._source, file);
  }
});

test('validity: only payload format 2.0 events with requestContext.http are accepted', () => {
  assert.throws(() => fromLambdaEvent(event('invalid-api-gateway-v1')), InvalidEventError);
  assert.throws(() => fromLambdaEvent(undefined), InvalidEventError);
  assert.throws(() => fromLambdaEvent({ version: '2.0', requestContext: {} }), InvalidEventError);
  assert.throws(() => fromLambdaEvent({ version: '1.0', requestContext: { http: { method: 'GET' } } }), InvalidEventError);
  assert.throws(() => fromLambdaEvent({ version: '2.0', requestContext: { http: {} } }), InvalidEventError);
  assert.throws(() => fromLambdaEvent({ version: '2.0', rawPath: 'no-slash', requestContext: { http: { method: 'GET' } } }), InvalidEventError);
});

test('method and url: raw path plus raw query, never decoded or rebuilt', () => {
  const request = fromLambdaEvent(event('get-encoded-path-repeated-query'));
  assert.equal(request.method, 'GET');
  assert.equal(request.url, '/dynamic/a%20b?a=1&a=2&b=x%20y');
  assert.equal(fromLambdaEvent(event('get-basic')).url, '/ssr');
  const minimal = fromLambdaEvent({ version: '2.0', requestContext: { http: { method: 'GET' } } });
  assert.equal(minimal.url, '/');
  assert.deepEqual(minimal.headers, []);
  assert.equal(fromLambdaEvent({ ...event('get-basic'), rawPath: '/a%2Fb', rawQueryString: '' }).url, '/a%2Fb');
});

test('headers are kept as given; comma-joined values are not split', () => {
  const request = fromLambdaEvent(event('get-cookies'));
  assert.ok(request.headers.some(([name, value]) => name === 'x-multi' && value === 'one,two'));
  assert.ok(request.headers.some(([name, value]) => name === 'x-amzn-trace-id' && value.startsWith('Root=')));
});

test('cookies[] becomes one cookie header joined with "; "', () => {
  const request = fromLambdaEvent(event('get-cookies'));
  assert.deepEqual(request.headers.filter(([name]) => name === 'cookie'), [['cookie', 'a=1; b=2']]);
  const none = fromLambdaEvent({ ...event('get-basic'), cookies: [] });
  assert.equal(none.headers.filter(([name]) => name === 'cookie').length, 0);
});

test('host: x-forwarded-host (first value) replaces the Function URL host and is kept', () => {
  const request = fromLambdaEvent(event('get-basic'));
  assert.deepEqual(request.headers.filter(([name]) => name === 'host'), [['host', 'app.example.test']]);
  assert.ok(request.headers.some(([name, value]) => name === 'x-forwarded-host' && value === 'app.example.test'));

  const chained = fromLambdaEvent({ ...event('get-basic'), headers: { host: 'x.on.aws', 'x-forwarded-host': ' a.test , b.test' } });
  assert.deepEqual(chained.headers.find(([name]) => name === 'host'), ['host', 'a.test']);

  const direct = fromLambdaEvent({ ...event('get-basic'), headers: { host: 'x.lambda-url.us-east-1.on.aws' } });
  assert.deepEqual(direct.headers, [['host', 'x.lambda-url.us-east-1.on.aws']]);

  const noHost = fromLambdaEvent({ ...event('get-basic'), headers: { 'x-forwarded-host': 'only.test' } });
  assert.ok(noHost.headers.some(([name, value]) => name === 'host' && value === 'only.test'));
});

test('body: absent, utf8 and base64', () => {
  assert.equal(fromLambdaEvent(event('get-basic')).body.length, 0);
  assert.equal(fromLambdaEvent({ ...event('get-basic'), body: '' }).body.length, 0);
  assert.equal(fromLambdaEvent(event('post-json')).body.toString('utf8'), '{"hello":"world","n":1}  ');
  assert.deepEqual(fromLambdaEvent(event('post-binary-base64')).body, binary);
});

// ---------------------------------------------------------------- toLambdaResponse

test('cookies: every set-cookie goes to cookies[] and never into headers', async () => {
  const result = await toLambdaResponse(response(200, [
    ['set-cookie', 'a=1; Path=/; Expires=Wed, 21 Oct 2026 07:28:00 GMT'],
    ['content-type', 'text/plain'],
    ['set-cookie', 'b=2; Path=/'],
  ], ['ok']));
  assert.deepEqual(result.cookies, ['a=1; Path=/; Expires=Wed, 21 Oct 2026 07:28:00 GMT', 'b=2; Path=/']);
  assert.deepEqual(result.headers, { 'content-type': 'text/plain' });
  assert.equal('cookies' in await toLambdaResponse(response(200, [], [])), false);
});

test('headers: lowercased, repeats joined with ", ", hop-by-hop and content-length dropped', async () => {
  const result = await toLambdaResponse(response(200, [
    ['Vary', 'rsc'],
    ['vary', 'accept'],
    ['Connection', 'keep-alive'],
    ['keep-alive', 'timeout=5'],
    ['transfer-encoding', 'chunked'],
    ['upgrade', 'h2c'],
    ['te', 'trailers'],
    ['trailer', 'x'],
    ['content-length', '2'],
    ['x-custom', 'v'],
  ], ['ok']));
  assert.deepEqual(result.headers, { vary: 'rsc, accept', 'x-custom': 'v' });

  const head = await toLambdaResponse(response(200, [['content-length', '1234']], []), { method: 'HEAD' });
  assert.deepEqual(head.headers, { 'content-length': '1234' });
});

test('encoding: textual UTF-8 bodies are strings, everything else base64', async () => {
  const text = async (type, body, extra = []) =>
    toLambdaResponse(response(200, [['content-type', type], ...extra], [body]));
  for (const type of ['text/html; charset=utf-8', 'application/json', 'application/ld+json',
    'application/xml', 'image/svg+xml', 'application/javascript', 'application/x-www-form-urlencoded', 'text/x-component']) {
    const result = await text(type, 'héllo');
    assert.equal(result.isBase64Encoded, false, type);
    assert.equal(result.body, 'héllo', type);
  }
  const png = await text('image/png', binary);
  assert.equal(png.isBase64Encoded, true);
  assert.deepEqual(Buffer.from(png.body, 'base64'), binary);

  const gzipped = await text('text/html', 'abc', [['content-encoding', 'gzip']]);
  assert.equal(gzipped.isBase64Encoded, true);
  assert.equal(Buffer.from(gzipped.body, 'base64').toString(), 'abc');
  assert.equal(gzipped.headers['content-encoding'], 'gzip');

  const invalidUtf8 = await text('text/plain', Buffer.from([0x61, 0xff, 0x62]));
  assert.equal(invalidUtf8.isBase64Encoded, true);
  assert.deepEqual(Buffer.from(invalidUtf8.body, 'base64'), Buffer.from([0x61, 0xff, 0x62]));

  const untyped = await toLambdaResponse(response(200, [], ['plain']));
  assert.equal(untyped.isBase64Encoded, true);

  const empty = await toLambdaResponse(response(200, [['content-type', 'image/png']], []));
  assert.deepEqual([empty.body, empty.isBase64Encoded], ['', false]);
});

test('bodyless: HEAD, 204, 205 and 304 return an empty body but drain the stream', async () => {
  for (const [status, method] of [[200, 'HEAD'], [204, 'GET'], [205, 'POST'], [304, 'GET']]) {
    let pulled = 0;
    const source = response(status, [['content-type', 'text/plain']], ['body', 'more']);
    const counting = {
      ...source,
      body: {
        [Symbol.asyncIterator]() {
          const inner = source.body[Symbol.asyncIterator]();
          return { next: async () => { pulled += 1; return inner.next(); }, return: inner.return };
        },
      },
    };
    const result = await toLambdaResponse(counting, { method });
    assert.deepEqual([result.statusCode, result.body, result.isBase64Encoded], [status, '', false]);
    assert.equal(pulled, 3, `${status} ${method} was not drained`);
  }
});

test('body: collection stops at maxBytes and releases the producer', async () => {
  const source = response(200, [['content-type', 'text/plain']], ['12345', '67890', 'abc']);
  await assert.rejects(toLambdaResponse(source, { maxBytes: 8 }), ResponseTooLargeError);
  assert.equal(source.state.returned, true);
  const exact = await toLambdaResponse(response(200, [['content-type', 'text/plain']], ['12345', '678']), { maxBytes: 8 });
  assert.equal(exact.body, '12345678');
});

test('size: encoded bodies above the payload limit fail with a controlled error', async () => {
  // 4.8 MB of binary is 6.4 MB as base64: above the 6,291,556 byte payload limit.
  const big = Buffer.alloc(4.8 * 1024 * 1024, 7);
  await assert.rejects(
    toLambdaResponse(response(200, [['content-type', 'application/octet-stream']], [big])),
    ResponseTooLargeError,
  );
  // 4.5 MB of text stays a string and fits.
  const text = 'a'.repeat(4.5 * 1024 * 1024);
  const fits = await toLambdaResponse(response(200, [['content-type', 'text/plain']], [text]));
  assert.equal(fits.body.length, text.length);
  assert.ok(Buffer.byteLength(JSON.stringify(fits)) <= LAMBDA_PAYLOAD_LIMIT);
});

test('a failure while producing the body rejects', async () => {
  await assert.rejects(toLambdaResponse(response(200, [], ['a', 'b'], { failAfter: 1 })), /late failure/);
});

// ---------------------------------------------------------------- createHandler

test('start() runs once, at creation, before the first invocation', async () => {
  let starts = 0;
  const runtime = fakeRuntime(response(200, [['content-type', 'text/plain']], ['ok']));
  const handler = createHandler({ start: () => { starts += 1; return runtime; } });
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(starts, 1);
  await handler(event('get-basic'));
  await handler(event('get-basic'));
  assert.equal(starts, 1);
});

test('init failure throws from the invocation and the next invocation retries start()', quietly(async (lines) => {
  let starts = 0;
  const runtime = fakeRuntime(response(200, [['content-type', 'text/plain']], ['ok']));
  const handler = createHandler({
    start: async () => {
      starts += 1;
      if (starts === 1) throw new Error('init failed');
      return runtime;
    },
  });
  await assert.rejects(handler(event('get-basic')), /init failed/);
  const result = await handler(event('get-basic'));
  assert.equal(result.statusCode, 200);
  assert.equal(starts, 2);
  assert.ok(lines.some((line) => JSON.parse(line).message === 'runtime initialization failed'));

  let syncStarts = 0;
  const sync = createHandler({ start: () => { syncStarts += 1; if (syncStarts === 1) throw new Error('sync init failed'); return runtime; } });
  await assert.rejects(sync(event('get-basic')), /sync init failed/);
  assert.equal((await sync(event('get-basic'))).statusCode, 200);
}));

test('normal flow converts, awaits settle() before returning, and passes the timeout', async () => {
  const calls = [];
  const runtime = fakeRuntime(response(201, [['content-type', 'application/json'], ['set-cookie', 'x=1']], ['{"a":1}']), calls);
  const handler = createHandler({ start: () => runtime, backgroundTimeoutMs: 1234 });
  const result = await handler(event('get-cookies'));
  assert.deepEqual(result, {
    statusCode: 201,
    headers: { 'content-type': 'application/json' },
    cookies: ['x=1'],
    body: '{"a":1}',
    isBase64Encoded: false,
  });
  assert.deepEqual(calls.map((call) => call.kind), ['invoke', 'settle']);
  assert.equal(calls[1].timeoutMs, 1234);
  const request = calls[0].request;
  assert.equal(request.url, '/api/echo');
  assert.ok(request.headers.some(([name, value]) => name === 'cookie' && value === 'a=1; b=2'));
});

test('binary round trip through base64 in both directions', async () => {
  const handler = createHandler({
    start: () => fakeRuntime((request) => response(200, [['content-type', 'application/octet-stream']], [request.body])),
  });
  const result = await handler(event('post-binary-base64'));
  assert.equal(result.isBase64Encoded, true);
  const bytes = Buffer.from(result.body, 'base64');
  assert.equal(crypto.createHash('sha256').update(bytes).digest('hex'),
    crypto.createHash('sha256').update(binary).digest('hex'));
});

test('HEAD and 304 through the handler carry no body', async () => {
  const handler = createHandler({
    start: () => fakeRuntime((request) => response(request.method === 'HEAD' ? 200 : 304,
      [['content-type', 'text/html'], ['content-length', '10']], ['0123456789'])),
  });
  const head = await handler(event('head'));
  assert.deepEqual([head.statusCode, head.body, head.headers['content-length']], [200, '', '10']);
  const notModified = await handler(event('get-basic'));
  assert.deepEqual([notModified.statusCode, notModified.body, notModified.headers['content-length']], [304, '', undefined]);
});

test('oversize and failed bodies become a controlled 502 without request data in logs', quietly(async (lines) => {
  const calls = [];
  const handler = createHandler({
    start: () => fakeRuntime((request) => request.url.startsWith('/big')
      ? response(200, [['content-type', 'text/plain']], ['x'.repeat(100)])
      : response(200, [], ['a'], { failAfter: 1 }), calls),
    maxResponseBytes: 50,
  });
  const secret = { ...event('post-json'), rawPath: '/big', cookies: ['session=SECRET-COOKIE'], body: 'SECRET-BODY' };
  const big = await handler(secret);
  assert.equal(big.statusCode, 502);
  assert.equal(big.headers['x-meshscale-error'], 'response-too-large');
  const failed = await handler({ ...secret, rawPath: '/fail' });
  assert.equal(failed.statusCode, 502);
  assert.equal(failed.headers['x-meshscale-error'], 'response-failed');
  assert.deepEqual(calls.filter((call) => call.kind === 'settle').length, 2, 'settle must run after a 502 too');
  for (const line of lines) {
    assert.doesNotMatch(line, /SECRET/);
    assert.equal(JSON.parse(line).level, 'error');
  }
  assert.equal(lines.length, 2);
}));

test('application 5xx responses are returned, not thrown', async () => {
  const handler = createHandler({ start: () => fakeRuntime(response(500, [['content-type', 'text/html']], ['<h1>error</h1>'])) });
  const result = await handler(event('get-basic'));
  assert.deepEqual([result.statusCode, result.body], [500, '<h1>error</h1>']);
});

test('probe answers without invoking Next, after the runtime has started', async () => {
  const calls = [];
  const handler = createHandler({ start: () => fakeRuntime(response(200, [], []), calls) });
  const result = await handler(event('probe'));
  assert.equal(result.statusCode, 200);
  assert.deepEqual(JSON.parse(result.body), { status: 'ok', probe: 'init' });
  assert.equal(calls.length, 0);

  const failing = createHandler({ start: async () => { throw new Error('init failed'); } });
  const original = console.log;
  console.log = () => {};
  try {
    await assert.rejects(failing(event('probe')), /init failed/);
  } finally {
    console.log = original;
  }

  // Any other probe value is an ordinary request.
  await handler({ ...event('probe'), headers: { 'x-meshscale-probe': 'other' } });
  assert.equal(calls[0].kind, 'invoke');
});

test('events that are not Function URL events are rejected', async () => {
  const handler = createHandler({ start: () => fakeRuntime(response(200, [], [])) });
  await assert.rejects(handler(event('invalid-api-gateway-v1')), InvalidEventError);
});
