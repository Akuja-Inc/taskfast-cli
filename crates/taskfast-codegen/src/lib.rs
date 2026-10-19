// SPDX-License-Identifier: MIT
// TODO: doc the public surface; taskfast-codegen is published but internal-priority so keep this deferred.
#![allow(missing_docs, clippy::doc_markdown)]

//! TaskFast OpenAPI spec tooling.
//!
//! The authoritative spec lives at `spec/openapi.yaml`. The platform team
//! evolves it against Elixir routes; clients generate from it. A small set of
//! structural redundancies would cause progenitor to emit distinct Rust types
//! for schemas that are semantically identical (e.g. multiple `{error, message}`
//! objects under different names). This crate's [`normalize_spec`] folds those
//! aliases into a single canonical shape *in memory*, leaving the on-disk spec
//! untouched.
//!
//! Consumers:
//!  - `cargo xtask sync-spec` (binary) — writes a normalized artifact for inspection.
//!  - `taskfast-client/build.rs` — pipes the normalized YAML into progenitor.
//!
//! ## Rewrite rules
//!
//! Today: every name in [`ERROR_ALIASES`] is a structural clone of
//! `#/components/schemas/Error`. The normalizer:
//!   1. Asserts each alias is byte-for-byte structurally equal to `Error`
//!      (ignoring `description`/`example` doc-only fields) — drift fails loud.
//!   2. Rewrites every `$ref: '#/components/schemas/<alias>'` to point at
//!      `#/components/schemas/Error`.
//!   3. Removes the alias definitions from `components.schemas`.
//!
//! Adding a new alias ⇒ append to [`ERROR_ALIASES`]. If a schema grows a real
//! distinguishing field, remove it from [`ERROR_ALIASES`]; the drift check
//! will refuse to normalize otherwise.
//!
//! ## Multipart strip
//!
//! progenitor 0.9 can't codegen `multipart/form-data` request bodies. Rather
//! than maintain a patched fork, the normalizer drops operations whose request
//! body declares only multipart variants — today that is
//! `POST /tasks/{task_id}/artifacts` (uploadArtifact). The upload path is
//! hand-rolled in `taskfast-client` using `reqwest::multipart` directly; the
//! Artifact *response* schema still comes through codegen because it's
//! referenced by the surviving `GET /tasks/{task_id}/artifacts` operation.
//! The removed operationIds are reported via [`Report::stripped_operations`]
//! so callers can verify nothing unexpected disappeared.
//!
//! ## Multi-media-type request collapse
//!
//! progenitor 0.x panics (`more media types than expected`) when a request
//! body declares more than one `content` media type. The OAuth endpoints
//! (`POST /oauth2/token`, `POST /oauth2/revoke`) accept both
//! `application/json` and `application/x-www-form-urlencoded`, each pointing
//! at the *same* request schema. The normalizer collapses such bodies to a
//! single preferred media type (`application/json` when present, else the
//! first declared), which is lossless here because the variants share a
//! schema. Runs *after* the multipart strip so multipart-bearing operations
//! are already gone and never get collapsed instead of removed. Count surfaced
//! via [`Report::request_media_collapsed`].
//!
//! ## Error response strip
//!
//! progenitor 0.9 asserts `response_types.len() <= 1` for both the success
//! and error response sets (see `progenitor-impl/src/method.rs` —
//! `extract_responses`). Nearly every TaskFast operation declares 2–5
//! distinct error response shapes (`Error`, `ValidationError`, `ClaimFailure`,
//! etc.), which trips the error-side assertion. Rather than synthesize a
//! union error type that we'd throw away anyway, the normalizer drops every
//! non-2xx (and non-`default`) response before handing the spec to progenitor.
//! Surfacing of 4xx/5xx failures is the job of `taskfast-client::errors::Error`,
//! which reads the response body manually on the way up.
//!
//! This means the generated client sees every unhappy status as
//! `progenitor_client::Error::UnexpectedResponse(reqwest::Response)` — we
//! re-classify into our typed `taskfast_client::errors::Error` in the calling layer.
//!
//! ## Closed-object strip
//!
//! Every `additionalProperties: false` is removed so generated types ignore
//! unknown keys — an additive server change must not break installed CLIs
//! (gh#172).
//!
//! ## Response-enum strip
//!
//! String enums reachable from success or `default` response bodies are opened
//! so additive server enum values decode in released CLIs (gh#176). Request-only
//! enums stay typed; shared schemas prioritize response tolerance.
//!
//! ## Response-required relax
//!
//! `required` lists in response-reachable schemas are cut down to
//! [`KEEP_REQUIRED`], the fields CLI code reads as plain values. Every other
//! response property becomes `Option`, so a server that drops a required field
//! the CLI never reads no longer breaks released CLIs (gh#178).

use std::collections::BTreeSet;

use anyhow::{anyhow, bail, Context, Result};
use serde_yaml::{Mapping, Value};

/// Schemas known to be structural clones of `#/components/schemas/Error`.
///
/// Empty since the server's `Error` schema grew optional envelope fields
/// (`details`, `status`, `fallback`, `errors`) that the minimal
/// `WalletBalanceNotFoundError` / `WebhookNoWebhookError` shapes don't carry.
/// Those two are still `{error, message}`-only and are referenced solely from
/// non-2xx responses, which the error-response strip pass removes before
/// progenitor — so they no longer need folding. Re-add a name here if a future
/// schema is a byte-for-byte structural clone of `Error`.
pub const ERROR_ALIASES: &[&str] = &[];

const NULL_VARIANT_SCHEMA_KEYS: &[&str] = &["oneOf", "anyOf"];

