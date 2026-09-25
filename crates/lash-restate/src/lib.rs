//! Restate durable execution adapter for Lash runtime effects.
//!
//! The primary entrypoint is [`RestateRuntimeEffectController`].
//! Restate recovery is handler replay with the same scope id and request data, not Lash
//! checkpoint reload.
//!
//! ```rust,ignore
//! use lash_core::TurnId;
//! use lash_restate::RestateRuntimeEffectController;
//! use restate_sdk::prelude::*;
//!
//! # #[derive(serde::Serialize, serde::Deserialize)]
//! # struct TurnRequest {
//! #     turn_id: TurnId,
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
//!         let authority_id = lash_restate::RestateAuthorityId::new(
//!             "production-restate-authority",
//!         ).map_err(TerminalError::from_error)?;
//!         let effect_controller = RestateRuntimeEffectController::new(
//!             ctx,
//!             authority_id,
//!         );
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
//! `ctx.run(...).name(lash:<replay_key>)` calls. Composite exec-code
//! interpreters are rebuilt on every handler attempt while their nested atomic
//! effects retain stable replay keys; a tool batch is a durable effect group
//! whose children run in their own dispatch invocations. Sleep commands map to
//! Restate's durable timer, and process commands call Restate workflow
//! scheduling directly through idempotent registry/workflow operations.
//! Substrate-native Restate turns do not use store-side in-flight replay rows;
//! Lash only commits final session state through turn-commit idempotency.
//!
//! An endpoint that serves lash work starts from
//! [`RestateBackend::endpoint_builder`], which binds every Restate service lash
//! itself serves; the host binds only its own services on it. Among lash's are
//! the durable-wait workflow, which owns exact-address promises and durable
//! deadline timers for every [`ExecutionScope`](lash_core::ExecutionScope),
//! and the durable-wait index, which indexes session-owned waits so
//! cancellation and deletion can resolve them durably.
//! Await-event identity epoch 6 uses the v2 wait-index namespace and marker;
//! requests and indexed wait values carry the `AwaitEventKey` preimage so each
//! handler derives scope, classification, and workflow address locally.
//! There is no migration across that cutover: every register, resolve,
//! renew, and woken-settle path crosses the index epoch gate and refuses
//! pre-cutover state, typed and before any effect. A pre-cutover invocation
//! suspended on a v2 workflow address never reaches that gate and is
//! unreachable from v4 resolutions, so it never self-terminates; an operator
//! cancels it.

mod backend;
mod controller;
mod durable_wait;
mod effect_group;
mod effect_host;
mod ingress;
mod process;
mod process_attach;
mod process_stop;
mod services;
mod session_administration;
mod turn;
mod turn_handler;

pub use restate_sdk;

pub use backend::{RestateBackend, RestateQueuedWork};
pub use controller::{
    EFFECT_JOURNAL_VERSION, PROCESS_COMMAND_JOURNAL_PAYLOAD_VERSION,
    RestateEffectControllerOptions, RestateEffectError, RestateRuntimeEffectController,
};
pub use durable_wait::{
    DURABLE_WAIT_INDEX_IDENTITY_EPOCH, DURABLE_WAIT_REQUEST_VERSION, RestateDurableWaitAddress,
    RestateDurableWaitAwaitInput, RestateDurableWaitAwaitRequest,
    RestateDurableWaitAwakeableRequest, RestateDurableWaitCancelDecidedRequest,
    RestateDurableWaitClassification, RestateDurableWaitDeadline, RestateDurableWaitEffectRequest,
    RestateDurableWaitGroupRequest, RestateDurableWaitIndexRequest, RestateDurableWaitRegistration,
    RestateDurableWaitResolveRequest, RestateDurableWaitResolveResponse, RestateDurableWaitScope,
    RestateDurableWaitSettleRequest,
};
pub use effect_group::{
    EFFECT_GROUP_INDEX_PROTOCOL_VERSION, EffectGroupAdmissionRequest, EffectGroupAdmissionResponse,
    EffectGroupAdoptRequest, EffectGroupCleanup, EffectGroupCleanupFacts,
    EffectGroupCloseDisposition, EffectGroupCloseRequest, EffectGroupCloseResponse,
    EffectGroupDispatchRequest, EffectGroupDispatchState, EffectGroupFinishRetirementResponse,
    EffectGroupOpenRequest, EffectGroupOpenResponse, EffectGroupPayloadGetResponse,
    EffectGroupPayloadPutRequest, EffectGroupPayloadPutResponse, EffectGroupPhase,
    EffectGroupProbeAdoptResponse, EffectGroupProbeResponse, EffectGroupReadRankRequest,
    EffectGroupReadRankResponse, EffectGroupRecordDispatchRequest,
    EffectGroupRecordDispatchResponse, EffectGroupRecordSettlementRequest,
    EffectGroupRecordSettlementResponse, EffectGroupRefusal, EffectGroupRefusalRequest,
    EffectGroupRegisterRefusalResponse, EffectGroupRegisterRequest, EffectGroupRegisterResponse,
    EffectGroupRetireResponse, EffectGroupRetirementCancelResponse, EffectGroupSettlementRecord,
    EffectGroupSettlementTerminal, EffectGroupShape, EffectGroupWaitResolution,
};
pub use effect_host::RestateEffectHost;
pub use ingress::{
    DeploymentOpenInvocations, RestateAdminClient, RestateAuthorityId, RestateConnection,
    RestateConnectionConfig, RestateHttpError, RestateIngressClient, RestateInvocationId,
    RestateInvocationLifecycle, RestateInvocationStatus,
};
pub use process::{
    RESTATE_PROCESS_JOURNAL_VERSION, RestateProcessAwaitRequest, RestateProcessCancelRequest,
    RestateProcessCancelSignal, RestateProcessCompleteRequest, RestateProcessDeployment,
    RestateProcessIngressRunner, RestateProcessServing, RestateProcessWorkerSlot,
    RestateProcessWorkflowInput, RestateProcessWorkflowOutput, SegmentStarted,
};
pub use process_attach::RestateProcessAttachRequest;
pub use session_administration::{RestateSessionAdministration, RestateSessionDeleteExecution};
pub use turn::RestateTurnAttach;
pub use turn_handler::{
    TURN_HANDLER_MAX_ATTEMPTS, park_generation_refused_turn, parked_turn_failure,
    turn_handler_options, turn_service,
};

// Adapter-internal wire and seam types. They are `pub` so the Restate SDK's
// generated handlers can name them; they are not a host contract.
pub use controller::RestateControllerContext;
pub use durable_wait::RestateTurnCancelRaceOutcome;

// Lash's own Restate services. A deployment binds them only through
// `RestateBackend::endpoint_builder`, so they are not a host contract; the
// crate's tests drive them one at a time.
#[cfg(test)]
pub(crate) use durable_wait::{
    LashDurableWaitIndex, LashDurableWaitIndexImpl, LashDurableWaitWorkflow,
    LashDurableWaitWorkflowImpl,
};
#[cfg(test)]
pub(crate) use effect_group::EffectGroupDispatch;
#[cfg(test)]
pub(crate) use process::{
    LashProcessWorkflow, LashProcessWorkflowImpl, RestateCoreProcessRunner, RestateProcessRunner,
};
pub(crate) use services::LashService;

#[cfg(test)]
mod tests;
