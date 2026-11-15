# Backlog

## GH#215: Deepen .taskfast/ local state and secret persistence into unified StateStore

### Goal
Consolidate `.taskfast/` local state directory persistence into a unified, deep `StateStore` module.

### Definition of Done
1. Atomic `0600`-from-creation file creation is enforced consistently for secrets and config.
2. `events::seen` no longer depends on `events::poll` path internals.
3. When `config_path` is `/dev/null`, in-memory storage handles all state without disk writes.
4. Unit and integration tests verify directory creation, permissions, and atomic rename error recovery.
5. All workspace tests and clippy checks pass.
6. `CHANGELOG.md` contains an entry under `## Unreleased`.

### Step-by-Step Implementation Plan
1. **Scaffold & Types**: Create `crates/taskfast-cli/src/state_store.rs` with `StateAsset` enum (`Config`, `HookSecret`, `EventsCursor`, `EventsSeen`), `StateBackend` enum (`Disk`, `Memory`), and `StateStore` struct.
2. **Directory & Atomic Write Primitives**: Implement `0700` directory creation, automatic `.taskfast/.gitignore` provisioning, and atomic file writes using `OpenOptions::mode(0o600)` on Unix.
3. **In-Memory Adapter**: Support `/dev/null` sentinel path for zero-disk in-memory execution.
4. **Migrate `config.rs`**: Delegate `Config::load` and `Config::save` to `StateStore`.
5. **Migrate `cmd/webhook.rs` & `cmd/init.rs`**: Replace umask-vulnerable `persist_secret` with `StateStore` atomic write.
6. **Migrate `cmd/events/poll.rs` & `cmd/events/seen.rs`**: Decouple cursor and seen persistence through `StateStore`.
7. **Verification**: Add comprehensive unit tests in `state_store.rs` and verify existing test suites across the workspace.
8. **Changelog & Polish**: Update `CHANGELOG.md` under `## Unreleased`.
