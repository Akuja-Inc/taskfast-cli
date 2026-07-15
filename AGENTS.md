# Overview

This is the TaskFast CLI for autonomous agents. It is a thin orchestrator over the taskfast API, not a place where business logic or policy lives. The API is the single source of truth; the CLI's job is to call endpoints and stitch them together, plus the one thing only it can do (sign with the poster's passkey, since v2 is non-custodial and the server holds no party keys). It should never compute or hardcode values the server owns.

### Deliberate client-side exceptions

These look like violations of the above but are intentional — don't "fix" them:

- **Fee-token allowlist** (`taskfast-chains/src/tempo/mod.rs`): a refuse-to-sign guard against a compromised server steering the fee `transfer` to an arbitrary ERC-20 — signing safety is the CLI's one unique job, so it deliberately does not trust the field it guards. Switch to server-sourced values once the API exposes a per-network fee-token address list.
- **Client-operational tuning**: HTTP/receipt timeouts, poll cadence, retry backoff — the CLI's own runtime behavior, not protocol policy.
- **Env→network mapping** (`taskfast-cli/src/lib.rs`): transitional until the server's one-network invariant lets the CLI select the sole advertised `/config/network` entry.
- **`taskRef` derivation** (`cmd/bond.rs`): verifier protocol layout computed client-side because it feeds locally-signed calldata; ideally server-issued bond-post params later.
- **Webhook `--default-events` list** (`cmd/webhook.rs`): CLI convenience default for a flag, not server policy.
- **Active-status filters** (`cmd/task.rs`, `cmd/bid.rs`): presentation-only workarounds for missing server-side status query params; delete when the API grows them.

## Guidance

- Apply Red Green Refactor TDD

## Releasing

Releases are cut by **pushing an annotated tag** `taskfast-cli-vX.Y.Z`, which
triggers `release.yml` (cargo-dist binaries), `docker.yml`, and
`publish-crates.yml` (crates.io). The intended entry point is the `bump.yml`
workflow (Actions → "Release bump"), or the documented local fallback:

```bash
cargo xtask bump <patch|minor|major>   # bumps versions + Cargo.lock
git commit -am "chore(release): vX.Y.Z"
git tag -a taskfast-cli-vX.Y.Z -m "Release X.Y.Z"
git push --follow-tags
```

**Cross-crate publish gotcha (load-bearing).** `taskfast-client` is on its own
`0.x` version line and is consumed by `taskfast-cli`. `cargo publish` verifies
`taskfast-cli`'s tarball against the **published** `taskfast-client`, not the
local path dep — so if `taskfast-client` gains public API but isn't bumped, the
crates.io publish of `taskfast-cli` fails (`E0425: cannot find function …`),
*after* the tag is already cut. **Local CI and `cargo-semver-checks` cannot
catch this** (they build with path deps). `cargo xtask bump` now bumps
`taskfast-client` in lockstep (its own version line + the workspace dep
requirement) to prevent this; if you ever bump versions by hand, do the same.
This bit the v0.9.0 release (gh#85) and required a recovery republish.

**Before tagging**, prefer validating the publish chain: dispatch
`publish-crates.yml` with `dry_run: true`, or run `cargo publish -p taskfast-cli
--dry-run` against the registry — failures there are cheap; a bad tag is not.