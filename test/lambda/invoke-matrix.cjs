'use strict';

// Phase 5: runs the test matrix against a real Lambda function with direct invokes
// (`aws lambda invoke`), using Function URL payload 2.0 events built from
// test/fixtures/events/get-basic.json. Needs the AWS CLI v2, already configured by you
// (this script never reads or handles credentials).
//
//   node test/lambda/invoke-matrix.cjs --function meshscale-phase5 --qualifier 1 \
//     --env-value phase5 --out test/lambda/results/direct-1024mb.json [--region us-east-1] [--profile sandbox]
//
// Each case records the invoke status, FunctionError, the REPORT line (duration, memory,
// Init Duration on cold starts) and a summary of the Lambda result (status, headers,
// cookies, body length and SHA-256). Exit code 1 if any expectation fails.

const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { execFile } = require('node:child_process');

const EVENTS = path.resolve(__dirname, '../fixtures/events');

function parseArgs(argv) {
  const args = { qualifier: null, region: null, profile: null, envValue: null, out: null, concurrency: 10 };
  for (let i = 0; i < argv.length; i += 1) {
    const name = argv[i];
    const value = argv[i + 1];
    if (name === '--function') args.function = value;
    else if (name === '--qualifier') args.qualifier = value;
    else if (name === '--region') args.region = value;
    else if (name === '--profile') args.profile = value;
    else if (name === '--env-value') args.envValue = value;
    else if (name === '--out') args.out = value;
    else if (name === '--concurrency') args.concurrency = Number(value);
    else throw new Error(`unknown argument ${name}`);
    i += 1;
  }
  if (!args.function) throw new Error('--function is required');
  return args;
}

const base = JSON.parse(fs.readFileSync(path.join(EVENTS, 'get-basic.json'), 'utf8'));
delete base._source;

function event(rawPath, { query = '', method = 'GET', headers = {}, cookies, body, base64 = false } = {}) {
  const result = structuredClone(base);
  result.rawPath = rawPath;
  result.rawQueryString = query;
  result.headers = { ...result.headers, ...headers };
  result.requestContext.http.method = method;
  result.requestContext.http.path = rawPath;
  result.requestContext.requestId = crypto.randomUUID();
  if (cookies) result.cookies = cookies;
  if (body !== undefined) {
    result.body = body;
    result.isBase64Encoded = base64;
  }
  return result;
}

function aws(args, options) {
  const full = [...args];
  if (options.region) full.push('--region', options.region);
  if (options.profile) full.push('--profile', options.profile);
  return new Promise((resolve) => {
    execFile('aws', full, { maxBuffer: 64 * 1024 * 1024, windowsHide: true }, (error, stdout, stderr) => {
      resolve({ error, stdout, stderr });
    });
  });
}

const REPORT_FIELDS = {
  durationMs: /\tDuration: ([\d.]+) ms/,
  billedMs: /Billed Duration: ([\d.]+) ms/,
  memoryMb: /Memory Size: (\d+) MB/,
  maxMemoryMb: /Max Memory Used: (\d+) MB/,
  initMs: /Init Duration: ([\d.]+) ms/,
};

async function invoke(payload, options) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'meshscale-invoke-'));
  try {
    const payloadFile = path.join(dir, 'event.json');
    const outFile = path.join(dir, 'out.json');
    fs.writeFileSync(payloadFile, JSON.stringify(payload));
    const args = ['lambda', 'invoke', '--function-name', options.function,
      '--cli-binary-format', 'raw-in-base64-out', '--payload', `fileb://${payloadFile}`,
      '--log-type', 'Tail', '--output', 'json'];
    if (options.qualifier) args.push('--qualifier', options.qualifier);
    args.push(outFile);
    const { error, stdout, stderr } = await aws(args, options);
    if (error) return { invokeError: String(stderr || error.message).trim().slice(0, 500) };
    const meta = JSON.parse(stdout);
    const log = meta.LogResult ? Buffer.from(meta.LogResult, 'base64').toString('utf8') : '';
    const reportLine = log.split('\n').find((line) => line.startsWith('REPORT')) || '';
    const report = {};
    for (const [name, pattern] of Object.entries(REPORT_FIELDS)) {
      const match = pattern.exec(reportLine);
      if (match) report[name] = Number(match[1]);
    }
    return {
      invokeStatus: meta.StatusCode,
      functionError: meta.FunctionError || null,
      report,
      logTail: log,
      result: JSON.parse(fs.readFileSync(outFile, 'utf8')),
    };
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
}

