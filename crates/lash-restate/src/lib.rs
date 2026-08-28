//! Restate durable execution adapter for Lash runtime effects.
//!
//! The primary entrypoint is [`RestateRuntimeEffectController`]. Construct it inside
//! a Restate service, object, or workflow handler, derive a stable
//! [`ScopedEffectController`](lash_core::ScopedEffectController) from an
//! [`ExecutionScope`](lash_core::ExecutionScope), and run Lash through the scoped API.
//! Restate recovery is handler replay with the same scope id and request data,
//! not Lash checkpoint reload.
//!
//! ```rust,ignore
//! use lash_restate::RestateRuntimeEffectController;
//! use restate_sdk::prelude::*;
//!
//! # #[derive(serde::Serialize, serde::Deserialize)]
//! # struct TurnRequest {
//! #     turn_id: String,
//! # }
//! # #[derive(serde::Serialize, serde::Deserialize)]
//! # struct TurnResponse;
//! # async fn run_lash_turn(
//! #     _scope: lash_core::ScopedEffectController<'_>,
//! #     _req: TurnRequest,
//! # ) -> Result<TurnResponse, std::io::Error> {
//! #     Ok(TurnResponse)
//! # }
//! #[restate_sdk::workflow]
//! pub trait AgentTurnWorkflow {
//!     async fn run(req: Json<TurnRequest>) -> HandlerResult<Json<TurnResponse>>;
//! }
//!
//! pub struct AgentTurnWorkflowImpl;
//!
//! impl AgentTurnWorkflow for AgentTurnWorkflowImpl {
//!     async fn run(
//!         &self,
//!         ctx: WorkflowContext<'_>,
//!         Json(req): Json<TurnRequest>,
//!     ) -> HandlerResult<Json<TurnResponse>> {
//!         let effect_controller = RestateRuntimeEffectController::new(ctx);
//!         let turn_id = req.turn_id.clone();
//!         let scoped_effect_controller = effect_controller
//!             .scoped_effect_controller(lash_core::ExecutionScope::turn("session", &turn_id))
//!             .map_err(TerminalError::from_error)?;
//!         let response = run_lash_turn(scoped_effect_controller, req)
//!             .await
//!             .map_err(TerminalError::from_error)?;
//!         Ok(Json(response))
//!     }
//! }
//! ```
//!
//! Restate's Rust SDK requires `ctx.run` closures to be awaited immediately and
//! not to call the Restate context from inside the closure. This adapter wraps
//! atomic Lash effects in immediately awaited
//! `ctx.run(...).name(lash:<replay_key>)` calls. Composite tool-batch and
//! exec-code interpreters are rebuilt on every handler attempt while their
//! nested atomic effects retain stable replay keys. Sleep commands map to
//! Restate's durable timer, and process commands call Restate workflow
//! scheduling directly through idempotent registry/workflow operations.
//! Substrate-native Restate turns do not use store-side in-flight replay rows;
//! Lash only commits final session state through turn-commit idempotency.
//!
//! Endpoints using this controller must also bind
//! [`LashDurableWaitWorkflowImpl`] and [`LashDurableWaitIndexImpl`]. The first
//! owns exact-address promises and durable deadline timers for every
//! [`ExecutionScope`](lash_core::ExecutionScope); the second indexes
//! session-owned waits so cancellation and deletion can resolve them durably.
//! Await-event identity epoch 4 uses the v2 wait-index namespace and marker;
//! requests and indexed wait values carry the `AwaitEventKey` preimage so each
//! handler derives scope, classification, and workflow address locally.
//! Before upgrading, drain and recreate both Restate services' state. Every
//! post-cutover register, resolve, renew, and woken-settle path crosses the
//! index epoch gate and rejects pre-cutover state with a recreate instruction.
//! A fully parked pre-cutover invocation cannot execute that new gate and its v2
//! workflow address is unreachable from v4 resolutions; it never
//! self-terminates. Draining and purging those invocations before the cutover is
//! the only remedy.

