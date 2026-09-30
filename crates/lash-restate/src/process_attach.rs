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
//!
//! Before it resolves the key, the workflow acquires the waiter's referrer
//! edge on every stored attachment the terminal delivers, in a journaled step
//! (ADR 0124): the waiter records the value only after it holds what the
//! value names, so the process's own edges may end once the key resolves.

use std::sync::Arc;

use lash_core::{AwaitEventKey, ProcessAwaitOutput, ProcessId, Resolution};
use restate_sdk::context::WorkflowContext;
use restate_sdk::errors::HandlerResult;
use serde::{Deserialize, Serialize};

use crate::compat::{Call, Reply};
use crate::controller::RestateControllerContext as _;
use crate::durable_wait::{
    LASH_REPLAY_KEY_HEADER, RestateDurableWaitAddress, RestateDurableWaitResolveRequest,
    durable_wait_index_object_key,
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    into = "crate::process::StampedAttachRequest",
    try_from = "serde_json::Value"
)]
pub struct RestateProcessAttachRequest {
    /// The process whose terminal resolves the wait. A minted id is never
    /// reused, so no other process can resolve it (ADR 0107).
    pub process_id: ProcessId,
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

/// Holds a process-terminal wait armed for a parked caller. Every lash
/// deployment serves it beside the process workflow and the durable-wait
/// services (`crate::services::bind_lash_services`): a deployment that armed
/// process terminals without it would park calls nothing ever resolves.
#[restate_sdk::workflow]
pub trait LashProcessAttach {
    async fn run(call: Call<RestateProcessAttachRequest>) -> HandlerResult<Reply<()>>;
}

/// The journaled step that acquires the waiter's edges on a terminal's
/// delivered attachments before the key resolves.
const PROCESS_ATTACH_ACQUIRE_STEP: &str = "process-attach-acquire";

/// [`LashProcessAttach`] in one deployment's namespace (FIG-3898).
#[derive(Clone)]
pub(crate) struct LashProcessAttachImpl {
    namespace: crate::RestateNamespace,
    /// The deployment's attachment referrers: the waiter's edges are
    /// acquired here before the key resolves.
    attachments: Arc<dyn lash_core::AttachmentReferrers>,
}

impl LashProcessAttachImpl {
    pub(crate) fn new(
        namespace: crate::RestateNamespace,
        attachments: Arc<dyn lash_core::AttachmentReferrers>,
    ) -> Self {
        Self {
            namespace,
            attachments,
        }
    }
}

impl LashProcessAttach for LashProcessAttachImpl {
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        call: Call<RestateProcessAttachRequest>,
    ) -> HandlerResult<Reply<()>> {
        let (wire, request) = call.open()?;
        let RestateProcessAttachRequest { process_id, key } = request;
        // The terminal lives on the stable root, whatever lane the process's
        // last segment ran under (FIG-3795).
        let output = crate::process::await_terminal_on_stable_root(
            &ctx,
            &self.namespace,
            process_id.clone(),
        )
        .call()
        .await;
        // A terminal is a fact, not an error of the wait: a failed or cancelled
        // process resolves its waiters successfully with that terminal as the
        // value, exactly as the inline await path returns it. Only a terminal
        // this workflow could not observe at all becomes an error resolution,
        // so the parked call reports why instead of hanging.
        let resolution = match output {
            Ok(reply) => match serde_json::to_value(
                self.acquire_delivered(&ctx, &key, reply.into_body())
                    .await?,
            ) {
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
        let replay_key = key.key_id.clone();
        let address = RestateDurableWaitAddress::for_key(&key);
        // Resolve through the index rather than the wait workflow directly: the
        // index retains the resolution for a registration that has not happened
        // yet, so a terminal that beats the parked turn's registration is not
        // lost.
        self.namespace
            .durable_wait_registry(&ctx, durable_wait_index_object_key(&address))
            .resolve(RestateDurableWaitResolveRequest { key, resolution })
            .header(LASH_REPLAY_KEY_HEADER.to_string(), replay_key)
            .call()
            .await?;
        Ok(Reply::at(wire, ()))
    }
}

impl LashProcessAttachImpl {
    /// Acquire the waiter's referrer edge on every stored attachment
    /// `output` delivers, in one journaled step, and answer the value the
    /// key resolves with: `output`, or the typed source-gone failure when a
    /// delivered attachment was already swept. A store fault ends the attempt
    /// retryably and records nothing.
    async fn acquire_delivered(
        &self,
        ctx: &WorkflowContext<'_>,
        key: &AwaitEventKey,
        output: ProcessAwaitOutput,
    ) -> HandlerResult<ProcessAwaitOutput> {
        let attachments = Arc::clone(&self.attachments);
        let receiver = key.scope.clone();
        let restate_sdk::serde::Json(delivered) = ctx
            .run_json_or_retry_send::<ProcessAwaitOutput, _>(
                PROCESS_ATTACH_ACQUIRE_STEP.to_string(),
                async move {
                    lash_core::runtime::attachment_delivery::deliver_output(
                        attachments.as_ref(),
                        &receiver,
                        output,
                    )
                    .await
                    .map_err(|error| error.to_string())
                },
            )
            .await?;
        Ok(delivered)
    }
}
