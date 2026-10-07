'use strict';

const fs = require('node:fs');
const path = require('node:path');
const { Readable, Writable } = require('node:stream');
const { pathToFileURL } = require('node:url');
const { resolveRoutes } = require('@next/routing');

process.env.NODE_ENV = 'production';

const root = __dirname;
const adapterPath = path.join(root, '.next', 'meshscale-adapter.json');
const metadata = JSON.parse(fs.readFileSync(adapterPath, 'utf8'));

const outputs = [
  ...(metadata.outputs.pages || []),
  ...(metadata.outputs.pagesApi || []),
  ...(metadata.outputs.appPages || []),
  ...(metadata.outputs.appRoutes || []),
];
const pathnames = outputs.map((output) => output.pathname);
const handlers = new Map();

const MAX_FRAME = 64 * 1024 * 1024;
let input = Buffer.alloc(0);
let stdoutTail = Promise.resolve();

function writeFrame(header, body = Buffer.alloc(0)) {
  if (body.length !== (header.length || 0)) {
    throw new Error('function protocol body length mismatch');
  }
  const json = Buffer.from(JSON.stringify(header));
  if (json.length > 1024 * 1024 || body.length > MAX_FRAME) {
    throw new Error('function protocol frame is too large');
  }

  const frame = Buffer.allocUnsafe(8 + json.length + body.length);
  frame.writeUInt32BE(json.length, 0);
  frame.writeUInt32BE(body.length, 4);
  json.copy(frame, 8);
  body.copy(frame, 8 + json.length);

  const job = stdoutTail.then(() => new Promise((resolve, reject) => {
    process.stdout.write(frame, (error) => error ? reject(error) : resolve());
  }));
  stdoutTail = job.catch(() => {});
  return job;
}

function queueFrame(header, body = Buffer.alloc(0)) {
  return writeFrame(header, body);
}

async function consumeStdin() {
  for await (const chunk of process.stdin) {
    input = Buffer.concat([input, chunk]);
    while (input.length >= 8) {
      const headerLength = input.readUInt32BE(0);
      const bodyLength = input.readUInt32BE(4);
      if (headerLength > 1024 * 1024 || bodyLength > MAX_FRAME) {
        throw new Error('function protocol frame is too large');
      }
      const total = 8 + headerLength + bodyLength;
      if (input.length < total) break;

      const json = input.subarray(8, 8 + headerLength);
      const body = input.subarray(8 + headerLength, total);
      input = input.subarray(total);

      const header = JSON.parse(json.toString('utf8'));
      await dispatchFrame(header, body);
    }
  }
}

async function loadHandler(output) {
  const key = output.id;
  if (handlers.has(key)) return handlers.get(key);

  const modulePath = path.resolve(root, output.filePath);
  let loaded;
  try {
    loaded = require(modulePath);
  } catch (error) {
    if (error && error.code === 'ERR_REQUIRE_ESM') {
      loaded = await import(pathToFileURL(modulePath).href);
    } else {
      throw error;
    }
  }

  const handler = loaded.handler || loaded.default || loaded;
  if (typeof handler !== 'function') {
    throw new Error('adapter output ' + output.id + ' does not export a handler');
  }
  handlers.set(key, handler);
  return handler;
}

function findOutput(pathname, requestUrl) {
  const rsc = requestUrl.headers.rsc || requestUrl.headers['next-router-prefetch'];
  const candidates = outputs.filter((output) => output.pathname === pathname);
  if (candidates.length === 1) return candidates[0];
  if (!rsc) {
    const normal = candidates.find((output) => !output.pathname.endsWith('.rsc'));
    if (normal) return normal;
  }
  return candidates[0];
}

function makeRequest(header, body) {
  const headers = {};
  const rawHeaders = [];

  for (const [name, value] of header.headers || []) {
    const lower = name.toLowerCase();
    if (lower === 'set-cookie') {
      headers[lower] = Array.isArray(headers[lower]) ? [...headers[lower], value] : [value];
    } else if (headers[lower] === undefined) {
      headers[lower] = value;
    } else if (Array.isArray(headers[lower])) {
      headers[lower].push(value);
    } else {
      headers[lower] = headers[lower] + ', ' + value;
    }
    rawHeaders.push(name, value);
  }

  const req = Readable.from(body.length ? [body] : []);
  req.method = header.method || 'GET';
  req.url = header.uri || '/';
  req.headers = headers;
  req.rawHeaders = rawHeaders;
  req.httpVersion = '1.1';
  req.httpVersionMajor = 1;
  req.httpVersionMinor = 1;
  req.complete = true;
  req.aborted = false;
  req.socket = {
    encrypted: false,
    remoteAddress: '127.0.0.1',
    remotePort: 0,
    localAddress: '127.0.0.1',
    localPort: 0,
  };
  req.connection = req.socket;
  return req;
}

class FunctionResponse extends Writable {
  constructor(id) {
    super();
    this.id = id;
    this.statusCode = 200;
    this.statusMessage = undefined;
    this.headers = new Map();
    this.headersSent = false;
    this._completion = new Promise((resolve, reject) => {
      this._resolveCompletion = resolve;
      this._rejectCompletion = reject;
    });
    this.once('finish', () => {
      this._resolveCompletion();
    });
    this.once('error', (error) => this._rejectCompletion(error));
  }

