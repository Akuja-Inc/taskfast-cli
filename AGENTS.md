# Overview

This is the TaskFast CLI for autonomous agents.

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

<!-- BEGIN BEADS INTEGRATION -->
## Issue Tracking with bd (beads)

**IMPORTANT**: This project uses **bd (beads)** for ALL issue tracking. Do NOT use markdown TODOs, task lists, or other tracking methods.

### Why bd?

- Dependency-aware: Track blockers and relationships between issues
- Git-friendly: Dolt-powered version control with native sync
- Agent-optimized: JSON output, ready work detection, discovered-from links
- Prevents duplicate tracking systems and confusion

### Quick Start

**Check for ready work:**

```bash
bd ready --json
```

**Create new issues:**

```bash
bd create "Issue title" --description="Detailed context" -t bug|feature|task -p 0-4 --json
bd create "Issue title" --description="What this issue is about" -p 1 --deps discovered-from:bd-123 --json
```

**Claim and update:**

```bash
bd update <id> --claim --json
bd update bd-42 --priority 1 --json
```

**Complete work:**

```bash
bd close bd-42 --reason "Completed" --json
```

### Issue Types

- `bug` - Something broken
- `feature` - New functionality
- `task` - Work item (tests, docs, refactoring)
- `epic` - Large feature with subtasks
- `chore` - Maintenance (dependencies, tooling)

### Priorities

- `0` - Critical (security, data loss, broken builds)
- `1` - High (major features, important bugs)
- `2` - Medium (default, nice-to-have)
- `3` - Low (polish, optimization)
- `4` - Backlog (future ideas)

### Workflow for AI Agents

1. **Check ready work**: `bd ready` shows unblocked issues
2. **Claim your task atomically**: `bd update <id> --claim`
3. **Work on it**: Implement, test, document
4. **Discover new work?** Create linked issue:
   - `bd create "Found bug" --description="Details about what was found" -p 1 --deps discovered-from:<parent-id>`
5. **Complete**: `bd close <id> --reason "Done"`

### Auto-Sync

bd automatically syncs via Dolt:

- Each write auto-commits to Dolt history
- Use `bd dolt push`/`bd dolt pull` for remote sync
- No manual export/import needed!

### Important Rules

- ✅ Use bd for ALL task tracking
- ✅ Always use `--json` flag for programmatic use
- ✅ Link discovered work with `discovered-from` dependencies
- ✅ Check `bd ready` before asking "what should I work on?"
- ❌ Do NOT create markdown TODO lists
- ❌ Do NOT use external issue trackers
- ❌ Do NOT duplicate tracking systems

For more details, see README.md and docs/QUICKSTART.md.

## Landing the Plane (Session Completion)

**When ending a work session**, you MUST complete ALL steps below. Work is NOT complete until `git push` succeeds.

**MANDATORY WORKFLOW:**

1. **File issues for remaining work** - Create issues for anything that needs follow-up
2. **Run quality gates** (if code changed) - Tests, linters, builds
3. **Update issue status** - Close finished work, update in-progress items
4. **PUSH TO REMOTE** - This is MANDATORY:
   ```bash
   git pull --rebase
   bd sync
   git push
   git status  # MUST show "up to date with origin"
   ```
5. **Clean up** - Clear stashes, prune remote branches
6. **Verify** - All changes committed AND pushed
7. **Hand off** - Provide context for next session

**CRITICAL RULES:**
- Work is NOT complete until `git push` succeeds
- NEVER stop before pushing - that leaves work stranded locally
- NEVER say "ready to push when you are" - YOU must push
- If push fails, resolve and retry until it succeeds

<!-- END BEADS INTEGRATION -->
