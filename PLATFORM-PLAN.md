# MeshScale deployment platform: end-to-end implementation plan

Status: plan, nothing built yet. This document covers the whole path from "user connects GitHub" to "a
request on a custom domain is served". It supersedes the earlier `ORCHESTRATOR-PLAN.md` draft and uses
`EDGE-PLAN.md` for the edge Worker details and `LAMBDA-PLAN.md` for the Lambda artifact itself.

Tags: **[FROM SOURCE]** was read from the repositories on 2026-10-12. **[VERIFY]** is a third-party fact
(AWS, Cloudflare, GitHub, Trigger.dev) to confirm against current documentation before relying on it.
**[ASSUMPTION]** is a guess about intent that the owner should confirm.

---

## 0. Decisions this plan is built on

| # | Decision | Source |
|---|---|---|
| D1 | The **orchestrator** is a service on our own Docker hosts. It receives an API call and runs the builder in a **disposable Docker container** from an image that contains `meshscale-builder`. | Owner |
| D2 | Deployment workflows run on **Trigger.dev Cloud**. Trigger.dev tasks call the orchestrator; they never run Docker themselves. | Owner |
| D3 | Generated URLs live under **`meshscale.app`** (a Cloudflare zone separate from `meshscale.cloud`). Custom domains CNAME to it. | Owner |
| D4 | Private repositories are accessed through a **GitHub App** (short-lived installation tokens, push webhooks). | Owner |
| D5 | Internal service-to-service calls into the orchestrator use **mutual TLS** with a private CA. | Owner |
| D6 | Customer code runs on **AWS Lambda** (artifact from `meshscale-builder package lambda`), static files from **R2**, routing at a **Cloudflare Worker** edge. | LAMBDA-PLAN, EDGE-PLAN |
| D7 | The builder makes **no AWS control-plane calls** and untrusted build containers hold **no credentials** other than a short-lived, read-only, single-repo GitHub token. | LAMBDA-PLAN D5, builder CLAUDE.md |

---

## 1. What exists today [FROM SOURCE]

### 1.1 Repositories (`C:\Users\Cal\projects\meshscale\`)

| Repo | What it is | State relevant here |
|---|---|---|
| `frontend` | Next.js 16.3 dashboard (`meshscale-web`), OIDC login, proxies `/api/meshscale/api/v1/*` to the API with the user's bearer token; uses `@meshscale/sdk` 0.1.7 | Lists deployments on `/dashboard/projects`. Project page, domains, git and deployment settings are placeholders. No "new project" or "deploy" UI. |
| `api` | Next.js 16.3 route handlers, `/api/v1/*`, better-auth resource-server token check, org scoping via `x-meshscale-organization`, in-memory rate limit, `openapi.json` | Has projects, project settings, regions, builds, deployments. `POST /deployments` only inserts rows: "No queueing or deployment execution is performed" (openapi). |
| `auth` | better-auth server (`login.meshscale.cloud`): email/password, GitHub **social login**, organizations, API keys, OAuth provider (JWT) | GitHub here is login only, not repository access. |
| `db` | `@meshscale/db`: Drizzle schema + Neon client | `src/schema/index.ts` exports `auth.js`, `meshscale_v1.js`, `cloudtrail.js`, but only `auth.ts` is in this checkout. The schema shared in chat is assumed to be `meshscale_v1.ts`. |
| `api-client`, `blog` | `@meshscale/api` client (organizations only), blog | Not involved. |
| `builder` | this repo | Builds, publishes static to R2, packages Lambda zips. |

### 1.2 Problems found while reading (fix in Phase P1)

1. **`POST /api/v1/deployments` cannot work for a real deployment.** It requires `artifactId`, and
   `deployments.artifact_id` is `NOT NULL`, but an artifact only exists after the build that the same call
   creates. The request also carries git fields chosen by the client, not resolved from GitHub.
2. **`PATCH`/`DELETE /api/v1/deployments/{id}` reference columns and values that the schema does not
   have**: `branch`, `syncMode`, `deletedAt`, statuses `pending|building|deploying|ready|stopped|failed|paused`
   and `deleted` (the enum is `QUEUED|PROVISIONING|ACTIVE|INACTIVE|PAUSED`). This should not type-check.
   It is also a user-facing route that lets any org member set a deployment's status.
3. **`new ApiError(msg, 403, "REGION_NOT_ENABLED")`** is called with three arguments in
   `deployments/route.ts`, `projects/route.ts` and `projects/[projectId]/settings/route.ts`, but the
   constructor takes two; the code is dropped and the response always says `BAD_REQUEST`.
4. **Version drift:** `api/package.json` asks for `@meshscale/db` `0.2.0`; `node_modules` has `0.1.1`; the
   `db` repo has neither `meshscale_v1.ts` nor a `0.2.0` version. Before any schema work, the real source of
   `meshscale_v1.ts` must be committed to `db`.
5. **Projects have no name, slug or repository**, so nothing can say what to build.
6. `builds.logs` is a `text` column (build logs can be many MB); `artifacts` has one `r2_object_key` and an
   `integer` size, which fits neither the R2-prefix-plus-Lambda-zip artifact nor files above 2 GiB.
7. `environment_variables.value` is plaintext (the schema comments ask for encryption elsewhere but not here).
8. `runtimes` assumes long-running containers on compute nodes (`compute_node_id NOT NULL`, port, IP).
9. Write routes do not check organization roles (`requireOrganizationRole` exists but is unused).

---

## 2. Target architecture

```
                         ┌────────────── Cloudflare ───────────────┐
 browser ── HTTPS ──►    │ frontend (dashboard)                     │
                         │ edge Worker  *.meshscale.app + custom    │◄── R2: static bucket, edge-config bucket
                         └───────┬─────────────────────────┬────────┘
                                 │ bearer (OIDC)           │ SigV4 (AWS_IAM Function URL)
                                 ▼                         ▼
                         api (Next.js) ──► Postgres     AWS functions account: Lambda ms-<project>
                           │   ▲  (Neon)  ▲                ▲
              tasks.trigger│   │webhooks  │ restricted     │ assume role
                           ▼   │ (GitHub) │ DB role        │
                     Trigger.dev Cloud  ──┘────────────────┘
                           │  mTLS (client cert "workflows")
                           ▼
                     orchestrator (build host, Docker)
                       ├─ phase A container: `meshscale-builder build --no-upload`  (untrusted, no creds)
                       └─ phase B container: `package lambda` + `upload`           (trusted, R2 token)
                           │ presigned PUT                        │ R2 S3 API
                           ▼                                      ▼
                     S3 artifacts bucket (lambda.zip)        R2 static bucket (static/ + manifest.json)
```

### 2.1 Components and where their code lives

| Component | Repo / location | Runtime | Holds which secrets |
|---|---|---|---|
| Dashboard | `frontend` | Next.js | OIDC client secret |
| Public API + GitHub webhook receiver | `api` | Next.js | DB URL (app role), GitHub App id + private key + webhook secret, Trigger.dev secret key, mTLS client cert `api` (logs only), env-var encryption key |
| Workflows | **new** `orchestrator/workflows` (Trigger.dev project) | Trigger.dev Cloud | DB URL (workflow role), GitHub App private key, AWS control-account key (assume-role only), R2 token for the edge-config bucket, mTLS client cert `workflows`, env-var encryption key |
| Orchestrator (build host service) | **new** `orchestrator/agent` | Node 24 service on Linux hosts with Docker | mTLS server cert, R2 token for the static bucket (handed to phase B only), registry pull credentials |
| Builder image | `builder/Dockerfile` (this repo) | Docker image `meshscale-builder:<version>` | none |
| Edge | **new** `edge` (EDGE-PLAN) | Cloudflare Worker | Lambda invoker key |
| Shared schema | `db` | npm package `@meshscale/db` | none |

