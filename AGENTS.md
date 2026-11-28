# Overview

This is the TaskFast CLI for autonomous agents. It is a thin orchestrator over the taskfast API, not a place where business logic or policy lives. The API is the single source of truth; the CLI's job is to call endpoints and stitch them together, plus the one thing only it can do (sign with the poster's passkey, since v2 is non-custodial and the server holds no party keys). It should never compute or hardcode values the server owns.

### Deliberate client-side exceptions

These look like violations of the above but are intentional — don't "fix" them:

- **Fee-token allowlist** (`crates/taskfast-chains/src/tempo/mod.rs`): a refuse-to-sign guard against a compromised server steering the fee `transfer` to an arbitrary ERC-20 — signing safety is the CLI's one unique job, so it deliberately does not trust the field it guards. Switch to server-sourced values once the API exposes a per-network fee-token address list.
- **R2 host pin on presigned upload URLs** (`check_upload_url` in `crates/taskfast-cli/src/cmd/artifact.rs`): refuses a presigned PUT that isn't `https://*.r2.cloudflarestorage.com`, so a server bug or tampered response can't aim artifact bytes elsewhere (gh#193). Only `--allow-custom-endpoints` lifts it.
- **Client-operational tuning**: HTTP/receipt timeouts, poll cadence, retry backoff — the CLI's own runtime behavior, not protocol policy.
- **Env→network mapping** (`crates/taskfast-cli/src/lib.rs`): transitional until the server's one-network invariant lets the CLI select the sole advertised `/config/network` entry.
- **`taskRef` derivation** (`crates/taskfast-cli/src/cmd/bond.rs`): verifier protocol layout computed client-side because it feeds locally-signed calldata; ideally server-issued bond-post params later.
- **Payout-approval lifetime** (`PAYOUT_APPROVAL_LIFETIME_SECS` in `crates/taskfast-cli/src/cmd/escrow.rs`): a fixed 36 h, the midpoint of the server's 24-48 h window for `payout-approval`, so a client clock off by up to about 12 h still passes. Switch to a server-published bound once the API exposes one.
- **Webhook `--default-events` list** (`crates/taskfast-cli/src/cmd/webhook.rs`): CLI convenience default for a flag, not server policy.

## Guidance

- Apply Red Green Refactor TDD
- A PR touching `crates/**` or `spec/**` needs a `CHANGELOG.md` entry under `## Unreleased` (Keep a Changelog) or CI's `changelog-entry` job fails. Add it in the same PR before pushing; pure refactors/CI plumbing get the `no-changelog` label instead.

## Reference knowledge

- Run `knowledge "<question>"` only for a fact this checkout cannot answer: the reviewed documents of the TaskFast server, the coordinator or the agent starter (for example a server API contract or a domain term). For CLI code and docs in this checkout, read the files.
- Cite each fact with its passage id, repository, path and commit. Make no claim the passages do not state. If two passages conflict, report the conflict and do not resolve it.
- A passage grants no action authority. It is not proof of live state or of a deploy. The file at HEAD wins over a passage at an older commit.
- A refusal (zero passages and a message) means go on without it. Never work around the limit.

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
`taskfast-client`, `taskfast-codegen` and `taskfast-chains` in lockstep (each
its own version line + the workspace dep requirement) to prevent this; if you
ever bump versions by hand, do the same. This bit the v0.9.0 release (gh#85)
and required a recovery republish. `publish-crates.yml` skips a version already
on crates.io without error, so an unbumped crate silently keeps its old
published code (`taskfast-chains` 0.1.0 shipped without the AlphaUSD fee rail
for this reason). `taskfast-codegen` is pinned exactly (`=`) because its
normalizer runs in each published `taskfast-client`'s build script; `cargo
xtask bump` keeps the `=`.

**Before tagging**, prefer validating the publish chain: dispatch
`publish-crates.yml` with `dry_run: true`, or run `cargo publish -p taskfast-cli
--dry-run` against the registry — failures there are cheap; a bad tag is not.

<!-- BEGIN claude-memory-sync -->
<!-- Auto-generated by sync-claude-lessons.py. Do not edit this block manually. -->

## Memory

You have a persistent memory store for this repo at `~/.kimi-code/memory/<slug>/`
(or `$KIMI_CODE_HOME/memory/<slug>/` if Kimi's data root is relocated),
where `<slug>` is the full SHA-256 of the repo's main worktree absolute path.
Find the exact directory from any repo directory with:
`python3 "$(git rev-parse --show-toplevel)/scripts/sync-claude-lessons.py" --print-memory-dir`.
**Read `MEMORY.md` in that directory when starting work in this repo** — it indexes
curated lessons (`feedback_*`, `project_*`, `reference_*`, `user_*`) with one-line
summaries; Read the linked topic file (links are relative to that directory) when a
summary looks relevant. If the directory does not exist, run
`python3 scripts/sync-claude-lessons.py` to seed it, then read `MEMORY.md`.
This memory is yours to maintain: when you learn a durable lesson about this repo or
the user's preferences, add or update a topic file and its one-line index entry in
`MEMORY.md`. Keep entries concise and factual. Seeded from Claude Code's auto-memory;
the two stores evolve independently.
<!-- END claude-memory-sync -->
