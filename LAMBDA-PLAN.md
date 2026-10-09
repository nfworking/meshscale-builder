# MeshScale Builder — AWS Lambda target: implementation plan

Audience: an AI coding agent (Claude Code) working in this repository, and the human reviewing it.
Read this whole document before starting. Work one phase at a time and stop for review after each.

## 0. How to read this document

**Verification status.** This plan was written from a read-through of the repository sources
(Rust, `.cjs` runtime files, README, DEPLOYMENT-CONTRACT, manifest schema, Cargo files, CI
workflow). Nothing was compiled and no Rust test was run. The only things actually executed
are `src/lambda_local_host.cjs` and `test/runtime/lambda-local-host.test.cjs` (Node 22, 18 tests,
all passing). It has not seen `tests/` (if present), git history, or a real Next.js build.

**Trust order.** Code you can see beats this document. This document beats AWS facts recalled
from memory. If something here contradicts the code, stop, report it, and propose a fix to the plan.

**Markers.**
- **[VERIFIED]** checked by running it.
- **[FROM SOURCE]** read directly in the code; re-check line numbers yourself.
- **[VERIFY]** a claim about AWS or Next.js taken from documentation recalled from memory. Confirm
  against current official documentation before relying on it, and fix this document if it is wrong.
- **[REPRODUCE FIRST]** a suspected bug. Write a failing test before changing anything. If you cannot
  reproduce it, drop the item and say so.

## 1. Goal and non-goals

Goal: produce, from the existing build pipeline, a deployable AWS Lambda ZIP (Node.js, ZIP
deployment, no containers) whose handler runs the same Next.js execution code as the local runner,
and make that handler testable locally with `meshscale-builder run --lambda-local` and without any
edge.

Non-goals for this plan: the MeshScale edge, Cloudflare/R2 changes beyond keeping the current
upload working, Lambda layers, response streaming, ISR/PPR/middleware support, multi-region
copying. Phase 6 and 7 describe what the orchestration and ingress work must satisfy, but that work
is not done in this repository unless the owner says so.

## 2. Ground truth: what the repo does today [FROM SOURCE]

Pipeline (`build.rs`, `artifact.rs`, `static_output.rs`):

1. Clone the repo, check out the exact commit, resolve `--dir`, detect Next.js and the package manager.
2. Install dependencies, then install `@next/routing@~<next major.minor>.0` (no save, no lockfile, no scripts).
3. Run the project's `build` script with `CI=true` and `NEXT_ADAPTER_PATH` pointing at a staged
   `.meshscale-next-adapter.cjs` (`next_adapter.cjs`). The adapter writes `.next/meshscale-adapter.json`
   (version 1: build id, `basePath`/`i18n`, `routing`, outputs: pages, pagesApi, appPages, appRoutes,
   staticFiles, prerenders) and **throws if the build has middleware**.
4. `create_output`: merge every `.next/**/*.nft.json` (skipping `.next/standalone`), trace the
   generated entrypoint with Next's bundled NFT, add adapter output files and assets, materialize pnpm
   package dependency context, copy into `runtime/`, copy `public/` and `.next/static` into `static/`,
   write `runtime/function-entry.cjs` (the `include_str!` of `src/function_entry.cjs`) and `manifest.json`.
5. `static_output::generate` derives exact static routes and prerender HTML objects, records platform
   (`std::env::consts::OS`/`ARCH` plus Node version and ABI from `node -e`), and writes the routing contract.
6. `upload` (run by default by `build`) publishes `static/` and then `manifest.json` to R2, create-only.

Runtime (`function_entry.cjs`, `runner.rs`):

- `function_entry.cjs` loads `.next/meshscale-adapter.json`, calls `@next/routing`'s `resolveRoutes`,
  picks an adapter output (`findOutput`), loads its handler (`loadHandler`, cached in a `Map`), builds a
  Node-like request (`makeRequest`) and response (`FunctionResponse`), and calls `handler(req, res, ctx)`.
  It also owns the framed stdin/stdout protocol (Appendix B).
- `runner.rs` owns HTTP: host routing, `routing.select` for static rules, static file serving, and
  lazy start of one warm Node worker per project (`Worker::start`: `node <entrypoint>` with cwd
  `runtime/`, `NODE_ENV=production`, stdin/stdout piped, stderr inherited, credentials removed).

Manifest/validation facts that constrain this work:

- `manifest.schema.json` and `Manifest::validate_v2` hard-code `runtime.entrypoint == "runtime/function-entry.cjs"`,
  `runtime.command == "node"`, `runtime.args == []`, `runtime.working_directory == "runtime"`.