`orchestrator` is one new repository with two packages (`agent`, `workflows`) and a shared `contracts`
package (zod schemas for the internal API, the build result, the host binding). Keeping them together
lets one pull request change both sides of the internal API.

### 2.2 Trust zones

1. **Untrusted:** phase A build containers (customer `install`/`build` scripts run here) and Lambda
   functions (customer runtime code).
2. **Trusted data plane:** orchestrator agent process and phase B containers (run only our binary on
   files that phase A produced, which are treated as untrusted input and validated by the builder).
3. **Control plane:** API, Trigger.dev tasks, Postgres, AWS deployer role, edge-config bucket.

A compromise of zone 1 must not yield credentials for zones 2 or 3. That drives the two-phase build
(section 7.3), gVisor for phase A, egress rules, and the token-revocation step after clone.

---

## 3. End-to-end flows

### 3.1 Connect GitHub and create a project

1. Dashboard "Connect GitHub" -> API `GET /api/v1/github/install-url` returns
   `https://github.com/apps/<app>/installations/new?state=<signed org+user+nonce, 10 min>`.
2. User installs the App on chosen repos. GitHub redirects to the App's **Setup URL**
   `https://<dashboard>/github/setup?installation_id=...&state=...`; the dashboard calls
   `POST /api/v1/github/installations` with both. The API verifies `state`, then calls GitHub
   `GET /app/installations/{id}` with an App JWT to confirm the installation exists and records it for the
   org. (Do not trust `installation_id` from the query string alone.)
3. "New project": the dashboard lists repos via `GET /api/v1/github/repositories?installationId=` (API
   mints an installation token, calls `GET /installation/repositories`, caches 60 s).
4. `POST /api/v1/projects` with `name`, `slug`, `repository` (installation id, repo id), `rootDirectory`,
   `productionBranch`, regions. The API inserts project, settings, enabled regions, git settings and the
   managed production domain `<slug>.meshscale.app` (or `<slug>-<org-slug>.meshscale.app` when taken),
   then optionally triggers the first production deployment (3.2).

### 3.2 Deploy (manual, API, or git push)

1. **Trigger.** One of:
   - `POST /api/v1/projects/{id}/deployments` `{ branch?, commitSha?, target: "production"|"preview" }`.
   - GitHub `push` webhook: production branch -> production; other branches -> preview when
     `team_settings.automatic_preview_build_and_deployment` is on.
   - Redeploy of an existing deployment's commit.
2. **API (synchronous, < 1 s).** Resolve the commit: `GET /repos/{owner}/{repo}/commits/{ref}` with an
   installation token (never trust a client-supplied SHA without checking it exists in that repo).
   In one transaction insert a `builds` row (PENDING) and one `deployments` row per target region
   (QUEUED, `artifact_id` NULL), an audit log entry, then `tasks.trigger("deployment-pipeline", { buildId },
   { idempotencyKey: buildId, tags: [project, org] })` and store the run id on the build. If triggering
   fails, mark the rows FAILED with `TRIGGER_UNAVAILABLE`. Return `201` with the deployment(s) and a
   Trigger.dev **public access token** scoped to that run (for realtime status in the dashboard).
3. **Workflow `deployment-pipeline`** (section 6) runs:
   1. `build.run`: mint a repo-scoped GitHub token, pick an orchestrator node, create a Trigger.dev
      waitpoint token, generate a presigned S3 PUT for the zip, call the orchestrator over mTLS, and wait
      on the waitpoint (no compute billed while waiting).
   2. Orchestrator runs phase A (build) and phase B (package + R2 upload), uploads `lambda.zip` to the
      presigned URL, stores the build log in R2, and completes the waitpoint with the result.
   3. `artifact.verify`: re-check everything independently (S3 checksum, R2 manifest bytes and hash,
      `lambda.json` consistency), insert the `artifacts` row, link deployments.
   4. Per region, `lambda.deploy`: copy the zip to the region's artifacts bucket if needed, create/update
      function, publish version, per-deployment alias and AWS_IAM Function URL, health checks.
   5. Write the deployment URL's host binding (`<slug>-<id8>.meshscale.app`), mark the deployment READY.
   6. Production target: `deployment.promote` (3.3). Preview: done.
4. **Dashboard** shows status from Trigger.dev realtime (stage, timings) and logs from
   `GET /api/v1/builds/{id}/logs` (live: proxied from the orchestrator over mTLS; finished: from R2).

### 3.3 Promote and roll back

`deployment.promote` (concurrency 1 per project): update alias `live`, write a new host binding (higher
generation) for every ACTIVE project domain, mark the new deployment ACTIVE and the previous one
INACTIVE, set `projects.status` (ACTIVE when every enabled region has an ACTIVE deployment, otherwise
PARTIAL_DEPLOYMENT). Rollback is promotion of a retained INACTIVE deployment
(`POST /api/v1/deployments/{id}/promote`).

### 3.4 Custom domain

1. `POST /api/v1/projects/{id}/domains { hostname }` -> row CUSTOM, PENDING_VERIFICATION. The API returns
   the DNS records to create: `CNAME <hostname> -> cname.meshscale.app` (subdomains), plus the TXT record(s)
   Cloudflare returns for ownership/certificate validation.
2. Workflow `domain.add`: Cloudflare for SaaS `POST /zones/{meshscale.app}/custom_hostnames`
   (`ssl.method: "http"` or `"txt"`, type `dv`) [VERIFY]. Store the custom hostname id and the
   validation records.
3. Scheduled workflow `domain.verify` (every minute for new domains, backing off to hourly, giving up after
   7 days): read the custom hostname status; when hostname and SSL are `active`, mark the domain ACTIVE,
   set `made_active_at`, and write its host binding to the project's ACTIVE deployment.
4. `DELETE` removes the binding first, then the Cloudflare custom hostname, then the row.

Apex domains (`example.com`) need CNAME flattening or ALIAS at the customer's DNS provider; Cloudflare for
SaaS apex proxying needs an Enterprise plan [VERIFY]. The UI says so.

### 3.5 Cancel

`POST /api/v1/deployments/{id}/cancel` -> `runs.cancel(runId)`. The pipeline's cancel hook calls the
orchestrator `POST /internal/v1/builds/{id}/cancel` (kills containers, deletes workspace), marks build and
deployments CANCELED. Lambda resources already created are left for pruning; host bindings are never
written for a cancelled deployment.

---

## 4. Database changes (`db` repo, `meshscale_v1.ts`)

All changes are additive or loosen constraints, so they can be one forward migration with no data loss.
Generate with `drizzle-kit generate`, review the SQL, publish `@meshscale/db` `0.3.0`, then bump `api` and
`orchestrator/workflows` together.

### 4.1 Enums

