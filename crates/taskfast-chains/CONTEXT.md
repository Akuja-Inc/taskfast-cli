# Chains

Chain abstraction for the TaskFast SDK — v1 ships **Tempo** only; other chains are compile-only **Stub chains** that exist to keep the feature-flag matrix honest in CI.

Spans: `crates/taskfast-chains/src/`.

## Language

**Chain**:
The trait providing identity (`id() -> &'static str`) plus associated-type seats for per-chain primitives (`Address`, `Signature`, `TxHash`, `EscrowRef`, `Network`); deliberately identity-only — no signing surface.
_Avoid_: blockchain, network (Network here is an associated type, not the trait).

**AnyChain**:
The enum dispatch wrapper that lets call sites hold "some Chain" without monomorphizing on the concrete type.
_Avoid_: dyn Chain, ChainEnum.

**Tempo**:
The only Chain impl with a body in v1 — TaskFast's settlement chain (mainnet `chain_id=4217`, testnet `chain_id=42431`).
_Avoid_: TaskFast chain (overloaded), L2.

**Stub chain**:
A `Chain` impl with `()` for every associated type, present to exercise the feature-flag matrix; calling stub-chain code is a compile error you don't reach. Currently: Avalanche, Polygon, Solana, NEAR, Stellar.
_Avoid_: placeholder, mock.

## Relationships

- **Chain** is the contract; **Tempo** and the **Stub chains** implement it.
- **AnyChain** wraps any `Chain` impl for dynamic dispatch.
- Signing functions (e.g. `tempo::sign_distribution`, `tempo_rpc::sign_and_broadcast_erc20_transfer`) live as **free functions per-chain**, not on the trait — see [`docs/adr/0001-chain-trait-identity-only.md`](../../docs/adr/0001-chain-trait-identity-only.md).

## Example dialogue

> **Dev:** "Why does `Avalanche` exist if it does nothing?"
> **Domain expert:** "It's a **Stub chain** — keeps the `--features avalanche` build path live in CI so a future implementation drops in without surprising compile failures. Mirrors the Elixir `TaskFast.Chain` behaviour split (beads am-6v7b.1 / am-6v7b.2)."

> **Dev:** "Can I add `sign_distribution` to the `Chain` trait so call sites don't have to know they're on Tempo?"
> **Domain expert:** "Not yet — see ADR-0001. With one real impl we'd be freezing a shape from one data point. When the second chain ships signing, lift the common surface into a `ChainSigner` trait without disturbing `Chain`."
