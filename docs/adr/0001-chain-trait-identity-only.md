# Chain trait is identity-only; signing lives as free functions per-chain

The `Chain` trait in `crates/taskfast-chains` exposes only identity (`id`, associated types) — no signing or approval-codec methods. Each chain module instead provides free functions (e.g. `tempo::sign_distribution`, `tempo_rpc::sign_and_broadcast_erc20_transfer`).

In v1 only **Tempo** has a body; the other Chain impls are stubs. Locking a uniform signing API into the trait now would freeze a shape we have only one data point for, and any non-Tempo impl would either fight the abstraction or force a breaking trait change. Free functions per-chain let each implementation grow independently; when a second chain ships signing, the common surface (if any) can be lifted into a separate `ChainSigner` trait without touching `Chain`.

Mirrors the Elixir split where `TaskFast.Chain` is identity + RPC and `ApprovalCodec` is a separate behaviour.
