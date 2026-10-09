'use strict';

// Records or compares request/response pairs against a running `meshscale-builder run`
// serving the fixture app (test/fixtures/next-app). Usage:
//
//   node test/golden/golden.cjs --base http://127.0.0.1:3100 --record
//   node test/golden/golden.cjs --base http://127.0.0.1:3100 --compare [--dir test/golden]
//
// Each case is stored as test/golden/<name>.json: status, headers (lowercase, in order,
// duplicates kept, volatile ones removed) and the body's length and SHA-256 (plus the
// text for small textual bodies, so diffs are readable). Requests are sent one at a time.

const crypto = require('node:crypto');
const fs = require('node:fs');
const http = require('node:http');
const path = require('node:path');

const VOLATILE = new Set(['date', 'keep-alive', 'connection']);
const TIMEOUT_MS = 15000;

function parseArgs(argv) {
  const args = { dir: path.join(__dirname), mode: null, base: null, only: null };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--record' || arg === '--compare') args.mode = arg.slice(2);
    else if (arg === '--base') args.base = argv[++i];
    else if (arg === '--dir') args.dir = path.resolve(argv[++i]);
    else if (arg === '--only') args.only = new Set(argv[++i].split(','));
    else throw new Error(`unknown argument ${arg}`);
  }
  if (!args.mode || !args.base) {
    throw new Error('usage: golden.cjs --base URL (--record | --compare) [--dir DIR] [--only a,b]');
  }
  return args;
}

function send(base, { method = 'GET', path: requestPath, headers = [], body }) {
  const url = new URL(base);
  return new Promise((resolve) => {
    const rawHeaders = [...headers];
    if (body) rawHeaders.push(['content-length', String(body.length)]);
    const request = http.request({
      host: url.hostname,
      port: url.port,
      method,
      path: requestPath,
      // An array of [name, value] pairs keeps duplicates as separate header lines.
      headers: rawHeaders.flat(),
      setHost: !rawHeaders.some(([name]) => name.toLowerCase() === 'host'),
    });
    const timer = setTimeout(() => {
      request.destroy();
      resolve({ error: 'timeout' });
    }, TIMEOUT_MS);
    request.on('response', (response) => {
      const chunks = [];
      response.on('data', (chunk) => chunks.push(chunk));
      response.on('end', () => {
        clearTimeout(timer);
        resolve({ response, body: Buffer.concat(chunks) });
      });
      response.on('error', (error) => {
        clearTimeout(timer);
        resolve({ error: `response error: ${error.code || error.message}` });
      });
    });
    request.on('error', (error) => {
      clearTimeout(timer);
      resolve({ error: `request error: ${error.code || error.message}` });
    });
    request.end(body);
  });
}

function summarize(result) {
  if (result.error) return { error: result.error };
  const { response } = result;
  let { body } = result;
  if ((response.headers['content-type'] || '').startsWith('text/x-component')) {
    // RSC payloads carry a random 22-character key per request (viewport/metadata boundary).
    body = Buffer.from(body.toString('utf8').replace(/"[A-Za-z0-9_-]{22}"/g, '"<random-key>"'));
  }
  if ((response.headers['content-type'] || '').startsWith('text/html')) {
    // Server action IDs (42 hex digits) are generated per build. Error digests hash the
    // stack trace, which contains absolute artifact paths and runtime file names.
    body = Buffer.from(body.toString('utf8')
      .replace(/(?<![0-9a-f])[0-9a-f]{42}(?![0-9a-f])/g, '<action-id>')
      .replace(/(digest\\?"\s*:\s*\\?")\d+/g, '$1<digest>'));
  }
  const headers = [];
  for (let i = 0; i < response.rawHeaders.length; i += 2) {
    const name = response.rawHeaders[i].toLowerCase();
    if (!VOLATILE.has(name) && !name.startsWith('x-meshscale-')) {
      headers.push([name, response.rawHeaders[i + 1]]);
    }
  }
  const type = response.headers['content-type'] || '';
  const textual = /^(text\/|application\/json)/.test(type) && !response.headers['content-encoding'];
  const summary = {
    status: response.statusCode,
    headers,
    body: {
      length: body.length,
      sha256: crypto.createHash('sha256').update(body).digest('hex'),
    },
  };
  if (textual && body.length <= 4096) summary.body.text = body.toString('utf8');
  // The fixture marks the interesting part of each HTML page with one of these ids.
  if (type.startsWith('text/html')) {
    const marked = [...body.toString('utf8').matchAll(/id="(echo|param|props|done)">([^<]*)</g)]
      .map((match) => [match[1], match[2]]);
    if (marked.length) summary.body.marked = marked;
  }
  return summary;
}

function multipart(fields) {
  const boundary = 'meshscalegoldenboundary';
  const parts = fields.map(([name, value]) =>
    `--${boundary}\r\ncontent-disposition: form-data; name="${name}"\r\n\r\n${value}\r\n`);
  return {
    type: `multipart/form-data; boundary=${boundary}`,
    body: Buffer.from(parts.join('') + `--${boundary}--\r\n`),
  };
}

