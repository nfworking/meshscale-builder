# MeshScale deployment contract v2

The [JSON Schema](manifest.schema.json) specifies the serialized shape. Consumers must also enforce the semantic invariants below; schema validation alone does not validate object references, deployment identity agreement, route order, HTTP dates or safe paths.

This contract separates immutable static publication from a logical Node server. The server runtime is never uploaded or packaged by the builder. It does not implement an edge, deploy that server, or activate a production alias. It is not compatible with Vercel's Build Output API.

## Identity and storage

- `version` is exactly `2`; `framework` is `nextjs`.
- `deployment` records build ID, repository, resolved full 40-character commit, branch, organization/project IDs and Next build ID/version.
- IDs use 1-128 ASCII letters, digits, underscores or hyphens.
- `server.target_id` equals `<org>/<project>/<build>`. Unbound local identities use `_local`; these are not deployable destinations.
- `static.directory` is `static`. `static.storage` is null before publication, or `{ "provider": "r2", "bucket": "...", "prefix": "<org>/<project>/<build>/static/" }`.
- `static.objects` maps literal relative object suffixes to bytes, lowercase SHA-256, content type, cache policy, HTTP Last-Modified, response class, status and allowlisted headers.
- An R2 object key is `static.storage.prefix + object suffix`. Do not URL-decode keys. Percent-encode each key segment only when constructing the storage HTTP request.
- The content ETag is the quoted SHA-256, for example `"abc...def"`. Do not substitute an R2/S3 ETag, which can use different semantics.
- No credentials, preview secrets, absolute source paths or raw Next manifests belong in this edge-facing document. Runtime fallback manifests stay private in the local `runtime/` directory.

Reject unknown manifest/routing versions, unknown actions/fields, links, unsafe relative paths, invalid hashes/headers and inconsistent identities. Relative paths cannot be absolute or contain backslashes, colons, NUL, empty segments, `.` or `..`.

## Ordered routing

`routing.semantics` is exactly `meshscale-routing-v1`. Evaluate rules in order; the first matching action wins. There is no implicit file-existence, extension or directory-index routing.

1. The mandatory first `server_variants` rule forwards a request if **any** of its explicit conditions match:
   - Its method is not in `methods_except` (`GET`, `HEAD`).
   - Any named `headers_present` header exists, case-insensitively, regardless of value.
   - Any named `cookies_present` cookie exists, matching the case-sensitive name before `=` across all Cookie headers.
   - Any named `query_present` parameter exists, matching its percent-decoded name.
2. Each `static` rule matches one exact decoded path, listed methods and a query policy:
   - `ignore`: any query is permitted, for ordinary assets.
   - `empty`: no query or an empty query is permitted, for prerendered responses.
3. The mandatory final `server` rule forwards everything else to the single logical `server.target_id`.

The required bypass headers are `rsc`, `next-action`, `next-router-state-tree`, `next-router-prefetch`, `next-router-segment-prefetch`, `x-prerender-revalidate`, `x-prerender-revalidate-if-generated`, `x-next-revalidated-tags`, `x-next-revalidate-tag-token` and `x-middleware-prefetch`. Required cookie names are `__prerender_bypass` and `__next_preview_data`; required query name is `_rsc`. Consumers must not remove or reorder this bypass.

Strictly validate percent escapes and UTF-8, then decode the request path exactly once. Reject traversal segments, backslashes, colons and NUL; preserve case and trailing slashes. Manifest paths are already decoded strings, so literal `%`, `?`, `#` and Unicode in a filename require escaping in the request URL. Do not decode a manifest path again.

Static rules reference existing inventory keys, have exactly `["GET", "HEAD"]`, and claim unique paths. Missing static content falls back to the server. The `_prerender/` storage namespace has no direct URL rule. `fallback_reasons` is diagnostic, not executable routing logic.

Illustrative static rule:

```json
{
  "action": "static",
  "path": "/about",
  "object": "_prerender/app/about.html",
  "methods": ["GET", "HEAD"],
  "query": "empty"
}
```