/// Response properties that stay required: `(schema key, property)`.
///
/// The compiler maintains most of this list. Every other response property
/// becomes `Option`, and a CLI read of it as a plain value fails to compile;
/// add the pair here when the CLI needs the value, so decode keeps failing fast
/// at the HTTP boundary if the server stops sending it. Handling it as `Option`
/// at the read site instead is a behavior decision: a missing money field must
/// not default to zero. A few entries are semantic, not compiler-found; their
/// comments say why.
///
/// The schema key names where the property is declared, not the generated Rust
/// type: a component name (`AgentProfile`), a nested inline object under it
/// (`AgentReadiness.settlement_domain`), or an `allOf` member
/// (`TaskDetail.allOf.1`). The private `schema_key` builds these keys.
pub const KEEP_REQUIRED: &[(&str, &str)] = &[
    // taskfast-agent events stream: paging needs the cursor envelope.
    ("AgentEventListResponse", "meta"),
    ("PaginatedMeta", "has_more"),
    // Semantic: the events stream already tolerates bad events one by one
    // (events::decode_raw_page puts them in `unparseable` with the raw JSON).
    // An event without its id or type can't be acked or dispatched, so it must
    // keep landing there instead of decoding with `None`s.
    ("AgentEvent", "data"),
    ("AgentEvent", "event"),
    ("AgentEvent", "event_id"),
    ("AgentEvent", "occurred_at"),
    // cmd/escrow.rs: ArbitratedEscrow deposit and poster-approval parameters.
    ("BidEscrowParamsResponse", "amount"),
    ("BidEscrowParamsResponse", "arbitrator_address"),
    ("BidEscrowParamsResponse", "chain_id"),
    ("BidEscrowParamsResponse", "decimals"),
    ("BidEscrowParamsResponse", "platform_fee_amount"),
    ("BidEscrowParamsResponse", "platform_wallet"),
    ("BidEscrowParamsResponse", "poster_approval_deadline"),
    ("BidEscrowParamsResponse", "poster_approval_min_lifetime"),
    ("BidEscrowParamsResponse", "task_escrow_contract"),
    ("BidEscrowParamsResponse", "task_id"),
    ("BidEscrowParamsResponse", "token_address"),
    ("BidEscrowParamsResponse", "worker_address"),
    // cmd/bond.rs: stake amount.
    ("getStakeQuote.200", "required_amount"),
    // cmd/post.rs: submission-fee signing and draft submit.
    ("TaskDraftPrepareResponse", "draft_id"),
    ("TaskDraftPrepareResponse", "payload_to_sign"),
    ("TaskDraftPrepareResponse", "token_address"),
    ("TaskDraftSubmitResponse", "id"),
    ("TaskDraftSubmitResponse", "status"),
    // cmd/settle.rs: EIP-712 settlement domain.
    ("AgentReadiness", "settlement_domain"),
    ("AgentReadiness.settlement_domain", "chain_id"),
    ("AgentReadiness.settlement_domain", "verifying_contract"),
    ("TaskDetail.allOf.1.settlement_domain", "chain_id"),
    ("TaskDetail.allOf.1.settlement_domain", "verifying_contract"),
    // cmd/init.rs: readiness checks and webhook config.
    ("AgentReadiness", "checks"),
    ("AgentReadiness.checks", "funded"),
    ("AgentReadiness.checks", "wallet"),
    ("AgentReadinessCheck", "status"),
    ("WebhookConfig", "url"),
    // cmd/task.rs: uploaded artifact ids.
    ("Artifact", "id"),
];

/// Normalize an OpenAPI YAML document: collapse [`ERROR_ALIASES`] into `Error`.
///
/// Returns the rewritten YAML as a string. Idempotent: running it on already
/// normalized output is a no-op (aliases are absent, so nothing rewrites).
pub fn normalize_spec(yaml: &str) -> Result<String> {
    let (out, report) = normalize_with_aliases(yaml, ERROR_ALIASES)?;
    tracing_nothing(&report); // suppress unused_variable warning when no tracing is wired
    Ok(out)
}

/// As [`normalize_spec`] but also returns a summary of what changed.
pub fn normalize_spec_with_report(yaml: &str) -> Result<(String, Report)> {
    normalize_with_aliases(yaml, ERROR_ALIASES)
}

/// Core of [`normalize_spec`], parameterized on the alias list. Production
/// callers pass [`ERROR_ALIASES`] (currently empty); the unit tests pass an
/// explicit list so the fold and drift-check passes stay covered regardless of
/// what the live spec needs today.
fn normalize_with_aliases(yaml: &str, aliases: &[&str]) -> Result<(String, Report)> {
    let mut doc: Value = serde_yaml::from_str(yaml).context("parse spec YAML")?;
    let report = normalize_in_place(&mut doc, aliases)?;
    let out = serde_yaml::to_string(&doc).context("re-serialize normalized spec")?;
    Ok((out, report))
}

#[derive(Debug, Default, Clone)]
pub struct Report {
    /// Aliases that were folded into `Error`. Populated only for aliases that
    /// were actually present in the input — missing aliases are silently skipped.
    pub folded_aliases: Vec<String>,
    /// Count of `$ref` rewrites performed across the document.
    pub refs_rewritten: usize,
    /// `operationId`s stripped because their only request-body variant was
    /// multipart/form-data (progenitor 0.9 limitation).
    pub stripped_operations: Vec<String>,
    /// Count of non-2xx response entries removed across all operations
    /// (progenitor 0.9 `response_types.len() <= 1` assertion).
    pub error_responses_stripped: usize,
    /// Count of `{type: "null"}` variants stripped from `oneOf` / `anyOf`
    /// arrays anywhere in the document (progenitor 0.9 doesn't support
    /// the OpenAPI 3.1 nullable-via-null-type pattern).
    pub null_variants_stripped: usize,
    /// Count of request bodies whose multiple `content` media types were
    /// collapsed to a single preferred type (progenitor 0.x can't pick among
    /// `>1` request media types — "more media types than expected").
    pub request_media_collapsed: usize,
    /// Count of string `enum` keywords removed from response-reachable schemas.
    pub response_enums_stripped: usize,
    /// Count of response properties dropped from `required` lists (not in
    /// [`KEEP_REQUIRED`]).
    pub response_required_relaxed: usize,
}

fn normalize_in_place(doc: &mut Value, aliases: &[&str]) -> Result<Report> {
    let mut report = Report::default();

    let schemas_path = ["components", "schemas"];
    let canonical_shape = {
        let error = get_mapping_path(doc, &schemas_path)
            .and_then(|m| m.get(Value::from("Error")))
            .ok_or_else(|| anyhow!("canonical schema `Error` not found in components.schemas"))?;
        structural_shape(error)
    };

    // Pass 1: drift-check every alias that is present.
    let present_aliases: Vec<String> = {
        let schemas = get_mapping_path(doc, &schemas_path)
            .ok_or_else(|| anyhow!("components.schemas not a mapping"))?;
        aliases
            .iter()
            .copied()
            .filter(|a| schemas.contains_key(Value::from(*a)))
            .map(str::to_owned)
            .collect()
    };

    for alias in &present_aliases {
        let schema = get_mapping_path(doc, &schemas_path)
            .and_then(|m| m.get(Value::from(alias.as_str())))
            .expect("alias presence confirmed above");
        let alias_shape = structural_shape(schema);
        if alias_shape != canonical_shape {
            bail!(
                "alias `{alias}` has drifted from canonical `Error` shape — \
                 either restore structural equality or remove `{alias}` from ERROR_ALIASES.\n\
                 canonical: {:#?}\nalias:     {:#?}",
                canonical_shape,
                alias_shape
            );
        }
    }

    // Pass 2: rewrite every $ref pointing at an alias.
    for alias in &present_aliases {
        let from = format!("#/components/schemas/{alias}");
        let to = "#/components/schemas/Error";
        report.refs_rewritten += rewrite_refs(doc, &from, to);
    }

    // Pass 3: drop alias definitions from components.schemas.
    if let Some(schemas) = get_mapping_path_mut(doc, &schemas_path) {
        for alias in &present_aliases {
            schemas.remove(Value::from(alias.as_str()));
        }
    }
    report.folded_aliases = present_aliases;

    // Pass 4: strip multipart-only operations (progenitor 0.9 limitation).
    report.stripped_operations = strip_multipart_only_operations(doc);

    // Pass 4b: collapse request bodies with >1 media type to a single
    // preferred type. Must run after the multipart strip so multipart ops are
    // already removed rather than collapsed onto their JSON variant.
    report.request_media_collapsed = collapse_multi_media_request_bodies(doc);

    // Pass 5: strip non-2xx responses from every surviving operation
    // (progenitor 0.9 extract_responses assertion, error side).
    report.error_responses_stripped = strip_non_success_responses(doc);

    // Pass 6: strip OpenAPI 3.1 `{type: "null"}` variants from oneOf/anyOf
    // arrays. Progenitor 0.9 panics on this nullable pattern; the caller's
    // hand-rolled JSON-RPC forwarder uses serde_json::Value and doesn't
    // need a generated type for the null branch.
    report.null_variants_stripped = strip_null_type_variants(doc);

    // Pass 7: open closed objects (gh#172). See [`strip_closed_objects`].
    strip_closed_objects(doc);

    // Pass 8: open response string enums (gh#176). See [`strip_response_enums`].
    report.response_enums_stripped = strip_response_enums(doc);

    // Pass 9: relax response `required` lists (gh#178). See [`relax_response_required`].
    report.response_required_relaxed = relax_response_required(doc);

    Ok(report)
}