```ts
// deployment_status: add values (ALTER TYPE ... ADD VALUE; keep existing ones)
QUEUED | BUILDING* | PROVISIONING | READY* | ACTIVE | INACTIVE | FAILED* | CANCELED* | PAUSED
//  QUEUED: rows created, workflow not yet started building
//  BUILDING: build in progress           PROVISIONING: Lambda deploy + health checks
//  READY: verified, reachable on its deployment URL, not serving project domains
//  ACTIVE: serving the project's domains in its region      INACTIVE: superseded, retained for rollback
//  FAILED / CANCELED: terminal            PAUSED: manually taken offline (bindings removed)

// build_status: add CANCELED. Use PENDING -> STARTED (container starting) -> BUILDING -> COMPLETED | ERROR | CANCELED

// new
deployment_target: PRODUCTION | PREVIEW
deployment_source: MANUAL | GIT_PUSH | API | REDEPLOY | ROLLBACK
runtime_provider: LAMBDA | CONTAINER
git_installation_status: ACTIVE | SUSPENDED | DELETED
```

### 4.2 Changed tables

**`projects`** add: `name text not null`, `slug text not null` (`^[a-z0-9](-?[a-z0-9])*$`, max 40),
unique `(organization_id, slug)` where `deleted_at is null`. Backfill existing rows with
`slug = 'project-' || left(id::text, 8)`.

**`project_settings`** add: `root_directory text not null default '.'`, `production_branch text not null
default 'main'`, `framework text` (detected, informational), `node_runtime text not null default
'nodejs24.x'`, `architecture text not null default 'x86_64'`, `memory_mb integer not null default 1024`,
`timeout_s integer not null default 30`, `auto_promote_production boolean not null default true`,
`retain_deployments integer not null default 5`, `health_check_path text not null default '/'`.

**`regions`** add: `provider text not null default 'aws'`, `provider_region text` (for example `us-east-2`),
`active boolean not null default true`. A region without `provider_region` cannot be deployed to.

**`builds`** add: `git_commit_hash text`, `trigger_run_id text`, `orchestrator_node text`,
`image_digest text`, `builder_version text`, `exit_code integer`, `error_code text`, `error_message text`,
`stage text` (last reported builder stage), `logs_object_key text`. Stop writing `logs` (drop it in a later
migration once nothing reads it).

**`artifacts`**: `r2_object_key` keeps meaning "manifest key" (`{org}/{project}/{build}/manifest.json`,
unique). Change `size` to `bigint` (static bytes). Add `manifest_sha256 text not null`,
`static_prefix text not null`, `lambda_zip_key text not null`, `lambda_zip_sha256 text not null`,
`lambda_zip_bytes bigint not null`, `lambda_metadata jsonb not null` (the full `lambda.json`),
`next_version text`, `architecture text not null`, `node_runtime text not null`. `hash` = manifest sha256
(kept for compatibility).

**`deployments`**: `artifact_id` becomes **nullable** (set after `artifact.verify`). Add
`target deployment_target not null default 'PREVIEW'`, `source deployment_source not null default
'MANUAL'`, `created_by text references user(id) on delete set null`, `promoted_at timestamptz`,
`ready_at timestamptz`, `error_code text`, `error_message text`, `url text` (deployment URL),
`superseded_by uuid references deployments(id)`. Index `(project_id, region_id, status)`.

**`runtimes`** (one row per deployment per region, describing what serves it): `compute_node_id` becomes
nullable; add `provider runtime_provider not null default 'CONTAINER'`, `function_name text`,
`function_version text`, `alias_name text`, `function_url text`, `code_sha256 text`,
`init_duration_ms integer`. Lambda rows use `provider = 'LAMBDA'`, `state` RUNNING after health checks,
FAILED otherwise, `desired_state` STOPPED once pruned.

**`domains`** add: `cloudflare_custom_hostname_id text`, `verification jsonb` (records to show the user,
SSL status), `last_checked_at timestamptz`, `error_message text`. Hostnames stored lowercased, punycode.
`routing_type` semantics [ASSUMPTION, confirm]: `DIRECT` = hostname resolves to the MeshScale edge
(managed or CNAME to `cname.meshscale.app`); `INDIRECT` = customer keeps their own proxy/CDN in front and
uses a MeshScale hostname as origin. Only DIRECT is implemented in this plan.

**`environment_variables`**: replace `value` with `value_ciphertext text not null`, `value_iv text not null`,
`key_version integer not null`; add `sensitive boolean not null default true`; unique
`(project_id, coalesce(region_id, '00000000-0000-0000-0000-000000000000'), type, key)`. Encrypt with
AES-256-GCM, additional data = `project_id|region_id|type|key`. Migration encrypts existing rows.

**`team_settings`**: `git_provider_id` stays unused; GitHub state moves to `git_installations`.

### 4.3 New tables

```ts
git_installations          // one per GitHub App installation
  id uuid pk, organization_id text fk -> organization (cascade),
  provider text default 'github', installation_id bigint unique, account_login text,
  account_type text, status git_installation_status, created_by text fk user, created_at, updated_at

project_repositories       // which repo a project builds
  project_id uuid pk fk -> projects (cascade), git_installation_id uuid fk -> git_installations,
  repository_id bigint not null, owner text not null, name text not null, default_branch text,
  created_at, updated_at
  index (git_installation_id, repository_id)     // webhook lookup

lambda_functions           // one per project per region
  id uuid pk, project_id uuid fk (cascade), region_id uuid fk, function_name text unique,
  function_arn text, execution_role_arn text, live_alias_version text, created_at, updated_at
  unique (project_id, region_id)

host_bindings              // what the edge was told, for audit and reconciliation
  hostname text pk, project_id uuid fk, deployment_id uuid fk, generation bigint not null,
  manifest_key text, manifest_sha256 text, origin_url text, written_at timestamptz,
  object_etag text

deployment_events          // timeline shown in the dashboard
  id uuid pk, deployment_id uuid fk (cascade), build_id uuid fk, at timestamptz default now(),
  step text, outcome text (started|succeeded|failed|retried|skipped), detail jsonb
  index (deployment_id, at)
```

`audit_logs` gets entries for deployment created/promoted/rolled back/cancelled, domain added/verified/
removed, env var changed (key only, never the value), GitHub installation connected/removed.

### 4.4 Database roles

- `meshscale_api`: what the API uses today.
- `meshscale_workflows`: `SELECT` on projects, settings, regions, repositories, installations,
  env vars, domains; `SELECT, INSERT, UPDATE` on builds, artifacts, deployments, runtimes, lambda_functions,
  host_bindings, deployment_events, domains (status columns only via a view or column grants),
  `INSERT` on audit_logs. No access to auth tables.

---

## 5. Public API changes (`api` repo)

Conventions stay as they are (`authenticate`, `jsonBody`, `noStore`, error shape `{error:{code,message,
requestId}}`, `X-MeshScale-API-Version`). Add zod validation, give `ApiError` a `code` argument, and require
role `owner|admin` (better-auth organization roles) for writes that deploy, promote, change domains or env
vars. Update `openapi.json` and `@meshscale/sdk` with each route.