const binary = Buffer.from(Array.from({ length: 4096 }, (_, i) => (i * 37 + 11) % 256));

// Cases run in this order. `/log` is last: with the pre-refactor runtime a console.log in a
// handler corrupts the frame stream (K3), which may wedge the worker for later requests.
const CASES = [
  { name: 'static-home', path: '/' },
  { name: 'static-public', path: '/hello.txt' },
  { name: 'static-public-head', method: 'HEAD', path: '/hello.txt' },
  { name: 'ssr', path: '/ssr', headers: [['x-test', 'one'], ['cookie', 'a=1'], ['cookie', 'b=2']] },
  { name: 'ssr-head', method: 'HEAD', path: '/ssr' },
  { name: 'ssr-rsc', path: '/ssr', headers: [['rsc', '1']] },
  { name: 'dynamic', path: '/dynamic/42' },
  { name: 'dynamic-encoded-space', path: '/dynamic/a%20b' },
  { name: 'dynamic-encoded-slash', path: '/dynamic/a%2Fb' },
  { name: 'pages-ssr', path: '/pages-ssr?q=1&q=2', headers: [['x-test', 'pages']] },
  { name: 'api-echo-get', path: '/api/echo?a=1&a=2&b=x%20y', headers: [['x-test', 'get'], ['x-multi', 'one'], ['x-multi', 'two']] },
  {
    name: 'api-echo-post-json',
    method: 'POST',
    path: '/api/echo',
    headers: [['content-type', 'application/json'], ['cookie', 'c=3'], ['cookie', 'd=4']],
    body: Buffer.from(JSON.stringify({ hello: 'world', n: 1 })),
  },
  { name: 'route-handler-get', path: '/route-handler?a=1&a=2' },
  {
    name: 'route-handler-post-binary',
    method: 'POST',
    path: '/route-handler',
    headers: [['content-type', 'application/octet-stream']],
    body: binary,
  },
  { name: 'redirect', path: '/redirect' },
  { name: 'missing', path: '/missing' },
  { name: 'unknown-path', path: '/does-not-exist' },
  { name: 'error', path: '/error' },
  { name: 'env', path: '/env' },
  { name: 'after', path: '/after?id=golden' },
  { name: 'after-marker', path: '/after-marker?id=golden', delayMs: 1500 },
  { name: 'action-page', path: '/action' },
  { name: 'action-submit', action: true },
  { name: 'big', path: '/big' },
  { name: 'stream', path: '/stream' },
  { name: 'log', path: '/log' },
  { name: 'after-log', path: '/env' },
];

async function runCase(base, testCase, state) {
  if (testCase.delayMs) await new Promise((resolve) => setTimeout(resolve, testCase.delayMs));
  if (testCase.action) {
    const html = state.actionPage ? state.actionPage.toString('utf8') : '';
    const match = /name="(\$ACTION_ID_[0-9a-f]+)"/.exec(html);
    if (!match) return { error: 'no server action id found in /action' };
    const form = multipart([[match[1], ''], ['name', 'golden']]);
    return summarize(await send(base, {
      method: 'POST',
      path: '/action',
      headers: [['content-type', form.type], ['origin', base]],
      body: form.body,
    }));
  }
  const result = await send(base, testCase);
  if (testCase.name === 'action-page' && result.body) state.actionPage = result.body;
  return summarize(result);
}

function diff(expected, actual, at = '') {
  if (JSON.stringify(expected) === JSON.stringify(actual)) return [];
  if (expected && actual && typeof expected === 'object' && typeof actual === 'object' &&
      !Array.isArray(expected) && !Array.isArray(actual)) {
    const keys = new Set([...Object.keys(expected), ...Object.keys(actual)]);
    return [...keys].flatMap((key) => diff(expected[key], actual[key], `${at}.${key}`));
  }
  return [`${at || '.'}: expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`];
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const state = {};
  let differences = 0;
  for (const testCase of CASES) {
    if (args.only && !args.only.has(testCase.name)) continue;
    const actual = await runCase(args.base, testCase, state);
    const file = path.join(args.dir, `${testCase.name}.json`);
    if (args.mode === 'record') {
      fs.writeFileSync(file, JSON.stringify(actual, null, 2) + '\n');
      console.log(`recorded ${testCase.name}: ${actual.error || actual.status}`);
      continue;
    }
    const expected = JSON.parse(fs.readFileSync(file, 'utf8'));
    const found = diff(expected, actual);
    if (found.length) {
      differences += 1;
      console.log(`DIFF ${testCase.name}`);
      for (const line of found) console.log(`  ${line.length > 400 ? line.slice(0, 400) + '...' : line}`);
    } else {
      console.log(`same ${testCase.name}`);
    }
  }
  if (differences) {
    console.log(`${differences} case(s) differ`);
    process.exitCode = 1;
  }
}

main().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
