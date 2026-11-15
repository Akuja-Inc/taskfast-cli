// SPDX-License-Identifier: MIT
//! Two-phase task creation for poster-role agents.
//!
//! Two-phase task posting is handled directly by `taskfast-cli/src/cmd/post.rs`:
//!
//! Phase 1: `prepare_task_draft` asks the server to prepare an unsigned payload
//! and ERC-20 token address for the submission fee.
//!
//! Phase 2: The poster signs and broadcasts the submission-fee transfer locally via
//! `TempoSession`, producing a transaction voucher submitted with `submit_task_draft`.
//!
//! Direct client pass-through functions were deprecated per GH#216.

use taskfast_client::api::types::{
    TaskDraftPrepareRequest, TaskDraftPrepareResponse, TaskDraftSubmitRequest,
    TaskDraftSubmitResponse,
};
use taskfast_client::{map_api_error, Result, TaskFastClient};
use uuid::Uuid;

/// `POST /task_drafts` — prepare an unsigned task payload for offline signing.
#[deprecated(note = "call client.inner().prepare_task_draft directly")]
pub async fn create_task_draft(
    client: &TaskFastClient,
    body: &TaskDraftPrepareRequest,
) -> Result<TaskDraftPrepareResponse> {
    match client.inner().prepare_task_draft(body).await {
        Ok(v) => Ok(v.into_inner()),
        Err(e) => Err(map_api_error(e).await),
    }
}

/// `POST /task_drafts/{draft_id}/submit` — exchange the signed payload for a live task.
#[deprecated(note = "call client.inner().submit_task_draft directly")]
pub async fn submit_task_draft(
    client: &TaskFastClient,
    draft_id: &Uuid,
    body: &TaskDraftSubmitRequest,
) -> Result<TaskDraftSubmitResponse> {
    match client.inner().submit_task_draft(draft_id, body).await {
        Ok(v) => Ok(v.into_inner()),
        Err(e) => Err(map_api_error(e).await),
    }
}
