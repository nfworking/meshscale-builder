'use strict';

// Phase 5: turns a Function URL event captured with capture-event.mjs into a fixture that
// is safe to commit. Account, URL and request identifiers, IP addresses, IAM caller details
// and signing headers are replaced with fixed placeholders; anything else that still
// contains the real account or URL ID is replaced too.
//
//   node test/lambda/redact-event.cjs test/lambda/captured/get.json test/fixtures/events/captured-get.json
//
// Review the output before committing it.

const fs = require('node:fs');

const ACCOUNT = '123456789012';
const URL_ID = 'abcdefghijklmnopqrstuvwxyz012345';
const IP = '203.0.113.10';
const REQUEST_ID = '00000000-0000-4000-8000-000000000000';
// Present them, never their values: the app would see these headers too.
const SIGNING_HEADERS = ['authorization', 'x-amz-security-token', 'x-amz-date', 'x-amz-content-sha256'];

function redactAll(value) {
  if (Array.isArray(value)) return value.map(redactAll);
  if (value && typeof value === 'object') {
    return Object.fromEntries(Object.entries(value).map(([key, inner]) => [key, redactAll(inner)]));
  }
  return typeof value === 'string' ? '<redacted>' : value;
}

function redact(event, source) {
  const result = structuredClone(event);
  const context = result.requestContext || {};
  const secrets = [context.accountId, context.apiId, context.domainPrefix].filter(Boolean);

  if (context.accountId) context.accountId = ACCOUNT;
  if (context.apiId) context.apiId = URL_ID;
  if (context.domainPrefix) context.domainPrefix = URL_ID;
  if (context.requestId) context.requestId = REQUEST_ID;
  if (context.http && context.http.sourceIp) context.http.sourceIp = IP;
  if (context.authorizer) context.authorizer = redactAll(context.authorizer);

  const headers = result.headers || {};
  for (const name of Object.keys(headers)) {
    const lower = name.toLowerCase();
    if (SIGNING_HEADERS.includes(lower)) headers[name] = '<redacted>';
    if (lower === 'x-forwarded-for') headers[name] = IP;
    if (lower === 'x-amzn-trace-id') headers[name] = 'Root=1-00000000-000000000000000000000000';
  }

  let text = JSON.stringify(result, null, 2);
  for (const secret of secrets) text = text.split(secret).join(secret === context.accountId ? ACCOUNT : URL_ID);
  return { _source: source, ...JSON.parse(text) };
}

function main() {
  const [input, output] = process.argv.slice(2);
  if (!input || !output) {
    console.error('usage: node test/lambda/redact-event.cjs <captured.json> <fixture.json>');
    process.exitCode = 1;
    return;
  }
  const event = JSON.parse(fs.readFileSync(input, 'utf8'));
  if (event.version !== '2.0' || !event.requestContext || !event.requestContext.http) {
    throw new Error(`${input} is not a Function URL payload 2.0 event`);
  }
  const source = `captured from a real Lambda Function URL on ${new Date().toISOString().slice(0, 10)}; redacted by test/lambda/redact-event.cjs`;
  const before = JSON.stringify(event);
  const redacted = redact(event, source);
  const after = JSON.stringify(redacted);
  // Fail loudly if an identifier survived anywhere.
  for (const secret of [event.requestContext.accountId, event.requestContext.apiId, event.requestContext.http.sourceIp]) {
    if (secret && after.includes(secret)) throw new Error('redaction left an identifier in the output; not written');
  }
  fs.writeFileSync(output, JSON.stringify(redacted, null, 2) + '\n');
  console.log(`wrote ${output} (${before.length} -> ${after.length} bytes); review it before committing`);
}

if (require.main === module) main();

module.exports = { redact };
