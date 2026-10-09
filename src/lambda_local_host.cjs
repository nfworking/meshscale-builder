'use strict';

// MeshScale local Lambda host.
//
// DEVELOPMENT ONLY. This file is embedded in the builder binary and written to a
// temporary location by `meshscale-builder run --lambda-local`. It is never part of
// an artifact and never part of lambda.zip.
//
// It speaks the runner's framed stdin/stdout protocol (identical to function-entry.cjs)
// on one side, and calls the artifact's lambda-entry.cjs `handler(event, context)` with
// AWS Lambda Function URL (payload format 2.0) events on the other. The event and
// response conversion here is written independently of lambda-adapter.cjs on purpose:
// if both sides shared code, a bug in that code would cancel itself out.
//
// Working directory must be the artifact's runtime/ directory.
//
// Environment knobs (all optional):
//   MESHSCALE_LAMBDA_LOCAL_TIMEOUT_MS      handler timeout, default 30000
//   MESHSCALE_LAMBDA_LOCAL_CONCURRENCY     simultaneous invocations, default 1
//                                          (a real Lambda execution environment runs
//                                          one invocation at a time)

const path = require('node:path');
const crypto = require('node:crypto');

// stdout is the frame channel. Application code (console.log, libraries) must never
// write to it, or the runner's frame parser breaks. Keep the raw writer for frames and
// send everything else to stderr.
const rawStdoutWrite = process.stdout.write.bind(process.stdout);
process.stdout.write = (chunk, encoding, callback) =>
  process.stderr.write(chunk, encoding, callback);

const MAX_FRAME = 64 * 1024 * 1024;
// Lambda's synchronous invoke payload cap (6 MB). AWS error messages quote
// 6291556 bytes; verify against current AWS documentation.
const LAMBDA_PAYLOAD_LIMIT = 6291556;
const TIMEOUT_MS = positiveNumber(process.env.MESHSCALE_LAMBDA_LOCAL_TIMEOUT_MS, 30000);
const CONCURRENCY = Math.floor(positiveNumber(process.env.MESHSCALE_LAMBDA_LOCAL_CONCURRENCY, 1));
const FUNCTION_NAME = 'meshscale-local';
const REGION = 'us-east-1';
const ACCOUNT_ID = '000000000000';
const URL_ID = 'meshscalelocal';
const URL_DOMAIN = `${URL_ID}.lambda-url.${REGION}.on.aws`;
const FUNCTION_ARN = `arn:aws:lambda:${REGION}:${ACCOUNT_ID}:function:${FUNCTION_NAME}`;

const HOP_BY_HOP_REQUEST = new Set(['connection', 'keep-alive', 'transfer-encoding', 'upgrade', 'te', 'trailer']);
const HOP_BY_HOP_RESPONSE = new Set(['connection', 'keep-alive', 'transfer-encoding', 'upgrade', 'te', 'trailer']);
const BODYLESS_STATUS = new Set([204, 205, 304]);

function positiveNumber(value, fallback) {
  const number = Number(value);
  return Number.isFinite(number) && number > 0 ? number : fallback;
}

function log(message) {
  process.stderr.write(`[lambda-local] ${message}\n`);
}

// ---------------------------------------------------------------- frame protocol

let stdoutTail = Promise.resolve();

function writeFrame(header, body = Buffer.alloc(0)) {
  if (body.length !== (header.length || 0)) {
    throw new Error('frame body length mismatch');
  }
  const json = Buffer.from(JSON.stringify(header));
  if (json.length > 1024 * 1024 || body.length > MAX_FRAME) {
    throw new Error('frame too large');
  }
  const frame = Buffer.allocUnsafe(8 + json.length + body.length);
  frame.writeUInt32BE(json.length, 0);
  frame.writeUInt32BE(body.length, 4);
  json.copy(frame, 8);
  body.copy(frame, 8 + json.length);
  const job = stdoutTail.then(
    () => new Promise((resolve, reject) => {
      rawStdoutWrite(frame, (error) => (error ? reject(error) : resolve()));
    }),
  );
  stdoutTail = job.catch(() => {});
  return job;
}

let chunks = [];
let buffered = 0;
let stage = 'prefix';
let headerLength = 0;
let bodyLength = 0;

