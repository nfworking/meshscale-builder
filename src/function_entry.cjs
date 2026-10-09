'use strict';

// MeshScale IPC shell: speaks the runner's framed stdin/stdout protocol and hands each
// request to runtime.cjs. Next.js execution lives in runtime.cjs.

// stdout is the frame channel. Application code (console.log, libraries) must never
// write to it, or the runner's frame parser breaks. Keep the raw writer for frames and
// send everything else to stderr.
const rawStdoutWrite = process.stdout.write.bind(process.stdout);
process.stdout.write = (chunk, encoding, callback) =>
  process.stderr.write(chunk, encoding, callback);
console.log = console.error;
console.info = console.error;
console.debug = console.error;

const { createRuntime } = require('./runtime.cjs');

const MAX_FRAME = 64 * 1024 * 1024;
let input = Buffer.alloc(0);
let stdoutTail = Promise.resolve();
let runtime;

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
    rawStdoutWrite(frame, (error) => error ? reject(error) : resolve());
  }));
  stdoutTail = job.catch(() => {});
  return job;
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

async function handleRequest(header, body) {
  const response = await runtime.invoke({
    method: header.method || 'GET',
    url: header.uri || '/',
    headers: header.headers || [],
    body,
  });
  const chunks = response.body[Symbol.asyncIterator]();
  let complete = false;
  try {
    await writeFrame({
      kind: 'headers',
      id: header.id,
      status: response.status,
      headers: response.headers,
      length: 0,
    });
    for (;;) {
      const { value, done } = await chunks.next();
      if (done) break;
      await writeFrame({ kind: 'chunk', id: header.id, length: value.length }, value);
    }
    complete = true;
    await response.done;
  } catch (error) {
    console.error('MeshScale function response failed:', error);
    await writeFrame({
      kind: 'error',
      id: header.id,
      error: String(error && error.message ? error.message : error),
      length: 0,
    }).catch(() => {});
    return;
  } finally {
    if (!complete) await chunks.return();
  }
  await writeFrame({ kind: 'end', id: header.id, length: 0 });
}

async function dispatchFrame(header, body) {
  if (header.kind === 'request') {
    if (body.length !== header.length) throw new Error('function request body length mismatch');
    void handleRequest(header, body).catch((error) => {
      console.error('MeshScale function request failed:', error);
      writeFrame({ kind: 'error', id: header.id, error: String(error), length: 0 }).catch(() => {});
    });
    return;
  }
  throw new Error('unknown function protocol frame');
}

createRuntime({ root: __dirname }).then(
  (created) => {
    runtime = created;
    writeFrame({ kind: 'ready', length: 0 }).catch((error) => {
      console.error('MeshScale function runtime failed to signal readiness:', error);
      process.exitCode = 1;
    });
    consumeStdin().catch((error) => {
      console.error('MeshScale function runtime protocol failed:', error);
      process.exitCode = 1;
    });
  },
  (error) => {
    console.error('MeshScale function runtime failed to start:', error);
    process.exitCode = 1;
  },
);