- `runner.rs` ensures the manifest entrypoint is `runtime/function-entry.cjs`.
- `upload.rs::snapshot` rejects any top-level artifact entry other than `runtime`, `static`, `manifest.json`.
- `artifact.rs::collect_entrypoint_trace` writes the entry source to a temp file **inside the project
  directory** (so Node resolution can walk up to the project's `node_modules`) and traces only that file.

## 3. Known issues found in review

These exist today. Items marked "plan" are fixed by the phase named. The rest are recorded for the owner.

| ID | Issue | Evidence | Action |
|---|---|---|---|
| K1 | `Cargo.lock` is stale. `Cargo.toml` lists `indicatif` and `tracing-appender` (used by `cli.rs` and `main.rs`) but the lock has no entries for them or their dependencies. `cargo build --locked` will fail. | compare `[dependencies]` with the `meshscale-builder` package entry in `Cargo.lock` | Phase 0: run `cargo update -w` (or `cargo build`), commit the lock, verify `cargo build --locked`. |
| K2 | **[REPRODUCE FIRST]** Default `build` (upload enabled) probably fails at upload. `main.rs` creates `output/build.log`; `upload.rs::snapshot` walks the whole output directory and bails on any top-level entry other than `runtime`/`static`/`manifest.json`. No test fixture contains a `build.log`. | `snapshot()`'s `matches!(name.split('/').next(), Some("runtime" \| "static" \| "manifest.json"))` | Phase 0: test, then allow `build.log` and `lambda` in the snapshot's allowlist (they are never copied into the snapshot because only `static/` is). |
| K3 | The IPC shell shares stdout with application code. Any `console.log` or direct `process.stdout.write` from a user's route handler is interleaved with binary frames and breaks the runner's frame parser. `function_entry.cjs` only uses `console.error` itself. | frames are written with `process.stdout.write`; no redirection exists | Phase 1: capture the real `stdout.write` for frames, redirect everything else to stderr (same technique as `lambda_local_host.cjs`, which has a test). |
| K4 | Duplicate request headers are joined with `", "`, including `cookie`. Multiple `Cookie` headers (HTTP/2 splits cookies) must be joined with `"; "`. | `makeRequest` in `function_entry.cjs` | Phase 1: join `cookie` with `"; "` in the shared request builder; test it. |
| K5 | `waitUntil` tasks are fire-and-forget. Fine for a long-lived worker, wrong for Lambda, which freezes the environment after the handler returns. | `ctx.waitUntil` in `handleRequest` | Phase 1: track promises in `runtime.cjs`, expose `settle()`; Phase 2: Lambda awaits it. IPC does not await it. |
| K6 | **[VERIFY]** `resolveRoutes` results other than `redirect` and `resolvedPathname` are ignored (for example resolved headers, status, rewritten query, external rewrites). The handler receives the original `req.url`. Custom `headers()` / rewrites may therefore not behave as in Next. Check the installed `@next/routing` types for the real result shape. | `handleRequest` | Do **not** fix during the refactor. Write characterization tests (Phase 1) and report findings. Note `static_output.rs` already marks projects with custom headers/rewrites/redirects as server-owned. |
| K7 | `README.md` documents `--static-cache-mib`, `--static-cache-max-file-mib` and a lazy in-memory static cache, and `cache.rs`/`stats.rs` exist, but `runner.rs` as provided neither imports them nor defines those flags; `RunArgs` only has `output`, `--project`, `--port`, `--idle-timeout-secs`. `dispatch` reads static files with `tokio::fs::read` on every request. | `runner.rs` imports `crate::{artifact, manifest}` only | Confirm which is the real working tree before Phase 3. Do not wire the cache in this plan. |
| K8 | `DEPLOYMENT-CONTRACT.md` says Next majors 14-16 are understood, but the build requires the adapter hook (`NEXT_ADAPTER_PATH`) and fails if `meshscale-adapter.json` is missing. **[VERIFY]** which Next versions support that hook. The effective minimum is probably higher than 14. | `artifact.rs::create_output` bails without the adapter metadata | Phase 4: find the real minimum, record it, fail early with a clear message if the installed Next is older. |
| K9 | CI (`.github/workflows/arch-gpt-check.yml`) only runs on pushes to `arch/gpt` and only syntax-checks `src/function_entry.cjs`. | workflow file | Phase 1: check every `.cjs`, run the Node tests; ask the owner about triggers. |
| K10 | Request header `transfer-encoding: chunked` is passed through to the worker even though the runner buffers the body. | `runner.rs::invoke` copies all headers | Record. The Lambda path drops it (the shim and adapter both do). |
| K11 | Child processes inherit the builder's full environment, minus a denylist (`remove_credentials`). The denylist covers `MESHSCALE_GITHUB_TOKEN` and the R2 names only. Anything else in the environment (`AWS_*`, cloud tokens) reaches customer install scripts. There is also no way to pass project build-time variables (for example `NEXT_PUBLIC_*`) except by putting them in the builder's own environment. | `build.rs::run_command`, `run_build`; `upload.rs::remove_credentials` | Phase 0. |

## 4. Design decisions

**D1 — Add files, do not rename.** `function-entry.cjs` stays as the IPC shell. New generated runtime files:

| Source (`src/`) | Generated (`runtime/`) | Role |
|---|---|---|
| `runtime.cjs` | `runtime.cjs` | Next execution engine. Transport-agnostic. |
| `function_entry.cjs` | `function-entry.cjs` | Existing IPC shell, now thin. Used by `run`. |
| `lambda_adapter.cjs` | `lambda-adapter.cjs` | Pure conversion + handler factory. No Next, no AWS SDK. Unit-testable. |
| `lambda_entry.cjs` | `lambda-entry.cjs` | The Lambda handler export. A few lines. |
| `lambda_local_host.cjs` | **not generated** | Dev-only shim for `run --lambda-local`. Embedded in the binary only. |

**D2 — The runtime API is transport-neutral and stream-shaped** (section 6.1), so buffered Lambda,
IPC streaming and later Lambda response streaming all sit on the same function.

**D3 — One package step, outside `build`.** A new `package lambda <output>` subcommand turns an existing
validated output into `lambda.zip` plus a sidecar `lambda.json`. `build` is unchanged apart from writing
the extra runtime files. This mirrors how `upload` works on an existing output.

**D4 — No new fields in the edge-facing `manifest.json`.** The manifest is published to R2 byte for byte
and consumed by the edge. Lambda package data (zip checksum, sizes, runtime id) goes in the sidecar
`lambda.json`. Revisit only if the owner wants a manifest-level contract.

**D5 — AWS control-plane calls stay out of Rust.** The builder produces and validates artifacts. A separate
orchestrator (TypeScript, Trigger.dev per the owner's design) creates/updates functions, publishes
versions, health-checks and moves aliases. The builder already hand-rolls SigV4 for R2; adding Lambda,
IAM and alias management by hand would be a mistake. The one AWS-facing Rust feature in this plan is the
optional `--lambda-endpoint` testing mode (Phase 5).

**D6 — The local Lambda host builds events independently of the code under test.** `lambda_local_host.cjs`
does not import `lambda-adapter.cjs`. If both sides shared conversion code, a bug there would pass
every local test and fail on AWS.

## 5. Phases

Each phase ends with the checks in `CLAUDE.md` ("Verification") and a stop for review.

### Phase 0 — Baseline and hardening (no change to artifact contents)

0.1 Run `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all-targets`,
`node --check` on each `.cjs`. Record pre-existing failures in the PR description. Do not fix them unless listed here.

0.2 **K1**: regenerate and commit `Cargo.lock`; verify `cargo build --locked`.

0.3 **K2**: add a test in `upload.rs` using the existing `fixture()` plus a top-level `build.log`; confirm it
fails; then change `snapshot()` so `build.log` and `lambda` are accepted (and not copied). Keep rejecting any
other unexpected top-level entry.

0.4 **K11**: replace the build-time denylist with an allowlist. Add one helper (suggested home:
`upload.rs` next to `remove_credentials`, or a new `env.rs`):
`fn restrict_build_env(command: &mut Command, project_vars: &[(String, String)])` that calls `env_clear()`
and then sets only:
- `PATH`, `HOME`, `LANG`, `LC_*`, `TZ`, `TMPDIR`/`TEMP`/`TMP`, `CI`
- proxy and CA settings: `HTTP_PROXY`, `HTTPS_PROXY`, `NO_PROXY` (either case), `SSL_CERT_FILE`, `NODE_EXTRA_CA_CERTS`
- Windows: `SystemRoot`, `SYSTEMDRIVE`, `COMSPEC`, `PATHEXT`, `USERPROFILE`, `APPDATA`, `LOCALAPPDATA`, `HOMEDRIVE`, `HOMEPATH`, `ProgramFiles`, `ProgramFiles(x86)`
- the explicit `project_vars`.

Apply it to every child that runs customer code or tooling: `build.rs::run_command` and `run_build`,
`artifact.rs::collect_entrypoint_trace`, and the `node -e` inspection in `static_output::generate`. Do **not**
apply it to `runner.rs::Worker::start`: local runs should inherit the developer's environment (they may
need their own AWS credentials), so the runner keeps only the existing denylist.
Add a `--build-env-file <path>` option on `build` (dotenv syntax, same parser rules as `load_dotenv`:
parse errors must not echo values) feeding `project_vars`. Values must never be logged. Tests: a child
started through the helper does not see a planted `AWS_SECRET_ACCESS_KEY`, `MESHSCALE_R2_SECRET_ACCESS_KEY`
or `MESHSCALE_GITHUB_TOKEN`, and does see an explicit project var and `PATH`.
Mention in the PR that npm may need `npm_config_*`/registry tokens for private registries: these must come
through `--build-env-file`, not through inheritance.

0.5 Operational requirement (document in README, no code): hosts running builds must block access to the
instance metadata service (169.254.169.254, `fd00:ec2::254`) and container credential endpoints from the
build process, and must not hold deploy credentials. Install scripts are arbitrary code.

Acceptance: the baseline is recorded; `cargo build --locked` works; K2 test passes; env tests pass.

### Phase 1 — Split the runtime (behavior-preserving, plus K3/K4/K5)

1.1 **Characterization first.** Before touching `function_entry.cjs`, pin current behavior. Find the
fixture app used by the existing opt-in integration tests; if none can be reused, create
`test/fixtures/next-app/` (Next 16, App Router and Pages Router, minimal dependencies) with these routes:

| Route | Purpose |
|---|---|
| `/` | static prerender (must never reach the function) |
| `/ssr` | App Router, `dynamic = 'force-dynamic'`, echoes headers and cookies |
| `/dynamic/[id]` | App Router dynamic segment |
| `/pages-ssr` | Pages Router `getServerSideProps` |
| `/api/echo` | Pages API: echoes method, query, headers, body; sets two cookies |
| `/route-handler` | App route handler GET/POST; returns length and SHA-256 of the request body |
| `/redirect`, `/missing`, `/error` | 307, 404, 500 |
| `/log` | calls `console.log` inside a handler |
| `/after` | schedules `after()` work that writes a marker (read back by another route) |
| `/env` | prints a named environment variable |
| `/action` | form with a Server Action |
| `/big` | response above 7 MB |
| `/stream` | Suspense page with delayed chunks |

Record request/response pairs through the **unchanged** local runner into golden files
(`test/golden/*.json`: status, headers minus volatile ones, body hash). These are the baseline for 1.5.

1.2 Create `src/runtime.cjs` (spec in 6.1) by moving the Next-specific logic out of `function_entry.cjs`:
adapter metadata loading, `resolveRoutes`, `findOutput`, `loadHandler` with its cache, the Node-like request
and response objects, and the not-found/redirect/500 handling. Do not change routing semantics (K6).

1.3 Reduce `function_entry.cjs` to the IPC shell: frame parsing, `runtime.invoke(...)`, frame writing. Fix K3:
```js
const rawStdoutWrite = process.stdout.write.bind(process.stdout);
process.stdout.write = (chunk, encoding, callback) =>
  process.stderr.write(chunk, encoding, callback);
// ... frames are written with rawStdoutWrite(frame, cb); console.log/info/debug -> console.error
```
Fix K4 in the shared request builder (`cookie` joins with `"; "`). Implement K5 tracking in `runtime.cjs`
(`settle()`), not awaited by the shell.

1.4 `artifact.rs` changes:
- Write the new files with `fs::write(runtime_dir.join(...), include_str!(...))` next to the existing write.
  Extend the collision check (`all_traced_files.contains_key("function-entry.cjs")`) to every generated name.
- Replace the single temp-file trace with a **staging directory inside `project_dir`**
  (`tempfile::Builder::new().prefix(".meshscale-runtime-").tempdir_in(project_dir)`), containing
  `runtime.cjs`, `function-entry.cjs`, `lambda-adapter.cjs`, `lambda-entry.cjs`. Trace
  **both entrypoints** (`function-entry.cjs` and `lambda-entry.cjs`) in one NFT call: change the Node script
  from `nodeFileTrace([process.argv[1]], ...)` to `nodeFileTrace(process.argv.slice(1), ...)`, pass the staged
  file names, and skip every traced path that lies under the staging directory (they are written separately).
  Why this matters: if `function-entry.cjs` does `require('./runtime.cjs')` and only one file is staged, NFT
  cannot resolve it, only warns, and silently drops `@next/routing`'s dependency tree from the artifact.
- Update the fake NFT module in `artifact.rs` tests: it returns only its first argument today
  (`async ([file]) => ...`); make it return all arguments.
- Add a test asserting the generated `runtime/` contains the four files and that a deliberately
  broken relative require in a staged file makes the build fail loudly (treat an unresolvable relative
  import from our own files as an error, not a warning).

1.5 Tests and CI:
- Re-run the golden requests against the refactored runner. The diff must be empty except the intentional
  changes (K3: `/log` now works; K4: cookie join). List each difference in the PR.
- Add `test/runtime/runtime.test.cjs` unit tests for `runtime.cjs` using a stub adapter/handler (no Next).
- CI: `node --check` every `src/*.cjs`; `node --test test/runtime/*.test.cjs`. (Pass a glob: Node 22 treats a
  bare directory argument as a module path [VERIFIED].) Ask the owner whether to trigger on PRs and `main`.

Acceptance: all golden diffs explained; `/log` through the IPC runner works; existing Rust tests pass;
artifact contains the four files and the traced dependencies of both entrypoints.

### Phase 2 — Lambda adapter and entrypoint (pure Node, no AWS)

2.1 `src/lambda_adapter.cjs` (spec 6.2) and `src/lambda_entry.cjs`:
```js
'use strict';
const { createHandler } = require('./lambda-adapter.cjs');
const { createRuntime } = require('./runtime.cjs');
exports.handler = createHandler({ start: () => createRuntime({ root: __dirname }) });
```
Remember the generated names use hyphens; the `require` paths above are the **generated** names.

2.2 Tests (`test/runtime/lambda-adapter.test.cjs`) with a fake runtime injected through `createHandler`:
every row of the conversion tables in 6.2, plus: init failure is thrown, then a later invocation retries;
`settle()` is awaited before returning; oversize responses; HEAD/204/304; binary round trip; probe header.
Event fixtures live in `test/fixtures/events/*.json`. Until a real Function URL event is captured from AWS,
fixtures are hand-written from the documented v2 shape (6.2/Appendix A); mark each with a `"_source"`
field and replace them in Phase 5.

2.3 Wire into `artifact.rs` (already done in 1.4) and add a Node-level smoke test that requires the
**generated** `runtime/lambda-entry.cjs` from a fixture artifact and invokes it with a synthetic event.

Acceptance: adapter tests pass; the generated `lambda-entry.cjs` loads from a built artifact without the
source checkout and answers a synthetic event.

### Phase 3 — `run --lambda-local` (the local Lambda front)

See section 7 for design, Rust wiring and limits. Deliverables: `src/lambda_local_host.cjs` and
`test/runtime/lambda-local-host.test.cjs` (already written and passing), the `--lambda-local` flag, a
Rust test for argument parsing, and a manual curl checklist run against the fixture app.

Acceptance: with the fixture app built, `run --lambda-local` serves every route in the matrix (section 8);
static files still come from the Rust runner; dynamic requests demonstrably pass through `lambda-entry.cjs`.

### Phase 4 — `package lambda`

CLI: `meshscale-builder package lambda <OUTPUT> --arch <x86_64|arm64> --node-runtime <nodejs24.x> [--out <dir>]`
(default `--out <OUTPUT>/lambda`). Add `Commands::Package` with a `lambda` subcommand in `main.rs`; put the
logic in a new `src/lambda.rs`. **[VERIFY]** supported runtime identifiers (`nodejs20.x`, `nodejs22.x`,
`nodejs24.x`) in current AWS documentation and put the list in one constant.

Checks, in order, each with a specific error:
1. Output validates (`artifact::validate_output`), manifest loads as v2.
2. `runtime/lambda-entry.cjs`, `lambda-adapter.cjs`, `runtime.cjs` exist; handler string is `lambda-entry.handler`.
3. `manifest.platform.os == "linux"`. `platform.arch` maps to the Lambda architecture:
   `x86_64 -> x86_64`, `aarch64 -> arm64`; must equal `--arch`. Node major in `platform.node_version`
   must equal the major of `--node-runtime`. (Same major implies same ABI.) Cross-building native
   packages is not supported: build on the target architecture, on **glibc** Linux (Lambda's Node runtimes
   run on Amazon Linux 2023 [VERIFY]; musl/Alpine builds fetch the wrong native binaries).
4. No path in the tree is a symlink, contains `..`, a backslash, or a `.env`/`.env.*` component.
5. Minimum supported Next version (K8).

Zip: root is the contents of `runtime/`, **excluding** `function-entry.cjs` (the IPC shell is not needed).
Determinism requirements: entries sorted bytewise by path; fixed modification time (`1980-01-01 00:00:00`,
the DOS epoch); explicit Unix modes (`0o644` files, `0o755` directories; Windows-built zips otherwise lack
them and Lambda can fail with permission errors [VERIFY]); one fixed compression method and level; no extra
timestamp fields; directory entries only for empty directories. Add the `zip` crate with default features
off and only deflate enabled; check its current API for options types. Acceptance test: zip the same
output twice, and a copy with different file mtimes, and compare SHA-256: identical. Note that
byte-for-byte reproducibility holds for one builder version/toolchain; record the builder version in the sidecar.

After writing, re-open the zip and compare its entry list and sizes with the source tree; reject names that
are absolute, contain `..` or backslashes.

Sizes (put limits in one constants block, **[VERIFY]** each): unzipped total <= 262,144,000 bytes
(250 MB, includes layers); a zip above 52,428,800 bytes (50 MB) must go through S3 rather than a direct upload.
Fail hard above the unzipped limit with the report below; never drop files silently.

`lambda.json` (sidecar, schema version 1): `build_id`, `commit`, `handler`, `runtime`, `architecture`,
`node_major`, `zip_sha256`, `zip_bytes`, `uncompressed_bytes`, `file_count`, `builder_version`, and a
size report: the 20 largest files and the 20 largest top-level packages under `node_modules`. Run this report
on a real app **before** deciding what to strip. Whether `.next/static` and `.map` files end up in
`runtime/` is an open question [FROM SOURCE is ambiguous: README says retained when traced]; let the
report answer it, then decide whether a Lambda-specific prune is worthwhile.

Emulator test (Linux, optional in CI): unzip into a temp dir and run the AWS Lambda Runtime Interface
Emulator, see section 8, layer 4.

Acceptance: deterministic zip; all checks have failing-case tests; the size report runs on the fixture app;
`lambda-entry.handler` answers a synthetic event from the unzipped package in an empty directory.

### Phase 5 — Real Lambda, no edge (manual, sandbox AWS account)

5.1 Create a throwaway function by hand with the Phase 4 zip. Run the matrix (section 8) with direct
`Invoke`. Replace hand-written event fixtures with events captured from a real Function URL (log
`JSON.stringify(event)` once from a temporary debug handler; redact nothing sensitive into the repo).
5.2 Add a Function URL on an **alias** with `AuthType AWS_IAM` and test over HTTP with signed requests
(section 8, layer 5). Never leave an `AuthType NONE` URL on a function containing real code or secrets.
5.3 Measure `Init Duration`, duration and max memory at several memory sizes.
5.4 Optional, recommended: `--lambda-endpoint` for `run` (section 7.4), so a browser can use a real Function URL
with static files served locally.

Acceptance: every matrix row passes on real Lambda or is recorded as unsupported with a reason.

### Phase 6 — Orchestration (outside this repository; requirements only)

The orchestrator (TypeScript) consumes `lambda.zip` and `lambda.json`. It must:
- Serialize deployments per project and region (a concurrency key). All deployments share one `$LATEST`.
- Upload the zip to an S3 bucket in the function's region, keyed by the zip's SHA-256 (skip if present).
- Create the function if missing; otherwise `UpdateFunctionCode`; **wait until the update completes**
  (`LastUpdateStatus` successful) before publishing.
- `PublishVersion` guarded with the expected `CodeSha256` and the function's `RevisionId`.
- Health-check the **version** with a direct `Invoke` using a synthetic Function URL v2 event: first the probe
  (header `x-meshscale-probe: init`, proves the runtime initializes), then at least one application route.
  Treat `FunctionError` as a failed deployment; treat an application 5xx as an application error, not a deployment failure,
  unless the check targets that route. Function URLs attach to aliases (or `$LATEST`), not to version numbers [VERIFY],
  so version checks use direct invoke or a temporary candidate alias.
- Move the production alias only after the checks pass. Roll back by moving the alias to an earlier verified version.
- Make every step idempotent. A retried Lambda deployment must not re-upload to R2: R2 publication is create-only,
  so a repeated upload of the same build id fails by design.
- Prune old versions (regional code storage is a quota) and apply reserved concurrency per function [VERIFY quotas].
- Runtime configuration: Lambda environment variables total 4 KB [VERIFY]. Larger configuration or secrets need
  Secrets Manager/SSM read during init; design that before promising large environments. Environment values are
  fixed per published version; changing one means a new version.
- Run customer functions in a **separate AWS account** from the control plane, each with its own minimal execution
  role and no access to artifact buckets.

### Phase 7 — Ingress and production (outside this repository; requirements only)

- Function URLs do not support mutual TLS. Options: `AWS_IAM` Function URL with SigV4 from the edge (edge
  identity can come from IAM Roles Anywhere, which exchanges an X.509 certificate for temporary credentials
  [VERIFY constraints]); direct `Invoke` (no public endpoint, but the edge must build Lambda events); or an API
  Gateway/ALB front with mTLS (extra component, different payload limits).
- A shared edge role cannot be limited to one project per request. The real controls: the target comes from a
  validated manifest or route table and never from request data; each customer function has its own role; customer
  functions live in a separate account.
- The edge must forward the original host in `x-forwarded-host`; the Lambda adapter reads it (6.2). Under a public
  (`NONE`) URL a client could forge it, so production requires an authenticated origin.
- Reject oversized request bodies at the edge with 413; map Lambda throttling (429) to 503 with retry guidance.

## 6. Specifications

### 6.1 `runtime.cjs`

```js
const runtime = await createRuntime({ root: __dirname });
const response = await runtime.invoke(request);   // MeshScaleResponse
await runtime.settle(timeoutMs);                  // wait for waitUntil work
```

`MeshScaleRequest`:
- `method: string`
- `url: string` — origin-form path plus optional query, exactly as received and still percent-encoded
  (for example `"/a%20b?x=1&x=2"`). Never decode or rebuild it from parts.
- `headers: Array<[string, string]>` — names as received (any case), duplicates preserved, order preserved
- `body: Buffer` — bytes (empty `Buffer` if none)

`MeshScaleResponse`:
- `status: number`
- `headers: Array<[string, string]>` — lowercase names, one pair per value, `set-cookie` never merged
- `body: AsyncIterable<Buffer>` — the response body as produced
- `done: Promise<void>` — resolves when the body is complete; rejects if production fails after the head was produced

Behavior:
- `invoke` resolves as soon as the response head is known (the handler called `writeHead`, `flushHeaders`, wrote, or ended).
- A failure before the head produces a controlled `500` response with a short fixed body (as today). A failure after the
  head surfaces as an error on `body` and a rejection of `done`.
- Not found, redirects from `resolveRoutes`, and handler loading are inside the runtime exactly as today.
- Loaded handlers are cached by output id for the life of the runtime; adapter metadata is parsed once.
- `settle(timeoutMs)` resolves when every promise registered through `ctx.waitUntil` has settled or the timeout
  passes, returning `{ pending, timedOut }`. A rejected task is logged (no request data) and does not reject `settle`.
- Forbidden inside `runtime.cjs`: reading stdin, writing stdout, `process.exit`, mention of Lambda or frames.
- The Node request object keeps today's semantics: lowercase header map, `set-cookie` as an array, `rawHeaders`,
  `httpVersion 1.1`, `socket` stub; `cookie` duplicates join with `"; "` (K4), other duplicates with `", "`.

### 6.2 `lambda_adapter.cjs`

Exports `createHandler`, `fromLambdaEvent`, `toLambdaResponse`.

`fromLambdaEvent(event)` -> `MeshScaleRequest`:

| Field | Rule |
|---|---|
| validity | `event.version === "2.0"` and `event.requestContext.http` present, else throw `InvalidEventError` |
| `method` | `event.requestContext.http.method` |
| `url` | `event.rawPath` (fallback `"/"`) + (`"?" + event.rawQueryString` if non-empty). Never use `queryStringParameters` (it collapses repeated keys). Never decode. |
| `headers` | `Object.entries(event.headers)` as given. v2 lowercases names and joins duplicate values with commas; do not try to split them. |
| cookies | if `event.cookies` is a non-empty array, append `["cookie", cookies.join("; ")]`. Payload v2 removes the cookie header from `headers`. |
| host | if `x-forwarded-host` is present (first value before any comma), replace `host` with it. Lambda's own host is not the application's host. Keep `x-forwarded-host` too. |
| `body` | absent or empty -> empty `Buffer`; `isBase64Encoded` -> `Buffer.from(body, "base64")`; else `Buffer.from(body, "utf8")` |

`toLambdaResponse(response, { method, maxBytes })` -> `{ statusCode, headers, cookies?, body, isBase64Encoded }`:

| Rule | Detail |
|---|---|
| cookies | every `set-cookie` pair goes into `cookies[]` and is removed from `headers`. Never comma-join. |
| headers | other names lowercased; repeated names joined with `", "`; drop `connection`, `keep-alive`, `transfer-encoding`, `upgrade`, `te`, `trailer`; drop `content-length` except for `HEAD`. [VERIFY with a real Function URL whether Lambda recomputes it] |
| body | collect the stream while counting bytes; stop and fail at `maxBytes` |
| encoding | `utf8` string only if the content type is textual (`text/*`, JSON, XML, JavaScript, form-urlencoded), there is no `content-encoding`, and the bytes round-trip through UTF-8; otherwise base64 with `isBase64Encoded: true` |
| bodyless | `HEAD`, `204`, `205`, `304` -> empty body |
| size | if the encoded body exceeds about 4 MiB, compute the final JSON size and fail above the Lambda limit (6,291,556 bytes [VERIFY]) with a controlled error |

`createHandler({ start, backgroundTimeoutMs = 5000 })`:
- Calls `start()` once when the module loads (so initialization runs in Lambda's init phase), keeps the promise,
  and attaches a no-op `.catch` to avoid an unhandled rejection crash.
- If the promise rejects, **throw** from the next invocation (an invocation error is a clear failure signal and
  appears in Lambda error metrics) after resetting the cached promise so the following invocation retries `start()`.
- Probe: a request with header `x-meshscale-probe: init` awaits the runtime and returns `200` with a small JSON body
  without invoking Next.
- Normal flow: `fromLambdaEvent` -> `runtime.invoke` -> `toLambdaResponse` -> `await runtime.settle(backgroundTimeoutMs)`
  -> return. If `done` rejects or the body exceeds the limit, return a controlled `502` and log the cause without
  request headers, cookies or bodies.
- Application 5xx responses are returned as responses, never thrown.
- Logging: structured single-line JSON to stdout is fine in real Lambda. (Only the IPC shell must avoid stdout.)

### 6.3 What a Function URL event and response look like (payload format 2.0)

See Appendix A. Treat a captured real event as authoritative (Phase 5).

## 7. The local Lambda front: `run --lambda-local`

### 7.1 Idea

The Rust runner already plays the edge: host routing, `routing.select`, static file serving, and forwarding
dynamic requests to a worker. In `--lambda-local` mode the worker is not `function-entry.cjs` but
`lambda_local_host.cjs`, which converts each framed HTTP request into a Function URL event, calls the artifact's real
`lambda-entry.cjs` `handler(event, context)`, and converts the Lambda result back into frames.

```
browser/curl
   -> Rust runner (host routing, routing.select, static files)      <- plays the edge
        static -> served from output/static
        dynamic -> framed request over stdin
             -> lambda_local_host.cjs                               <- plays Lambda's HTTP front
                  event (payload v2) -> lambda-entry.cjs handler    <- the code under test
                  Lambda result -> framed response over stdout
```

No change is needed to `Worker`, `FunctionManager`, the frame protocol or the static serving path.
Only the worker's entrypoint differs. The shim runs with cwd `runtime/` like the real worker.

### 7.2 What the shim does [VERIFIED by `test/runtime/lambda-local-host.test.cjs`]

- Builds a v2 event: `version`, `routeKey`, raw (still encoded) `rawPath`, `rawQueryString`, `headers` (lowercase,
  duplicates comma-joined, hop-by-hop dropped), `cookies[]` (cookie header split and removed from `headers`),
  `requestContext` (including `http.method`, `http.path`, `domainName`, `requestId`, times), `isBase64Encoded`, `body`.
- Rewrites `host` to a fake `*.lambda-url.<region>.on.aws` and passes the browser's host in `x-forwarded-host`;
  sets `x-forwarded-proto: https`, `x-forwarded-port: 443`, `x-forwarded-for`.
- Text request bodies stay strings; others (or invalid UTF-8) become base64 with `isBase64Encoded: true`.
- Calls the handler with a context object (`awsRequestId`, `getRemainingTimeInMillis`, ...). Sets Lambda-style
  environment variables if unset (`AWS_LAMBDA_FUNCTION_NAME`, `AWS_REGION`, `AWS_EXECUTION_ENV`, `LAMBDA_TASK_ROOT`, ...),
  never credentials.
- Serializes invocations by default (one at a time, like one Lambda execution environment); configurable.
- Enforces a handler timeout (default 30 s) and the payload limit on both request (413) and response (502).
- Requires the explicit result shape (`statusCode` integer, string headers, `cookies` array of strings). Real Lambda
  would infer a 200 JSON response from some malformed returns; the shim deliberately turns them into 502 so
  mistakes show up locally.
- Converts the result back: `cookies[]` -> separate `set-cookie` frames headers, base64 decode, drops hop-by-hop
  headers and (except HEAD) `content-length`, drops bodies for HEAD/204/205/304.
- Redirects `console.log/info/debug` and raw stdout writes to stderr so application logging cannot corrupt the frame stream.
- Fails startup (before the ready frame) if `lambda-entry.cjs` throws on load or does not export `handler`.

### 7.3 Rust wiring (small)

`runner.rs`:

```rust
const LAMBDA_LOCAL_HOST: &str = include_str!("lambda_local_host.cjs");

#[derive(Args, Debug)]
pub struct RunArgs {
    // ... existing fields ...
    /// Send dynamic requests through the artifact's lambda-entry.cjs using a local
    /// Lambda Function URL emulation instead of function-entry.cjs.
    #[arg(long)]
    pub lambda_local: bool,
}

fn stage_lambda_host() -> Result<tempfile::TempPath> {
    use std::io::Write;
    let mut file = tempfile::Builder::new()
        .prefix("meshscale-lambda-local-")
        .suffix(".cjs")
        .tempfile()
        .context("failed to stage the local Lambda host")?;
    file.write_all(LAMBDA_LOCAL_HOST.as_bytes())?;
    Ok(file.into_temp_path()) // closes the handle; the file is deleted when dropped
}
```

In `run_async`, before `load_projects`:

```rust
let lambda_host = args.lambda_local.then(stage_lambda_host).transpose()?;
let projects = load_projects(&args, lambda_host.as_deref())?;
// keep `lambda_host` alive until the function returns so the file exists for the whole run
if args.lambda_local {
    info!("LAMBDA LOCAL MODE: dynamic requests go through runtime/lambda-entry.cjs; this is an emulation, not AWS");
}
```

In `load_projects(args, lambda_host: Option<&Path>)`, after `cwd` and `entrypoint` are computed (keep the existing
`manifest.runtime.entrypoint == "runtime/function-entry.cjs"` check):

```rust
let entrypoint = match lambda_host {
    Some(host) => {
        ensure!(
            cwd.join("lambda-entry.cjs").is_file(),
            "--lambda-local needs runtime/lambda-entry.cjs in {}; rebuild with a Lambda-capable builder",
            output.display()
        );
        host.to_path_buf()
    }
    None => entrypoint,
};
```

Tests: extend `parses_run_defaults_and_projects` for the new default (`false`) and add a parse test for
`--lambda-local`; add `stage_lambda_host` test (file exists, deleted on drop). The Node tests already cover the shim.
Update README (a "Local Lambda mode" section and the limits in 7.5).

Useful flags while testing: `--idle-timeout-secs 5` makes the runner stop the worker after 5 idle seconds,
so the next request starts a fresh Node process (a local stand-in for a cold start; `Init` cost shows up in the
`[lambda-local] handler module loaded in N ms` stderr line). Runtime environment variables for the app are just
the runner's environment: `MESHSCALE_TEST_VALUE=x meshscale-builder run ... --lambda-local`.

### 7.4 Optional: `--lambda-endpoint` (Phase 5.4)

A second mode where dynamic requests go to a **real** Function URL while static files are still served locally,
so a browser can exercise real Lambda. Mutually exclusive with `--lambda-local`.

- Flags: `--lambda-endpoint <URL>`, `--lambda-region <REGION>`. Credentials from `AWS_ACCESS_KEY_ID`,
  `AWS_SECRET_ACCESS_KEY`, optional `AWS_SESSION_TOKEN` in the runner's own environment (never passed to workers).
- In `dispatch`, for requests that `routing.select` leaves server-bound: buffer the body (reject above the Lambda
  limit with 413), build `endpoint + path_and_query`, copy headers except hop-by-hop, `host` and `content-length`,
  add `x-forwarded-host` = the incoming host, sign with `aws-sigv4` (service name `lambda`, the given region),
  send with `reqwest`, stream the status, headers and body back.
- Signing differs from the R2 uploader: for requests with a body, IAM-authenticated Function URLs expect the real
  SHA-256 payload hash in `x-amz-content-sha256` rather than `UNSIGNED-PAYLOAD` [VERIFY]. Reuse the shape of
  `Uploader::request_at` in `upload.rs`, but with `SignableBody::Bytes` and service `lambda`.
- Because the real Function URL builds the event, none of the shim is involved.
- Required IAM permission for the calling principal: `lambda:InvokeFunctionUrl`; AWS also introduced a requirement
  for `lambda:InvokeFunction` on newly created URLs [VERIFY current rules].

### 7.5 What local mode does not prove

| Not emulated | Consequence |
|---|---|
| Freeze/thaw between invocations | `waitUntil`/`after()` bugs are invisible. Test with the unit test that asserts completion before return, and on real Lambda. |
| Read-only code directory, `/tmp` semantics | Use the emulator layer (section 8, layer 4) or real Lambda. |
| Memory and CPU limits, cold-start timing | Measure on real Lambda only. |
| IAM auth, Function URL error pages | Real error statuses/bodies differ (the shim returns its own 502/413 with `x-meshscale-lambda-local-error`). |
| The exact base64 decision AWS makes for request bodies | Handlers must accept both forms (they do; keep it so). |
| Response streaming | Buffered only. |
| Real header normalization by Lambda | Compare against captured real events in Phase 5. |

Local mode is necessary but not sufficient.

## 8. Test strategy

Layers, from cheapest to most faithful:

1. **Handler unit tests** (Node, no AWS, CI): `lambda-adapter` tests; generated `lambda-entry.cjs` smoke test from an
   unzipped package in an empty directory.
2. **Parity**: send the same requests through `run` (IPC) and `run --lambda-local`; diff status, headers (minus
   `date`, `etag`, `last-modified`, `x-meshscale-*`) and body. Any difference is a bug in a transport adapter.
3. **Local Lambda front**: `run --lambda-local` with curl and a browser (section 7).
4. **Emulator and filesystem fidelity** (Linux): the AWS Lambda Runtime Interface Emulator. Easiest is the AWS Node base image
   (needs Docker for testing only; deployment stays ZIP-based):
   `docker run --rm -p 9000:8080 --read-only --tmpfs /tmp -v "$PWD/pkg:/var/task:ro" public.ecr.aws/lambda/nodejs:<major> lambda-entry.handler`
   then `curl -s -XPOST http://localhost:9000/2015-03-31/functions/function/invocations -d @event.json`
   [VERIFY image tag and endpoint path]. Run on a host whose CPU architecture matches the package. Any write to the code
   directory fails here, so Next's runtime writes show up before AWS.
5. **Real Lambda, no edge** (sandbox account): direct invoke
   `aws lambda invoke --function-name NAME --qualifier N --cli-binary-format raw-in-base64-out --payload file://event.json --log-type Tail out.json --query LogResult --output text | base64 -d`
   (the `REPORT` line has `Init Duration` on cold starts); and HTTP through an `AWS_IAM` Function URL on an alias, with
   `curl --aws-sigv4 "aws:amz:<region>:lambda" --user "$AK:$SK" -H "x-amz-security-token: $TOKEN" -H "x-forwarded-host: app.example.test" https://<id>.lambda-url.<region>.on.aws/path`
   (curl 7.75+; for requests with a body also send `x-amz-content-sha256` [VERIFY]).
6. **Cold, warm, concurrent**: new version or changed env var forces a cold start; second invoke must lack `Init Duration`;
   N parallel invokes each get their own environment; no state from one request appears in another's response. Sweep memory
   (for example 512, 1024, 1769, 2048 MB) and record init and handler time. [VERIFY] 1,769 MB ~ one vCPU.

Matrix (run at layers 1-3, and 4-6 where applicable), using the fixture app:

| Case | Expect |
|---|---|
| static page `/` | served by the runner/edge, never reaches Lambda |
| `/ssr`, `/pages-ssr`, `/dynamic/42` | correct HTML, status, route params |
| `/api/echo` GET and POST JSON | method, query, headers, body preserved; two `Set-Cookie` as two headers |
| `/route-handler` POST binary | byte length and SHA-256 match (base64 round trip) |
| repeated query `?a=1&a=2` | both values reach the app |
| encoded path `/dynamic/a%20b`, `%2F` | single decode only |
| cookie round trip | cookies sent in, cookies set out, none merged |
| `/redirect`, `/missing`, `/error` | 307 with `location`, 404, 500 returned as responses (not invocation errors) |
| HEAD and 204/304 | no body |
| RSC request (`rsc: 1`), prefetch headers | variant bypass routes to Lambda; response correct or explicitly unsupported |
| Server Action POST (`next-action`) | works, or explicitly unsupported; origin check passes with `x-forwarded-host` |
| `/log` | works (K3); logs visible |
| `/after` | marker written before the handler returns |
| `/env` | runtime environment variable visible |
| `/big` (> 7 MB) | controlled 502, not a truncated body |
| request body > 6 MB | 413 locally; rejected at the edge in production |
| `/stream` | works buffered (document the latency cost) |
| warm vs cold, serial vs concurrent | no cross-request leakage |

Unsupported-by-design until explicitly implemented (assert the build/deploy refuses or the response is a clear error):
middleware/proxy (adapter already throws), ISR and `revalidateTag` (need a shared cache handler; instance-local state
diverges), PPR/Cache Components, image optimization (needs native `sharp` for the target; also verify how the adapter
routes `/_next/image`), i18n beyond what is tested.

## 9. Open decisions (owner)

1. Which CPU architecture do build hosts have? Recommended: build on the Lambda architecture; do not cross-build.
2. How does the zip get from the builder to the orchestrator? Suggested: a write-only S3 artifact bucket keyed by
   build id/SHA, reusing the SigV4 signing code with a configurable region (the uploader hard-codes region `auto` and
   the R2 endpoint today). Note the builder then holds one narrowly scoped AWS credential that must be stripped from
   all child environments (Phase 0.4 does this).
3. Edge ingress choice (section Phase 7).
4. Do you want `--lambda-endpoint` (Phase 5.4)?
5. Is a manifest-level Lambda contract wanted, or is the sidecar enough (D4)?
6. CI triggers (K9) and whether Docker is acceptable in CI for the emulator layer.

## Appendix A — Function URL payload format 2.0 [VERIFY against a captured real event]

Request event (fields the runtime depends on are marked *):

```json
{
  "version": "2.0",                                  // *
  "routeKey": "$default",
  "rawPath": "/products/a%20b",                      // * raw, percent-encoded
  "rawQueryString": "ref=a&ref=b",                   // * raw; "" when there is no query
  "cookies": ["a=1", "b=2"],                         // * omitted when there are none; the cookie header is NOT in headers
  "headers": { "host": "<id>.lambda-url.<region>.on.aws", "x-forwarded-host": "app.example.com", "accept": "text/html,application/json" },
  "queryStringParameters": { "ref": "a,b" },         // do not use: repeated keys are joined
  "requestContext": {
    "accountId": "123456789012", "apiId": "<url-id>", "domainName": "<url-id>.lambda-url.<region>.on.aws",
    "domainPrefix": "<url-id>", "requestId": "…", "routeKey": "$default", "stage": "$default",
    "time": "09/Oct/2026:12:00:00 +0000", "timeEpoch": 1791547200000,
    "http": { "method": "GET", "path": "/products/a%20b", "protocol": "HTTP/1.1", "sourceIp": "…", "userAgent": "…" }   // * method
  },
  "body": "…",                                       // omitted when empty
  "isBase64Encoded": false                           // *
}
```

Response:

```json
{ "statusCode": 200, "headers": { "content-type": "text/html; charset=utf-8" },
  "cookies": ["a=1; Path=/; HttpOnly", "b=2; Path=/"], "body": "…", "isBase64Encoded": false }
```

## Appendix B — IPC frame protocol (`function_entry.cjs` and `runner.rs`) [FROM SOURCE]

Frame = `u32 BE header_len` + `u32 BE body_len` + `header_len` bytes of UTF-8 JSON + `body_len` bytes of body.
The header's `length` field must equal `body_len`. Limits: header 1 MiB, body 64 MiB.

| Direction | `kind` | Fields | Body |
|---|---|---|---|
| worker -> runner | `ready` | `length: 0` | none, sent once after load |
| runner -> worker | `request` | `id`, `method`, `uri` (path + query, as received), `headers: [[name, value], ...]`, `length` | request body |
| worker -> runner | `headers` | `id`, `status`, `headers: [[name, value], ...]` (repeat a name for repeated values), `length: 0` | none |
| worker -> runner | `chunk` | `id`, `length` | body bytes |
| worker -> runner | `end` | `id`, `length: 0` | none |
| worker -> runner | `error` | `id`, `error`, `length: 0` | none |

Responses for different ids may interleave. stdout carries only frames; stderr is inherited by the runner.
