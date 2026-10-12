# MeshScale Edge: implementation plan (routing only)

Status: plan. Nothing here is built yet. Companion documents: `PLATFORM-PLAN.md` (the deployment workflows there write the
configuration this edge reads), `LAMBDA-PLAN.md` (Phases 6 and 7 list the requirements this plan
implements) and `DEPLOYMENT-CONTRACT.md` (the manifest contract).

Tags used below: **[FROM SOURCE]** was read from this repository's code. **[VERIFY]** is a platform
fact that must be confirmed against current Cloudflare or AWS documentation before it is relied on.

## 1. Goal and non-goals

The edge receives every request for a customer hostname and decides one of two things:

1. **Static:** the request matches a static rule in the deployment's `manifest.json`. Stream the object
   from R2 with the headers the manifest records.
2. **Function:** everything else. Forward the request to the deployment's Lambda Function URL, signed with
   SigV4, and stream the response back.

It gets its configuration from R2 and caches it. It holds no deployment state of its own.

Non-goals for this plan: TLS and custom-domain onboarding (Cloudflare for SaaS), WAF, rate limiting,
authentication of end users, response caching of function output, image optimization, middleware,
ISR/revalidation, WebSockets/upgrades, Lambda response streaming, multi-region failover, analytics
beyond logs and basic metrics. Each can be added later without changing the configuration format.

## 2. Ground truth the edge must match [FROM SOURCE]

The local runner already behaves like this edge. The edge is a port of that behavior, and the runner is
the reference for every disagreement.

| Concern | Reference in this repo |
|---|---|
| Rule types and matching | `src/routing.rs`: `Routing`, `Rule`, `QueryPolicy`, `select()`, `decoded_path()`, `validate()` |
| How rules are derived | `src/static_output.rs`: `generate()` |
| Static response headers | `src/runner.rs`: `dispatch()` (status, `content-type`, `cache-control`, `etag` = `"<sha256>"`, `last-modified`, then `object.headers`; HEAD has an empty body) |
| Forwarding to a Function URL | `src/lambda_endpoint.rs`: header filtering, `x-forwarded-host`, SigV4 (service `lambda`, `x-amz-content-sha256`), local 413 above 6,291,456 bytes |
| Manifest fields | `src/manifest.rs`, `manifest.schema.json` (strict; `deny_unknown_fields`) |
| R2 layout | `src/upload.rs`: `{org}/{project}/{deployment}/static/<key>` and `.../manifest.json`, all create-only (`If-None-Match: *`), manifest last |

Facts the port depends on:

- `routing.semantics` is `meshscale-routing-v1`. Rule 0 is always `server_variants` with fixed contents;
  the last rule is always `server`; everything between is an exact `static` rule. `validate()` rejects
  anything else.
- Static rules have unique paths, methods `["GET","HEAD"]`, and a query policy: `ignore` (any query) or
  `empty` (no query string, or `?` with nothing after it).
- `server_variants` sends a request to the function when the method is not GET or HEAD, when any listed
  header is present, when any listed cookie name is present, or when any listed query parameter name is
  present. Query names are the part before `=`, percent-decoded (lossy UTF-8; `+` is **not** a space).
  Cookie names come from every `cookie` header, split on `;`, trimmed, the part before `=`.
- `decoded_path()` rejects a `%` not followed by two hex digits, invalid UTF-8 after decoding, a path not
  starting with `/`, any `\`, `:` or NUL, and any `.` or `..` segment. The runner answers those with 400.
- R2 object keys are the manifest's object keys, unencoded, below the deployment prefix
  (`upload.rs` percent-encodes them only for the S3 HTTP path). `manifest.static.storage.prefix` records the
  prefix (`{org}/{project}/{deployment}/static/`) once the manifest is bound for upload.
- The Lambda adapter answers `x-meshscale-probe: init` itself (`src/lambda_adapter.cjs`), and reads the
  original host from `x-forwarded-host`.

## 3. Architecture

```
client ──TLS──► Cloudflare Worker "meshscale-edge"
                 │ 1. host  ─► host binding   (R2 bucket EDGE_CONFIG, mutable, short TTL)
                 │ 2. binding ─► manifest.json (R2 bucket STATIC, immutable, cached long)
                 │ 3. select(method, url, headers)
                 ├── static ─► R2 STATIC get {prefix}static/<key> ─► response with manifest headers
                 └── function ─► SigV4 fetch ─► Lambda Function URL (AWS_IAM, on alias d-<deployment>)
