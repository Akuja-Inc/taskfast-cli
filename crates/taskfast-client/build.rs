// SPDX-License-Identifier: MIT
//! Generates the typed TaskFast client from `spec/openapi.yaml`.
//!
//! Pipeline:
//!   1. Read the authoritative spec (`openapi.yaml`, bundled into the crate
//!      for published builds — see `include` in Cargo.toml).
//!   2. Normalize in-memory via `taskfast_codegen::normalize_spec` — folds
//!      structurally identical error aliases into `#/components/schemas/Error`
//!      so progenitor emits a single `Error` type instead of duplicates.
//!   3. Feed the normalized spec to `progenitor::Generator`.
//!   4. Write the rendered Rust to `$OUT_DIR/codegen.rs`; `src/lib.rs` uses
//!      `include!` to pull it into the crate.
//!
//! Spec resolution (works both in-tree and inside a published tarball):
//!   * in-tree workspace dev: `<manifest>/../../spec/openapi.yaml`
//!   * published package:    `<manifest>/openapi.yaml` (bundled via `include`)
//! The xtask/codegen library source intentionally does not rerun-if-changed —
//! it's a regular Rust dep and Cargo already tracks its compilation freshness.

use std::{env, fs, path::PathBuf};

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    // Candidate spec locations, in priority order. The bundled copy
    // (`<manifest>/spec/openapi.yaml`) ships inside the published package so
    // `cargo publish` verification (which builds the tarball in isolation,
    // with no workspace root to walk up to) can still run codegen. The
    // workspace path is the dev/source-of-truth location used in-tree.
    let candidates = [
        manifest_dir.join("spec/openapi.yaml"), // bundled (published package)
        manifest_dir.join("../../spec/openapi.yaml"), // in-tree workspace
    ];
    let spec_path = candidates.iter().find(|p| p.exists()).unwrap_or_else(|| {
        panic!(
            "spec/openapi.yaml not found at any of: {}",
            candidates
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )
    });
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let out_path = out_dir.join("codegen.rs");

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", spec_path.display());

    let raw = fs::read_to_string(spec_path)
        .unwrap_or_else(|e| panic!("read spec at {}: {e}", spec_path.display()));

    let normalized =
        taskfast_codegen::normalize_spec(&raw).unwrap_or_else(|e| panic!("normalize spec: {e:#}"));

    // progenitor consumes `openapiv3::OpenAPI`. It also accepts JSON via
    // serde_json::Value round-trip — cheapest path from our YAML normalizer
    // is YAML → Value → JSON value → OpenAPI, but we can also go
    // YAML → OpenAPI directly since serde_yaml implements Deserializer.
    let spec: openapiv3::OpenAPI = serde_yaml::from_str(&normalized)
        .unwrap_or_else(|e| panic!("parse normalized spec as OpenAPI: {e}"));

    let mut generator = progenitor::Generator::default();
    let tokens = generator
        .generate_tokens(&spec)
        .unwrap_or_else(|e| panic!("progenitor generate_tokens: {e}"));

    // Format via syn + prettyplease so the generated file is human-readable
    // when inspecting via `cargo expand` or target/.
    let ast: syn::File =
        syn::parse2(tokens).unwrap_or_else(|e| panic!("parse generated tokens: {e}"));
    let rendered = prettyplease::unparse(&ast);

    fs::write(&out_path, rendered)
        .unwrap_or_else(|e| panic!("write codegen.rs at {}: {e}", out_path.display()));
}
