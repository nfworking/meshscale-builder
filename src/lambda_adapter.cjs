'use strict';

// MeshScale AWS Lambda adapter: converts Lambda Function URL events (payload format 2.0)
// into runtime requests and runtime responses back into Lambda results, and builds the
// Lambda handler around a runtime. Pure conversion: no Next.js, no AWS SDK.
//
// Deliberately independent of lambda_local_host.cjs (the local test front), so a
// conversion bug cannot cancel itself out in local tests.

// Lambda's synchronous invoke response limit. AWS error messages quote 6291556 bytes.
// [VERIFY] against current AWS documentation.
const LAMBDA_PAYLOAD_LIMIT = 6291556;
// Encoded bodies below this size cannot push the JSON result over the payload limit.
const PAYLOAD_CHECK_THRESHOLD = 4 * 1024 * 1024;
const PROBE_HEADER = 'x-meshscale-probe';
const HOP_BY_HOP = new Set(['connection', 'keep-alive', 'transfer-encoding', 'upgrade', 'te', 'trailer']);
const BODYLESS_STATUS = new Set([204, 205, 304]);

class InvalidEventError extends Error {
  constructor(message) {
    super(message);
    this.name = 'InvalidEventError';
  }
}

class ResponseTooLargeError extends Error {
  constructor(message) {
    super(message);
    this.name = 'ResponseTooLargeError';
  }
}

function fromLambdaEvent(event) {
  if (!event || typeof event !== 'object' || event.version !== '2.0' ||
      !event.requestContext || !event.requestContext.http) {
    throw new InvalidEventError('expected a Lambda Function URL event (payload format 2.0)');
  }
  const method = event.requestContext.http.method;
  if (typeof method !== 'string' || !method) {
    throw new InvalidEventError('event has no requestContext.http.method');
  }
  const rawPath = typeof event.rawPath === 'string' && event.rawPath ? event.rawPath : '/';
  if (!rawPath.startsWith('/')) {
    throw new InvalidEventError('event rawPath must start with "/"');
  }
  // Raw and still percent-encoded. Never decode, and never rebuild from
  // queryStringParameters (it joins repeated keys).
  const url = typeof event.rawQueryString === 'string' && event.rawQueryString
    ? `${rawPath}?${event.rawQueryString}`
    : rawPath;

  // Payload v2 lowercases names and comma-joins duplicate values; keep them as given.
  const headers = Object.entries(event.headers || {})
    .filter(([, value]) => value !== undefined && value !== null)
    .map(([name, value]) => [name, String(value)]);

  // Payload v2 moves cookies out of the headers.
  if (Array.isArray(event.cookies) && event.cookies.length) {
    headers.push(['cookie', event.cookies.join('; ')]);
  }

  // Lambda's own host is the Function URL domain, not the application's. The edge
  // forwards the original host in x-forwarded-host (kept as well).
  const forwarded = headers.find(([name]) => name.toLowerCase() === 'x-forwarded-host');
  const forwardedHost = forwarded ? forwarded[1].split(',')[0].trim() : '';
  if (forwardedHost) {
    const host = headers.find(([name]) => name.toLowerCase() === 'host');
    if (host) host[1] = forwardedHost;
    else headers.push(['host', forwardedHost]);
  }

  let body = Buffer.alloc(0);
  if (typeof event.body === 'string' && event.body.length) {
    body = event.isBase64Encoded ? Buffer.from(event.body, 'base64') : Buffer.from(event.body, 'utf8');
  }
  return { method, url, headers, body };
}

function isTextual(contentType) {
  const type = String(contentType || '').split(';')[0].trim().toLowerCase();
  return type.startsWith('text/') ||
    type === 'application/json' || type.endsWith('+json') ||
    type === 'application/xml' || type.endsWith('+xml') ||
    type === 'application/javascript' || type === 'application/x-javascript' ||
    type === 'application/x-www-form-urlencoded';
}