```

- **Platform:** a Cloudflare Worker written in TypeScript, deployed with Wrangler. R2 is reached through
  **bindings** (no S3 credentials in the Worker). Two bindings: `STATIC` (the bucket `upload` writes) and
  `EDGE_CONFIG` (a separate bucket only the deployment workflows (Trigger.dev) write; see 4.1).
- **Location:** a sibling of this repository (`../edge`, i.e. `meshscale/edge`) unless the owner prefers
  otherwise. It shares nothing at build time with the Rust crate except the test vectors in Phase E0.
- **Dependencies:** none at runtime except, optionally, `aws4fetch` for SigV4 (small, Workers-native,
  WebCrypto-based). Hand-rolling SigV4 with WebCrypto is the alternative; either way the signer is tested
  against the same vector the Rust test uses (`lambda_endpoint.rs`, epoch `20261009T120000Z`).

## 4. Configuration in R2

### 4.1 Host binding (new; written by the deployment workflows)

The manifest says *how* to route a deployment. Something must say *which* deployment serves a host and
*where* its function is. That is the host binding: one small mutable JSON object per hostname.

Bucket `EDGE_CONFIG` (for example `meshscale-edge-config`), key `hosts/v1/<host>.json`, where `<host>` is
the lowercased hostname without port, IDNA/punycode form.

A separate bucket is deliberate: org IDs may contain only `[A-Za-z0-9_-]` (`upload::validate_id`), so a
prefix inside the static bucket could still collide with an org, and R2 API tokens are scoped per bucket,
not per prefix [VERIFY]. The workflows get write access to this bucket only; build hosts never do.

```json
{
  "schema": "meshscale-host-binding-v1",
  "host": "app.example.com",
  "generation": 42,
  "updated_at": "2026-10-12T10:00:00Z",
  "org_id": "org_123",
  "project_id": "project_456",
  "deployment_id": "build_789",
  "manifest": {
    "key": "org_123/project_456/build_789/manifest.json",
    "sha256": "<hex sha256 of the manifest bytes>"
  },
  "origin": {
    "type": "lambda_function_url",
    "url": "https://<id>.lambda-url.us-east-2.on.aws/",
    "region": "us-east-2"
  }
}
```

Rules the edge enforces when it loads a binding (any failure: treat the host as unconfigured, see 7):

- `schema` is exactly `meshscale-host-binding-v1`; unknown fields are rejected (mirrors the manifest's
  strictness, so a format change is never half-understood).
- `host` equals the requested host. IDs match `validate_id`. `manifest.key` equals
  `{org_id}/{project_id}/{deployment_id}/manifest.json`.
- `origin.url` is `https`, a bare origin, and its hostname ends in `.lambda-url.<region>.on.aws` with the
  same `region`. The edge never forwards to a host that came from anywhere else (LAMBDA-PLAN Phase 7:
  "the target comes from a validated manifest or route table and never from request data").
- `generation` is an integer. A cached binding is never replaced by one with a lower generation (protects
  against an older copy surfacing from a cache layer).

### 4.2 Manifest (existing; written by `meshscale-builder upload`)

Read from `STATIC` at `manifest.key`. Before use:

1. SHA-256 of the bytes equals `binding.manifest.sha256`.
2. `deployment.id`, `org_id`, `project_id` equal the binding's.
3. `routing` is present and passes a TypeScript port of `Routing::validate()` (semantics, rule 0 equal
   to the fixed variants rule, last rule `server`, only exact static rules between, unique paths, every
   static rule's object exists in `static.objects`).
4. `static.storage.prefix`, when present, equals `{org}/{project}/{deployment}/static/`.

The edge ignores manifest fields it does not need (`runtime`, `platform`, `server`), but the port of
`validate()` must not silently accept a rule shape it does not understand.

### 4.3 Compiled form

Static rules are exact paths with unique values, so after validation the edge compiles the manifest into:

- the variants rule (sets of methods, header names, cookie names, query names), and
- `Map<decodedPath, {objectKey, query, object}>`.

`select()` becomes: decode path, check variants, one map lookup, check method and query policy. This is
equivalent to the Rust loop because the rule list has no overlapping static paths and the `server` rule is
last. It keeps lookup O(1) for manifests with tens of thousands of objects.

## 5. Caching

Three layers, checked in order. All keys include what makes the value immutable.

| What | L1: isolate memory | L2: Cache API (per data center) | L3: R2 |
|---|---|---|---|
| Host binding | 15 s fresh, served stale up to 24 h if L2/L3 fail | 30 s, synthetic key `https://edge-config.internal/hosts/v1/<host>` | `EDGE_CONFIG.get` |
| Unknown host (negative) | 10 s | 10 s | `EDGE_CONFIG.get` returned null |
| Compiled manifest | until evicted (immutable, keyed by `manifest.key` + `sha256`) | 7 days, key includes the sha256 | `STATIC.get` |
| Static objects | not cached in memory | optional later phase | `STATIC.get` per request |