/// Remove string `enum` keywords from schemas reachable from 2xx or `default`
/// response bodies, following local `$ref`s transitively. Returns the count.
///
/// typify emits closed Rust enums that reject server-added values (gh#176).
/// Opening response enums makes them `String`; request-body and parameter
/// enums stay typed unless shared with a response (e.g. CompletionCriterionInput),
/// in which case response tolerance wins. Non-string enums are left alone.
/// Runs after the non-2xx and null-type-variant strips: error-only schemas are
/// out of scope, and removed null branches must not make schemas reachable.
/// `nullable: true` is preserved even when the enum contains `null`, so typify
/// still emits `Option<String>` for nullable response values.
fn strip_response_enums(doc: &mut Value) -> usize {
    let enums: Vec<_> = response_schema_paths(doc)
        .into_iter()
        .filter(|path| {
            value_at_path(doc, path).is_some_and(|schema| {
                schema.get("type").and_then(Value::as_str) == Some("string")
                    && schema.get("enum").is_some()
            })
        })
        .collect();
    let mut stripped = 0;
    for path in enums {
        if value_at_path_mut(doc, &path)
            .and_then(Value::as_mapping_mut)
            .and_then(|map| map.remove("enum"))
            .is_some()
        {
            stripped += 1;
        }
    }
    stripped
}

/// Cut every `required` list in a response-reachable schema down to the
/// properties named in [`KEEP_REQUIRED`]. Returns how many entries were dropped.
///
/// typify emits a required property as a plain field and an optional one as
/// `Option`, so a required property the server stops sending fails decode in
/// every released CLI even when no CLI code reads it (gh#178). Kept properties
/// still fail fast at the HTTP boundary, which is what the CLI wants for values
/// it uses. Request-only schemas are untouched; a schema shared with a request
/// is relaxed too (the CLI only serializes request bodies, and an optional
/// field it always sets serializes the same). Optional arrays become
/// `#[serde(default)] Vec<T>`, not `Option`.
fn relax_response_required(doc: &mut Value) -> usize {
    let mut relaxed = 0;
    for path in response_schema_paths(doc) {
        let key = schema_key(doc, &path);
        let Some(map) = value_at_path_mut(doc, &path).and_then(Value::as_mapping_mut) else {
            continue;
        };
        let Some(Value::Sequence(required)) = map.get_mut("required") else {
            continue;
        };
        let before = required.len();
        required.retain(|property| {
            property
                .as_str()
                .is_some_and(|property| KEEP_REQUIRED.contains(&(key.as_str(), property)))
        });
        relaxed += before - required.len();
        if required.is_empty() {
            map.remove("required");
        }
    }
    relaxed
}

/// The [`KEEP_REQUIRED`] key for a schema path: where the schema is declared,
/// with `properties` segments dropped. `components.schemas.X.properties.p` →
/// `X.p`; an `allOf` member → `X.allOf.1`; an inline response schema →
/// `<operationId>.<status>`; a reusable response → `responses.<name>`.
fn schema_key(doc: &Value, path: &[String]) -> String {
    let (head, rest) = match path {
        [c, s, name, rest @ ..] if c == "components" && s == "schemas" => (name.clone(), rest),
        [c, r, name, _content, _media, _schema, rest @ ..]
            if c == "components" && r == "responses" =>
        {
            (format!("responses.{name}"), rest)
        }
        [p, route, verb, _responses, code, _content, _media, _schema, rest @ ..]
            if p == "paths" =>
        {
            let op = value_at_path(doc, &path[..3])
                .and_then(|op| op.get("operationId"))
                .and_then(Value::as_str)
                .map_or_else(|| format!("{verb} {route}"), str::to_owned);
            (format!("{op}.{code}"), rest)
        }
        _ => (path.join("."), &[][..]),
    };
    std::iter::once(head)
        .chain(rest.iter().filter(|seg| *seg != "properties").cloned())
        .collect::<Vec<_>>()
        .join(".")
}

/// Every schema path reachable from a 2xx or `default` response body,
/// following local `$ref`s transitively through composition, properties,
/// arrays and map values.
fn response_schema_paths(doc: &Value) -> BTreeSet<Vec<String>> {
    let mut visited = BTreeSet::new();
    let mut schemas = BTreeSet::new();
    if let Some(paths) = doc.get("paths").and_then(Value::as_mapping) {
        for (path, item) in paths {
            let Some(path) = path.as_str() else { continue };
            for verb in [
                "get", "put", "post", "delete", "patch", "options", "head", "trace",
            ] {
                let Some(responses) = item
                    .get(verb)
                    .and_then(|op| op.get("responses"))
                    .and_then(Value::as_mapping)
                else {
                    continue;
                };
                for code in responses
                    .keys()
                    .filter_map(Value::as_str)
                    .filter(|code| is_success_or_default(code))
                {
                    let root = ["paths", path, verb, "responses", code]
                        .map(str::to_owned)
                        .to_vec();
                    collect_response_schemas(doc, &root, &mut visited, &mut schemas);
                }
            }
        }
    }
    schemas
}

fn value_at_path<'a>(doc: &'a Value, path: &[String]) -> Option<&'a Value> {
    path.iter().try_fold(doc, |node, key| match node {
        Value::Sequence(seq) => seq.get(key.parse::<usize>().ok()?),
        _ => node.get(key.as_str()),
    })
}

fn value_at_path_mut<'a>(doc: &'a mut Value, path: &[String]) -> Option<&'a mut Value> {
    path.iter().try_fold(doc, |node, key| match node {
        Value::Sequence(seq) => seq.get_mut(key.parse::<usize>().ok()?),
        _ => node.get_mut(key.as_str()),
    })
}

fn local_ref_path(node: &Value) -> Option<Vec<String>> {
    Some(
        node.get("$ref")?
            .as_str()?
            .strip_prefix("#/")?
            .split('/')
            .map(|key| key.replace("~1", "/").replace("~0", "~"))
            .collect(),
    )
}

