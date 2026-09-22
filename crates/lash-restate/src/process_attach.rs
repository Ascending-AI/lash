#![allow(
    deprecated,
    reason = "Restate SDK 0.11 retains the trait service API while its replacement is staged"
)]

//! The durable-wait arming a parked `processes.await` rides on.
//!
//! One responsibility: hold the wait for a process terminal *outside* the
//! invocation that parked on it. The turn that calls `processes.await` must go
//! on to park on its completion key through the ordinary await-event path, so
//! it cannot also sit on the process terminal — a second concurrent wait inside
//! that handler would put the terminal read in its journal, where its position
//! is no longer replay-deterministic against the park. Arming therefore sends a
//! one-way invocation to this workflow, which does the waiting and then
//! resolves the key through the same durable-wait index every other resolver
//! uses.
//!
//! The workflow is keyed by the wait's own durable-wait address, which makes
//! the arming idempotent for free: a redrive of the parked turn re-sends the
//! same invocation to the same workflow key, and Restate attaches to the run
//! already in flight instead of starting a second waiter.

use lash_core::{AwaitEventKey, ProcessRef, Resolution};
use restate_sdk::context::{ContextClient, WorkflowContext};
use restate_sdk::errors::HandlerResult;
use restate_sdk::serde::Json;
use serde::{Deserialize, Serialize};

use crate::durable_wait::{
    LashDurableWaitIndexClient, RestateDurableWaitAddress, RestateDurableWaitResolveRequest,
    durable_wait_index_object_key,
};
use crate::process::{LashProcessWorkflowClient, RestateProcessAwaitRequest};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RestateProcessAttachRequest {
    /// The exact incarnation whose terminal resolves the wait. A later
    /// incarnation of the same id is a different process and never resolves it.
    pub process_ref: ProcessRef,
    /// The wait to resolve once that terminal lands.
    pub key: AwaitEventKey,
}

/// The Restate address of the attach workflow that owns `key`'s arming.
///
/// Deliberately the wait's own durable-wait workflow key: one arming per wait,
/// re-sendable, and trivially correlated with the promise it resolves.
pub(crate) fn process_attach_workflow_key(key: &AwaitEventKey) -> String {
    RestateDurableWaitAddress::for_key(key).workflow_key
}

/// Bind [`LashProcessAttachImpl::serve`] on every endpoint that binds
/// [`LashProcessWorkflow`](crate::process::LashProcessWorkflow) and the
/// durable-wait services: a deployment that arms process terminals without it
/// parks calls nothing will ever resolve.
#[restate_sdk::workflow]
pub trait LashProcessAttach {
    async fn run(request: Json<RestateProcessAttachRequest>) -> HandlerResult<Json<()>>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct LashProcessAttachImpl;

impl LashProcessAttach for LashProcessAttachImpl {
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(request): Json<RestateProcessAttachRequest>,
    ) -> HandlerResult<Json<()>> {
        let RestateProcessAttachRequest { process_ref, key } = request;
        let output = ctx
            .workflow_client::<LashProcessWorkflowClient>(process_ref.process_id.to_string())
            .await_terminal(Json(RestateProcessAwaitRequest {
                process_id: process_ref.process_id.clone(),
            }))
            .call()
            .await;
        // A terminal is a fact, not an error of the wait: a failed or cancelled
        // process resolves its waiters successfully with that terminal as the
        // value, exactly as the inline await path returns it. Only a terminal
        // this workflow could not observe at all becomes an error resolution,
        // so the parked call reports why instead of hanging.
        let resolution = match output {
            Ok(Json(output)) => match serde_json::to_value(&output) {
                Ok(value) => Resolution::Ok(value),
                Err(error) => Resolution::Err(lash_core::runtime::ExternalCompletionError {
                    code: lash_core::TurnFailureCode::from_wire("process_terminal_encode").into(),
                    message: error.to_string(),
                    raw: None,
                }),
            },
            Err(error) => Resolution::Err(lash_core::runtime::ExternalCompletionError {
                code: lash_core::TurnFailureCode::from_wire("process_terminal_unobservable").into(),
                message: error.to_string(),
                raw: None,
            }),
        };
        let address = RestateDurableWaitAddress::for_key(&key);
        // Resolve through the index rather than the wait workflow directly: the
        // index retains the resolution for a registration that has not happened
        // yet, so a terminal that beats the parked turn's registration is not
        // lost.
        let Json(_outcome) = ctx
            .object_client::<LashDurableWaitIndexClient>(durable_wait_index_object_key(&address))
            .resolve(Json(RestateDurableWaitResolveRequest { key, resolution }))
            .call()
            .await?;
        Ok(Json(()))
    }
}