- **Single flight:** concurrent misses for the same key in one isolate share one R2 read (a `Map` of
  in-flight promises).
- **Bounded memory:** L1 manifest cache is an LRU bounded by an estimate of compiled size (default 32 MB
  total) so a few huge manifests cannot exhaust the isolate's memory [VERIFY Workers memory limit, currently
  understood as 128 MB per isolate].
- **Stale-if-error:** if R2 is unreachable, serve the last good binding and manifest and log it. Never
  serve a binding that failed validation.
- **Propagation:** a promotion is visible within about 30 s plus L1's 15 s worst case. The platform
  plan accepts this (it keeps the previous deployment's alias, Function URL and static prefix alive, so
  a request routed with an older binding still works). If faster switching is needed later, add an
  authenticated purge endpoint or move bindings to Workers KV; neither changes the binding format.
- **Consistency within a request:** one request uses one binding and the manifest it names. Static files
  and the Function URL always belong to the same deployment.

## 6. Request handling

### 6.1 Order of operations

1. Reject `Upgrade` requests with 501 (the runner does the same).
2. Normalize the host: lowercase, strip port. Load binding (5). Missing: 404 with a plain body
   `Unknown host`; invalid: 502 `x-meshscale-error: config`.
3. Load and compile the manifest. Failure to load or validate: forward to the function (degraded mode;
   correct but slower, and `/_next/static` may 404 if the function does not serve it). Log and count it.
4. Decode the path with the port of `decoded_path()`. Failure: 400.
5. `select()`. Static: 6.2. Function: 6.3.

### 6.2 Static response

- `STATIC.get(prefix + objectKey)`; for HEAD use `STATIC.head` (no body read).
- Status `object.status`; headers in the runner's order: `content-type`, `cache-control`,
  `etag: "<sha256>"`, `last-modified`, then each `object.headers` entry.
- Missing R2 object (should not happen after a successful upload): 404 for GET/HEAD, logged as an
  integrity error.
- Later phase (E4), not in parity scope: conditional requests (`If-None-Match`, `If-Modified-Since` -> 304)
  and `Range`. The runner does not do these today, so they are recorded as intended differences.

### 6.3 Forward to the function

Mirrors `lambda_endpoint.rs`:

- **URL:** `origin.url` + the original path and query **as received** (raw, not re-encoded).
- **Request headers dropped:** hop-by-hop (`connection`, `keep-alive`, `proxy-connection`,
  `transfer-encoding`, `upgrade`, `te`, `trailer`); signature-owned (`host`, `content-length`,
  `authorization`, `x-amz-date`, `x-amz-security-token`, `x-amz-content-sha256`); `x-forwarded-host`
  (replaced); `x-meshscale-probe` (internal health-check header; clients must not trigger it); Cloudflare
  internal headers (`cf-*`, `cdn-loop`) except as mapped below.
- **Request headers set:** `x-forwarded-host: <original host>`, `x-forwarded-proto: https`,
  `x-forwarded-for: <cf-connecting-ip>`.
- **Body:** read up to 6,291,456 bytes. Reject earlier from `content-length` when it is larger, otherwise
  count while reading; over the limit returns 413 without contacting Lambda. Lambda's own event overhead
  and base64 can still push a smaller body over its limit; that comes back from AWS and is passed through
  as 413 (LAMBDA-PLAN Phase 5 result).
- **Signing:** SigV4, service `lambda`, region `origin.region`, payload hash in `x-amz-content-sha256`.
  Credentials come from Worker secrets (6.4).
- **Timeout:** abort after the function's timeout plus 5 s (the workflows pass the function timeout as
  part of the binding in a later schema revision; until then a fixed 35 s matching the Phase 5 function's
  30 s).
- **Response:** pass status and headers through, minus hop-by-hop and `x-amzn-trace-id`; keep
  `x-amzn-requestid` renamed to `x-meshscale-request-id` for debugging. Stream the body.
- **Error mapping:** Lambda throttling (429 with an `x-amzn-errortype` header) -> 503 with `retry-after: 1`;
  network failure or timeout -> 502/504 with `x-meshscale-error: origin`. A 502 that already carries
  `x-meshscale-error` from the adapter (for example the oversized-response guard) is passed through.

### 6.4 AWS credentials

For this phase: an IAM user in the customer-functions account (see PLATFORM-PLAN 8.1) whose only
policy allows `lambda:InvokeFunctionUrl` and `lambda:InvokeFunction` on `function:ms-*` with the condition
`lambda:FunctionUrlAuthType = AWS_IAM` [VERIFY that both actions are required for Function URL callers;
the Phase 5 runbook needed both]. Its keys are Worker secrets (`AWS_ACCESS_KEY_ID`,
`AWS_SECRET_ACCESS_KEY`), never in `wrangler.toml`, logs or responses, and rotated on a schedule.
LAMBDA-PLAN Phase 7 lists the production replacement (IAM Roles Anywhere or similar); the signer takes a
credentials provider so that swap does not touch request code.

## 7. Errors, logs and metrics

- Every response the edge generates itself carries `x-meshscale-error: <reason>` (`config`, `origin`,
  `too-large`) and a short plain-text body. Unknown hosts get 404 without the header.
- One structured log line per request: host, deployment id, decision (`static` / `function` /
  `degraded`), status, R2 or origin latency, cache layer hit for binding and manifest, request id. No
  query strings, cookies, authorization headers or bodies.
- Counters (Workers Analytics Engine or logs-derived): decisions by type, config load failures,
  stale-if-error uses, 413s, origin errors by kind.

## 8. Testing

1. **Cross-implementation vectors (Phase E0):** the Rust `select()` and `decoded_path()` are the oracle.
   A Rust test writes `test/fixtures/routing-vectors.json` (manifest routing plus a list of
   method/URL/headers -> expected decision or 400), and fails if the committed file is stale. The edge's
   unit tests load the same file. Any divergence fails one side's CI.
2. **Unit tests** (Vitest with `@cloudflare/vitest-pool-workers`, which runs inside workerd): binding
   validation, manifest validation, compiled `select()`, header filtering, SigV4 against the fixed vector,
   cache layers with fake clocks, single flight, stale-if-error, generation ordering.