mod controller;
mod durable_wait;
mod effect_group;
mod effect_host;
mod ingress;
mod process;
mod turn;

pub use restate_sdk;

pub use controller::{
    RestateEffectControllerOptions, RestateEffectError, RestateRuntimeEffectController,
};
pub use durable_wait::{
    LashDurableWaitIndex, LashDurableWaitIndexClient, LashDurableWaitIndexImpl,
    LashDurableWaitWorkflow, LashDurableWaitWorkflowClient, LashDurableWaitWorkflowImpl,
    RestateDurableWaitAddress, RestateDurableWaitAwaitRequest, RestateDurableWaitAwakeableRequest,
    RestateDurableWaitClassification, RestateDurableWaitIndexRequest,
    RestateDurableWaitRegistration, RestateDurableWaitResolveRequest, RestateDurableWaitScope,
    RestateDurableWaitSettleRequest, ServeLashDurableWaitIndex, ServeLashDurableWaitWorkflow,
};
pub use effect_group::{
    EffectGroupAdmissionRequest, EffectGroupAdmissionResponse, EffectGroupAdoptRequest,
    EffectGroupCleanup, EffectGroupCleanupFacts, EffectGroupCloseDisposition,
    EffectGroupCloseRequest, EffectGroupCloseResponse, EffectGroupDispatch,
    EffectGroupDispatchRequest, EffectGroupDispatchState, EffectGroupFinishRetirementResponse,
    EffectGroupIndex, EffectGroupOpenRequest, EffectGroupOpenResponse, EffectGroupPayload,
    EffectGroupPayloadGetResponse, EffectGroupPayloadPutRequest, EffectGroupPayloadPutResponse,
    EffectGroupPhase, EffectGroupProbeAdoptResponse, EffectGroupProbeResponse,
    EffectGroupReadRankRequest, EffectGroupReadRankResponse, EffectGroupRecordDispatchRequest,
    EffectGroupRecordDispatchResponse, EffectGroupRecordSettlementRequest,
    EffectGroupRecordSettlementResponse, EffectGroupRefusal, EffectGroupRefusalRequest,
    EffectGroupRegisterRefusalResponse, EffectGroupRegisterRequest, EffectGroupRegisterResponse,
    EffectGroupRetireResponse, EffectGroupRetirementCancelResponse, EffectGroupSettlementRecord,
    EffectGroupSettlementTerminal, EffectGroupShape, EffectGroupWaitResolution,
    RestateEffectGroupRetryPolicy, RestateEffectGroupServices, RestateEffectGroupWaitServices,
};
pub use effect_host::RestateEffectHost;
pub use ingress::{
    DeploymentOpenInvocations, RestateAdminClient, RestateConnection, RestateConnectionConfig,
    RestateHttpError, RestateIngressClient, RestateInvocationId, RestateInvocationStatus,
};
pub use process::{
    LashProcessWorkflow, LashProcessWorkflowClient, LashProcessWorkflowImpl,
    RestateCoreProcessRunner, RestateProcessAwaitRequest, RestateProcessCancelRequest,
    RestateProcessCancelSignal, RestateProcessCompleteRequest, RestateProcessDeployment,
    RestateProcessIngressRunner, RestateProcessRunner, RestateProcessWorkflowInput,
    RestateProcessWorkflowOutput, ServeLashProcessWorkflow,
};
pub use turn::{RestateTurnAttach, RestateTurnDeployment};

// Adapter-internal wire and seam types. They are `pub` so the Restate SDK's
// generated handlers can name them, and `doc(hidden)` because they are not a
// host contract.
#[doc(hidden)]
pub use controller::RestateControllerContext;
#[doc(hidden)]
pub use durable_wait::RestateTurnCancelRaceOutcome;

#[cfg(test)]
mod tests;
