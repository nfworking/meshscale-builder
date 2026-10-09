'use strict';

// Tests for test/lambda/redact-event.cjs. Run with: node --test test/runtime/*.test.cjs

const test = require('node:test');
const assert = require('node:assert/strict');
const { redact } = require('../lambda/redact-event.cjs');

const captured = {
  version: '2.0',
  rawPath: '/api/echo',
  rawQueryString: 'a=1',
  headers: {
    host: 'realurlid0123456789abcdefghijklm.lambda-url.eu-west-1.on.aws',
    authorization: 'AWS4-HMAC-SHA256 Credential=AKIAREALKEY/20261009/eu-west-1/lambda/aws4_request, Signature=abc',
    'x-amz-security-token': 'REAL-SESSION-TOKEN',
    'x-amz-date': '20261009T120000Z',
    'x-amz-content-sha256': 'e3b0',
    'x-forwarded-for': '198.51.100.77',
    'x-amzn-trace-id': 'Root=1-6706ff00-real',
    'x-forwarded-host': 'app.example.test',
  },
  requestContext: {
    accountId: '999988887777',
    apiId: 'realurlid0123456789abcdefghijklm',
    domainName: 'realurlid0123456789abcdefghijklm.lambda-url.eu-west-1.on.aws',
    domainPrefix: 'realurlid0123456789abcdefghijklm',
    requestId: 'real-request-id',
    authorizer: { iam: { accessKey: 'AKIAREALKEY', accountId: '999988887777', callerId: 'AIDREAL', userArn: 'arn:aws:iam::999988887777:user/cal', userId: 'AIDREAL' } },
    http: { method: 'GET', path: '/api/echo', protocol: 'HTTP/1.1', sourceIp: '198.51.100.77', userAgent: 'curl/8.0' },
  },
  isBase64Encoded: false,
};

test('identifiers, IAM caller details and signing headers are replaced', () => {
  const result = redact(captured, 'test');
  const text = JSON.stringify(result);
  for (const secret of ['999988887777', 'realurlid0123456789abcdefghijklm', 'AKIAREALKEY', 'REAL-SESSION-TOKEN',
    '198.51.100.77', 'user/cal', 'AIDREAL', 'Signature=abc', 'real-request-id', '6706ff00-real']) {
    assert.ok(!text.includes(secret), `${secret} survived`);
  }
  assert.equal(result._source, 'test');
  assert.equal(result.headers.authorization, '<redacted>');
  assert.equal(result.headers['x-forwarded-host'], 'app.example.test');
  assert.equal(result.headers.host, 'abcdefghijklmnopqrstuvwxyz012345.lambda-url.eu-west-1.on.aws');
  assert.equal(result.requestContext.http.method, 'GET');
  assert.equal(result.rawQueryString, 'a=1');
  assert.deepEqual(captured.headers['x-amz-security-token'], 'REAL-SESSION-TOKEN', 'input is not modified');
});