fn collect_response_schemas(
    doc: &Value,
    path: &[String],
    visited: &mut BTreeSet<Vec<String>>,
    schemas: &mut BTreeSet<Vec<String>>,
) {
    if !visited.insert(path.to_vec()) {
        return;
    }
    let Some(response) = value_at_path(doc, path) else {
        return;
    };
    if let Some(target) = local_ref_path(response) {
        collect_response_schemas(doc, &target, visited, schemas);
    }
    if let Some(content) = response.get("content").and_then(Value::as_mapping) {
        for media in content.keys().filter_map(Value::as_str) {
            let mut schema_path = path.to_vec();
            schema_path.extend(["content".to_owned(), media.to_owned(), "schema".to_owned()]);
            collect_schemas(doc, &schema_path, visited, schemas);
        }
    }
}

fn collect_schemas(
    doc: &Value,
    path: &[String],
    visited: &mut BTreeSet<Vec<String>>,
    schemas: &mut BTreeSet<Vec<String>>,
) {
    if !visited.insert(path.to_vec()) {
        return;
    }
    let Some(schema) = value_at_path(doc, path) else {
        return;
    };
    schemas.insert(path.to_vec());
    if let Some(target) = local_ref_path(schema) {
        collect_schemas(doc, &target, visited, schemas);
    }
    for key in [
        "allOf",
        "oneOf",
        "anyOf",
        "properties",
        "additionalProperties",
        "items",
    ] {
        let Some(child) = schema.get(key) else {
            continue;
        };
        let mut child_path = path.to_vec();
        child_path.push(key.to_owned());
        match child {
            Value::Sequence(seq) => {
                for index in 0..seq.len() {
                    let mut item_path = child_path.clone();
                    item_path.push(index.to_string());
                    collect_schemas(doc, &item_path, visited, schemas);
                }
            }
            Value::Mapping(map) if key == "properties" => {
                for property in map.keys().filter_map(Value::as_str) {
                    let mut property_path = child_path.clone();
                    property_path.push(property.to_owned());
                    collect_schemas(doc, &property_path, visited, schemas);
                }
            }
            _ => collect_schemas(doc, &child_path, visited, schemas),
        }
    }
}

/// Recursively remove every `additionalProperties: false`.
///
/// typify turns that keyword into `#[serde(deny_unknown_fields)]`, so a single
/// field the server adds to a response would fail decode in every released
/// CLI. Stripping it everywhere (not just responses) is safe: the attribute
/// only affects deserialization, and the CLI only ever serializes request
/// bodies. Non-`false` values (`true`, a schema) are left alone — they change
/// the generated type shape, not just strictness.
fn strip_closed_objects(v: &mut Value) {
    match v {
        Value::Mapping(map) => {
            if map.get("additionalProperties") == Some(&Value::Bool(false)) {
                map.remove("additionalProperties");
            }
            for (_, child) in map.iter_mut() {
                strip_closed_objects(child);
            }
        }
        Value::Sequence(seq) => seq.iter_mut().for_each(strip_closed_objects),
        _ => {}
    }
}

/// Recursively walk `doc` and drop any `{type: "null"}` entries from `oneOf`
/// and `anyOf` arrays. Returns the total count stripped.
///
/// A schema is considered a null-type variant when its `type` field equals
/// the string `"null"` — sibling keys like `description` are tolerated so
/// codegen tools that re-emit the spec with extra annotations still match.
///
/// If stripping leaves an empty `oneOf`/`anyOf`, the keyword is removed
/// outright: an empty schema array is invalid OpenAPI and would trip the
/// downstream generator harder than the original null-variant did.
fn strip_null_type_variants(doc: &mut Value) -> usize {
    fn is_null_type_schema(v: &Value) -> bool {
        v.as_mapping()
            .and_then(|m| m.get("type"))
            .and_then(Value::as_str)
            == Some("null")
    }
    fn walk(v: &mut Value, count: &mut usize) {
        match v {
            Value::Mapping(map) => {
                for key in NULL_VARIANT_SCHEMA_KEYS {
                    let Some(seq) = map.get_mut(*key).and_then(Value::as_sequence_mut) else {
                        continue;
                    };
                    let before = seq.len();
                    seq.retain(|item| !is_null_type_schema(item));
                    *count += before - seq.len();
                    if before > 0 && seq.is_empty() {
                        map.remove(*key);
                    }
                }
                for (_, child) in map.iter_mut() {
                    walk(child, count);
                }
            }
            Value::Sequence(seq) => {
                for child in seq.iter_mut() {
                    walk(child, count);
                }
            }
            _ => {}
        }
    }
    let mut count = 0;
    walk(doc, &mut count);
    count
}

/// Remove operations whose request body has exactly one content variant,
/// `multipart/form-data`. Returns the removed `operationId`s (stable order).
///
/// We don't touch operations that declare multipart *alongside* another
/// variant — those are handled by a preferred-content-type collapse pass that
/// doesn't exist yet; today no such operations exist in the TaskFast spec.
fn strip_multipart_only_operations(doc: &mut Value) -> Vec<String> {
    let mut stripped = Vec::new();
    let Some(paths) = doc
        .as_mapping_mut()
        .and_then(|m| m.get_mut(Value::from("paths")))
        .and_then(Value::as_mapping_mut)
    else {
        return stripped;
    };

    const HTTP_VERBS: &[&str] = &["get", "put", "post", "delete", "patch", "options", "head"];
    for (_path_key, path_item) in paths.iter_mut() {
        let Some(ops) = path_item.as_mapping_mut() else {
            continue;
        };
        let mut to_remove = Vec::new();
        for verb in HTTP_VERBS {
            if let Some(op) = ops.get(Value::from(*verb)) {
                if operation_is_multipart_only(op) {
                    let op_id = op
                        .get(Value::from("operationId"))
                        .and_then(Value::as_str)
                        .unwrap_or("<anonymous>")
                        .to_string();
                    to_remove.push(*verb);
                    stripped.push(op_id);
                }
            }
        }
        for verb in to_remove {
            ops.remove(Value::from(verb));
        }
    }
    stripped
}

/// Collapse any `requestBody.content` mapping with more than one media type
/// down to a single preferred entry: `application/json` if present, otherwise
/// the first declared media type (insertion order preserved). Returns the
/// number of request bodies collapsed.
///
/// progenitor 0.x cannot pick among multiple request media types and panics
/// with `more media types than expected`. Today the only affected operations
/// are the OAuth endpoints, whose JSON and form-urlencoded variants reference
/// the same schema, so the collapse is lossless. Multipart-bearing operations
/// must already be stripped before this runs (see [`strip_multipart_only_operations`]).
fn collapse_multi_media_request_bodies(doc: &mut Value) -> usize {
    let mut collapsed = 0usize;
    let Some(paths) = doc
        .as_mapping_mut()
        .and_then(|m| m.get_mut(Value::from("paths")))
        .and_then(Value::as_mapping_mut)
    else {
        return 0;
    };

    const HTTP_VERBS: &[&str] = &["get", "put", "post", "delete", "patch", "options", "head"];
    for (_path_key, path_item) in paths.iter_mut() {
        let Some(ops) = path_item.as_mapping_mut() else {
            continue;
        };
        for verb in HTTP_VERBS {
            let Some(content) = ops
                .get_mut(Value::from(*verb))
                .and_then(Value::as_mapping_mut)
                .and_then(|op| op.get_mut(Value::from("requestBody")))
                .and_then(Value::as_mapping_mut)
                .and_then(|body| body.get_mut(Value::from("content")))
                .and_then(Value::as_mapping_mut)
            else {
                continue;
            };
            if content.len() <= 1 {
                continue;
            }
            let keep = if content.contains_key(Value::from("application/json")) {
                "application/json".to_string()
            } else {
                content
                    .keys()
                    .next()
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .expect("content has >1 entry")
            };
            content.retain(|k, _| k.as_str() == Some(keep.as_str()));
            collapsed += 1;
        }
    }
    collapsed
}