function take(count) {
  const all = chunks.length === 1 ? chunks[0] : Buffer.concat(chunks, buffered);
  const out = all.subarray(0, count);
  const rest = all.subarray(count);
  chunks = rest.length ? [rest] : [];
  buffered = rest.length;
  return out;
}

function onData(chunk) {
  chunks.push(chunk);
  buffered += chunk.length;
  for (;;) {
    if (stage === 'prefix') {
      if (buffered < 8) return;
      const prefix = take(8);
      headerLength = prefix.readUInt32BE(0);
      bodyLength = prefix.readUInt32BE(4);
      if (headerLength > 1024 * 1024 || bodyLength > MAX_FRAME) {
        log('protocol error: frame too large');
        process.exit(1);
      }
      stage = 'frame';
    }
    if (stage === 'frame') {
      if (buffered < headerLength + bodyLength) return;
      const frame = take(headerLength + bodyLength);
      stage = 'prefix';
      const header = JSON.parse(frame.subarray(0, headerLength).toString('utf8'));
      const body = Buffer.from(frame.subarray(headerLength));
      if (header.kind !== 'request') {
        log(`protocol error: unknown frame kind ${header.kind}`);
        process.exit(1);
      }
      schedule(() => handleRequest(header, body));
    }
  }
}

// ---------------------------------------------------------------- concurrency

let active = 0;
const waiting = [];

function schedule(task) {
  waiting.push(task);
  pump();
}

function pump() {
  while (active < CONCURRENCY && waiting.length) {
    const task = waiting.shift();
    active += 1;
    task()
      .catch((error) => log(`unexpected failure: ${error && error.stack ? error.stack : error}`))
      .finally(() => {
        active -= 1;
        pump();
      });
  }
}

// ---------------------------------------------------------------- event building

function isTextual(contentType) {
  const type = String(contentType || '').toLowerCase();
  return (
    type.startsWith('text/') ||
    type.includes('json') ||
    type.includes('xml') ||
    type.includes('javascript') ||
    type.includes('x-www-form-urlencoded')
  );
}

function awsTime(date) {
  const months = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];
  const two = (n) => String(n).padStart(2, '0');
  return `${two(date.getUTCDate())}/${months[date.getUTCMonth()]}/${date.getUTCFullYear()}:` +
    `${two(date.getUTCHours())}:${two(date.getUTCMinutes())}:${two(date.getUTCSeconds())} +0000`;
}

