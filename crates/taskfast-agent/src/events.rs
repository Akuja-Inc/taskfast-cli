// SPDX-License-Identifier: MIT
//! Fetch single pages of agent events using opaque server cursors.

use taskfast_client::api::types::{AgentEvent, AgentEventListResponse, PaginatedMeta};
use taskfast_client::{Error, Result, TaskFastClient};

/// One unparseable event carried alongside the decoded page so callers
/// can surface partial success instead of failing the whole batch.
#[derive(Debug, Clone)]
pub struct UnparseableEvent {
    /// Raw JSON as the server sent it. Kept so operators can diff
    /// against the expected schema off-line.
    pub raw: serde_json::Value,
    /// serde decode error message in canonical form.
    pub error: String,
}

/// Event page tolerant of per-item decode failures.
///
/// `meta` is still parsed strictly — if the pagination envelope breaks,
/// the whole call fails (we cannot advance the cursor without it).
/// Only individual `data[]` entries are allowed to fail independently.
#[derive(Debug, Clone)]
pub struct TolerantEventPage {
    pub events: Vec<AgentEvent>,
    pub unparseable: Vec<UnparseableEvent>,
    pub meta: PaginatedMeta,
}

/// One-shot `GET /agents/me/events?cursor=…&limit=…`.
pub async fn list_events_page(
    client: &TaskFastClient,
    cursor: Option<&str>,
    limit: Option<i64>,
) -> Result<AgentEventListResponse> {
    // progenitor 0.14 / typify generates `limit` as `Option<NonZeroU64>`
    // (the spec's `minimum: 1` now lives in the type).
    let limit = limit.and_then(taskfast_client::page_limit);
    client.call(|c| c.list_agent_events(cursor, limit)).await
}

/// Decode a single event JSON value tolerantly into an [`AgentEvent`].
pub fn decode_event_item(
    item: serde_json::Value,
) -> std::result::Result<AgentEvent, UnparseableEvent> {
    match serde_json::from_value::<AgentEvent>(item.clone()) {
        Ok(ev) => Ok(ev),
        Err(e) => Err(UnparseableEvent {
            raw: item,
            error: e.to_string(),
        }),
    }
}

/// Tolerant variant of [`list_events_page`].
///
/// Tries the strict progenitor decode first. On [`Error::Decode`] (the
/// only failure mode a schema-drift-shaped single event can produce),
/// falls back to a raw JSON fetch and decodes `data[]` item-by-item so
/// one malformed event does not poison the whole page.
///
/// The pagination envelope (`meta`) is always strict — without it we
/// cannot advance the cursor, so bailing is the only safe choice.
///
/// Cost: two HTTP round-trips on the Decode path (strict then raw).
/// Acceptable because Decode only fires on schema drift — hot path is
/// one request. Do **not** "optimize" by making raw the default; the
/// strict path keeps type safety for the common case.
pub async fn list_events_page_tolerant(
    client: &TaskFastClient,
    cursor: Option<&str>,
    limit: Option<i64>,
) -> Result<TolerantEventPage> {
    match list_events_page(client, cursor, limit).await {
        Ok(page) => Ok(TolerantEventPage {
            events: page.data,
            unparseable: Vec::new(),
            meta: page.meta,
        }),
        Err(Error::Decode(_)) => decode_raw_page(client, cursor, limit).await,
        Err(e) => Err(e),
    }
}

async fn decode_raw_page(
    client: &TaskFastClient,
    cursor: Option<&str>,
    limit: Option<i64>,
) -> Result<TolerantEventPage> {
    let raw = client.list_agent_events_raw(cursor, limit).await?;
    // serde_json::from_value on Null produces a proper serde_json::Error,
    // which Error::Decode converts via #[from]. Meta is strict by design:
    // a broken envelope means we cannot advance the cursor.
    let meta_v = raw.get("meta").cloned().unwrap_or(serde_json::Value::Null);
    let meta: PaginatedMeta = serde_json::from_value(meta_v)?;
    let data = raw
        .get("data")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut events = Vec::with_capacity(data.len());
    let mut unparseable = Vec::new();
    for item in data {
        match decode_event_item(item) {
            Ok(ev) => events.push(ev),
            Err(e) => unparseable.push(e),
        }
    }
    Ok(TolerantEventPage {
        events,
        unparseable,
        meta,
    })
}