| Method and path | Purpose |
|---|---|
| `GET /api/v1/github/install-url` | Signed `state`, App install URL |
| `POST /api/v1/github/installations` | Record installation after the setup redirect (verify `state`, confirm with GitHub) |
| `GET /api/v1/github/installations` | List the org's installations |
| `GET /api/v1/github/repositories?installationId=` | Repos the App can access |
| `GET /api/v1/github/branches?projectId=` | Branches for the deploy dialog |
| `POST /api/v1/projects` | **Extended**: name, slug, repository, root directory, production branch, regions, `deploy: boolean` |
| `PATCH /api/v1/projects/{id}/settings` | **Extended** with section 4.2 settings (validated ranges: memory 128-10240, timeout 1-900) [VERIFY limits] |
| `POST /api/v1/projects/{id}/deployments` | Create build + deployments, trigger workflow (3.2). Replaces `POST /api/v1/deployments`, which returns 410 with a pointer |
| `GET /api/v1/deployments/{id}` | Deployment, build, artifact, runtimes, events, URLs |
| `POST /api/v1/deployments/{id}/cancel` | 3.5 |
| `POST /api/v1/deployments/{id}/promote` | Promote or roll back (READY or INACTIVE, still retained) |
| `POST /api/v1/deployments/{id}/redeploy` | New build of the same commit |
| `GET /api/v1/deployments/{id}/realtime-token` | Fresh Trigger.dev public token for the run (15 min) |
| `PATCH /api/v1/deployments/{id}` | **Removed** for status changes (only workflows set status). Keep only `PAUSED` toggle for admins, which triggers a workflow that removes or restores bindings |
| `DELETE /api/v1/deployments/{id}` | Allowed only for non-ACTIVE deployments; marks for pruning |
| `GET /api/v1/builds/{id}/logs` | `text/event-stream` while running (proxy to orchestrator over mTLS), `text/plain` from R2 when finished |
| `GET/POST /api/v1/projects/{id}/domains`, `DELETE /api/v1/domains/{id}`, `POST /api/v1/domains/{id}/verify` | 3.4 |
| `GET/PUT/DELETE /api/v1/projects/{id}/environment-variables[/{key}]` | Values write-only when `sensitive`; validation in 5.2 |
| `POST /api/webhooks/github` | Not under `/api/v1`, no bearer auth; verifies `X-Hub-Signature-256` (HMAC-SHA256, constant-time compare), dedupes by `X-GitHub-Delivery`, handles `push`, `installation`, `installation_repositories`; returns 202 quickly |

### 5.1 Push webhook rules

- Ignore pushes that delete a branch (`deleted: true`) and tags.
- Find projects by `(installation_id, repository_id)`; skip deleted projects.
- Branch equals `production_branch` -> PRODUCTION target; else PREVIEW if previews are enabled.
- Coalesce: if a QUEUED (not yet building) deployment exists for the same project, target and branch,
  cancel it in favour of the newer commit.
- Optional later: GitHub commit statuses / deployments API for "deployed at <url>" on the commit.

### 5.2 Environment variable rules (must match the builder)

- Keys: `^[A-Za-z_][A-Za-z0-9_]*$`. Rejected for all types: names starting with `AWS_`,
  `MESHSCALE_GITHUB_TOKEN`, the `MESHSCALE_R2_*` names (and the `MESHScale_R2_*` aliases) - the builder's
  `load_build_env_file` refuses them [FROM SOURCE `src/env.rs`].
