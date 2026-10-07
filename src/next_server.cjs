'use strict';

process.env.NODE_ENV = 'production';

const fs = require('node:fs');
const path = require('node:path');
const { config } = JSON.parse(
  fs.readFileSync(path.join(__dirname, '.next', 'required-server-files.json'), 'utf8')
);

if (!config || typeof config !== 'object') {
  throw new Error('Next.js required-server-files.json is missing its build configuration');
}

// Use the build-time configuration instead of reloading the source next.config file.
process.env.__NEXT_PRIVATE_STANDALONE_CONFIG = JSON.stringify(config);
require('next');
const { startServer } = require('next/dist/server/lib/start-server');

const portValue = process.env.PORT || '3000';
const port = Number(portValue);
if (!/^\d+$/.test(portValue) || !Number.isInteger(port) || port < 1 || port > 65535) {
  throw new Error('PORT must be an integer between 1 and 65535');
}

startServer({
  dir: __dirname,
  config,
  isDev: false,
  hostname: process.env.HOSTNAME || '127.0.0.1',
  port,
  allowRetry: false,
}).catch((error) => {
  console.error('Next.js runtime failed:', error);
  process.exit(1);
});
