# Agent orchestration

Shared logic the CLI calls to bring an Agent online and keep it signing safely: Bootstrap, wallet / keystore handling, webhook delivery + verification, Tempo RPC, lifecycle event polling, retry policy.

Spans: `crates/taskfast-agent/src/`.

## Language

### Bootstrap

**Bootstrap**:
The sequence `init` runs to bring an Agent to ready: validate auth → fetch Readiness → optionally provision/register a Wallet → optionally configure a Webhook → final readiness assert.
_Avoid_: setup, init flow (`init` is the CLI command surface; Bootstrap is the orchestration sequence).

**Validate auth**:
The single round-trip that confirms the API key resolves to a live Agent identity (`bootstrap::validate_auth`).
_Avoid_: ping, healthcheck.

### Wallet

**Wallet**:
A Tempo (EVM) address plus a way to access its private key for signing — the address half is registered server-side once via `POST /agents/me/wallet`.
_Avoid_: account (collides with server Identity), keypair.

**Keystore**:
The local password-encrypted JSON file holding the wallet's private key.
_Avoid_: key file, vault.

**Keystore source** (`TEMPO_KEY_SOURCE`):
The indirection that resolves to a Keystore — accepts a file path or other configured form; consumed by `taskfast-cli` via `wallet_args`.
_Avoid_: key path (the value isn't always a path).

**Password file** (`TASKFAST_WALLET_PASSWORD_FILE`):
The on-disk file holding the Keystore decryption password; the password-via-env path is permitted but flagged with a `password_env_var` Security warning.
_Avoid_: secret file.

**Wallet lock**:
The advisory exclusive file-lock at `<keystore_dir>/.taskfast-wallet.lock` held across the `eth_getTransactionCount` → `eth_sendRawTransaction` critical section to prevent nonce collisions between concurrent same-host post invocations; same-host only — multi-host setups need external coordination.
_Avoid_: mutex, signing lock.

**Faucet**:
The `init --generate-wallet --fund` testnet-only path that requests Tempo testnet drops for a freshly-generated wallet; prod has no automated funding.
_Avoid_: airdrop, top-up.

### Webhook

**Webhook**:
The Agent-hosted HTTPS endpoint registered via `PUT /agents/me/webhooks` that receives Lifecycle event deliveries.
_Avoid_: callback, listener.

**Webhook signing secret**:
The shared HMAC key returned exactly once at webhook registration; persisted by the CLI as a `0600` file referenced by `webhook_secret_path` in Config.
_Avoid_: webhook key, signing key (collides with wallet signing — see flagged ambiguities).

**Webhook signature**:
The HMAC-SHA256 over the raw delivery body using Webhook signing secret, expected in the delivery header — verifiable via `POST /agents/me/webhooks/verify` or locally.

### Tempo RPC

**Tempo RPC proxy**:
The authenticated HTTP JSON-RPC endpoint at `{api_base}/api/rpc/{network}` that fronts the Tempo network for the CLI; `X-API-Key` authenticates the proxy itself.
_Avoid_: RPC URL (the override flag, distinct from the proxy), node.

### Lifecycle events

**Lifecycle event**:
A single record in the Agent's event stream — pulled via `GET /agents/me/events` (one page per call, surfaced as `taskfast events poll`).
_Avoid_: notification (Webhook deliveries are notifications; Lifecycle events are the pull form).

### Retry

**Retry policy**:
The 3× attempts / 10s backoff applied to AI-generation and RPC operations that surface transient "no response" / 5xx errors.
_Avoid_: backoff loop, reconnect.

## Relationships

- **Bootstrap** consumes **Readiness** (root context) and may produce/register a **Wallet**.
- A **Wallet** is held under **Keystore**, decrypted via **Password file**, guarded by **Wallet lock** during signing.
- **Webhook signing secret** is returned exactly once; subsequent reads of `GET /agents/me/webhooks` return `null` for the secret field.
- **Tempo RPC proxy** is the default RPC path; an override (`--rpc-url`) requires **Allow-custom-endpoints** (root context).
- **Webhook** deliveries and **Lifecycle event** polling are alternative surfaces for the same event stream.

## Example dialogue

> **Dev:** "Two `taskfast post` jobs are running against the same wallet — will nonces collide?"
> **Domain expert:** "Same host, no — the **Wallet lock** serializes them across `eth_getTransactionCount`/`eth_sendRawTransaction`. Multi-host, yes — operators must coordinate per-host."

> **Dev:** "Where do I retrieve a lost webhook secret?"
> **Domain expert:** "You don't — **Webhook signing secret** is one-shot at registration. Re-run `taskfast webhook register` to mint a new one and update consumers."

> **Dev:** "Should the agent poll events if it has a webhook configured?"
> **Domain expert:** "Either, not both. **Webhook** is push, **Lifecycle event** poll is pull. Use poll for crash-recovery catch-up; push for steady state."

## Flagged ambiguities

- **"signing key" overload** — used in some prose to mean wallet ECDSA key *and* webhook HMAC key. Lock: "wallet signing key" for the ECDSA private key; "Webhook signing secret" for the HMAC key. Never bare "signing key".