- BUILD values must not contain `$` (the builder's env-file loader rejects it) or a newline.
- RUNTIME variables: total size of keys + values for a region must stay under 4 KB (Lambda limit)
  [VERIFY]; Lambda reserved keys are rejected [VERIFY list].
- `NEXT_PUBLIC_*` must be BUILD variables to reach the client bundle; the UI explains this.

---

## 6. Workflows (Trigger.dev Cloud, `orchestrator/workflows`)

Uses Trigger.dev v4 features: tasks and subtasks, queues with `concurrencyKey`, idempotency keys,
waitpoint tokens, run metadata, realtime, scheduled tasks [VERIFY each against the v4 docs at
implementation time]. All AWS, R2 and DB calls are made from task code; nothing long-running holds
compute while waiting for a build.

### 6.1 Tasks

| Task | Queue / concurrency | Does |
|---|---|---|
| `deployment-pipeline` | `deployments`, `concurrencyKey = projectId` | Parent: runs the subtasks below in order, keeps `metadata.stage`, owns cancellation and the final status |
| `build-run` | `builds`, global limit = total orchestrator slots | Token, node choice, waitpoint, mTLS call to orchestrator, wait |
| `artifact-verify` | inherits | Independent verification, `artifacts` row |
| `lambda-deploy` | `lambda`, `concurrencyKey = projectId:regionId` | Section 8.2 |
| `deployment-promote` | `deployments`, `concurrencyKey = projectId` | 3.3 |
| `domain-add`, `domain-remove` | `domains`, `concurrencyKey = hostname` | Cloudflare for SaaS |
| `domain-verify` | scheduled every minute | Polls pending domains with backoff |
| `lambda-prune` | scheduled daily + after promote | Section 8.5 |
| `reconcile` | scheduled every 10 min | Finds builds/deployments stuck in non-terminal states past their deadline, checks orchestrator and AWS, fails or resumes them; rewrites host bindings whose stored generation or ETag differs from R2 |

Every subtask is triggered with `idempotencyKey = <buildId or deploymentId>:<task>` so a retried parent
never repeats a completed step.

### 6.2 `build-run` in detail

1. Load build, project, settings, repository, BUILD env vars (decrypt), regions.
2. Mint a GitHub installation token restricted to `repositories: [<repo name>]`,
   `permissions: { contents: "read" }` (1 h validity) [VERIFY].
3. Choose a node: `GET /internal/v1/node` on each configured orchestrator (env `ORCHESTRATOR_NODES`, a JSON
   list of `{id, url, arch}`), pick the matching architecture with the most free slots. None free: retry
   with backoff (the `builds` queue limit should make this rare).
4. Create a waitpoint token (timeout 60 min) and a presigned S3 PUT URL for
   `s3://meshscale-artifacts-<acct>-<default region>/lambda/<buildId>.zip` (15 min validity is too short
   for a long build: presign at 75 min, or have the agent request it at upload time through a second
   waitpoint; this plan uses a 75 min URL bound to the build id key).
5. `POST https://<node>/internal/v1/builds` over mTLS (7.2). `202` -> set build STARTED, record node.
   `409` with the same build id -> already accepted (retry case). `503` -> pick another node.
6. `wait.forToken(token)`. On timeout: cancel on the orchestrator, fail with `BUILD_TIMEOUT`.
7. The completion payload is **untrusted** (anyone with the waitpoint URL could post it): it is only used
   to know what to check. `artifact-verify` re-derives every fact.

### 6.3 `artifact-verify`

- `HeadObject` the zip with checksum mode; compare size and SHA-256 with the payload's `lambda.json`.
- `GET` `{org}/{project}/{buildId}/manifest.json` from the static R2 bucket; SHA-256; check
  `deployment.id == buildId`, `org_id`, `project_id`, `commit == builds.git_commit_hash`,
  `routing.semantics == "meshscale-routing-v1"`, `static.storage.prefix`.
- Check `lambda.json`: `schema_version == 1`, `build_id == buildId`, `commit`, `handler ==
  "lambda-entry.handler"`, `runtime == settings.node_runtime`, `architecture == settings.architecture`.
- Insert `artifacts`, set `deployments.artifact_id`, build COMPLETED with timings.

### 6.4 Secrets in Trigger.dev environment variables

`DATABASE_URL` (workflows role), `GITHUB_APP_ID`, `GITHUB_APP_PRIVATE_KEY`, `AWS_ACCESS_KEY_ID` /
`AWS_SECRET_ACCESS_KEY` (control-account user that can only `sts:AssumeRole` the deployer role) and
`AWS_DEPLOYER_ROLE_ARN` + external id, `R2_EDGE_CONFIG_*` (token scoped to the edge-config bucket),
`CLOUDFLARE_API_TOKEN` (Custom Hostnames edit on the `meshscale.app` zone only), `MTLS_CLIENT_CERT` /
`MTLS_CLIENT_KEY` / `MTLS_CA` (PEM, base64), `ORCHESTRATOR_NODES`, `ENV_ENCRYPTION_KEYS` (versioned).
Separate Trigger.dev environments (dev, staging, prod) with separate values; production values are set by
an operator, never committed.

---

## 7. Orchestrator (build host service, `orchestrator/agent`)

### 7.1 Host

- Linux VM (start with one EC2 `m7i.2xlarge` or similar, x86_64, in the default region), Docker Engine
  with the **gVisor (`runsc`) runtime** installed for phase A [VERIFY Next.js builds under gVisor; fall back
  to `runc` + the other controls if incompatible, and record it].
- IMDSv2 required with hop limit 1 so containers cannot reach instance metadata; the instance role has no
  permissions beyond pulling the builder image (or no role at all if the image comes from GHCR).
- Docker network `ms-build` (phase A): egress to the internet allowed (npm, GitHub); `DOCKER-USER`
  iptables rules drop traffic to `169.254.0.0/16`, `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`,
  `100.64.0.0/10` and the host itself, except the DNS resolver.
- Docker network `ms-publish` (phase B): egress only to R2 (`*.r2.cloudflarestorage.com`) and S3, via
  an egress proxy or DNS-pinned rules [simplest acceptable v1: same as `ms-build`].
- Disk: workspaces on a dedicated volume `/var/lib/meshscale/builds` (XFS with project quotas so each
  workspace has a hard size limit, default 20 GiB).
- The agent runs as a systemd service with access to the Docker socket. The Docker socket is never
  mounted into any container.

### 7.2 Internal API (mTLS, port 8443)

TLS 1.3 only, `requestCert: true`, `rejectUnauthorized: true`, CA = MeshScale internal CA. Authorization by
the client certificate's URI SAN (section 9):

| Route | Allowed callers | Purpose |
|---|---|---|
| `POST /internal/v1/builds` | `workflows` | Start a build (body below). Idempotent by `buildId` |
| `GET /internal/v1/builds/{id}` | `workflows`, `api` | State, stage, timings, exit codes |
| `GET /internal/v1/builds/{id}/logs?follow=1&offset=` | `api`, `workflows` | SSE stream of log lines; finished builds return 410 and the API reads the log from R2 |
| `POST /internal/v1/builds/{id}/cancel` | `workflows` | Kill containers, clean workspace, complete waitpoint as cancelled |
| `GET /internal/v1/node` | `workflows`, `api` | Version, image digest, arch, slots total/free, disk free |
| `GET /internal/v1/health` | any valid cert | Liveness |

Start-build body (validated with the shared zod schema; unknown fields rejected):

```json
{
  "buildId": "8c0d...uuid",
  "orgId": "org_...", "projectId": "6f1e...uuid",
  "repository": { "owner": "acme", "name": "shop", "branch": "main", "commit": "<40 hex>" },
  "rootDirectory": ".",
  "githubToken": "<installation token>",
  "buildEnv": { "NEXT_PUBLIC_API": "https://..." },
  "lambda": { "architecture": "x86_64", "nodeRuntime": "nodejs24.x" },
  "zipUpload": { "url": "<presigned PUT>", "headers": { "x-amz-checksum-sha256": "?" } },
  "callback": { "waitpointUrl": "<Trigger.dev token URL>" },
  "limits": { "timeoutSeconds": 1800, "cpus": 4, "memoryMb": 8192, "diskGb": 20 }
}
```

The SHA-256 of the zip is not known when the URL is presigned, so the presign cannot bind
`x-amz-checksum-sha256`; integrity comes from `artifact-verify` comparing the object with `lambda.json`
and from the key being unique per build. `githubToken`, `buildEnv` and the waitpoint URL are never logged
and are deleted from agent memory and disk when the build ends.

Agent state is kept in a local SQLite file so a restart can find its builds again: on start it lists
containers with label `meshscale.build`, kills those without a live job, marks the jobs failed and
completes their waitpoints with `AGENT_RESTARTED`.

### 7.3 Running a build: two disposable containers

Workspace `/var/lib/meshscale/builds/<buildId>/` with `work/` (rw for phase A), `out/` (phase B output),
`env/build.env` (tmpfs-backed, mode 0400), `logs/`.

**Phase A: build (untrusted).**

```bash
docker run --rm --name ms-build-<buildId> --label meshscale.build=<buildId> \
  --runtime runsc --network ms-build \
  --cpus 4 --memory 8g --memory-swap 8g --pids-limit 4096 \
  --cap-drop ALL --security-opt no-new-privileges --user 10001:10001 \
  --read-only --tmpfs /tmp:rw,size=2g \
  -v /var/lib/meshscale/builds/<id>/work:/work:rw \
  -v /var/lib/meshscale/builds/<id>/env/build.env:/run/meshscale/build.env:ro \
  -e MESHSCALE_GITHUB_TOKEN -e TMPDIR=/work/tmp -e HOME=/work/home \
  meshscale-builder@sha256:<digest> \
  build --git-username <owner> --git-repo <name> --git-hash <commit> --git-branch <branch> \
        --dir <rootDirectory> --build-id <buildId> --org-id <org> --project-id <project> \
        --no-upload --output /work/output --build-env-file /run/meshscale/build.env \
        --progress json
```

- The token is passed through the environment of `docker run` (not on the command line). The builder
  already keeps it out of package-manager and build child processes [FROM SOURCE `src/env.rs`], but a
  build script running as the same user could read the builder's `/proc/<pid>/environ`. Mitigations:
  the token is read-only, single-repo, 1 h; and the agent **revokes it** (`DELETE /installation/token`) as
  soon as the builder reports the clone stage finished (needs `--progress json`, section 10).
- Wall-clock timeout from `limits.timeoutSeconds`; on expiry `docker kill`, result `BUILD_TIMEOUT`.
- `--org-id/--project-id` with `--no-upload`: the builder accepts them together and records them in
  the manifest [FROM SOURCE README]. [VERIFY at implementation: flag names under `DestinationArgs`.]

**Phase B: package and publish (trusted, no customer code runs).**

```bash
docker run --rm --name ms-publish-<buildId> --label meshscale.build=<buildId> \
  --network ms-publish --cpus 2 --memory 2g --cap-drop ALL --security-opt no-new-privileges \
  --user 10001:10001 --read-only --tmpfs /tmp \
  -v .../work/output:/output:ro -v .../out:/out:rw \
  -e MESHSCALE_R2_ACCOUNT_ID -e MESHSCALE_R2_ACCESS_KEY_ID -e MESHSCALE_R2_SECRET_ACCESS_KEY \
  -e MESHSCALE_R2_BUCKET \
  meshscale-builder@sha256:<digest> \
  sh -c 'meshscale-builder package lambda /output --arch x86_64 --node-runtime nodejs24.x --out /out/lambda \
      && meshscale-builder upload /output --org-id <org> --project-id <project> --build-id <buildId>'
```

`package lambda` and `upload` read files phase A produced and validate them (no symlinks, no `.env*`,
inventory checks, size limits) and never execute them [FROM SOURCE]. `upload` snapshots the output
first, so phase A leftovers cannot change it mid-upload. The output mount is read-only, which is why
`package lambda` writes to `/out/lambda` (`--out`). [VERIFY: `upload` accepts a read-only output directory;
if it writes `_upload.json` locally, mount a copy instead.]

**After the containers:** the agent PUTs `out/lambda/lambda.zip` to the presigned URL, uploads the combined
log (container stdout/stderr + `output/build.log`) to the private R2 logs bucket at
`build-logs/<org>/<project>/<buildId>.log`, POSTs the result to the waitpoint URL, and deletes the
workspace. The workspace is deleted on every exit path (success, failure, cancel, timeout, agent restart).

Completion payload:

```json
{
  "buildId": "...", "status": "succeeded" | "failed" | "cancelled" | "timed_out",
  "errorCode": null, "failedStage": null, "exitCode": 0,
  "manifest": { "key": "org/project/build/manifest.json", "sha256": "<hex>" },
  "lambda": { "...": "lambda.json contents" },
  "zip": { "key": "lambda/<buildId>.zip", "bytes": 12345, "sha256": "<hex>" },
  "logs": { "key": "build-logs/..." },
  "timings": { "queuedMs": 0, "buildMs": 0, "packageMs": 0, "uploadMs": 0 },
  "image": { "digest": "sha256:...", "builderVersion": "0.x.y" }
}
```

### 7.4 Logs

The agent tees container output to `logs/build.log` and keeps the last N lines plus offsets in memory for
SSE followers. Lines are passed through a redactor that removes the GitHub token, R2 settings and any
BUILD env value marked `sensitive` before they are stored or streamed. The API proxies
`GET /internal/v1/builds/{id}/logs?follow=1` to the browser with its own `api` client certificate.

### 7.5 Capacity and scaling

Slots per host = `floor(min(cpus / 4, memory / 8 GiB))`, configurable. The Trigger.dev `builds` queue limit
is the sum of slots. More capacity = more hosts in `ORCHESTRATOR_NODES` (later: a `build_nodes` table with
heartbeats). arm64 Lambda targets need an arm64 (Graviton) host, because native modules are compiled for
the build host's architecture; start with x86_64 only.

---

## 8. Lambda deployment (`lambda-deploy`, `deployment-promote`, `lambda-prune`)

Requirements from LAMBDA-PLAN Phase 6, made concrete.

### 8.1 AWS layout

- **Control account:** an IAM user for Trigger.dev that can only `sts:AssumeRole` into the deployer role
  (external id required).
- **Functions account:**
  - `meshscale-deployer` role: Lambda management on `function:ms-*`; `iam:CreateRole`/`PutRolePolicy`/
    `DeleteRole*` on `role/ms-exec-*` **with a mandatory permissions boundary**; `iam:PassRole` on
    `role/ms-exec-*` to `lambda.amazonaws.com`; `s3:*Object` on the artifacts buckets; logs retention.
  - Artifacts bucket per region `meshscale-artifacts-<acct>-<region>`, private, lifecycle delete after 30 days.
    The build-region bucket receives the presigned PUT; other regions get a server-side `CopyObject`.
  - One execution role per function `ms-exec-<suffix>`, logs to its own log group only.
  - Edge invoker IAM user: `lambda:InvokeFunctionUrl` + `lambda:InvokeFunction` on `function:ms-*`,
    condition `lambda:FunctionUrlAuthType = AWS_IAM` [VERIFY both actions required].
- Function name: `ms-<first 40 chars of project slug>-<8 hex of sha256(projectId)>` (64 char limit
  [VERIFY]), stored in `lambda_functions`. Alias per deployment: `d-<first 12 hex of deployment uuid
  without dashes>`.

### 8.2 `lambda-deploy` steps (one region)

1. `GetFunction`; if missing create the execution role (retry `CreateFunction` on "role cannot be assumed"
   instead of sleeping), `CreateFunction` from S3 with handler `lambda-entry.handler`, runtime,
   architecture, memory, timeout, RUNTIME env vars, `Publish=false`, tags; wait `function-active-v2`; set
   log retention.
2. `UpdateFunctionConfiguration` if runtime/handler/memory/timeout/environment differ; wait
   `function-updated-v2`.
3. `UpdateFunctionCode` (S3 location, architectures, `RevisionId`); wait until `LastUpdateStatus =
   Successful`; check `CodeSha256 == base64(hex zip sha256)` (Lambda reports base64, `lambda.json` hex).
4. `PublishVersion` with `CodeSha256` and `RevisionId`, `Description = "meshscale <deploymentId>"`; on retry
   reuse an existing version with that description. Precondition failure: repeat from 2 once, then fail.
5. `CreateAlias d-<id>` -> version; `CreateFunctionUrlConfig` on the alias, `AuthType=AWS_IAM`,
   `InvokeMode=BUFFERED`. Never `NONE`.
6. Health checks: direct `Invoke` of the version with a payload-2.0 event carrying `x-meshscale-probe:
   init` (must return the probe's 200), then `GET <health_check_path>` (a `FunctionError` fails; a 5xx
   fails only for production targets), then one SigV4 request to the new Function URL.
7. Upsert `runtimes` (LAMBDA, version, alias, URL, init duration), write the deployment URL's host binding,
   status READY, events.

### 8.3 `deployment-promote`

Per region, after all regions of a production build are READY: `UpdateAlias live` -> version; increment
generation; write host bindings for every ACTIVE project domain (one region per binding in v1: the project's
default region; multi-region origins are binding schema v2, EDGE-PLAN open decision); update statuses and
`projects.status`; audit log. A failure after some bindings were written is retried until all are written;
`reconcile` repairs leftovers.

### 8.4 Failure rules

- A failed or cancelled deployment never touches `live` or any project-domain binding.
- `$LATEST` may hold code from a failed deployment; nothing routes to `$LATEST`.
- AWS throttling and `ResourceConflictException` are retried with backoff; other errors fail the step with
  the AWS error code and request id in `deployment_events`.

### 8.5 Pruning

Keep the ACTIVE deployment, the last `retain_deployments` READY/INACTIVE ones per project and region, and
anything younger than 24 h. For older ones: delete the deployment-URL binding, the Function URL config,
the alias, and the version when nothing else references it; set `runtimes.desired_state = STOPPED`. Static
R2 prefixes and build logs: separate retention job (default 90 days, never for retained deployments).

---

## 9. Mutual TLS and internal PKI

- **CA:** a private two-tier CA. Offline root (kept off any server), online intermediate run with
  `step-ca` on a small control host (or AWS Private CA if budget allows [VERIFY cost]). Certificates carry a
  URI SAN identity: `spiffe://meshscale/workflows`, `spiffe://meshscale/api`,
  `spiffe://meshscale/orchestrator/<node>`.
- **Lifetimes:** client and server certs 30 days, renewed at 2/3 of life. The agent renews its own server
  cert through step-ca's ACME/JWK provisioner. Trigger.dev and API client certs are renewed by a scheduled
  job that writes the new PEMs to the Trigger.dev and API environment (Trigger.dev management API or a
  manual runbook in v1) [VERIFY Trigger.dev env var API].
- **Server side (agent):** Node `https.createServer({ minVersion: 'TLSv1.3', requestCert: true,
  rejectUnauthorized: true, ca })`; an authorization middleware maps the peer cert's URI SAN to allowed
  routes (7.2) and rejects everything else with 403. Revocation: short lifetimes plus a deny-list of serials
  pushed with the agent config.
- **Client side:** `undici.Agent({ connect: { cert, key, ca, servername } })` in both the API and the
  workflows; the server certificate must chain to the internal CA and match the node's DNS name
  (`build-1.internal.meshscale.cloud`).
- **Network:** Trigger.dev Cloud egress IPs are not fixed [VERIFY static IP option], so the agent port is
  reachable from the internet; mTLS is the gate. A security group allows only 8443 (and SSH via SSM, not
  open). Add an IP allowlist if Trigger.dev offers static egress.
- **Is mTLS needed elsewhere?** Not in v1. Workflows talk to Postgres (TLS + role password), AWS (SigV4),
  Cloudflare (API token) and GitHub (App JWT). The API does not expose internal routes: workflows write to
  the database directly with a restricted role. If that changes later, internal API routes go behind an
  mTLS-terminating proxy in front of the API, using the same CA.

---

## 10. Builder changes (this repository)

Each item follows the builder's CLAUDE.md process (pin behavior with a test first, update README and
DEPLOYMENT-CONTRACT, stop for review).

