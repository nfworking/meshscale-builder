'use strict';

// MeshScale Next.js execution engine. Transport-agnostic: the IPC shell
// (function-entry.cjs) and other transports call invoke() and settle().
//
//   const runtime = await createRuntime({ root: __dirname });
//   const response = await runtime.invoke({ method, url, headers, body });
//   // response: { status, headers: [[name, value], ...], body: AsyncIterable<Buffer>, done }
//   await runtime.settle(timeoutMs); // waits for ctx.waitUntil work
//
// This module never reads stdin, writes stdout or exits the process.

const fs = require('node:fs');
const path = require('node:path');
const { AsyncLocalStorage } = require('node:async_hooks');
const { Readable, Writable, finished } = require('node:stream');
const { pathToFileURL } = require('node:url');

// Next.js server modules expect AsyncLocalStorage to be exposed on the global
// object by the Next runtime. MeshScale invokes adapter output modules directly
// from its own Node process, so provide the same runtime primitive before any
// Next.js modules are loaded.
if (!globalThis.AsyncLocalStorage) {
  globalThis.AsyncLocalStorage = AsyncLocalStorage;
}

const { resolveRoutes } = require('@next/routing');

process.env.NODE_ENV = 'production';

const SETTLE_TIMEOUT = Symbol('settle timeout');

function makeRequest({ method, url, headers: headerList, body }) {
  const headers = {};
  const rawHeaders = [];

  for (const [name, value] of headerList || []) {
    const lower = name.toLowerCase();
    if (lower === 'set-cookie') {
      headers[lower] = Array.isArray(headers[lower]) ? [...headers[lower], value] : [value];
    } else if (headers[lower] === undefined) {
      headers[lower] = value;
    } else if (Array.isArray(headers[lower])) {
      headers[lower].push(value);
    } else {
      // Multiple Cookie headers (HTTP/2 splits cookies) form one cookie list.
      headers[lower] = headers[lower] + (lower === 'cookie' ? '; ' : ', ') + value;
    }
    rawHeaders.push(name, value);
  }

  const bytes = body || Buffer.alloc(0);
  const req = Readable.from(bytes.length ? [bytes] : []);
  req.method = method || 'GET';
  req.url = url || '/';
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

// A Node-like ServerResponse whose head and body are handed to the transport:
// the head through response(), the body as an async iterable pulled by the transport.
class RuntimeResponse extends Writable {
  constructor() {
    super();
    this.statusCode = 200;
    this.statusMessage = undefined;
    this.headers = new Map();
    this.headersSent = false;
    this._queue = [];
    this._ended = false;
    this._failure = null;
    this._wake = null;
    this._head = new Promise((resolve, reject) => {
      this._resolveHead = resolve;
      this._rejectHead = reject;
    });
    this._head.catch(() => {});
    this._done = new Promise((resolve, reject) => {
      finished(this, { readable: false }, (error) => (error ? reject(error) : resolve()));
    });
    this._done.catch(() => {});
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
    this._resolveHead({ status: this.statusCode, headers });
  }

  _notify() {
    const wake = this._wake;
    this._wake = null;
    if (wake) wake();
  }

  _write(chunk, encoding, callback) {
    this._sendHeaders();
    const bytes = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk, encoding);
    if (bytes.length === 0) {
      callback();
      return;
    }
    // The write completes when the transport pulls the chunk (backpressure).
    this._queue.push({ chunk: bytes, callback });
    this._notify();
  }

  _final(callback) {
    this._sendHeaders();
    this._ended = true;
    this._notify();
    callback();
  }

  _destroy(error, callback) {
    // autoDestroy also lands here, without an error, after a normal finish.
    if (error || !this._ended) {
      this._failure = error || new Error('response was destroyed before it ended');
    }
    this._rejectHead(this._failure || new Error('response ended without a head'));
    this._notify();
    callback(error);
  }

  _body() {
    const response = this;
    return {
      [Symbol.asyncIterator]() {
        return {
          async next() {
            for (;;) {
              if (response._queue.length) {
                const { chunk, callback } = response._queue.shift();
                callback();
                return { value: chunk, done: false };
              }
              if (response._failure) throw response._failure;
              if (response._ended) return { value: undefined, done: true };
              await new Promise((resolve) => {
                response._wake = resolve;
              });
            }
          },
          async return() {
            // The transport stopped reading: release the handler instead of blocking it forever.
            if (!response._ended && !response.destroyed) {
              response.destroy(new Error('response body consumer stopped'));
            }
            return { value: undefined, done: true };
          },
        };
      },
    };
  }

  async response() {
    try {
      const head = await this._head;
      return { ...head, body: this._body(), done: this._done };
    } catch {
      // Destroyed before a head was produced: same controlled 500 as a thrown error.
      return internalError();
    }
  }
}

function internalError() {
  const body = Buffer.from('Internal Server Error');
  return {
    status: 500,
    headers: [],
    body: (async function* () {
      yield body;
    })(),
    done: Promise.resolve(),
  };
}

async function createRuntime({ root }) {
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
  const background = new Set();

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

  function findOutput(pathname, req) {
    const rsc = req.headers.rsc || req.headers['next-router-prefetch'];
    const candidates = outputs.filter((output) => output.pathname === pathname);
    if (candidates.length === 1) return candidates[0];
    if (!rsc) {
      const normal = candidates.find((output) => !output.pathname.endsWith('.rsc'));
      if (normal) return normal;
    }
    return candidates[0];
  }

  function waitUntil(promise) {
    const task = Promise.resolve(promise).then(
      () => {},
      (error) => {
        console.error('MeshScale waitUntil task failed:', error);
      },
    );
    background.add(task);
    task.then(() => background.delete(task));
  }

  async function handle(req, res) {
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
        return;
      }

      if (!result.resolvedPathname) {
        res.statusCode = 404;
        res.end('Not Found');
        return;
      }

      const output = findOutput(result.resolvedPathname, req);
      if (!output) {
        res.statusCode = 404;
        res.end('Not Found');
        return;
      }

      const handler = await loadHandler(output);
      await handler(req, res, {
        waitUntil,
        requestMeta: {
          relativeProjectDir: '.',
          hostname: req.headers.host || '127.0.0.1',
        },
      });

      if (!res.writableEnded) res.end();
    } catch (error) {
      console.error('MeshScale function invocation failed:', error);
      if (res.writableEnded || res.destroyed) return;
      if (!res.headersSent) {
        res.statusCode = 500;
        res.end('Internal Server Error');
      } else {
        // The head is already with the transport: surface the failure on the body.
        res.destroy(error instanceof Error ? error : new Error(String(error)));
      }
    }
  }

  async function invoke(request) {
    const req = makeRequest(request);
    const res = new RuntimeResponse();
    void handle(req, res);
    return res.response();
  }

  async function settle(timeoutMs = Infinity) {
    const deadline = Date.now() + timeoutMs;
    while (background.size) {
      const remaining = deadline - Date.now();
      if (remaining <= 0) return { pending: background.size, timedOut: true };
      let timer;
      const timeout = Number.isFinite(remaining)
        ? new Promise((resolve) => {
          timer = setTimeout(resolve, remaining, SETTLE_TIMEOUT);
        })
        : new Promise(() => {});
      const outcome = await Promise.race([Promise.all([...background]), timeout]);
      clearTimeout(timer);
      if (outcome === SETTLE_TIMEOUT) return { pending: background.size, timedOut: true };
    }
    return { pending: 0, timedOut: false };
  }

  return { invoke, settle };
}

module.exports = { createRuntime };
