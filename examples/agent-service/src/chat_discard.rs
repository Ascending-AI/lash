#![allow(
    deprecated,
    reason = "Restate SDK 0.11 retains the trait service API while its replacement is staged"
)]

//! Discarding a half-built chat fork's session.
//!
//! A session is deleted by the engine: on Restate the delete's close runs as a
//! journaled effect, so it must be issued from inside a handler. The service
//! binds this workflow beside lash's own services and its fork compensator
//! calls it through ingress, keyed by the chat id, so a retried or recovered
//! discard of one chat is the same invocation.

use axum::http::StatusCode;
use lash::LashCore;
use lash_restate::{
    RestateAuthorityId, RestateConnection, RestateIngressClient, RestateSessionAdministration,
};
use restate_sdk::context::WorkflowContext;
use restate_sdk::errors::{HandlerError, HandlerResult, TerminalError};
use restate_sdk::serde::Json;
use serde::{Deserialize, Serialize};

use crate::state::{AppError, AppResult};

/// The Restate service name the workflow is bound under.
const CHAT_DISCARD_SERVICE: &str = "AgentServiceChatDiscard";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ChatDiscardRequest {
    chat_id: String,
}

#[restate_sdk::workflow]
pub(crate) trait AgentServiceChatDiscard {
    async fn run(request: Json<ChatDiscardRequest>) -> HandlerResult<Json<()>>;
}

/// The workflow: deletes one chat's session through the engine.
pub(crate) struct AgentServiceChatDiscardImpl {
    administration: RestateSessionAdministration,
}

impl AgentServiceChatDiscardImpl {
    /// The workflow over `core`'s sessions, issuing its effects to the
    /// deployment `connection` reaches under `authority`.
    pub(crate) async fn new(
        core: &LashCore,
        connection: RestateConnection,
        authority: RestateAuthorityId,
    ) -> Self {
        Self {
            administration: RestateSessionAdministration::new(
                core.session_administration().await,
                connection,
                authority,
            ),
        }
    }
}

impl AgentServiceChatDiscard for AgentServiceChatDiscardImpl {
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(request): Json<ChatDiscardRequest>,
    ) -> HandlerResult<Json<()>> {
        let execution = self.administration.for_invocation(ctx);
        let context = execution
            .delete_context(&request.chat_id)
            .map_err(TerminalError::from_error)?;
        LashCore::delete_session(context)
            .await
            .map_err(|error| -> HandlerError {
                if error.is_retryable() {
                    HandlerError::from(error)
                } else {
                    TerminalError::from_error(error).into()
                }
            })?;
        Ok(Json(()))
    }
}

/// Delete `chat_id`'s session through the workflow and wait for it.
pub(crate) async fn discard_chat_session(
    ingress: &RestateIngressClient,
    chat_id: &str,
) -> AppResult<()> {
    ingress
        .call_workflow_json::<_, ()>(
            CHAT_DISCARD_SERVICE,
            chat_id,
            "run",
            &ChatDiscardRequest {
                chat_id: chat_id.to_string(),
            },
        )
        .await
        .map_err(|error| AppError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: format!("discard chat `{chat_id}`'s session: {error}"),
        })
}