function summarize(invocation) {
  const { result } = invocation;
  if (!result || typeof result.statusCode !== 'number') return { raw: result };
  const bytes = result.body ? Buffer.from(result.body, result.isBase64Encoded ? 'base64' : 'utf8') : Buffer.alloc(0);
  const text = bytes.toString('utf8');
  const summary = {
    statusCode: result.statusCode,
    headers: result.headers || {},
    cookies: result.cookies || [],
    isBase64Encoded: Boolean(result.isBase64Encoded),
    body: { length: bytes.length, sha256: crypto.createHash('sha256').update(bytes).digest('hex') },
  };
  if (!result.isBase64Encoded && bytes.length <= 2000) summary.body.text = text;
  const marked = [...text.matchAll(/id="(echo|param|props|done)">([^<]*)</g)]
    .map((match) => [match[1], match[2].replace(/&quot;/g, '"')]);
  if (marked.length) summary.body.marked = marked;
  return { summary, text, bytes };
}

const binary = Buffer.from(Array.from({ length: 4096 }, (_, i) => (i * 37 + 11) % 256));
const binarySha = crypto.createHash('sha256').update(binary).digest('hex');
const markerId = `phase5-${Date.now()}`;

function multipart(fields) {
  const boundary = 'meshscalephase5boundary';
  const body = fields.map(([name, value]) =>
    `--${boundary}\r\ncontent-disposition: form-data; name="${name}"\r\n\r\n${value}\r\n`).join('') + `--${boundary}--\r\n`;
  return { type: `multipart/form-data; boundary=${boundary}`, body: Buffer.from(body).toString('base64') };
}