function buildEvent(header, body) {
  const uri = header.uri || '/';
  const queryStart = uri.indexOf('?');
  // Raw, still percent-encoded, exactly as received. Never decode here.
  const rawPath = queryStart === -1 ? uri : uri.slice(0, queryStart);
  const rawQueryString = queryStart === -1 ? '' : uri.slice(queryStart + 1);

  const headers = {};
  const cookies = [];
  let originalHost;
  for (const [rawName, value] of header.headers || []) {
    const name = rawName.toLowerCase();
    if (HOP_BY_HOP_REQUEST.has(name)) continue;
    if (name === 'cookie') {
      // Payload v2 moves cookies out of `headers` into `cookies`.
      for (const part of String(value).split(';')) {
        const cookie = part.trim();
        if (cookie) cookies.push(cookie);
      }
      continue;
    }
    if (name === 'host') {
      originalHost = value;
      continue;
    }
    if (name === 'x-forwarded-host' || name === 'x-forwarded-proto' ||
        name === 'x-forwarded-port' || name === 'x-forwarded-for') {
      continue; // The local runner plays the edge and sets these itself.
    }
    // Duplicate headers are comma-joined in payload v2.
    headers[name] = Object.prototype.hasOwnProperty.call(headers, name)
      ? `${headers[name]},${value}`
      : value;
  }

  // A real Function URL sees its own domain as Host. The original host must be
  // forwarded by the edge in a separate header.
  headers.host = URL_DOMAIN;
  if (originalHost !== undefined) headers['x-forwarded-host'] = originalHost;
  headers['x-forwarded-proto'] = 'https';
  headers['x-forwarded-port'] = '443';
  headers['x-forwarded-for'] = '127.0.0.1';
  headers['x-amzn-trace-id'] = `Root=1-${Math.floor(Date.now() / 1000).toString(16).padStart(8, '0')}-${crypto.randomBytes(12).toString('hex')}`;
  if (body.length) headers['content-length'] = String(body.length);

  const now = new Date();
  const method = header.method || 'GET';
  const event = {
    version: '2.0',
    routeKey: '$default',
    rawPath,
    rawQueryString,
    headers,
    requestContext: {
      accountId: ACCOUNT_ID,
      apiId: URL_ID,
      domainName: URL_DOMAIN,
      domainPrefix: URL_ID,
      http: {
        method,
        path: rawPath,
        protocol: 'HTTP/1.1',
        sourceIp: '127.0.0.1',
        userAgent: headers['user-agent'] || '',
      },
      requestId: crypto.randomUUID(),
      routeKey: '$default',
      stage: '$default',
      time: awsTime(now),
      timeEpoch: now.getTime(),
    },
    isBase64Encoded: false,
  };
  if (cookies.length) event.cookies = cookies;

  if (rawQueryString) {
    // Best effort. The MeshScale runtime must use rawQueryString; this is only here
    // so handlers that read queryStringParameters see something realistic.
    const params = {};
    for (const [key, value] of new URLSearchParams(rawQueryString)) {
      params[key] = Object.prototype.hasOwnProperty.call(params, key) ? `${params[key]},${value}` : value;
    }
    event.queryStringParameters = params;
  }

  if (body.length) {
    const text = body.toString('utf8');
    // Text bodies stay as strings only if they survive a UTF-8 round trip. The exact
    // rule AWS applies is not documented precisely, so handlers must accept both forms.
    if (isTextual(headers['content-type']) && Buffer.from(text, 'utf8').equals(body)) {
      event.body = text;
    } else {
      event.body = body.toString('base64');
      event.isBase64Encoded = true;
    }
  }
  return event;
}

function buildContext() {
  const deadline = Date.now() + TIMEOUT_MS;
  return {
    awsRequestId: crypto.randomUUID(),
    functionName: FUNCTION_NAME,
    functionVersion: '$LATEST',
    invokedFunctionArn: FUNCTION_ARN,
    memoryLimitInMB: process.env.AWS_LAMBDA_FUNCTION_MEMORY_SIZE,
    logGroupName: `/aws/lambda/${FUNCTION_NAME}`,
    logStreamName: 'local',
    callbackWaitsForEmptyEventLoop: false,
    getRemainingTimeInMillis: () => Math.max(0, deadline - Date.now()),
  };
}

// ---------------------------------------------------------------- response conversion

class GatewayError extends Error {}

function convertResult(result, method) {
  if (!result || typeof result !== 'object' || !Number.isInteger(result.statusCode)) {
    // Real Lambda would infer a 200 JSON response from some return values. MeshScale
    // requires the explicit shape so mistakes surface locally.
    throw new GatewayError('handler must return an object with an integer statusCode');
  }
  if (result.statusCode < 100 || result.statusCode > 599) {
    throw new GatewayError(`invalid statusCode ${result.statusCode}`);
  }
  if (result.cookies !== undefined &&
      (!Array.isArray(result.cookies) || result.cookies.some((c) => typeof c !== 'string'))) {
    throw new GatewayError('cookies must be an array of strings');
  }
  if (result.headers !== undefined && (typeof result.headers !== 'object' || result.headers === null)) {
    throw new GatewayError('headers must be an object');
  }
  if (result.body !== undefined && result.body !== null && typeof result.body !== 'string') {
    throw new GatewayError('body must be a string');
  }
  if (Buffer.byteLength(JSON.stringify(result)) > LAMBDA_PAYLOAD_LIMIT) {
    throw new GatewayError(`response exceeds the ${LAMBDA_PAYLOAD_LIMIT} byte Lambda payload limit`);
  }

  const headers = [];
  for (const [name, value] of Object.entries(result.headers || {})) {
    if (typeof value !== 'string') {
      throw new GatewayError(`header ${name} must be a string`);
    }
    const lower = name.toLowerCase();
    if (HOP_BY_HOP_RESPONSE.has(lower)) continue;
    if (lower === 'content-length' && method !== 'HEAD') continue;
    headers.push([lower, value]);
  }
  for (const cookie of result.cookies || []) headers.push(['set-cookie', cookie]);

  let body = Buffer.alloc(0);
  if (typeof result.body === 'string' && result.body.length) {
    body = result.isBase64Encoded ? Buffer.from(result.body, 'base64') : Buffer.from(result.body, 'utf8');
  }
  if (method === 'HEAD' || BODYLESS_STATUS.has(result.statusCode)) body = Buffer.alloc(0);
  return { status: result.statusCode, headers, body };
}