1. **`Dockerfile`** (multi-stage):
   - Stage 1 `rust:<pinned>-bookworm`: `cargo build --release --locked`.
   - Stage 2 based on **`public.ecr.aws/lambda/nodejs:24`** (Amazon Linux 2023, the same glibc and Node
     major as the Lambda runtime, so native modules compiled during `npm install` load on Lambda) with
     `ENTRYPOINT` reset; install `git`, `tar`, `xz`, `ca-certificates`; enable pnpm and yarn at pinned
     versions (`corepack` if present in Node 24, otherwise `npm i -g pnpm@<v> yarn@<v>`) [VERIFY]; copy the
     binary to `/usr/local/bin/meshscale-builder`; create user `10001`; `ENTRYPOINT ["meshscale-builder"]`.
   - [VERIFY] the Rust binary built on bookworm runs on AL2023 (glibc 2.34): build stage 1 on an older
     glibc (for example `rust:<v>-bullseye`) or on AL2023 itself to be safe.
   - Image published to GHCR/ECR as `meshscale-builder:<semver>` and referenced **by digest** by the agent.
2. **CI** (GitHub Actions): on tag, run the full verification list, build the image for `linux/amd64`
   (arm64 later), run `cargo test builds_fixture_app -- --ignored` **inside the image**, push, record digest.