// Each case: event (or a function of earlier results) and an expectation returning problems.
function cases(options) {
  const status = (expected) => (s) => (s.statusCode === expected ? [] : [`status ${s.statusCode}, expected ${expected}`]);
  const marked = (s, id) => {
    const found = (s.body.marked || []).find(([name]) => name === id);
    return found ? found[1] : undefined;
  };
  return [
    { name: 'probe', event: () => event('/', { headers: { 'x-meshscale-probe': 'init' } }), expect: status(200) },
    { name: 'static-home', note: 'served by the edge in production; K12/K13/K15 apply if it reaches Lambda', event: () => event('/'), expect: () => [] },
    {
      name: 'ssr', event: () => event('/ssr', { headers: { 'x-test': 'one' }, cookies: ['a=1', 'b=2'] }),
      expect: (s) => {
        const echo = JSON.parse(marked(s, 'echo') || '{}');
        const problems = status(200)(s);
        if (echo.host !== 'app.example.test') problems.push(`host ${echo.host}`);
        if (JSON.stringify(echo.cookies) !== JSON.stringify([['a', '1'], ['b', '2']])) problems.push(`cookies ${JSON.stringify(echo.cookies)}`);
        return problems;
      },
    },
    { name: 'ssr-head', event: () => event('/ssr', { method: 'HEAD' }), expect: (s) => [...status(200)(s), ...(s.body.length ? ['HEAD has a body'] : [])] },
    {
      name: 'ssr-rsc', event: () => event('/ssr', { headers: { rsc: '1' } }),
      expect: (s) => [...status(200)(s), ...(String(s.headers['content-type']).startsWith('text/x-component') ? [] : [`content-type ${s.headers['content-type']}`])],
    },
    { name: 'dynamic', event: () => event('/dynamic/42'), expect: (s) => [...status(200)(s), ...(marked(s, 'param') === '"42"' ? [] : [`param ${marked(s, 'param')}`])] },
    { name: 'dynamic-encoded-space', note: 'IPC golden: "a%20b"', event: () => event('/dynamic/a%20b'), expect: (s) => [...status(200)(s), ...(marked(s, 'param') === '"a%20b"' ? [] : [`param ${marked(s, 'param')}`])] },
    { name: 'pages-ssr', event: () => event('/pages-ssr', { query: 'q=1&q=2' }), expect: (s) => [...status(200)(s), ...(String(marked(s, 'props')).includes('"q":["1","2"]') ? [] : [`props ${marked(s, 'props')}`])] },
    {
      name: 'api-echo-get', event: () => event('/api/echo', { query: 'a=1&a=2&b=x%20y', headers: { 'x-multi': 'one,two' } }),
      expect: (s) => {
        const body = JSON.parse(s.body.text || '{}');
        const problems = status(200)(s);
        if (s.cookies.length !== 2) problems.push(`cookies ${JSON.stringify(s.cookies)}`);
        if (JSON.stringify(body.query && body.query.a) !== '["1","2"]') problems.push(`query ${JSON.stringify(body.query)}`);
        return problems;
      },
    },
    {
      name: 'api-echo-post-json',
      event: () => event('/api/echo', { method: 'POST', headers: { 'content-type': 'application/json' }, body: '{"hello":"world","n":1}' }),
      expect: (s) => [...status(200)(s), ...(JSON.parse(s.body.text || '{}').body?.hello === 'world' ? [] : ['body not echoed'])],
    },
    {
      name: 'route-handler-post-binary',
      event: () => event('/route-handler', { method: 'POST', headers: { 'content-type': 'application/octet-stream' }, body: binary.toString('base64'), base64: true }),
      expect: (s) => [...status(200)(s), ...(JSON.parse(s.body.text || '{}').sha256 === binarySha ? [] : ['binary body changed'])],
    },
    { name: 'redirect', event: () => event('/redirect'), expect: (s) => [...status(307)(s), ...(String(s.headers.location).includes('/ssr') ? [] : [`location ${s.headers.location}`])] },
    { name: 'missing', event: () => event('/missing'), expect: status(404) },
    { name: 'error', event: () => event('/error'), expect: status(500) },
    { name: 'env', event: () => event('/env'), expect: (s) => [...status(200)(s), ...(options.envValue === null || s.body.text === options.envValue ? [] : [`env ${s.body.text}`])] },
    { name: 'after', event: () => event('/after', { query: `id=${markerId}` }), expect: status(200) },
    {
      name: 'after-marker', note: 'needs the same execution environment as "after" (serial invokes usually get it)',
      event: () => event('/after-marker', { query: `id=${markerId}` }),
      expect: (s) => [...status(200)(s), ...(s.body.text === '{"marker":"done"}' ? [] : [`marker ${s.body.text}`])],
    },
    { name: 'big', event: () => event('/big'), expect: (s) => [...status(502)(s), ...(s.headers['x-meshscale-error'] === 'response-too-large' ? [] : ['not the controlled 502'])] },
    { name: 'stream', event: () => event('/stream'), expect: (s) => [...status(200)(s)] },
    { name: 'log', event: () => event('/log'), expect: (s, invocation) => [...status(200)(s), ...(invocation.logTail.includes('fixture console.log') ? [] : ['console.log not in the log tail'])] },
    { name: 'action-page', event: () => event('/action'), expect: status(200) },
    {
      name: 'action-submit',
      event: (results) => {
        const html = results['action-page'] && results['action-page'].text;
        const match = /name="(\$ACTION_ID_[0-9a-f]+)"/.exec(html || '');
        if (!match) return null;
        const form = multipart([[match[1], ''], ['name', 'phase5']]);
        return event('/action', { method: 'POST', headers: { 'content-type': form.type, origin: 'https://app.example.test' }, body: form.body, base64: true });
      },
      expect: status(303),
    },
    {
      name: 'oversize-request', note: 'Lambda rejects payloads above 6 MB before the function runs',
      event: () => event('/route-handler', { method: 'POST', headers: { 'content-type': 'application/octet-stream' }, body: Buffer.alloc(6.5 * 1024 * 1024, 1).toString('base64'), base64: true }),
      expectInvokeError: true,
    },
  ];
}