3. **Integration, local:** Miniflare R2 seeded from a real builder output
   (`upload` semantics: `{org}/{project}/{deployment}/static/...` plus `manifest.json`), a binding object,
   and a fake Function URL server that checks the SigV4 signature and answers from fixtures.
4. **Golden parity:** run `test/golden/golden.cjs` against `wrangler dev` (seeded as in 3) with the origin
   set to the real AWS_IAM Function URL from Phase 5, and compare with the runner in `--lambda-endpoint`
   mode. Allowed differences must be listed (see 9).
5. **Staging:** a `*.staging` hostname bound to a sandbox deployment; repeat 4 against the deployed Worker.

## 9. Known intended differences from the local runner

- **URL normalization:** the Workers runtime parses `request.url` with the WHATWG URL parser, which
  resolves dot segments (including `%2e`) and converts `\` to `/` before the Worker sees the path. The
  runner rejects those requests with 400; the edge sees an already-normalized path and routes it. The
  function receives the same normalized path, so static and function decisions stay consistent.
  Cloudflare zone URL-normalization settings also apply [VERIFY current defaults].
- Function responses gain `x-meshscale-request-id`; `x-amzn-trace-id` is removed.
- Unknown host returns 404 (the runner has no such case for a single project).
- After Phase E4: 304 and 206 responses for static objects.

## 10. Phases

Each phase ends with its tests green and a short written result, then stops for review, the same way as
LAMBDA-PLAN.

### Phase E0: routing test vectors (in this repository)

- Add a Rust test that generates `test/fixtures/routing-vectors.json` from `Routing::new()` plus a set of
  static rules covering both query policies, base path, and every variant trigger, and checks the committed
  file matches. Include `decoded_path()` rejection cases.
- No change to routing behavior or the manifest schema.
- Acceptance: `cargo test` passes; the file is committed and documented in `DEPLOYMENT-CONTRACT.md`.

### Phase E1: Worker skeleton and configuration loading

- Wrangler project, two R2 bindings, TypeScript strict, Vitest pool.
- Host binding loader and validator; manifest loader, sha256 check, `validate()` port, compiler.
- L1 cache with single flight; L2 Cache API; negative caching; stale-if-error.
- Debug endpoint disabled in production: `GET /__meshscale/edge/config` returning the resolved deployment
  id, generation and cache layer for the request's host, only when an `EDGE_DEBUG_TOKEN` secret matches.
- Acceptance: unit tests for every validation rule and cache behavior.

### Phase E2: routing and static serving

- `decoded_path()` and `select()` ports passing every vector from E0.
- Static responses from R2 with runner-identical headers; HEAD without body.
- Requests that select the function return 501 `x-meshscale-error: function-not-wired` in this phase only.
- Acceptance: vectors pass; Miniflare integration serves every static object of a seeded fixture build
  with the bytes and headers the runner serves.

### Phase E3: forwarding to Lambda

- SigV4 signer (vector test), header filtering, body limit and 413, timeout, response passthrough, error
  mapping, probe-header stripping.
- Acceptance: integration tests against the fake Function URL; golden parity (8.4) against the real
  sandbox Function URL with only the differences in 9.

### Phase E4: hardening (optional before first production traffic)

- Conditional requests and `Range` for static objects; L2 caching of small static objects.
- Load test: p50/p99 added latency for static hits and function forwards; memory with a 50k-object
  manifest.
- Staging deployment and runbook (bind a host, promote, roll back, rotate the invoker keys).

## 11. Open decisions (owner)

1. Repository location: sibling `meshscale/edge` (default) or a folder in this repository.
2. `aws4fetch` versus a hand-written SigV4 signer.
3. Binding propagation target: is about 45 s worst case acceptable, or is a purge endpoint / KV needed now?
4. Degraded mode when a manifest fails validation: forward everything to the function (default) or fail
   closed with 502.
5. Whether the binding should also carry the function timeout and a maximum body size (schema v2).