3. **`--progress json`** for `build`: newline-delimited JSON stage events on stderr
   (`{"event":"stage","stage":"clone","status":"finished","ms":1234}`), the same stages the human UI
   shows. Needed for token revocation after clone and for status in the dashboard. No change to the
   default human output.
4. **Machine-readable results**: `package lambda` and `upload` print a single JSON line with
   `--json` (`upload`: bucket, prefix, manifest key, files, bytes; `package`: paths and the sidecar). The
   agent then does not parse human text.
5. Confirm (and test) that `upload` works from a read-only output mount, or document that it needs a
   writable copy.
6. Docs: README "Running in a container" section; DEPLOYMENT-CONTRACT: note that `manifest.deployment.id`
   is the platform build UUID.

No AWS SDK, no orchestration logic and no manifest schema change in the builder.

---

## 11. Edge and domains

The Worker is specified in `EDGE-PLAN.md`; platform-specific parts:

- **Zone `meshscale.app`** on Cloudflare: wildcard DNS `*.meshscale.app` proxied; Worker route
  `*.meshscale.app/*`; Universal SSL covers one label (`<x>.meshscale.app`), so generated hostnames never
  have two labels below the zone [VERIFY].
- **Hostnames:** production `<slug>.meshscale.app` (fallback `<slug>-<org-slug>`); deployment URLs
  `<slug>-<id8>.meshscale.app` where `id8` = first 8 base36 characters derived from the deployment uuid.
  Labels are at most 63 characters; the API reserves `www`, `api`, `cname`, `fallback`, `edge`, `status`,
  `admin` and similar.
- **Custom hostnames:** Cloudflare for SaaS on `meshscale.app` with fallback origin `fallback.meshscale.app`
  and customers CNAMEing to `cname.meshscale.app`; the Worker route must also match custom hostnames
  ("Workers for Platforms/SaaS" route `*/*` on the zone) [VERIFY current routing for custom hostnames].
- **Host bindings** (EDGE-PLAN 4.1) are written by workflows (`deployment-promote`, `lambda-deploy` for
  deployment URLs, `domain-verify`) to the edge-config bucket, mirrored in the `host_bindings` table.
- **Preview protection** (later): deployment URLs are public by default; a later phase can add an
  edge-checked signed cookie for previews.

---

## 12. Secrets and identities inventory

| Secret / identity | Lives in | Scope | Rotation |
|---|---|---|---|
| GitHub App private key | API env, Trigger.dev env | Mint installation tokens | Yearly or on incident; App allows two keys during rotation |
| GitHub webhook secret | API env | Verify webhooks | Yearly |
| Installation tokens | memory of workflows/agent, phase A env | One repo, contents:read, 1 h | Revoked after clone |
| Trigger.dev secret key | API env | Trigger runs, create public tokens | On staff change |
| DB URL `meshscale_api` / `meshscale_workflows` | API / Trigger.dev | 4.4 | Quarterly |
| AWS control-account key | Trigger.dev env | `sts:AssumeRole` only | 90 days |
| Deployer role | functions account | 8.1 | n/a |
| Edge invoker key | Worker secrets | invoke `ms-*` URLs | 90 days |
| R2 static-bucket token | agent host (to phase B only) | Object R/W on the static bucket and logs bucket | 90 days |
| R2 edge-config token | Trigger.dev env | Object R/W on the edge-config bucket | 90 days |
| Cloudflare API token | Trigger.dev env | Custom Hostnames on `meshscale.app` | 90 days |
| mTLS certs | Trigger.dev, API, agent | 9 | 30 days |
| Env-var encryption keys | API env, Trigger.dev env | AES-256-GCM, versioned | Yearly, re-encrypt job |

Nothing in this table is ever written to a log, a build log, a container image, an artifact or the
repository. The agent's redactor (7.4) is a second line of defence, not the first.

---

## 13. Frontend changes (`frontend` repo)

1. **GitHub connection** in Settings -> Git: "Connect GitHub" (install flow 3.1), list of installations,
   "Manage on GitHub" link. A `/github/setup` route handles the redirect and calls the API.
2. **New project wizard** (`/dashboard/projects/new`): pick installation and repository, name and slug
   (live availability check), root directory, production branch, regions (from team-enabled regions), BUILD
   and RUNTIME env vars, "Deploy" button.
3. **Project page** (`/dashboard/projects/[projectId]`, currently a placeholder): production URL and active
   deployment, deployments list (status, branch, commit, target, author, age), "Deploy" dialog (branch
   picker), domains summary.
4. **Deployment page** (`/dashboard/deployments/[deploymentId]`; sidebar already links to
   `/dashboard/deployments`): status timeline from `deployment_events` and Trigger.dev realtime
   (`@trigger.dev/react-hooks` `useRealtimeRun` with the public token), live log viewer (SSE from
   `/api/meshscale/api/v1/builds/{id}/logs`; the proxy must stream instead of buffering, and pass
   `accept: text/event-stream`), actions: cancel, promote/rollback, redeploy, open URL.
5. **Domains** (project settings and Settings -> Domains): add domain, show required DNS records and
   verification state, remove.
6. **Environment variables** (project settings): table by type and region, write-only sensitive values,
   validation messages that match 5.2.
7. SDK: regenerate `@meshscale/sdk` from the updated `openapi.json`.