This selects `<org>/<project>/<build>/static/_prerender/app/about.html`. `/about?user=1`, RSC, preview, prefetch and POST requests remain server-bound. `/_prerender/app/about.html` is not an alias for `/about`.

## Next.js eligibility and responses

The builder requires Next.js 16.2.0 or later: the first release with the stable Adapter API (`NEXT_ADAPTER_PATH`, `onBuildComplete` with `routing`, `outputs` and `buildId`; 16.0 and 16.1 shipped an incompatible alpha). Older versions fail the build before `next build` runs. The routing classification reads routes-manifest v3, prerender-manifest v4 and middleware-manifest v3. Unsupported versions fail the build rather than guessing.

Public files and Next static assets retain exact URLs, including supported base paths. Eligible complete, non-revalidating Pages HTML/JSON and App HTML receive exact rules. Enumerated dynamic prerenders require no runtime fallback; no dynamic regex/wildcard is emitted. Canonical trailing slashes are explicit; redirects stay on Next.

ISR, PPR/Cache Components, runtime/partial responses, metadata bodies, middleware/proxy functions, custom routing/headers/redirects, locales and custom asset prefixes conservatively remain server-owned. RSC/Flight/segment/action responses are never accelerated. Unknown or sensitive prerender headers disable acceleration. Internal cache tags and preview keys are not exposed. Non-empty base-path roots remain server-owned to avoid canonical redirect ambiguity.

Supported static status is 200. Next hashed assets use `public, max-age=31536000, immutable`; public and prerendered responses use `public, max-age=0, must-revalidate`. Prerendered objects have explicit HTML/JSON content types. Safe additional headers are limited to content-language, content-disposition and x-content-type-options.

The local reference evaluator supports HEAD, conditional ETag/date requests and ranges on both cached and streamed static responses. Dynamic responses are streamed and never placed in the static cache.

## Function runtime and platform

`runtime` starts `node runtime/function-entry.cjs` from working directory `runtime`, with no extra arguments. The entrypoint is a MeshScale function runtime: it does not call `next start` or `startServer`. It resolves the adapter-produced routing table and invokes Next.js Node entrypoints through the public `handler(req, res, ctx)` interface. `function-entry.cjs` is the IPC shell; the Next.js execution code lives in the generated `runtime/runtime.cjs` next to it, which other transports can reuse. Only protocol frames are written to the worker's stdout; application output goes to stderr. The artifact also contains `runtime/lambda-entry.cjs` and `runtime/lambda-adapter.cjs`, an AWS Lambda handler (`lambda-entry.handler`) for Function URL events that runs the same `runtime.cjs`. They are not referenced by the manifest. `package lambda` turns `runtime/` into `lambda.zip` with a separate `lambda.json` sidecar; Lambda packaging data never goes into this edge-facing manifest. The MeshScale edge owns HTTP routing; the local runner starts this worker lazily and keeps it warm for subsequent requests.

`platform` records OS, architecture, Node version and ABI. Native dependencies require a compatible host; Windows artifacts are not Linux artifacts. The local runner enforces OS/architecture and ABI. Production infrastructure should use the recorded Node version and platform.

## Publication and activation

1. Validate and snapshot the full artifact; verify static digests. Only `static/` is copied into the upload snapshot.
2. Bind organization/project/bucket and reserve `<org>/<project>/<build>/_upload.json`.
3. Upload inventory objects under `static/`, with conditional create-only PUTs.
4. Persist the finalized local manifest and upload `<org>/<project>/<build>/manifest.json` last.

The remote manifest and the local `manifest.json` have identical bytes. R2 never receives `runtime/`. Publication failure never produces a new completion manifest, overwrites objects or automatically deletes partial state. Use a fresh build ID after any partial/uncertain upload.

A completed remote manifest means static transfer succeeded. A separate control plane must obtain and deploy the server runtime from the local output, resolve the logical server target and confirm readiness before activating traffic. No server origin is embedded in the manifest, and request data cannot select one.

Local build/run needs no R2 credentials. Version-1 artifacts remain locally runnable but must be rebuilt for v2 publication. Version-2 manifests built before ZIP packaging was removed still load; their obsolete `archive` field is ignored.