/// For each operation under `paths.*.<verb>.responses`, remove every entry
/// whose key is not a 2xx status code or the literal `default`. Returns the
/// total count of removed entries across the document.
///
/// Rationale: progenitor 0.9 asserts that both the success and error response
/// sets have at most one distinct body type. TaskFast endpoints declare
/// multiple distinct error shapes (`Error`, `ValidationError`, feature-specific
/// failure envelopes). Surfacing those as typed variants in the generated
/// client is not the generator's job here — `taskfast-client::errors::Error`
/// reconstructs error semantics from the raw response body. Dropping non-2xx
/// response definitions short-circuits the assertion without hiding anything
/// the client layer doesn't already re-implement.
fn strip_non_success_responses(doc: &mut Value) -> usize {
    let mut removed = 0usize;
    let Some(paths) = doc
        .as_mapping_mut()
        .and_then(|m| m.get_mut(Value::from("paths")))
        .and_then(Value::as_mapping_mut)
    else {
        return 0;
    };

    const HTTP_VERBS: &[&str] = &["get", "put", "post", "delete", "patch", "options", "head"];
    for (_path_key, path_item) in paths.iter_mut() {
        let Some(ops) = path_item.as_mapping_mut() else {
            continue;
        };
        for verb in HTTP_VERBS {
            let Some(op) = ops.get_mut(Value::from(*verb)) else {
                continue;
            };
            let Some(responses) = op
                .as_mapping_mut()
                .and_then(|m| m.get_mut(Value::from("responses")))
                .and_then(Value::as_mapping_mut)
            else {
                continue;
            };

            let to_remove: Vec<Value> = responses
                .iter()
                .filter_map(|(k, _)| {
                    let key = k.as_str()?;
                    if is_success_or_default(key) {
                        None
                    } else {
                        Some(k.clone())
                    }
                })
                .collect();
            for k in to_remove {
                if responses.remove(&k).is_some() {
                    removed += 1;
                }
            }
        }
    }
    removed
}

fn is_success_or_default(code: &str) -> bool {
    if code == "default" {
        return true;
    }
    // 2XX ranges (e.g. "2XX") and explicit codes 200..=299.
    match code.chars().next() {
        Some('2') => true,
        _ => false,
    }
}

fn operation_is_multipart_only(op: &Value) -> bool {
    let Some(body) = op
        .as_mapping()
        .and_then(|m| m.get(Value::from("requestBody")))
    else {
        return false;
    };
    let Some(content) = body
        .as_mapping()
        .and_then(|m| m.get(Value::from("content")))
        .and_then(Value::as_mapping)
    else {
        return false;
    };
    let keys: Vec<&str> = content.keys().filter_map(Value::as_str).collect();
    // Match either (a) sole multipart variant or (b) any operation that
    // declares multipart alongside other variants — progenitor 0.9 trips on
    // multi-variant bodies too (`more media types than expected`), and the
    // upload path is hand-rolled in taskfast-client regardless.
    keys.contains(&"multipart/form-data")
}

/// Structural projection: strip doc-only fields so two schemas differing only
/// in `description`/`example` compare equal.
fn structural_shape(v: &Value) -> Value {
    match v {
        Value::Mapping(m) => {
            let mut out = Mapping::new();
            for (k, val) in m {
                let keep = match k.as_str() {
                    Some("description") | Some("example") | Some("summary") | Some("default")
                    | Some("deprecated") | Some("title") => false,
                    _ => true,
                };
                if keep {
                    out.insert(k.clone(), structural_shape(val));
                }
            }
            Value::Mapping(out)
        }
        Value::Sequence(s) => Value::Sequence(s.iter().map(structural_shape).collect()),
        other => other.clone(),
    }
}

/// Walk `doc`, replacing every `$ref` whose value equals `from` with `to`.
/// Returns the number of rewrites performed.
fn rewrite_refs(doc: &mut Value, from: &str, to: &str) -> usize {
    let mut count = 0;
    rewrite_refs_inner(doc, from, to, &mut count);
    count
}

fn rewrite_refs_inner(v: &mut Value, from: &str, to: &str, count: &mut usize) {
    match v {
        Value::Mapping(m) => {
            // A $ref object is typically `{"$ref": "..."}` — a single-key mapping.
            // We still walk siblings because spec-extension keys may coexist.
            if let Some(target) = m.get_mut(Value::from("$ref")) {
                if target.as_str() == Some(from) {
                    *target = Value::from(to);
                    *count += 1;
                }
            }
            for (_, val) in m.iter_mut() {
                rewrite_refs_inner(val, from, to, count);
            }
        }
        Value::Sequence(s) => {
            for val in s.iter_mut() {
                rewrite_refs_inner(val, from, to, count);
            }
        }
        _ => {}
    }
}

fn get_mapping_path<'a>(v: &'a Value, path: &[&str]) -> Option<&'a Mapping> {
    let mut cur = v;
    for key in path {
        cur = cur.as_mapping()?.get(Value::from(*key))?;
    }
    cur.as_mapping()
}

fn get_mapping_path_mut<'a>(v: &'a mut Value, path: &[&str]) -> Option<&'a mut Mapping> {
    let mut cur = v;
    for key in path {
        cur = cur.as_mapping_mut()?.get_mut(Value::from(*key))?;
    }
    cur.as_mapping_mut()
}

fn tracing_nothing(_r: &Report) {}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE_SPEC: &str = r#"
openapi: 3.0.0
info:
  title: test
  version: 0.0.0
paths:
  /foo:
    get:
      responses:
        '200':
          description: ok
        '404':
          description: not found
          content:
            application/json:
              schema:
                $ref: '#/components/schemas/WalletBalanceNotFoundError'
        '503':
          description: unavailable
          content:
            application/json:
              schema:
                $ref: '#/components/schemas/Error'
components:
  schemas:
    Error:
      type: object
      required:
        - error
        - message
      properties:
        error:
          type: string
          description: Error code
        message:
          type: string
          description: Human message
    WalletBalanceNotFoundError:
      type: object
      required:
        - error
        - message
      properties:
        error:
          type: string
          description: Error code
          example: "wallet_not_applicable"
        message:
          type: string
          description: Human message
          example: "not a tempo agent"
    Untouched:
      type: object