The frontend's API proxy only forwards `accept`, `content-type`, `if-none-match` and
`x-meshscale-organization` and buffers request bodies [FROM SOURCE]; that is fine for JSON, and for SSE the
response body is already streamed (`new NextResponse(response.body)`), so only the `accept` header needs to
reach the API.

---

## 14. Observability

- Every request and task carries `deploymentId`/`buildId` as correlation ids (API `X-Request-ID`, Trigger
  tags, agent log fields, Lambda tags).
- Metrics: build queue wait, build duration per stage, failure codes, cold start (`init_duration_ms`),
  promotion latency, edge decisions (EDGE-PLAN 7), orchestrator slot usage and disk.
- Alerts: build failure rate per hour, any deployment stuck past deadline (from `reconcile`), cert expiry
  < 7 days, Lambda code storage > 70% [VERIFY quota], agent disk > 80%.

---

## 15. Testing strategy

| Layer | What |
|---|---|
| db | Migration up on a copy of production data (Neon branch); enum additions; backfills; encryption migration round-trip |
| api | Route tests for every new endpoint (auth, org scoping, roles, validation), webhook signature tests with GitHub's published examples, idempotency of deployment creation |
| contracts | zod schemas shared by agent and workflows; JSON fixtures for start-build and completion payloads |
| agent | Unit: request validation, cert-SAN authorization, redaction, slot accounting. Integration on a Linux CI runner with Docker: run the real image on `builder/test/fixtures/next-app` end to end with a local MinIO standing in for S3/R2 and a fake waitpoint server; kill-and-restart recovery; timeout; cancel; workspace always removed |
| workflows | Trigger.dev local dev (`trigger dev`) against a staging database, the sandbox AWS account and a staging orchestrator; `aws-sdk-client-mock` unit tests for each Lambda step including crash-retry convergence |
| edge | EDGE-PLAN section 8 |
| end to end | Staging: connect a test GitHub org, create a project from a fixture repo, push -> preview URL works; merge to main -> production domain switches; roll back; add a custom domain on a test zone; delete project -> cleanup |
| security | mTLS negative tests (no cert, wrong CA, expired, wrong SAN -> rejected); phase A container cannot reach IMDS, the agent port, private ranges, or read the R2 token; token revoked after clone; secrets absent from logs |

---

## 16. Implementation phases

Each phase ends with its tests green, a short written result, and a review stop. Phases P2/P3 and P6
can run in parallel with P1 once P0 is done.

### P0. Foundations (no product code)
- Commit the real `meshscale_v1.ts` (and `cloudtrail.ts`) to the `db` repo; tag the current published
  version; make `api` build against it.
- Create: Cloudflare zone `meshscale.app`; R2 buckets `meshscale-static`, `meshscale-edge-config`,
  `meshscale-build-logs`; AWS functions account with the deployer role and boundary, artifacts bucket,
  edge invoker user, control-account user; GitHub App (permissions: Contents read, Metadata read; events:
  push, installation, installation_repositories; Setup URL; webhook URL); Trigger.dev project with dev,
  staging and prod environments; internal CA. Infrastructure as code (Terraform) for AWS and Cloudflare.
- Acceptance: a checklist with every resource id recorded (no secrets) in the orchestrator repo.

### P1. Schema and API corrections
- Section 4 migration, publish `@meshscale/db` 0.3.0.
- Fix 1.2 items 1-3 and 9; remove status writes from the public PATCH; `ApiError` codes.
- Extended `POST /projects` with name, slug, settings (repository fields come in P7).
- Acceptance: `pnpm test` (tsc) passes in `api`; route tests; migration tested on a Neon branch.

### P2. Builder image and builder flags (this repository)
- Section 10 items 1-6.
- Acceptance: CI builds the image, the fixture app builds and its Lambda entry answers inside the image;
  `--progress json` covered by tests; README updated.

### P3. Orchestrator agent
- `orchestrator/contracts` + `orchestrator/agent`: mTLS server, routes, SQLite state, two-phase run, log
  streaming and redaction, zip upload to a presigned URL, waitpoint completion, cancel, restart recovery,
  host provisioning script (Docker, gVisor, iptables, quotas).
- Acceptance: agent integration tests (section 15) pass against the real image; a staging host runs it.

### P4. Workflows: build path
- Trigger.dev project; `deployment-pipeline` (build stages only), `build-run`, `artifact-verify`,
  `reconcile`; DB role `meshscale_workflows`; API `POST /projects/{id}/deployments` triggers it.
- Acceptance: triggering a deployment for the fixture repo produces a verified `artifacts` row, a zip in
  S3, static files and manifest in R2, a log in R2; cancel and timeout paths tested.

### P5. Workflows: Lambda deployment
- `lambda-deploy`, health checks, `runtimes`/`lambda_functions` rows, deployment-URL host binding.
- Acceptance: the fixture deployment reaches READY in the sandbox account; `test/lambda/invoke-matrix.cjs`
  passes against its alias; a broken build (init throws) ends FAILED at health check.

### P6. Edge (EDGE-PLAN E0-E3) on `meshscale.app`
- Acceptance: the deployment URL from P5 serves static files from R2 and dynamic routes from Lambda;
  golden parity per EDGE-PLAN.

**Milestone: first end-to-end deployment through the API.**

### P7. GitHub App, projects and the dashboard
- API GitHub routes and webhook; project repository settings; frontend sections 13.1-13.4 and 13.7;
  promotion and rollback (`deployment-promote`), managed production domain bindings.
- Acceptance: from a fresh org, connect GitHub, create a project, deploy, open the production URL, roll back.

### P8. Push-to-deploy and previews
- Webhook push handling, coalescing, previews per team settings, optional GitHub commit statuses.
- Acceptance: push to a branch creates a preview URL; push to production branch promotes.

### P9. Custom domains and environment variables
- Domain workflows and UI (3.4, 13.5); env var encryption, routes and UI (5.2, 13.6); RUNTIME vars applied
  per region on the next deployment.
- Acceptance: a test custom domain becomes ACTIVE with TLS and serves the project; env var changes appear
  in the next deployment and never in logs.

### P10. Hardening and operations
- Pruning and retention jobs, cert rotation automation, alerts (section 14), notifications using
  `team_notification_sets` (deployment success/failure, build failure, domain changes), load test of the
  agent with concurrent builds, security review of the whole path, runbooks (stuck deployment, rotate each
  secret, add a build host, restore an edge binding).

---

## 17. Open questions for the owner

1. **Domain `routing_type`:** is `DIRECT`/`INDIRECT` meant as in 4.2 (edge-direct vs customer proxy), or
   something else (for example region-specific vs global routing)?
2. **Region model:** should one deployment build fan out to every enabled project region (this plan), or
   only to the default region until multi-region edge routing exists? And which AWS region does each
   `regions` row map to?
3. **Who may deploy:** which better-auth organization roles may deploy, promote and manage domains/env vars
   (this plan: `owner` and `admin` for promote/domains/env; any member may create preview deployments)?
4. **Build limits per plan:** default timeout (30 min), CPU/memory per build, concurrent builds per org.
5. **Hosting of the agent:** EC2 in the functions account, a separate "build" account (recommended, so a
   container escape lands in an account with nothing in it), or another provider.
6. **Image registry:** GHCR or ECR.
7. **Previews:** public by default (v1) or protected from day one?
8. **Logs retention** for build logs and Lambda CloudWatch logs.
