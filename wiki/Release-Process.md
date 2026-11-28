# Release Process

> Canonical source: [`CONTRIBUTING.md — Release flow`](https://github.com/Akuja-Inc/taskfast-cli/blob/main/CONTRIBUTING.md#release-flow-maintainers-only). Maintainer-only.

Releases are triggered by pushing a git tag of the form `taskfast-cli-vX.Y.Z`. The tag push fires `.github/workflows/release.yml` (cargo-dist), which builds artifacts, pushes the Homebrew formula, and creates the GitHub Release with notes generated from Conventional Commits.

`cargo xtask bump <level>` bumps every version group in lockstep. The groups are listed in the canonical guide linked above.

## Option A — CI button (patch releases, recommended)

Actions tab → **Release bump (patch)** → **Run workflow** on `main`. CI runs `cargo xtask bump patch`, commits `chore(release): vX.Y.Z`, tags `taskfast-cli-vX.Y.Z`, and pushes via `RELEASE_PLZ_TOKEN`. The tag push triggers cargo-dist. No local action required.

**Prerequisites (one-time repo setup):**

- **Secret `RELEASE_PLZ_TOKEN`** — Personal Access Token with `Contents: Read and write` on this repo. Required instead of `GITHUB_TOKEN` because GitHub does not fire dependent workflows for pushes authenticated by `GITHUB_TOKEN` (anti-loop protection), so the release workflow would never trigger.
- **Branch protection on `main`** — the token owner (or bot account) must be permitted to push directly. Otherwise the CI commit will be rejected.

## Option B — local (minor / major releases, or full manual control)

Follow the recipe in [`CONTRIBUTING.md — Release flow`](https://github.com/Akuja-Inc/taskfast-cli/blob/main/CONTRIBUTING.md#release-flow-maintainers-only), Option B: bump, commit, run `scripts/publish-dry-run.sh`, then create an annotated tag and push.

## Scope notes

- `taskfast-cli` + `taskfast-agent` version in lockstep via `version.workspace = true`. `xtask` also inherits, but `publish = false`.
- The `taskfast-cli-v` prefix is a cargo-dist *Singular Announcement*: only the `taskfast-cli` binary is built and published, even though `taskfast-agent`'s Cargo version bumped in lockstep.

## After the release

- Verify `gh release view taskfast-cli-vX.Y.Z` shows the expected artifacts (shell installer, archives for each target, Homebrew formula push).
- Spot-check the Homebrew tap auto-update commit in the `akuja-inc/homebrew-taskfast` repo.
- `curl -LsSf https://github.com/Akuja-Inc/taskfast-cli/releases/latest/download/taskfast-cli-installer.sh | sh` sanity check on a clean machine.
