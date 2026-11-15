# Learnings

## StateStore Unification (GH#215)
- Modeled local state assets into an exhaustive `StateAsset` enum (`Config`, `HookSecret`, `EventsCursor`, `EventsSeen`), decoupling callers from relative path assumptions.
- By enforcing `OpenOptions::mode(0o600)` at creation time on Unix, atomic temp writes eliminate the momentary umask exposure window previously present in `persist_secret`.
- Ephemeral in-memory backend cleanly handles `/dev/null` config paths, eliminating scattered path checks (such as parent `/dev`) across commands.
- Unified state store in `Ctx` provides consistent access across subcommands without breaking test construction patterns relying on `Ctx { config_path, ..Default::default() }`.
