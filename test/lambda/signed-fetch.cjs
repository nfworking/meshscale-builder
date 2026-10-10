'use strict';

// Phase 5: sends SigV4-signed HTTPS requests to an AWS_IAM Lambda Function URL. Credentials
// come from the AWS CLI's credential chain (`aws configure export-credentials`), stay in this
// process's memory and are never printed or passed on a command line.
//
//   node test/lambda/signed-fetch.cjs --url https://<id>.lambda-url.<region>.on.aws/ssr \
//     --region us-east-2 [--profile default] [--method POST] [-H 'name: value']... \
//     [--data-file body.bin] [--out response.json] [--include] [--unsigned]
//   node test/lambda/signed-fetch.cjs --url .../ssr --region us-east-2 --concurrent 10
//
// --include prints the status line and headers; --out writes the body to a file (otherwise
// it goes to stdout). --concurrent N sends N requests at once, each with its own x-test
// value, and checks that every response echoes its own value (fixture route /ssr).

const crypto = require('node:crypto');
const fs = require('node:fs');
const https = require('node:https');
const { execFileSync } = require('node:child_process');

function parseArgs(argv) {
  const args = { method: 'GET', headers: [], include: false, unsigned: false, concurrent: 0 };
  for (let i = 0; i < argv.length; i += 1) {
    const name = argv[i];
    const take = () => argv[++i];
    if (name === '--url') args.url = take();
    else if (name === '--region') args.region = take();
    else if (name === '--profile') args.profile = take();
    else if (name === '--method' || name === '-X') args.method = take().toUpperCase();
    else if (name === '-H' || name === '--header') {
      const header = take();
      const colon = header.indexOf(':');
      if (colon < 1) throw new Error(`invalid header ${header}`);
      args.headers.push([header.slice(0, colon).trim().toLowerCase(), header.slice(colon + 1).trim()]);
    } else if (name === '--data-file') args.dataFile = take();
    else if (name === '--out') args.out = take();
    else if (name === '--include' || name === '-i') args.include = true;
    else if (name === '--unsigned') args.unsigned = true;
    else if (name === '--concurrent') args.concurrent = Number(take());
    else throw new Error(`unknown argument ${name}`);
  }
  if (!args.url || !args.region) throw new Error('--url and --region are required');
  return args;
}

function credentials(profile) {
  const command = ['configure', 'export-credentials', '--format', 'process'];
  if (profile) command.push('--profile', profile);
  const parsed = JSON.parse(execFileSync('aws', command, { encoding: 'utf8', windowsHide: true }));
  return { accessKeyId: parsed.AccessKeyId, secretAccessKey: parsed.SecretAccessKey, sessionToken: parsed.SessionToken };
}

