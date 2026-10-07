'use strict';

const fs = require('node:fs');
const path = require('node:path');
const http = require('node:http');
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

function loadHandler(output) {
  const key = output.id;
  if (handlers.has(key)) return handlers.get(key);

  const modulePath = path.resolve(root, output.filePath);
  const loaded = require(modulePath);
  const handler =
    loaded.handler ||
    loaded.default ||
    loaded;
  if (typeof handler !== 'function') {
    throw new Error(`adapter output ${output.id} does not export a handler`);
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

async function handle(req, res) {
  try {
    const requestUrl = new URL(
      req.url || '/',
      `http://${req.headers.host || '127.0.0.1'}`,
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

    const handler = loadHandler(output);

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

    if (!res.writableEnded) {
      res.end();
    }
  } catch (error) {
    console.error('MeshScale function invocation failed:', error);
    if (!res.headersSent) res.statusCode = 500;
    if (!res.writableEnded) res.end('Internal Server Error');
  }
}

const portValue = process.env.PORT || '3000';
const port = Number(portValue);
if (!Number.isInteger(port) || port < 1 || port > 65535) {
  throw new Error('PORT must be an integer between 1 and 65535');
}

const server = http.createServer(handle);
server.keepAliveTimeout = 5000;
server.headersTimeout = 60000;

server.listen(port, '127.0.0.1', () => {
  console.log(`MeshScale function runtime ready on 127.0.0.1:${port}`);
});
