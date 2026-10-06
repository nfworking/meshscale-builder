# MeshScale Builder

Initial Rust prototype for the MeshScale application builder.

## Current flow

1. Clone a GitHub repository using a personal access token.
2. Check out the requested commit SHA.
3. Resolve the optional project root directory.
4. Detect the framework from `package.json`.
5. Detect the package manager from `packageManager` or a lockfile.
6. Install dependencies.
7. Run the package's `build` script.
8. Package `.next/` and `package.json` into a ZIP artifact.
9. Print a machine-readable JSON result containing the status, artifact path, duration, and ZIP size.

Docker, NATS, Trigger.dev, S3 and builder orchestration are intentionally not part of this CLI yet.

## Usage

```bash
cargo run -- deploy \
  --git-username nfworking \
  --git-repo example \
  --git-hash 0123456789abcdef0123456789abcdef01234567 \
  --git-branch main \
  --access-token "$GITHUB_TOKEN" \
  --build-id build_123
```

For a monorepo where the Next.js application's `package.json` is not at the repository root:

```bash
cargo run -- deploy \
  --dir apps/web \
  --git-username nfworking \
  --git-repo example \
  --git-hash 0123456789abcdef0123456789abcdef01234567 \
  --git-branch main \
  --access-token "$GITHUB_TOKEN" \
  --build-id build_123
```

The artifact is written to the current working directory as `artifact-<build-id>.zip`.

## Framework detection

The prototype currently recognizes Next.js when `next` exists in `dependencies`, `devDependencies`, or `optionalDependencies`.

The detector is intentionally isolated behind a small function so additional framework rules can be added without changing the build orchestration.

## Package managers

The prototype prefers the `packageManager` field in `package.json`, then falls back to:

- `pnpm-lock.yaml` → pnpm
- `yarn.lock` → Yarn
- `package-lock.json` → npm

If no package manager metadata is present, it falls back to `npm install`.

## Security notes

The current prototype accepts the GitHub PAT through `--access-token` because that is the interface being prototyped. This should be replaced with a safer secret-delivery mechanism before the CLI is used by `builderd), so the token is not exposed through process arguments.

Build execution is deliberately not sandboxed yet. Do not run this prototype against untrusted repositories outside an isolated development environment.