async function sendResponse(id, response) {
  await writeFrame({ kind: 'headers', id, status: response.status, headers: response.headers, length: 0 });
  if (response.body.length) {
    await writeFrame({ kind: 'chunk', id, length: response.body.length }, response.body);
  }
  await writeFrame({ kind: 'end', id, length: 0 });
}

function gatewayResponse(status, reason) {
  const body = Buffer.from(`${status === 413 ? 'Payload Too Large' : 'Bad Gateway'}: ${reason}\n`, 'utf8');
  return {
    status,
    headers: [
      ['content-type', 'text/plain; charset=utf-8'],
      ['x-meshscale-lambda-local-error', '1'],
    ],
    body,
  };
}

// ---------------------------------------------------------------- invocation

let handler;

async function handleRequest(header, body) {
  const id = header.id;
  const method = header.method || 'GET';
  try {
    const event = buildEvent(header, body);
    if (Buffer.byteLength(JSON.stringify(event)) > LAMBDA_PAYLOAD_LIMIT) {
      await sendResponse(id, gatewayResponse(413, 'request exceeds the Lambda payload limit'));
      return;
    }
    const context = buildContext();
    let timer;
    const timeout = new Promise((_, reject) => {
      timer = setTimeout(() => reject(new GatewayError(`handler timed out after ${TIMEOUT_MS} ms`)), TIMEOUT_MS);
    });
    let result;
    try {
      result = await Promise.race([Promise.resolve().then(() => handler(event, context)), timeout]);
    } finally {
      clearTimeout(timer);
    }
    await sendResponse(id, convertResult(result, method));
  } catch (error) {
    const reason = error && error.message ? error.message : String(error);
    log(`invocation failed: ${error && error.stack ? error.stack : reason}`);
    await sendResponse(id, gatewayResponse(502, reason)).catch((sendError) => {
      log(`could not send error response: ${sendError}`);
    });
  }
}

// ---------------------------------------------------------------- startup

function configureEnvironment() {
  const defaults = {
    AWS_LAMBDA_FUNCTION_NAME: FUNCTION_NAME,
    AWS_LAMBDA_FUNCTION_VERSION: '$LATEST',
    AWS_LAMBDA_FUNCTION_MEMORY_SIZE: '1024',
    AWS_REGION: REGION,
    AWS_DEFAULT_REGION: REGION,
    AWS_EXECUTION_ENV: `AWS_Lambda_nodejs${process.versions.node.split('.')[0]}.x`,
    LAMBDA_TASK_ROOT: process.cwd(),
    NODE_ENV: 'production',
  };
  for (const [name, value] of Object.entries(defaults)) {
    if (process.env[name] === undefined) process.env[name] = value;
  }
  // Application logging written with console.log must not reach the frame channel.
  console.log = console.error;
  console.info = console.error;
  console.debug = console.error;
}

function main() {
  configureEnvironment();
  const entry = path.join(process.cwd(), 'lambda-entry.cjs');
  const started = Date.now();
  try {
    handler = require(entry).handler;
  } catch (error) {
    log(`failed to load ${entry}: ${error && error.stack ? error.stack : error}`);
    process.exit(1);
  }
  if (typeof handler !== 'function') {
    log('lambda-entry.cjs does not export a handler function');
    process.exit(1);
  }
  log(`handler module loaded in ${Date.now() - started} ms (concurrency ${CONCURRENCY}, timeout ${TIMEOUT_MS} ms)`);
  process.stdin.on('data', onData);
  process.stdin.on('end', () => process.exit(0));
  writeFrame({ kind: 'ready', length: 0 }).catch((error) => {
    log(`could not signal readiness: ${error}`);
    process.exit(1);
  });
}

main();