"#;

    #[test]
    fn normalize_folds_alias_refs_and_drops_schema() {
        let (out, report) =
            normalize_with_aliases(BASE_SPEC, &["WalletBalanceNotFoundError"]).unwrap();
        assert_eq!(
            report.folded_aliases,
            vec!["WalletBalanceNotFoundError".to_string()]
        );
        assert_eq!(report.refs_rewritten, 1);

        // The canonical Error schema definition still lives in components
        // (non-2xx response strip removes the *usages*, not the shared type).
        assert!(
            out.contains("Error:\n      type: object"),
            "canonical Error schema missing:\n{out}"
        );
        assert!(
            !out.contains("WalletBalanceNotFoundError"),
            "alias name fully removed, got:\n{out}"
        );

        // Siblings untouched.
        assert!(out.contains("Untouched:"));
    }

    #[test]
    fn idempotent_when_no_aliases_present() {
        let (out1, _) = normalize_spec_with_report(BASE_SPEC).unwrap();
        let (out2, report2) = normalize_spec_with_report(&out1).unwrap();
        assert_eq!(out1, out2);
        assert!(report2.folded_aliases.is_empty());
        assert_eq!(report2.refs_rewritten, 0);
    }

    #[test]
    fn drift_check_rejects_mismatched_alias() {
        // Same as BASE_SPEC but WalletBalanceNotFoundError gets an extra required field.
        let drifted = BASE_SPEC.replace(
            "WalletBalanceNotFoundError:\n      type: object\n      required:\n        - error\n        - message",
            "WalletBalanceNotFoundError:\n      type: object\n      required:\n        - error\n        - message\n        - drift_field",
        );
        let err = normalize_with_aliases(&drifted, &["WalletBalanceNotFoundError"]).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("drifted from canonical"), "got: {msg}");
    }

    #[test]
    fn missing_error_schema_is_error() {
        let bad = BASE_SPEC.replace("Error:", "NotError:");
        let err = normalize_spec(bad.as_str()).unwrap_err();
        assert!(format!("{err:#}").contains("canonical schema `Error` not found"));
    }

    const MULTIPART_SPEC: &str = r#"
openapi: 3.0.0
info: { title: test, version: 0.0.0 }
paths:
  /upload:
    post:
      operationId: uploadThing
      requestBody:
        required: true
        content:
          multipart/form-data:
            schema:
              type: object
              required: [file]
              properties:
                file: { type: string, format: binary }
      responses:
        '201': { description: ok }
    get:
      operationId: listThings
      responses:
        '200': { description: ok }
  /json_only:
    post:
      operationId: postJsonThing
      requestBody:
        required: true
        content:
          application/json:
            schema: { type: object }
      responses:
        '201': { description: ok }
components:
  schemas:
    Error:
      type: object
      required: [error, message]
      properties:
        error: { type: string }
        message: { type: string }
"#;

    #[test]
    fn non_success_responses_are_stripped() {
        let (out, report) = normalize_spec_with_report(BASE_SPEC).unwrap();
        // BASE_SPEC has one op with 200/404/503 — 404 and 503 must go.
        assert_eq!(report.error_responses_stripped, 2);
        // The success code survives.
        assert!(out.contains("'200':"));
        // Error codes are gone.
        assert!(!out.contains("'404':"), "404 not stripped:\n{out}");
        assert!(!out.contains("'503':"), "503 not stripped:\n{out}");
    }

    #[test]
    fn multi_media_request_body_collapses_to_json() {
        const SPEC: &str = r#"
openapi: 3.0.0
info: { title: test, version: 0.0.0 }
paths:
  /oauth2/token:
    post:
      operationId: oauthToken
      requestBody:
        required: true
        content:
          application/json:
            schema: { $ref: '#/components/schemas/Tok' }
          application/x-www-form-urlencoded:
            schema: { $ref: '#/components/schemas/Tok' }
      responses:
        '200': { description: ok }
  /forms:
    post:
      operationId: formOnly
      requestBody:
        content:
          application/x-www-form-urlencoded:
            schema: { type: object }
          text/plain:
            schema: { type: string }
      responses:
        '200': { description: ok }
components:
  schemas:
    Error:
      type: object
      required: [error, message]
      properties:
        error: { type: string }
        message: { type: string }
    Tok:
      type: object
"#;
        let (out, report) = normalize_spec_with_report(SPEC).unwrap();
        // Both multi-media request bodies collapse to a single type.
        assert_eq!(report.request_media_collapsed, 2);
        // oauthToken: JSON preferred — exactly one json media type remains
        // (its form variant is gone).
        assert_eq!(
            out.matches("application/json:").count(),
            1,
            "expected oauth body kept as json:\n{out}"
        );
        // formOnly: no JSON, so the first declared type (form) wins and the
        // text/plain variant is dropped.
        assert_eq!(
            out.matches("application/x-www-form-urlencoded:").count(),
            1,
            "expected form-only body kept as form-urlencoded:\n{out}"
        );
        assert!(
            !out.contains("text/plain"),
            "lower-priority text/plain variant not collapsed:\n{out}"
        );
    }

    #[test]
    fn multipart_only_operation_is_stripped() {
        let (out, report) = normalize_spec_with_report(MULTIPART_SPEC).unwrap();
        assert_eq!(report.stripped_operations, vec!["uploadThing".to_string()]);
        assert!(!out.contains("uploadThing"), "uploadThing not stripped");
        // Siblings untouched.
        assert!(out.contains("listThings"), "sibling GET removed by mistake");
        assert!(
            out.contains("postJsonThing"),
            "JSON-only POST removed by mistake"
        );
        // The path entry itself survives because listThings still lives there.
        assert!(out.contains("/upload:"));
    }

    const NULL_VARIANT_SPEC: &str = r#"
openapi: 3.0.0
info: { title: test, version: 0.0.0 }
paths:
  /thing:
    get:
      responses:
        '200':
          description: ok
          content:
            application/json:
              schema:
                oneOf:
                  - { type: object, properties: { name: { type: string } } }
                  - { type: "null" }
components:
  schemas:
    Error:
      type: object
      required: [error, message]
      properties:
        error: { type: string }
        message: { type: string }
    Nullable:
      anyOf:
        - { type: integer }
        - { type: "null", description: "explicit absence" }
    AllNull:
      oneOf:
        - { type: "null" }
        - { type: "null" }
    Untouched:
      oneOf:
        - { type: integer }
        - { type: string }