// RFC 3986 encoding as SigV4 requires.
const encode = (value) => encodeURIComponent(value).replace(/[!'()*]/g, (c) => `%${c.charCodeAt(0).toString(16).toUpperCase()}`);
const sha256 = (data) => crypto.createHash('sha256').update(data).digest('hex');
const hmac = (key, data) => crypto.createHmac('sha256', key).update(data).digest();

function canonicalQuery(rawQuery) {
  if (!rawQuery) return '';
  return rawQuery.split('&').filter(Boolean).map((pair) => {
    const index = pair.indexOf('=');
    const key = index === -1 ? pair : pair.slice(0, index);
    const value = index === -1 ? '' : pair.slice(index + 1);
    return [encode(decodeURIComponent(key)), encode(decodeURIComponent(value))];
  }).sort(([a, x], [b, y]) => (a < b ? -1 : a > b ? 1 : x < y ? -1 : x > y ? 1 : 0))
    .map(([key, value]) => `${key}=${value}`).join('&');
}

function sign({ method, url, headers, body, region, creds, now = new Date() }) {
  const amzDate = now.toISOString().replace(/[-:]/g, '').replace(/\.\d{3}/, '');
  const date = amzDate.slice(0, 8);
  const payloadHash = sha256(body);
  const signed = [['host', url.host], ['x-amz-content-sha256', payloadHash], ['x-amz-date', amzDate]];
  if (creds.sessionToken) signed.push(['x-amz-security-token', creds.sessionToken]);
  signed.sort(([a], [b]) => (a < b ? -1 : 1));
  // Non-S3 services sign each path segment URI-encoded once more (double encoding).
  const canonicalUri = url.pathname.split('/').map(encode).join('/') || '/';
  const canonicalRequest = [
    method,
    canonicalUri,
    canonicalQuery(url.search.slice(1)),
    signed.map(([name, value]) => `${name}:${value.trim()}\n`).join(''),
    signed.map(([name]) => name).join(';'),
    payloadHash,
  ].join('\n');
  const scope = `${date}/${region}/lambda/aws4_request`;
  const stringToSign = ['AWS4-HMAC-SHA256', amzDate, scope, sha256(canonicalRequest)].join('\n');
  const key = ['lambda', 'aws4_request'].reduce(hmac, hmac(hmac(`AWS4${creds.secretAccessKey}`, date), region));
  const signature = crypto.createHmac('sha256', key).update(stringToSign).digest('hex');
  return [
    ...headers,
    ...signed.filter(([name]) => name !== 'host'),
    ['authorization', `AWS4-HMAC-SHA256 Credential=${creds.accessKeyId}/${scope}, SignedHeaders=${signed.map(([name]) => name).join(';')}, Signature=${signature}`],
  ];
}

function send(url, method, headers, body) {
  return new Promise((resolve, reject) => {
    const all = [['host', url.host], ...headers];
    if (body.length || method === 'POST' || method === 'PUT') all.push(['content-length', String(body.length)]);
    const request = https.request({ host: url.hostname, path: url.pathname + url.search, method, headers: all.flat() }, (response) => {
      const chunks = [];
      response.on('data', (chunk) => chunks.push(chunk));
      response.on('end', () => resolve({ response, body: Buffer.concat(chunks) }));
      response.on('error', reject);
    });
    request.on('error', reject);
    request.setTimeout(60000, () => request.destroy(new Error('timeout')));
    request.end(body);
  });
}

async function request(args, creds, extraHeaders = []) {
  const url = new URL(args.url);
  const body = args.dataFile ? fs.readFileSync(args.dataFile) : Buffer.alloc(0);
  const base = [...args.headers, ...extraHeaders];
  const headers = args.unsigned ? base : sign({ method: args.method, url, headers: base, body, region: args.region, creds });
  return send(url, args.method, headers, body);
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const creds = args.unsigned ? null : credentials(args.profile);
  if (args.concurrent > 0) {
    const ids = Array.from({ length: args.concurrent }, (_, i) => `concurrent-${i}-${crypto.randomBytes(4).toString('hex')}`);
    const started = Date.now();
    const results = await Promise.all(ids.map((id) => request(args, creds, [['x-test', id]])));
    const problems = results.flatMap(({ response, body }, i) => {
      const echo = /id="echo">([^<]*)</.exec(body.toString('utf8'));
      const value = echo && JSON.parse(echo[1].replace(/&quot;/g, '"'))['x-test'];
      return response.statusCode === 200 && value === ids[i] ? [] : [`${ids[i]}: status ${response.statusCode}, echoed ${value}`];
    });
    console.log(`${problems.length ? 'FAIL' : 'ok'} ${ids.length} concurrent requests in ${Date.now() - started} ms${problems.length ? `: ${problems.join('; ')}` : ''}`);
    process.exitCode = problems.length ? 1 : 0;
    return;
  }
  const { response, body } = await request(args, creds);
  if (args.include) {
    console.log(`HTTP/${response.httpVersion} ${response.statusCode} ${response.statusMessage}`);
    for (let i = 0; i < response.rawHeaders.length; i += 2) console.log(`${response.rawHeaders[i]}: ${response.rawHeaders[i + 1]}`);
    console.log();
  }
  if (args.out) fs.writeFileSync(args.out, body);
  else process.stdout.write(body);
  process.exitCode = response.statusCode >= 400 && !args.unsigned ? 1 : 0;
}

if (require.main === module) {
  main().catch((error) => {
    console.error(error.message);
    process.exitCode = 1;
  });
}

module.exports = { sign, canonicalQuery };
