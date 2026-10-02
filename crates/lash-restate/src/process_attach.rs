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
//!
//! The workflow lives as long as the wait it serves, not as long as the
//! process. It registers a watch on its wait with the wait's index and races
//! the terminal against it: a wait that ends first, cancelled, timed out or
//! closed by its call's cancel decision, ends the workflow, which cancels its
//! own terminal read. A process nothing will ever end, or one its caller
//! chose to leave running, therefore holds no waiter for a caller that is
//! gone.

use std::sync::Arc;

use lash_core::runtime::attachment_delivery::{DeliveryAcquisition, source_gone_output};
use lash_core::{AwaitEventKey, ProcessAwaitOutput, ProcessId, Resolution};
use restate_sdk::context::{
    CallFuture as _, ContextAwakeables as _, ContextClient as _, WorkflowContext,
};
use restate_sdk::errors::{HandlerResult, TerminalError};
use restate_sdk::serde::Json;
use serde::{Deserialize, Serialize};

use crate::compat::{Call, Reply};
use crate::controller::RestateControllerContext as _;
use crate::durable_wait::{
    LASH_REPLAY_KEY_HEADER, RestateDurableWaitAddress, RestateDurableWaitAwakeableRequest,
    RestateDurableWaitRegistration, RestateDurableWaitResolveRequest, RestateTurnCancelWake,
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
        let replay_key = key.key_id.clone();
        let address = RestateDurableWaitAddress::for_key(&key);
        let index_key = durable_wait_index_object_key(&address);
        // Watch the wait before reading the terminal: the index wakes the
        // watch however the wait ends, and at once for a wait that already
        // ended, so no terminal read outlives the wait it would resolve.
        let (awakeable_id, wait_ended) = ctx.awakeable::<Json<RestateTurnCancelWake>>();
        let watch = RestateDurableWaitAwakeableRequest {
            key: key.clone(),
            awakeable_id,
            hand_over: None,
        };
        let registration = self
            .namespace
            .durable_wait_registry(&ctx, index_key.clone())
            .register_awakeable(watch.clone())
            .header(LASH_REPLAY_KEY_HEADER.to_string(), replay_key.clone())
            .call()
            .await?
            .into_body();
        if registration == RestateDurableWaitRegistration::Revoked {
            return Ok(Reply::at(wire, ()));
        }
        // The terminal lives on the stable root, whatever lane the process's
        // last segment ran under (FIG-3795).
        let terminal =
            crate::process::await_terminal_on_stable_run(&ctx, &self.namespace, process_id.clone())
                .call();
        let terminal_read = terminal
            .invocation_handle()
            .await?
            .invocation_id()
            .to_owned();
        // The wait's end is listed first: with both ready, a terminal nobody
        // is left to receive is not acquired for them.
        let output = restate_sdk::select! {
            wake = wait_ended => {
                wake?;
                // The read is this workflow's own call, so it ends here with
                // the workflow.
                ctx.invocation_handle(terminal_read).cancel();
                return Ok(Reply::at(wire, ()));
            },
            output = terminal => output,
        };
        // A terminal is a fact, not an error of the wait: a failed or cancelled
        // process resolves its waiters successfully with that terminal as the
        // value, exactly as the inline await path returns it.
        let resolution = match output {
            Ok(reply) => {
                let output = reply.into_body();
                let delivered = match self.acquire_delivered(&ctx, &key, &output).await? {
                    DeliveryAcquisition::Held => Ok(output),
                    DeliveryAcquisition::SourceGone { digest } => Ok(source_gone_output(&digest)),
                    DeliveryAcquisition::ReceiverEnded { .. } => {
                        // Nothing resolves the key, so nothing else drops
                        // the watch.
                        self.namespace
                            .durable_wait_registry(&ctx, index_key)
                            .unregister_awakeable(watch)
                            .header(LASH_REPLAY_KEY_HEADER.to_string(), replay_key)
                            .call()
                            .await?;
                        return Ok(Reply::at(wire, ()));
                    }
                    DeliveryAcquisition::Refused { refusal } => Err(refusal),
                };
                match delivered {
                    Ok(output) => match serde_json::to_value(output) {
                        Ok(value) => Resolution::Ok(value),
                        Err(error) => {
                            Resolution::Err(lash_core::runtime::ExternalCompletionError {
                                code: lash_core::TurnFailureCode::from_wire(
                                    "process_terminal_encode",
                                )
                                .into(),
                                message: error.to_string(),
                                raw: None,
                            })
                        }
                    },
                    Err(refusal) => Resolution::Err(lash_core::runtime::ExternalCompletionError {
                        code: (&refusal.code).into(),
                        message: refusal.message,
                        raw: None,
                    }),
                }
            }
            Err(error) => Resolution::Err(lash_core::runtime::ExternalCompletionError {
                code: lash_core::TurnFailureCode::from_wire("process_terminal_unobservable").into(),
                message: error.to_string(),
                raw: None,
            }),
        };
        // Resolve through the index rather than the wait workflow directly: the
        // index retains the resolution for a registration that has not happened
        // yet, so a terminal that beats the parked turn's registration is not
        // lost. The resolve ends the wait, so it also drops this workflow's
        // watch.
        self.namespace
            .durable_wait_registry(&ctx, index_key)
            .resolve(RestateDurableWaitResolveRequest { key, resolution })
            .header(LASH_REPLAY_KEY_HEADER.to_string(), replay_key)
            .call()
            .await?;
        Ok(Reply::at(wire, ()))
    }
}

impl LashProcessAttachImpl {
    /// Acquire the waiter's referrer edge on every stored attachment
    /// `output` delivers, recording its typed acquisition verdict once.
    /// Only a transient storage fault retries without recording a result.
    async fn acquire_delivered(
        &self,
        ctx: &WorkflowContext<'_>,
        key: &AwaitEventKey,
        output: &ProcessAwaitOutput,
    ) -> HandlerResult<DeliveryAcquisition> {
        let attachments = Arc::clone(&self.attachments);
        let receiver = key.scope.clone();
        let output = output.clone();
        let restate_sdk::serde::Json(delivered) = ctx
            .run_json_or_retry_send::<DeliveryAcquisition, _>(
                PROCESS_ATTACH_ACQUIRE_STEP.to_string(),
                async move {
                    lash_core::runtime::attachment_delivery::acquire_delivered_attachments(
                        attachments.as_ref(),
                        &receiver,
                        &output,
                    )
                    .await
                    .map_err(|error| error.to_string())
                },
            )
            .await?;
        Ok(delivered)
    }
}