"#;

    #[test]
    fn null_type_variants_are_stripped_and_counted() {
        let (out, report) = normalize_spec_with_report(NULL_VARIANT_SPEC).unwrap();
        // 1 in /thing oneOf + 1 in Nullable.anyOf + 2 in AllNull.oneOf = 4.
        assert_eq!(report.null_variants_stripped, 4);

        // Surviving variants kept.
        assert!(out.contains("Nullable:"));
        assert!(out.contains("Untouched:"));
        // Sibling keys (description) on a null-type variant don't shield it.
        assert!(
            !out.contains("explicit absence"),
            "null variant with sibling description not stripped:\n{out}"
        );
        // Untouched non-null oneOf preserved verbatim.
        assert!(
            out.contains("Untouched:") && out.matches("type: string").count() >= 2,
            "Untouched.oneOf altered:\n{out}"
        );
    }

    #[test]
    fn empty_oneof_after_strip_is_removed_outright() {
        let (out, _report) = normalize_spec_with_report(NULL_VARIANT_SPEC).unwrap();
        // AllNull had only null variants; the oneOf key must be gone, leaving
        // the (now-empty) schema definition without an invalid `oneOf: []`.
        // Find the AllNull block and assert no oneOf inside it.
        let idx = out.find("AllNull:").expect("AllNull schema present");
        let tail = &out[idx..];
        // Stop at the next sibling schema or end-of-doc.
        let block_end = tail[8..].find("\n    ").map_or(tail.len(), |n| n + 8);
        let block = &tail[..block_end];
        assert!(
            !block.contains("oneOf"),
            "empty oneOf not removed from AllNull:\n{block}"
        );
    }

    #[test]
    fn null_strip_is_idempotent() {
        let (out1, _) = normalize_spec_with_report(NULL_VARIANT_SPEC).unwrap();
        let (out2, report2) = normalize_spec_with_report(&out1).unwrap();
        assert_eq!(out1, out2);
        assert_eq!(report2.null_variants_stripped, 0);
    }

    #[test]
    fn response_enums_are_opened_but_request_enums_stay_typed() {
        const SPEC: &str = r#"
openapi: 3.0.0
info: { title: test, version: 0.0.0 }
paths:
  /thing:
    post:
      parameters:
        - { name: filter, in: query, schema: { type: string, enum: [known] } }
      requestBody:
        content:
          application/json:
            schema: { $ref: '#/components/schemas/Request' }
      responses:
        '200':
          description: ok
          content:
            application/json:
              schema: { $ref: '#/components/schemas/Response' }
        default:
          description: fallback
          content:
            application/json:
              schema: { type: string, enum: [fallback] }
        '400':
          description: bad request
          content:
            application/json:
              schema: { $ref: '#/components/schemas/Error' }
components:
  schemas:
    Error: { type: string, enum: [bad] }
    Request:
      type: object
      properties:
        kind: { type: string, enum: [known] }
        shared: { $ref: '#/components/schemas/Shared' }
    Shared: { type: string, enum: [known] }
    Response:
      allOf:
        - type: object
          properties:
            status: { type: string, enum: [active] }
            nullable_status: { type: string, nullable: true, enum: [active, null] }
            shared: { $ref: '#/components/schemas/Shared' }
            recursive: { $ref: '#/components/schemas/Response' }
            choices:
              oneOf:
                - { $ref: '#/components/schemas/Choice' }
                - { type: "null" }
            alternatives:
              anyOf:
                - { type: string, enum: [known] }
            list:
              type: array
              items: { $ref: '#/components/schemas/Choice' }
            map:
              type: object
              additionalProperties: { $ref: '#/components/schemas/Choice' }
            number: { type: integer, enum: [1, 2] }
    Choice: { type: string, enum: [known] }
"#;
        let (out, _) = normalize_spec_with_report(SPEC).unwrap();
        let doc: Value = serde_yaml::from_str(&out).unwrap();
        let schemas = &doc["components"]["schemas"];
        let properties = &schemas["Response"]["allOf"][0]["properties"];
        assert!(properties["status"].get("enum").is_none());
        assert!(properties["nullable_status"].get("enum").is_none());
        assert_eq!(properties["nullable_status"]["nullable"], Value::Bool(true));
        assert!(schemas["Request"]["properties"]["kind"]
            .get("enum")
            .is_some());
        assert!(doc["paths"]["/thing"]["post"]["parameters"][0]["schema"]
            .get("enum")
            .is_some());
        assert!(schemas["Shared"].get("enum").is_none());
        assert!(schemas["Choice"].get("enum").is_none());
        assert!(properties["alternatives"]["anyOf"][0].get("enum").is_none());
        assert!(properties["number"].get("enum").is_some());
        assert!(schemas["Error"].get("enum").is_some());
        assert!(
            doc["paths"]["/thing"]["post"]["responses"]["default"]["content"]["application/json"]
                ["schema"]
                .get("enum")
                .is_none()
        );
    }

    #[test]
    fn reusable_responses_follow_refs_without_opening_example_enums() {
        const SPEC: &str = r#"
openapi: 3.0.0
info: { title: test, version: 0.0.0 }
paths:
  /thing:
    get:
      responses:
        '2XX': { $ref: '#/components/responses/Thing' }
components:
  responses:
    Thing:
      description: ok
      content:
        application/json:
          schema: { $ref: '#/components/schemas/Thing~1Status~0' }
          example: { type: string, enum: [literal_payload] }
  schemas:
    Error: { type: object }
    Thing/Status~: { type: string, enum: [active] }
"#;
        let (out, report) = normalize_spec_with_report(SPEC).unwrap();
        let doc: Value = serde_yaml::from_str(&out).unwrap();
        assert!(doc["components"]["schemas"]["Thing/Status~"]
            .get("enum")
            .is_none());
        assert!(
            doc["components"]["responses"]["Thing"]["content"]["application/json"]["example"]
                .get("enum")
                .is_some()
        );
        assert_eq!(report.response_enums_stripped, 1);
    }

    #[test]
    fn live_spec_response_enum_inventory_is_normalized() {
        // Independent inventory of the vendored spec, including transitive refs.
        // Counts are measured here, not used as limits by the production pass.
        fn enum_count(node: &Value) -> usize {
            match node {
                Value::Mapping(map) => {
                    usize::from(
                        map.get("type").and_then(Value::as_str) == Some("string")
                            && map.contains_key("enum"),
                    ) + map.values().map(enum_count).sum::<usize>()
                }
                Value::Sequence(seq) => seq.iter().map(enum_count).sum(),
                _ => 0,
            }
        }
        fn refs(node: &Value, schemas: &Mapping, found: &mut BTreeSet<String>) {
            match node {
                Value::Mapping(map) => {
                    if let Some(name) = map
                        .get("$ref")
                        .and_then(Value::as_str)
                        .and_then(|target| target.strip_prefix("#/components/schemas/"))
                    {
                        if found.insert(name.to_owned()) {
                            refs(&schemas[name], schemas, found);
                        }
                    }
                    for child in map.values() {
                        refs(child, schemas, found);
                    }
                }
                Value::Sequence(seq) => {
                    for child in seq {
                        refs(child, schemas, found);
                    }
                }
                _ => {}
            }
        }
        let src = include_str!("../../../spec/openapi.yaml");
        let doc: Value = serde_yaml::from_str(src).unwrap();
        let schemas = doc["components"]["schemas"].as_mapping().unwrap();
        let mut response_refs = BTreeSet::new();
        let mut request_refs = BTreeSet::new();
        let mut inline_response_ops = 0;
        let mut inline_request_ops = 0;
        let mut inline_response_enums = 0;
        for item in doc["paths"].as_mapping().unwrap().values() {
            refs(&item["parameters"], schemas, &mut request_refs);
            for verb in [
                "get", "put", "post", "delete", "patch", "options", "head", "trace",
            ] {
                let Some(op) = item.get(verb) else { continue };
                refs(&op["requestBody"], schemas, &mut request_refs);
                refs(&op["parameters"], schemas, &mut request_refs);
                inline_request_ops +=
                    usize::from(enum_count(&op["requestBody"]) + enum_count(&op["parameters"]) > 0);
                let mut op_enums = 0;
                for (code, response) in op["responses"].as_mapping().unwrap() {
                    let code = code.as_str().unwrap();
                    if code == "default" || code.starts_with('2') {
                        refs(&response["content"], schemas, &mut response_refs);
                        op_enums += enum_count(&response["content"]);
                    }
                }
                inline_response_ops += usize::from(op_enums > 0);
                inline_response_enums += op_enums;
            }
        }
        let enum_components: BTreeSet<String> = schemas
            .iter()
            .filter(|(_, schema)| enum_count(schema) > 0)
            .map(|(name, _)| name.as_str().unwrap().to_owned())
            .collect();
        let response_components: BTreeSet<_> = enum_components
            .intersection(&response_refs)
            .cloned()
            .collect();
        let request_components: BTreeSet<_> = enum_components
            .intersection(&request_refs)
            .cloned()
            .collect();
        assert_eq!(
            response_components.difference(&request_components).count(),
            52
        );
        assert_eq!(
            request_components.difference(&response_components).count(),
            8
        );
        assert_eq!(
            response_components
                .intersection(&request_components)
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["CompletionCriterionInput"]
        );
        assert_eq!(
            enum_components
                .difference(
                    &response_components
                        .union(&request_components)
                        .cloned()
                        .collect()
                )
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["RateLimitError"]
        );
        assert_eq!(inline_response_ops, 3);
        assert_eq!(inline_request_ops, 10);
        let expected = inline_response_enums
            + response_components
                .iter()
                .map(|name| enum_count(&schemas[name.as_str()]))
                .sum::<usize>();
        let (out, report) = normalize_spec_with_report(src).unwrap();
        assert_eq!(report.response_enums_stripped, expected);
        let normalized: Value = serde_yaml::from_str(&out).unwrap();
        for name in &response_components {
            assert_eq!(
                enum_count(&normalized["components"]["schemas"][name.as_str()]),
                0,
                "response component {name}"
            );
        }
        for name in request_components.difference(&response_components) {
            assert_eq!(
                enum_count(&normalized["components"]["schemas"][name.as_str()]),
                enum_count(&schemas[name.as_str()]),
                "request-only component {name}"
            );
        }
        let (again, report) = normalize_spec_with_report(&out).unwrap();
        assert_eq!(again, out);
        assert_eq!(report.response_enums_stripped, 0);
    }

    #[test]
    fn response_required_is_relaxed_except_kept_and_request_fields() {
        // `WebhookConfig.url` and `getStakeQuote.200.required_amount` are real
        // KEEP_REQUIRED entries (a component and an inline response schema).
        const SPEC: &str = r#"
openapi: 3.0.0
info: { title: test, version: 0.0.0 }
paths:
  /hooks:
    put:
      requestBody:
        content:
          application/json:
            schema: { $ref: '#/components/schemas/Request' }
      responses:
        '200':
          description: ok
          content:
            application/json:
              schema: { $ref: '#/components/schemas/WebhookConfig' }
  /stake/quote:
    get:
      operationId: getStakeQuote
      responses:
        '200':
          description: ok
          content:
            application/json:
              schema:
                type: object
                required: [required_amount, currency]
                properties:
                  required_amount: { type: integer }
                  currency: { type: string }
components:
  schemas:
    Error: { type: object }
    Request:
      type: object
      required: [url]
      properties:
        url: { type: string }
    WebhookConfig:
      type: object
      required: [url, secret, nested]
      properties:
        url: { type: string }
        secret: { type: string }
        nested:
          type: object
          required: [inner]
          properties:
            inner: { type: string }
"#;
        let (out, report) = normalize_spec_with_report(SPEC).unwrap();
        let doc: Value = serde_yaml::from_str(&out).unwrap();
        let schemas = &doc["components"]["schemas"];
        let required = |node: &Value| -> Vec<String> {
            node.get("required")
                .and_then(Value::as_sequence)
                .map(|seq| {
                    seq.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default()
        };
        assert_eq!(required(&schemas["WebhookConfig"]), ["url"]);
        assert!(schemas["WebhookConfig"]["properties"]["nested"]
            .get("required")
            .is_none());
        assert_eq!(required(&schemas["Request"]), ["url"]);
        let quote = &doc["paths"]["/stake/quote"]["get"]["responses"]["200"]["content"]
            ["application/json"]["schema"];
        assert_eq!(required(quote), ["required_amount"]);
        // secret, nested, nested.inner, currency.
        assert_eq!(report.response_required_relaxed, 4);
        let (again, report) = normalize_spec_with_report(&out).unwrap();
        assert_eq!(again, out);
        assert_eq!(report.response_required_relaxed, 0);
    }

    #[test]
    fn every_kept_required_entry_exists_in_the_vendored_spec() {
        // A renamed or removed server field must not leave a dead entry that
        // silently keeps nothing.
        let src = include_str!("../../../spec/openapi.yaml");
        let doc: Value = serde_yaml::from_str(src).unwrap();
        let mut present = BTreeSet::new();
        for path in response_schema_paths(&doc) {
            let key = schema_key(&doc, &path);
            if let Some(required) = value_at_path(&doc, &path)
                .and_then(|schema| schema.get("required"))
                .and_then(Value::as_sequence)
            {
                for property in required.iter().filter_map(Value::as_str) {
                    present.insert((key.clone(), property.to_owned()));
                }
            }
        }
        let missing: Vec<_> = KEEP_REQUIRED
            .iter()
            .filter(|(key, property)| {
                !present.contains(&((*key).to_owned(), (*property).to_owned()))
            })
            .collect();
        assert!(
            missing.is_empty(),
            "stale KEEP_REQUIRED entries: {missing:?}"
        );
    }

    const CLOSED_OBJECT_SPEC: &str = r#"
openapi: 3.0.0
info:
  title: test
  version: 0.0.0
paths:
  /thing:
    get:
      responses:
        '200':
          description: ok
          content:
            application/json:
              schema:
                type: object
                additionalProperties: false
                properties:
                  id:
                    type: string
components:
  schemas:
    Error:
      type: object
      additionalProperties: false
      properties:
        error:
          type: string
    Open:
      type: object
      additionalProperties: true
"#;

    #[test]
    fn closed_objects_are_opened_everywhere() {
        let (out, _) = normalize_spec_with_report(CLOSED_OBJECT_SPEC).unwrap();
        assert!(
            !out.contains("additionalProperties: false"),
            "closed object survived:\n{out}"
        );
        assert!(
            out.contains("additionalProperties: true"),
            "non-false value must be left alone:\n{out}"
        );
    }
}
