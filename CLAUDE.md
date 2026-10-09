# CLAUDE.md — meshscale-builder

Internal Rust build service/CLI. It turns a GitHub repo into a relocatable Next.js
deployment output (`.meshscale/output/`: `manifest.json`, `static/`, `runtime/`),
publishes the static half to Cloudflare R2, and can run the output locally.
It is NOT the customer-facing MeshScale CLI.

Read `README.md` and `DEPLOYMENT-CONTRACT.md` before changing anything. They are
partly out of date (see "Known doc/code mismatches" in `LAMBDA-PLAN.md`); where they
disagree with the code, the code wins, and you should record the mismatch.

## Current task

Add an AWS Lambda execution target. The full plan is `LAMBDA-PLAN.md`. Read all of it
first, then work **one phase at a time**:

1. Do the phase's tasks and tests.
2. Run every check in "Verification" below.
3. Summarize what changed, what you verified, and what you could not verify.
4. **Stop and wait for review.** Do not start the next phase on your own.

If a phase's instructions conflict with what you see in the code, stop and say so
instead of guessing. The plan was written from a read-through of the source; only
`src/lambda_local_host.cjs` and its tests were actually executed.

## Commands

```bash
cargo build
cargo run -- build --git-username <owner> --git-repo <repo> --git-hash <40-hex-sha> \
  --git-branch main --build-id build_1 --no-upload      # local build, no R2 needed
cargo run -- run .meshscale/output --port 3000          # local runner
cargo run -- run .meshscale/output --lambda-local       # after Phase 3 of the plan
cargo run -- upload .meshscale/output --org-id o --project-id p

cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
node --check src/<file>.cjs                              # every .cjs under src/
node --test test/runtime/*.test.cjs                      # pass a glob, not a directory
```

Node must be on PATH (the builder shells out to it). Supported hosts: Linux and
Windows. Opt-in integration tests need npm, Node and network access. If a `tests/`
directory exists, read it before changing the runner or artifact code; the plan was
written without seeing it.

## Layout

| Path | Role |
|---|---|
| `src/main.rs` | CLI (`build`, `run`, `upload`), build progress UI, logging setup |
| `src/build.rs` | clone, checkout, detect, install, adapter install, `next build` |
| `src/artifact.rs` | NFT trace collection, pnpm materialization, runtime/static copy, validation |
| `src/static_output.rs` | static inventory, prerender eligibility, routing derivation, `generate()` |
| `src/routing.rs` | `meshscale-routing-v1` rules, `select()`, path decoding |
| `src/manifest.rs`, `manifest.schema.json` | manifest types and validation (strict, `deny_unknown_fields`) |
| `src/upload.rs` | R2 uploader (SigV4 by hand), env/dotenv loading, `remove_credentials` |
| `src/runner.rs` | local edge: host routing, static serving, Node worker + framed IPC |
| `src/function_entry.cjs` | generated into `runtime/function-entry.cjs`; routes and invokes Next handlers |
| `src/next_adapter.cjs` | Next.js adapter staged during `next build` (`NEXT_ADAPTER_PATH`) |
| `src/cache.rs`, `src/stats.rs` | static cache and stats (check whether `runner.rs` actually uses them) |

Source file names use underscores (`function_entry.cjs`); generated artifact names use
hyphens (`runtime/function-entry.cjs`). Files are embedded with `include_str!`.

## Invariants — do not break these

- The local runner (`run`) keeps working at every phase. Existing tests keep passing.
- The IPC entrypoint keeps the name `runtime/function-entry.cjs`. It is hard-coded in
  `manifest.schema.json`, `manifest.rs`, `runner.rs` and test fixtures. Add new files;
  do not rename this one.
- `manifest.json` is strict: unknown fields are rejected by both serde and the JSON
  schema. Any schema change updates `manifest.schema.json`, `manifest.rs`,
  `DEPLOYMENT-CONTRACT.md` and the fixtures in the same change, and needs approval first.
- The remote `manifest.json` in R2 has byte-identical content to the local one, and is
  uploaded last. R2 never receives anything from `runtime/`.
- Artifacts contain no symlinks or junctions, no `.env*` files, no credentials.
- The first routing rule (`server_variants`) and the final `server` rule are mandatory
  and must not be reordered.
- Credentials (`MESHSCALE_GITHUB_TOKEN`, R2 settings, anything `AWS_*`) must never reach
  package-manager, build, tracing or worker child processes, and must never be logged.
- No `unwrap()`/`expect()` in non-test code paths that handle external input.
- Do not add the AWS SDK to the Rust crate. AWS control-plane calls live outside this
  repo (see Phase 6 of the plan). Rust signs requests with `aws-sigv4` only where the plan says so.

## Conventions

- Errors: `anyhow` with `.context(...)`. Messages say what failed and which path/value.
- Tests live next to the code in `#[cfg(test)] mod tests`. Node tests live in
  `test/runtime/*.test.cjs` and use `node:test` and `node:assert/strict` only.
- Runtime `.cjs` files: `'use strict'`, CommonJS, no dependencies except Node built-ins
  and `@next/routing`. Nothing under `runtime/` may write to stdout except the IPC shell
  writing frames. Use `console.error` for diagnostics in IPC code paths.
- Keep diffs minimal and preserve existing structure. Change what the phase asks for.
  Where you find an unrelated bug, record it instead of fixing it, unless the plan lists it.
- Handle cleanup, error paths and edge cases (teardown of temp files and child
  processes, timeouts, partial failure) without being asked.
- When behavior changes, update `README.md`, `DEPLOYMENT-CONTRACT.md` and
  `manifest.schema.json` as applicable, in the same change.

## Before you change pre-existing behavior

Write a test that pins the current behavior first, run it against the unchanged code,
then change the code. Phase 1 of the plan depends on this.

## Verification (run all of these before reporting a phase done)

1. `cargo fmt --all -- --check`
2. `cargo clippy --all-targets -- -D warnings`
3. `cargo test --all-targets`
4. `node --check` on every `.cjs` under `src/`
5. `node --test test/runtime/*.test.cjs`
6. Any manual check the phase lists (for example `run --lambda-local` with curl).

Before Phase 0 changes anything, run 1-3 once and record which failures already exist,
so you do not attribute them to your work. Report failures you could not fix; do not
claim a check passed if you did not run it.

## Security notes

Builds execute arbitrary repository code with the builder's privileges and are not
sandboxed. Do not run builds of untrusted repositories on a machine holding real
credentials. Do not paste tokens into commands, logs, tests or fixtures.