  setHeader(name, value) {
    if (this.headersSent) throw new Error('headers already sent');
    this.headers.set(String(name).toLowerCase(), value);
    return this;
  }

  getHeader(name) {
    return this.headers.get(String(name).toLowerCase());
  }

  getHeaders() {
    return Object.fromEntries(this.headers);
  }

  getHeaderNames() {
    return [...this.headers.keys()];
  }

  hasHeader(name) {
    return this.headers.has(String(name).toLowerCase());
  }

  removeHeader(name) {
    if (this.headersSent) throw new Error('headers already sent');
    this.headers.delete(String(name).toLowerCase());
  }

  writeHead(statusCode, reasonOrHeaders, maybeHeaders) {
    if (typeof reasonOrHeaders === 'object' && reasonOrHeaders !== null) {
      Object.entries(reasonOrHeaders).forEach(([name, value]) => this.setHeader(name, value));
    } else if (maybeHeaders) {
      Object.entries(maybeHeaders).forEach(([name, value]) => this.setHeader(name, value));
    }
    this.statusCode = statusCode;
    this._sendHeaders();
    return this;
  }

  flushHeaders() {
    this._sendHeaders();
  }

  _sendHeaders() {
    if (this.headersSent) return;
    this.headersSent = true;

    const headers = [];
    for (const [name, value] of this.headers) {
      if (Array.isArray(value)) {
        for (const item of value) headers.push([name, String(item)]);
      } else {
        headers.push([name, String(value)]);
      }
    }

    queueFrame({
      kind: 'headers',
      id: this.id,
      status: this.statusCode,
      headers,
      length: 0,
    }).catch((error) => this.destroy(error));
  }

  _write(chunk, encoding, callback) {
    try {
      this._sendHeaders();
      queueFrame({
        kind: 'chunk',
        id: this.id,
        length: Buffer.byteLength(chunk),
      }, Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk, encoding))
        .then(() => callback(), callback);
    } catch (error) {
      callback(error);
    }
  }

  _final(callback) {
    this._sendHeaders();
    queueFrame({
      kind: 'end',
      id: this.id,
      length: 0,
    }).then(() => callback(), callback);
  }

  completion() {
    return this._completion;
  }
}

async function handleRequest(header, body) {
  const req = makeRequest(header, body);
  const res = new FunctionResponse(header.id);

  try {
    const requestUrl = new URL(
      req.url || '/',
      'http://' + (req.headers.host || '127.0.0.1'),
    );

    const result = await resolveRoutes({
      url: requestUrl,
      buildId: metadata.buildId,
      basePath: metadata.config.basePath || '',
      i18n: metadata.config.i18n || undefined,
      headers: new Headers(req.headers),
      requestBody: req,
      pathnames,
      routes: metadata.routing,
      invokeMiddleware: async () => ({}),
    });

    if (result.redirect) {
      res.statusCode = result.redirect.status;
      res.setHeader('location', result.redirect.url.toString());
      res.end();
      await res.completion();
      return;
    }

    if (!result.resolvedPathname) {
      res.statusCode = 404;
      res.end('Not Found');
      await res.completion();
      return;
    }

    const output = findOutput(result.resolvedPathname, req);
    if (!output) {
      res.statusCode = 404;
      res.end('Not Found');
      await res.completion();
      return;
    }

    const handler = await loadHandler(output);
    await handler(req, res, {
      waitUntil: (promise) => {
        Promise.resolve(promise).catch((error) => {
          console.error('MeshScale waitUntil task failed:', error);
        });
      },
      requestMeta: {
        relativeProjectDir: '.',
        hostname: req.headers.host || '127.0.0.1',
      },
    });

    if (!res.writableEnded) res.end();
    await res.completion();
  } catch (error) {
    console.error('MeshScale function invocation failed:', error);
    try {
      if (!res.headersSent) res.statusCode = 500;
      if (!res.writableEnded) res.end('Internal Server Error');
      await res.completion();
    } catch {
      await queueFrame({
        kind: 'error',
        id: header.id,
        error: String(error && error.message ? error.message : error),
        length: 0,
      }).catch(() => {});
    }
  }
}

async function dispatchFrame(header, body) {
  if (header.kind === 'request') {
    if (body.length !== header.length) throw new Error('function request body length mismatch');
    void handleRequest(header, body).catch((error) => {
      console.error('MeshScale function request failed:', error);
      queueFrame({ kind: 'error', id: header.id, error: String(error), length: 0 }).catch(() => {});
    });
    return;
  }
  throw new Error('unknown function protocol frame');
}

writeFrame({ kind: 'ready', length: 0 }).catch((error) => {
  console.error('MeshScale function runtime failed to signal readiness:', error);
  process.exitCode = 1;
});

consumeStdin().catch((error) => {
  console.error('MeshScale function runtime protocol failed:', error);
  process.exitCode = 1;
});