async function toLambdaResponse(response, options = {}) {
  const method = String(options.method || 'GET').toUpperCase();
  const maxBytes = options.maxBytes ?? LAMBDA_PAYLOAD_LIMIT;
  const maxPayloadBytes = options.maxPayloadBytes ?? LAMBDA_PAYLOAD_LIMIT;
  const bodyless = method === 'HEAD' || BODYLESS_STATUS.has(response.status);

  const headers = {};
  const cookies = [];
  for (const [rawName, rawValue] of response.headers) {
    const name = rawName.toLowerCase();
    const value = String(rawValue);
    if (name === 'set-cookie') {
      cookies.push(value);
      continue;
    }
    if (HOP_BY_HOP.has(name)) continue;
    // [VERIFY] with a real Function URL whether Lambda recomputes content-length.
    if (name === 'content-length' && method !== 'HEAD') continue;
    headers[name] = Object.prototype.hasOwnProperty.call(headers, name)
      ? `${headers[name]}, ${value}`
      : value;
  }

  // Always drain the body so the handler completes; keep it only when it is sent.
  const chunks = [];
  let total = 0;
  for await (const chunk of response.body) {
    if (bodyless) continue;
    total += chunk.length;
    if (total > maxBytes) {
      throw new ResponseTooLargeError(`response body exceeds ${maxBytes} bytes`);
    }
    chunks.push(chunk);
  }
  await response.done;

  const bytes = Buffer.concat(chunks, total);
  let body = '';
  let isBase64Encoded = false;
  if (bytes.length) {
    const text = bytes.toString('utf8');
    if (isTextual(headers['content-type']) && !headers['content-encoding'] &&
        Buffer.from(text, 'utf8').equals(bytes)) {
      body = text;
    } else {
      body = bytes.toString('base64');
      isBase64Encoded = true;
    }
  }

  const result = { statusCode: response.status, headers };
  if (cookies.length) result.cookies = cookies;
  result.body = body;
  result.isBase64Encoded = isBase64Encoded;
  if (body.length > PAYLOAD_CHECK_THRESHOLD &&
      Buffer.byteLength(JSON.stringify(result)) > maxPayloadBytes) {
    throw new ResponseTooLargeError(`encoded response exceeds the ${maxPayloadBytes} byte Lambda payload limit`);
  }
  return result;
}

function headerValue(headers, wanted) {
  const found = headers.find(([name]) => name.toLowerCase() === wanted);
  return found ? found[1] : undefined;
}

// Structured single-line logs. Never include request headers, cookies or bodies.
function log(level, message, error) {
  const entry = { level, source: 'meshscale-lambda', message };
  if (error) entry.error = error && error.stack ? String(error.stack) : String(error);
  console.log(JSON.stringify(entry));
}

function gatewayError(reason) {
  return {
    statusCode: 502,
    headers: { 'content-type': 'text/plain; charset=utf-8', 'x-meshscale-error': reason },
    body: 'Bad Gateway',
    isBase64Encoded: false,
  };
}

function createHandler({ start, backgroundTimeoutMs = 5000, maxResponseBytes, maxPayloadBytes } = {}) {
  if (typeof start !== 'function') throw new TypeError('createHandler needs a start() function');

  let runtimePromise = null;
  function begin() {
    runtimePromise = Promise.resolve().then(start);
    // Failures surface on the next invocation, not as an unhandled rejection.
    runtimePromise.catch(() => {});
    return runtimePromise;
  }
  // Start immediately, so initialization runs in Lambda's init phase.
  begin();

  async function getRuntime() {
    const current = runtimePromise || begin();
    try {
      return await current;
    } catch (error) {
      // Throw (an invocation error), and let the next invocation retry start().
      if (runtimePromise === current) runtimePromise = null;
      log('error', 'runtime initialization failed', error);
      throw error;
    }
  }

  return async function handler(event) {
    const runtime = await getRuntime();
    const request = fromLambdaEvent(event);

    if (headerValue(request.headers, PROBE_HEADER) === 'init') {
      return {
        statusCode: 200,
        headers: { 'content-type': 'application/json', 'cache-control': 'no-store' },
        body: JSON.stringify({ status: 'ok', probe: 'init' }),
        isBase64Encoded: false,
      };
    }

    const response = await runtime.invoke(request);
    let result;
    try {
      result = await toLambdaResponse(response, {
        method: request.method,
        maxBytes: maxResponseBytes,
        maxPayloadBytes,
      });
    } catch (error) {
      const reason = error instanceof ResponseTooLargeError ? 'response-too-large' : 'response-failed';
      log('error', `response could not be returned (${reason})`, error);
      result = gatewayError(reason);
    }

    // Lambda freezes the environment after returning: finish waitUntil/after() work first.
    const settled = await runtime.settle(backgroundTimeoutMs);
    if (settled.timedOut) {
      log('warn', `${settled.pending} background task(s) still pending after ${backgroundTimeoutMs} ms`);
    }
    return result;
  };
}

module.exports = {
  createHandler,
  fromLambdaEvent,
  toLambdaResponse,
  InvalidEventError,
  ResponseTooLargeError,
  LAMBDA_PAYLOAD_LIMIT,
};
