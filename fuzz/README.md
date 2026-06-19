# Fuzzing (`cargo-fuzz` / libFuzzer)

Coverage-guided fuzz harnesses for the parser/decoder surfaces that run on
untrusted input. Fuzzing finds panics, unbounded recursion, and parser
mismatches that the property tests don't reach.

> Resolves [#32](https://github.com/Akuja-Inc/taskfast-cli/issues/32).

## Targets

| Target             | Exercises                                                                 | Untrusted source |
| ------------------ | ------------------------------------------------------------------------- | ---------------- |
| `config_parse`     | `serde_json::from_slice::<taskfast_cli::Config>` — the `.taskfast/config.json` serde path behind `Config::load` | config file in a cloned repo (F2 guard) |
| `chain_validation` | `taskfast_chains::tempo::{is_known_network, is_allowed_fee_token}` — chain-id check + hex fee-token normalization | config / CLI / server values |
| `response_decode`  | `serde_json::from_slice` of `taskfast_client::{NetworkConfigResponse, UserProfile}` | API response bodies off the wire |

> **Note on "envelope decoding":** the CLI's output `Envelope` is
> `Serialize`-only, so it is not a decode surface. The untrusted-decode path is
> the `taskfast-client` response types, which is what `response_decode` covers.

Harnesses are deterministic and single-threaded — each is a pure function of
its input bytes, with no clock, RNG, threads, or I/O.

## Prerequisites

```sh
rustup toolchain install nightly      # libFuzzer needs nightly + a sanitizer
cargo install cargo-fuzz --locked
```

## Running

From the **repo root** (not this directory):

```sh
cargo +nightly fuzz list                       # config_parse, chain_validation, response_decode
cargo +nightly fuzz run config_parse            # run until a crash (Ctrl-C to stop)
cargo +nightly fuzz run config_parse -- -max_total_time=60   # time-boxed
```

Crashing inputs are written to `fuzz/artifacts/<target>/`. Reproduce one with:

```sh
cargo +nightly fuzz run config_parse fuzz/artifacts/config_parse/crash-<hash>
```

## Corpus

Minimal seeds live in `fuzz/corpus/<target>/` and are checked in. libFuzzer
grows the corpus in place as it discovers new coverage; only the seeds are
tracked (see `.gitignore`).

## Adding a target

1. Create `fuzz_targets/<name>.rs` with `#![no_main]` + a `fuzz_target!` body
   calling a `pub` item from a fuzzed crate.
2. Add a matching `[[bin]]` block to `Cargo.toml`.
3. Drop a seed or two in `corpus/<name>/`.
4. Add the target to the matrix in `.github/workflows/fuzz.yml`.

Keep targets deterministic: no time, randomness, network, or filesystem.
