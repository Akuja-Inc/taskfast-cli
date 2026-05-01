# TaskFast CLI

Rust workspace for the `taskfast` CLI and supporting crates — the toolchain an autonomous **Agent** uses to bid, post, settle, and listen on the TaskFast marketplace.

## Contexts

- Root (this file) — CLI surface: command shapes, envelope, exit codes, config, env/network rules, two-phase post, headless agent creation. Owns `crates/taskfast-cli/`.
- [Agent orchestration](./crates/taskfast-agent/CONTEXT.md) — bootstrap, wallet/keystore, webhook signing, lifecycle events, Tempo RPC, retry policy.
- [Chains](./crates/taskfast-chains/CONTEXT.md) — chain abstraction (`Chain` trait, `AnyChain`, Tempo).

`crates/taskfast-client/` is a thin HTTP wrapper over `spec/openapi.normalized.yaml`; the OpenAPI spec is SSOT, no separate context.

## Language

### Actors

**Agent**:
The autonomous CLI caller — primary actor of this entire toolchain; the server-side `Agent` model, the `taskfast-agent` crate, and the `skills/taskfast-agent/` skill are all surfaces of this one concept.
_Avoid_: caller, principal, client (overloaded with `taskfast-client`).

**User**:
A human; distinct from Agent and appears narrowly during `init --human-api-key` to bootstrap an Agent identity.
_Avoid_: poster (Poster is a role, see below).

**Worker**:
The role an Agent plays when bidding on or executing a Task.
_Avoid_: assignee, contractor.

**Poster**:
The role an Agent plays when creating a Task.
_Avoid_: requester, client.

### Server contracts

**Profile**:
The server-side identity record returned by `GET /agents/me` — name, description, rate, capabilities; mutable via `PUT /agents/me`.
_Avoid_: account, identity, me (the CLI command, see below).

**Readiness**:
The server-side onboarding gate returned by `GET /agents/me/readiness` — per-check status (api_key/wallet/webhook) plus `ready_to_work`; consumed by Bootstrap.
_Avoid_: status, health.

**`taskfast me`**:
A CLI affordance that composes Profile and Readiness into one envelope; the bundling is CLI-only, not a server concept.

### Output contract

**Envelope**:
The JSON document every command emits on stdout, success or error: `{ ok, environment, dry_run, data | error, security_warnings }`.
_Avoid_: response, output, payload.

**Security warning**:
A non-fatal observation on the Envelope keyed by stable `code` (e.g. `custom_api_base`, `custom_tempo_rpc`, `password_env_var`); the array is always present, possibly empty.
_Avoid_: alert, notice.

**Exit code**:
The deterministic numeric process status mapped from `CmdError` and pinned in `crates/taskfast-cli/src/exit.rs` — `0`/`2`/`3`/`4`/`5`/`6`/`7`/`70` are stable contract for orchestrators; changing one is breaking.
_Avoid_: status, return code.

### Configuration

**Environment**:
The single source of truth for both API base and network — `prod | staging | local`, selected via `--env` or `TASKFAST_ENV`; mapping is total and frozen (see `docs/NETWORK.md`).
_Avoid_: stage, deployment.

**API base**:
The HTTPS root of a TaskFast deployment, derived from Environment; overridable per-invocation via `--api-base` / `TASKFAST_API`, never persisted.
_Avoid_: host, endpoint, server URL.

**Allow-custom-endpoints**:
The opt-in security gate (`--allow-custom-endpoints` / `TASKFAST_ALLOW_CUSTOM_ENDPOINTS=1`) required when API base or RPC URL falls outside the well-known set.
_Avoid_: insecure mode, override mode.

**Config**:
The persisted JSON file at `./.taskfast/config.json` (overridable via `--config` / `TASKFAST_CONFIG`), `schema_version=2`, written `0600` on unix via atomic temp+rename; precedence is `flag > env > config > default`.
_Avoid_: settings, profile (Profile is the server identity).

**Dry-run**:
The `--dry-run` mode that short-circuits server mutations while preserving reads — asymmetric, not a no-op.
_Avoid_: preview, simulate.

### Lifecycle flows

**Two-phase post**:
The `taskfast post` flow: prepare draft (`POST /api/task_drafts`) → locally sign+broadcast the ERC-20 submission-fee transfer → submit the draft using the resulting tx hash as Voucher.

**Voucher (tx-hash form)**:
The `0x`-prefixed 64-hex transaction hash submitted to `POST /api/task_drafts/{id}/submit`; the server polls for confirmation. The alternate raw-RLP voucher form is accepted by the server but is not the path this CLI takes.
_Avoid_: receipt, proof.

**Headless agent creation**:
The `init --human-api-key <tf_user_*>` path that mints an Agent server-side from a User PAT; the persisted Config thereafter holds the agent API key, not the User PAT.
_Avoid_: bootstrap (Bootstrap is the agent-orchestration sequence — see crate context).

**Skill**:
A markdown contract under `skills/`; `skills/taskfast-agent/` documents how an autonomous Agent boots, bids, posts, recovers, and settles.
_Avoid_: doc, guide.

## Relationships

- Every command emits exactly one **Envelope**; **Exit code** is set in lockstep with envelope `ok` / `error.code`.
- **Environment** determines **API base** and the Tempo network; an override of either requires **Allow-custom-endpoints** unless the value is well-known.
- **`taskfast me`** = Profile + Readiness; either can also be fetched in isolation via the underlying server endpoints.
- **Two-phase post** is the only command that signs locally; everything else is API call + Envelope.

## Example dialogue

> **Dev:** "Why does `me` return both name *and* wallet status? Are those one thing?"
> **Domain expert:** "Different concepts, two endpoints — `me` is **Profile**, `me/readiness` is **Readiness**. The CLI command bundles them; an SDK building a UI dashboard should call them separately."

> **Dev:** "Can I pass `--network=mainnet` to override post? README shows it."
> **Domain expert:** "No — README is stale. Network is derived from **Environment**, no flag exists. To target mainnet pass `--env prod`."

> **Dev:** "If a `post` 4xxs after I've already broadcast the fee transfer, did I lose the money?"
> **Domain expert:** "Funds are in your wallet's outgoing tx — the **Voucher** is just the hash. Re-running `post` with the same hash idempotently submits; the server polls confirmation either way."

## Flagged ambiguities

- **README `--network` flag drift** — `README.md` example for `taskfast post` shows `--network=mainnet|testnet`; code derives network from Environment and `docs/NETWORK.md` confirms no such flag exists. Likely pre-v0.6.0 residue from before the env-as-SSOT change (commit 302ea22). Needs README fix.

- **`payload_to_sign` is misnamed** — in the post flow the field is the encoded ERC-20 `transfer` calldata, not an ECDSA signing payload. Module doc in `crates/taskfast-cli/src/cmd/post.rs` calls this out; rename pending.

- **README phrasing of `taskfast me`** — "profile + readiness in one envelope" describes the bundling, not the meaning of "me"; readers sometimes infer that `me` is itself a single concept. Profile and Readiness are distinct server contracts.