async function main() {
  const options = parseArgs(process.argv.slice(2));
  const results = {};
  const report = { function: options.function, qualifier: options.qualifier, startedAt: new Date().toISOString(), cases: [] };
  let failures = 0;
  for (const testCase of cases(options)) {
    const payload = testCase.event(results);
    if (!payload) {
      report.cases.push({ name: testCase.name, skipped: 'prerequisite missing' });
      console.log(`SKIP ${testCase.name}`);
      failures += 1;
      continue;
    }
    const invocation = await invoke(payload, options);
    const entry = { name: testCase.name, note: testCase.note, report: invocation.report };
    let problems;
    if (invocation.invokeError) {
      entry.invokeError = invocation.invokeError;
      problems = testCase.expectInvokeError ? [] : [`invoke failed: ${invocation.invokeError}`];
    } else if (testCase.expectInvokeError) {
      problems = ['expected the invoke to be rejected'];
    } else if (invocation.functionError) {
      entry.functionError = invocation.functionError;
      entry.raw = invocation.result;
      problems = [`FunctionError ${invocation.functionError}`];
    } else {
      const { summary, text } = summarize(invocation);
      results[testCase.name] = { summary, text };
      entry.result = summary;
      problems = summary ? testCase.expect(summary, invocation) : ['result is not a Lambda HTTP response'];
    }
    entry.problems = problems;
    if (problems.length) failures += 1;
    report.cases.push(entry);
    const timing = invocation.report && invocation.report.durationMs !== undefined
      ? ` ${invocation.report.durationMs} ms${invocation.report.initMs ? ` (init ${invocation.report.initMs} ms)` : ''}`
      : '';
    console.log(`${problems.length ? 'FAIL' : 'ok  '} ${testCase.name}${entry.result ? ` ${entry.result.statusCode}` : ''}${timing}${problems.length ? ` - ${problems.join('; ')}` : ''}`);
  }

  // Concurrent invocations: each response must echo its own request, never another's.
  const ids = Array.from({ length: options.concurrency }, (_, i) => `concurrent-${i}-${crypto.randomBytes(4).toString('hex')}`);
  const concurrent = await Promise.all(ids.map((id) => invoke(event('/ssr', { headers: { 'x-test': id } }), options)));
  const leaks = concurrent.flatMap((invocation, i) => {
    if (invocation.invokeError || invocation.functionError) return [`${ids[i]}: ${invocation.invokeError || invocation.functionError}`];
    const { summary } = summarize(invocation);
    const echo = (summary.body.marked || []).find(([name]) => name === 'echo');
    return echo && JSON.parse(echo[1])['x-test'] === ids[i] ? [] : [`${ids[i]}: got ${echo && echo[1]}`];
  });
  report.concurrent = {
    count: ids.length,
    coldStarts: concurrent.filter((invocation) => invocation.report && invocation.report.initMs).length,
    problems: leaks,
  };
  if (leaks.length) failures += 1;
  console.log(`${leaks.length ? 'FAIL' : 'ok  '} concurrent x${ids.length} (${report.concurrent.coldStarts} cold)${leaks.length ? ` - ${leaks.join('; ')}` : ''}`);

  report.failures = failures;
  if (options.out) {
    fs.mkdirSync(path.dirname(path.resolve(options.out)), { recursive: true });
    fs.writeFileSync(options.out, JSON.stringify(report, null, 2) + '\n');
    console.log(`wrote ${options.out}`);
  }
  console.log(failures ? `${failures} case(s) failed` : 'all cases passed');
  process.exitCode = failures ? 1 : 0;
}

main().catch((error) => {
  console.error(error.message);
  process.exitCode = 1;
});
