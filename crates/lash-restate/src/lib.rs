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
//! [`RestateEngine::endpoint_builder`], which binds every Restate service lash
//! itself serves; the host binds only its own services on it. Among lash's are
//! the durable-wait workflow, which owns exact-address promises and durable
//! deadline timers for every [`ExecutionScope`](lash_core::ExecutionScope),
//! and the durable-wait index, which indexes session-owned waits so
//! cancellation and deletion can resolve them durably.
//! The durable-wait services use the v2 wait-index namespace; requests and
//! indexed wait values carry the `AwaitEventKey` preimage so each handler
//! derives scope, classification, and workflow address locally. A pre-cutover
//! invocation suspended on a v2 workflow address is unreachable from v4
//! resolutions, so it never self-terminates; an operator cancels it.

mod controller;
mod durable_wait;
mod effect_group;
mod effect_host;
mod engine;
mod formats;
mod ingress;
mod object_state;
mod process;
mod process_attach;
mod process_stop;
mod sentinel;
mod services;
mod session_administration;
mod session_control;
mod session_driver;
mod session_reconcile;
mod turn;
mod turn_handler;

pub use restate_sdk;

pub use controller::{
    EFFECT_JOURNAL_VERSION, PROCESS_COMMAND_JOURNAL_PAYLOAD_VERSION,
    RestateEffectControllerOptions, RestateEffectError, RestateRuntimeEffectController,
};
pub use durable_wait::{
    DURABLE_WAIT_REGISTRY_FORMAT_VERSION, DURABLE_WAIT_REQUEST_VERSION, RestateDurableWaitAddress,
    RestateDurableWaitAwaitInput, RestateDurableWaitAwaitRequest,
    RestateDurableWaitAwakeableRequest, RestateDurableWaitCancelDecidedRequest,
    RestateDurableWaitClassification, RestateDurableWaitDeadline, RestateDurableWaitEffectRequest,
    RestateDurableWaitGroupRequest, RestateDurableWaitIndexRequest, RestateDurableWaitRegistration,
    RestateDurableWaitResolveRequest, RestateDurableWaitResolveResponse, RestateDurableWaitScope,
    RestateDurableWaitSettleRequest,
};
pub use effect_group::{
    EFFECT_GROUP_DISPATCH_JOURNAL_VERSION, EFFECT_GROUP_PAYLOAD_FORMAT_VERSION,
    EFFECT_GROUP_STATE_FORMAT_VERSION, EFFECT_GROUP_WIRE_VERSION, EffectGroupAdmissionRequest,
    EffectGroupAdmissionResponse, EffectGroupAdoptRequest, EffectGroupCleanup,
    EffectGroupCleanupFacts, EffectGroupCloseDisposition, EffectGroupCloseRequest,
    EffectGroupCloseResponse, EffectGroupDispatchRequest, EffectGroupDispatchState,
    EffectGroupFinishRetirementResponse, EffectGroupOpenRequest, EffectGroupOpenResponse,
    EffectGroupPayloadGetResponse, EffectGroupPayloadPutRequest, EffectGroupPayloadPutResponse,
    EffectGroupPhase, EffectGroupProbeAdoptResponse, EffectGroupProbeResponse,
    EffectGroupReadRankRequest, EffectGroupReadRankResponse, EffectGroupRecordDispatchRequest,
    EffectGroupRecordDispatchResponse, EffectGroupRecordSettlementRequest,
    EffectGroupRecordSettlementResponse, EffectGroupRefusal, EffectGroupRefusalRequest,
    EffectGroupRegisterRefusalResponse, EffectGroupRegisterRequest, EffectGroupRegisterResponse,
    EffectGroupRetireResponse, EffectGroupRetirementCancelResponse, EffectGroupSettlementRecord,
    EffectGroupSettlementTerminal, EffectGroupShape, EffectGroupWaitResolution,
};
pub use effect_host::RestateEffectHost;
pub use engine::{RestateConfig, RestateEngine, deployment_path};
pub use formats::{EngineDurableFormat, durable_formats};
pub use ingress::{
    DeploymentOpenInvocations, RestateAdminClient, RestateAuthorityId, RestateConnection,
    RestateConnectionConfig, RestateHttpError, RestateIngressClient, RestateInvocationId,
    RestateInvocationLifecycle, RestateInvocationStatus, RestatePausedInvocation,
};
pub use process::{
    JOURNAL_LOGIC_EPOCH, PROCESS_HANDLER_MAX_ATTEMPTS, ProcessParkReconcileReport,
    RESTATE_PROCESS_JOURNAL_VERSION, RestateProcessAwaitRequest, RestateProcessCancelRequest,
    RestateProcessCancelSignal, RestateProcessCompleteRequest, RestateProcessDeployment,
    RestateProcessIngressRunner, RestateProcessServing, RestateProcessWorkerSlot,
    RestateProcessWorkflowInput, RestateProcessWorkflowOutput, RestateProcessWorkflowPayload,
    SegmentStarted, reconcile_process_parks, resume_parked_process,
};
pub use process_attach::RestateProcessAttachRequest;
pub use session_administration::{RestateSessionAdministration, RestateSessionDeleteExecution};
pub use session_driver::{
    LASH_SESSION_DRIVE_VERSION, RestateSessionDriveRequest, RestateSessionDriverSlot,
    RestateSessionWork, RestateTurnDriveRequest, turn_workflow_key,
};
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
// `RestateEngine::endpoint_builder`, so they are not a host contract; the
// crate's tests drive them one at a time.
#[cfg(test)]
pub(crate) use durable_wait::{
    LashDurableWaitRegistry, LashDurableWaitRegistryImpl, LashDurableWaitWorkflow,
    LashDurableWaitWorkflowImpl,
};
#[cfg(test)]
pub(crate) use effect_group::{EffectGroupDispatch, EffectGroupDispatchImpl};
#[cfg(test)]
pub(crate) use process::{
    LashProcessWorkflow, LashProcessWorkflowImpl, RestateCoreProcessRunner, RestateProcessRunner,
};
pub(crate) use services::LashService;

/// The wall clock a journaled wait request converts its deadline on when the
/// invoking path carries no configured clock: a host-side await API has no
/// clock channel, so the request names the system clock through this seam
/// rather than inside scanned drive code.
pub(crate) fn system_clock() -> &'static dyn lash_core::Clock {
    &lash_core::facade_support::SystemClock
}

/// A fresh, unguessable nonce drawn from OS randomness. Journaled verdicts
/// that must mint a discriminator no redrive can reproduce — a process
/// segment's admission nonce — draw it through this seam so the scanned drive
/// paths name the draw rather than spelling `Uuid::new_v4` (FIG-3672). The
/// Restate context RNG and the invocation id are never substitutes: both
/// repeat after a purge, and the nonce exists to tell those apart.
pub(crate) fn journaled_nonce() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// The boxed completion a journaled engine-context operation returns. The
/// controller context's methods hand back the Restate `ctx.run` step's
/// future; boxing keeps the trait object-safe across its generic methods.
/// FIG-3672 names the erased shape once here, outside the scanned drive
/// paths, so drive code refers to the seam by name.
pub(crate) type JournaledFuture<'a, T, E = restate_sdk::errors::TerminalError> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<T, E>> + Send + 'a>>;

/// A journaled step's body as the recorded-effect protocol hands it to the
/// engine: its output is the entry the journal slot keeps. Same erased-shape
/// seam as [`JournaledFuture`], for bodies that produce a record rather than
/// a `Result` (FIG-3672).
pub(crate) type JournaledStepFuture<'a, T> =
    std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

#[cfg(test)]
mod tests;
