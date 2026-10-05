#![allow(
    deprecated,
    reason = "Restate SDK 0.11 retains the trait service API while its replacement is staged"
)]

//! The `SessionShifts` on Restate (FIG-3600, ADR 0104 O1/O2/O6): the engine
//! that runs every session's shift.
//!
//! Two lash services split one shift across handlers, each on its own
//! journal:
//!
//! - **`LashSession/{session}`** serializes shift legs. Its exclusive `shift`
//!   handler records the leg start, calls one `LashTurn` per immutable request
//!   and ordinal, and applies the kernel's stop rules to the recorded reply.
//!   It hands off at its run bound or the first live boundary after replay,
//!   so each invocation has its own retry budget (FIG-4506).
//! - **`LashTurn/{session}:{request}#{ordinal}`** owns admission and execution.
//!   Its first recorded effect selects work on [`shift_admission_scope`],
//!   then it retains that selection and runs on [`shift_run_scope`]. A parked
//!   execution keeps this journal for its recorded build. A finished run sends
//!   its owed scope close to the same key's shared `close` handler, whose own
//!   journal records `CloseRunScope` beside the session's next admission.
//!   Operation runs have their own invocation and root journal.
//!
//! The kernel owns what a shift admits and how a run executes; these handlers
//! only give each step its journal. The run's admission step repairs orphaned
//! inputs and records its admission together with the store-backed decision
//! about the admitted head, and a follow-on recovery
//! run's `RecoverFollowOn` step records its recovery decision. On replay the
//! steps return their recorded outcomes: the inspection's live check runs only when its
//! step is the attempt's live frontier, and a replay honours its recorded
//! verdict (FIG-3824, FIG-4058, ADR 0105 §2). Rule 6 of `scripts/check-substrate-boundary.sh` pins direct
//! store calls and the repair helper in the session shift. The core installs its
//! [`SessionShifts`] on the engine ([`SessionWorkEngine::install_session_shifts`]),
//! and both handlers read it from the deployment's
//! [`RestateSessionShiftsSlot`], so a host wires nothing.
//!
//! **Scheduling (O2).** A shift is a send to `LashSession/{session}/shift`
//! whose idempotency key is the shift's request id. Every ask names its own
//! request: the admitted row and attempt its ingress obligation asks for,
//! `ingress:{item}:{attempt}`, or a continuation. It never names the session
//! or a running shift. The engine coalesces the asks of one session (FIG-4036,
//! [`asks`]). An ask that finds none of the session's shifts in flight from
//! this process is sent at once, under its own request.
//! [`SessionWorkEngine::request_shift`] answers once Restate accepted it, and
//! `schedule_shift` is its fire-and-forget twin. An ask that finds a shift in
//! flight is never deduplicated into that shift. It joins the one shift queued
//! behind it, which the engine sends once the shift in flight has ended.
//! That shift's first admission is the re-check that admits whatever the
//! shift before it left pending. So a burst of sends queues one shift, not
//! one per send. An ask lost with its process, or a shift the engine lost
//! before it admitted the row, is the ingress relay's to ask again from the
//! row's obligation (ADR 0109 §3). Nothing scans the session catalog for
//! unexecuted rows.
//!
//! **Wire (ADR 0115 §3.1).** Both handlers take a versioned
//! [`Call`](crate::Call) and answer a [`Reply`](crate::Reply). A request
//! carries no journal stamp: a shift pinned to one build sends its immutable
//! intent to the stable `LashTurn`, which the newest build serves, so the
//! request crosses builds and its stamp cannot act as a drain gate. The
//! journal the call starts is the serving build's, and the generation
//! sentinel below guards its replay. [`LASH_SESSION_SHIFT_VERSION`] stays an
//! input to the drain generation `G`.
//!
//! **The recorded outcome (ADR 0115 §3.4).** `LashTurn` keeps the run's
//! status under the stamped `{format, body}` envelope at
//! [`LASH_TURN_OUTCOME_FORMAT_VERSION`], and `outcome` dispatches on the
//! stamp before it decodes, so a later reader of another build is refused
//! typed rather than by an accident of decoding.
//!
//! **Drain generation (ADR 0106 §1, FIG-3795).** Each handler's first
//! recorded effect carries the folded generation sentinel and the executing
//! build's drain generation `G`. A replay that reads back another `G` (the code behind a
//! pinned deployment changed) parks its attempt, typed with the recorded `G`,
//! before decoding the recorded outcome. A `LashTurn` request carries the `G` of the shift
//! that dispatched the intent (`sender_generation`), so a run the latest build
//! cannot run can be routed back to its writer's generation. The step's name
//! and output are frozen. `G` is the engine's: the facade derives it from the
//! build's drain formats and hands it in through
//! [`RestateConfig`](crate::RestateConfig) (FIG-3795 A).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use lash_core::engine::{
    AdmitVerdict, Admitted, BuildGeneration, MAX_RUNS_PER_SHIFT, RunEnd, RunOutcome, ShiftAbort,
    ShiftLoop, ShiftOutcome, ShiftRequest, ShiftRequestId, ShiftStop, shift_admission_scope,
    shift_run_scope,
};
use lash_core::{SessionId, SessionShifts, SessionWorkEngine};
use restate_sdk::context::{
    CallFuture, ContextReadState, ContextSideEffects, ObjectContext, RunFuture,
    SharedWorkflowContext, WorkflowContext,
};
use restate_sdk::errors::{HandlerError, HandlerResult, TerminalError};
use restate_sdk::serde::Json;
use serde::{Deserialize, Serialize};

use crate::compat::{Call, Reply};
use crate::object_state::{self, FleetView, StoredValueFormats, StoredValueWriter};
use crate::sentinel::FoldedSentinel;
use crate::{
    LashService, RestateAuthorityId, RestateIngressClient, RestateRuntimeEffectController,
    parked_turn_failure,
};

mod slot;
pub use slot::RestateSessionShiftsSlot;

mod asks;
mod continuation;

pub use continuation::SendShiftError;
use continuation::{continuation_generation, drain_answered, session_shift_continuation};

/// The generation of the `SessionShifts`'s journaled command prefix
/// (ADR 0105 §12): a drain surface, and so an input to the build's drain
/// generation `G`.
///
/// It owns what the two handlers journal ahead of the kernel's own recorded
/// effects: `LashSession`'s admission steps and its `LashTurn` calls, and the
/// run start marker and seal `LashTurn` records first. Any change to those
/// commands, their order, or what they key on bumps it, which moves `G`, and
/// the generation sentinel parks a journal of another `G` before it replays.
/// No request carries it: a request crosses builds (ADR 0115 §3.1).
///
/// Generation 2 (FIG-3815): `LashTurn` records the run's start marker
/// (`shift-run-start:{admission}`) before its seal.
///
/// Generation 3 (FIG-3600 S7-A): `LashSession` reads a run's recorded
/// outcome through `LashTurn`'s `outcome` handler when its `run` call ends
/// without one.
///
/// Generation 4 (FIG-3600 S7-A): a journaled admission or shift stop that
/// names a terminal run carries the run's terminal kind and, when a head
/// commit ended it, that commit, in place of the commit alone.
///
/// Generation 4 changed in place under the pre-1.0 version freeze
/// (FIG-3980): neither handler journals a separate generation sentinel step;
/// its generation rides the handler's first command, the shift's leg start
/// (admission 0 before FIG-4556) or the run's start marker. It changed in place again for FIG-4035: `LashTurn`'s `run`
/// journals a send to its key's `close` handler where it recorded the run's
/// `CloseRunScope` step, and `close` records that step. And again for
/// FIG-4506: `LashSession`'s `shift` records a `lash.shift.boundary` step
/// after each run it goes on from, short of its run bound. And again for
/// FIG-4523: `shift` records a `lash.shift.leg` step after admission 0, before
/// it calls the leg's first run, and the continuation it sends carries the
/// stop rules' memory of the leg. And again for FIG-4556: `lash.shift.leg` is
/// `shift`'s first command, ahead of admission 0, and carries its generation.
/// And again for FIG-4639: on a recorded `AdmitVerdict::Draining`, `shift`
/// sends its continuation under the stable name and stops `Draining`.
///
/// Generation 5 (FIG-4850): run and shift replies carry terminal status;
/// the answer body lives in the run's durable terminal record.
///
/// Generation 5 changed in place under the pre-1.0 version freeze for
/// FIG-4888: an admission may name an operation run
/// (`AdmittedWork::Operation`), a host task's own `LashTurn` run, whose
/// journal records the task's cancel peek and its effects; the command run
/// stops at a task instead of applying it.
///
/// version_guard(
///     shapes(path = "crates/lash-restate/src/session_shifts/intent.rs", cover(RestateSessionShiftRequest, RestateRunRequest, RestateRunOutcome)),
///     shapes(
///         path = "crates/lash-core-execution/src/engine/admission.rs",
///         path = "crates/lash-core-execution/src/engine/shift.rs",
///         path = "crates/lash-core-execution/src/engine/contracts.rs",
///         path = "crates/lash-core-store/src/store/shift_admission.rs",
///         cover(
///             Admitted, AdmittedWork, AdmitRequest, AdmitVerdict, SealVerdict, RunOutcome,
///             ShiftOutcome, ShiftStop, ShiftRequest,
///         ),
///     ),
///     items(
///         SHIFT_HANDLER, TURN_OUTCOME_STATE, shift_session_journal,
///         execute_run_journal,
///     ),
///     items(path = "crates/lash-restate/src/session_shifts/intent.rs", turn_invocation_key),
///     items(path = "crates/lash-restate/src/sentinel.rs", GENERATION_SENTINEL),
/// )
/// version_surface = "drain"
/// format_manifest = "engine:restate.session_shift"
pub const LASH_SESSION_SHIFT_VERSION: u32 = 5;

/// The shift handler's name on `LashSession`.
const SHIFT_HANDLER: &str = "shift";

/// The journal name of the step `shift` records first, ahead of the leg's
/// first admission. Its body marks the attempt that runs it fresh: an attempt
/// served the step from the journal follows a failed attempt or a suspension.
const LEG_START_STEP: &str = "lash.shift.leg";

/// What [`LEG_START_STEP`] records: the generation sentinel, which rides the
/// handler's first command (FIG-3980), as a recorded effect's entry names it.
#[derive(Serialize, Deserialize)]
struct LegStart {
    build_generation: Option<serde_json::Value>,
}

/// The journal name of the step `shift` records at a run boundary: whether
/// the attempt that reached the boundary was not the leg's fresh one, and so
/// hands the rest of the shift to its continuation.
const RUN_BOUNDARY_STEP: &str = "lash.shift.boundary";

/// The `LashTurn` handler `run` sends its run's owed scope close to.
const CLOSE_HANDLER: &str = "close";

/// The `LashTurn` state entry retaining selection and completion. Both shared
/// handlers project from this stamped record.
const TURN_OUTCOME_STATE: &str = "outcome";

/// The stored format of the turn state `LashTurn` records under its
/// `outcome` state, in the stamped `{format, body}` envelope (ADR 0115
/// §3.4). Stored shapes change in place during the version freeze. Handler
/// command changes move the journal logic epoch and retain the old drain lane.
///
/// version_guard(
///     roots(path = "crates/lash-restate/src/session_shifts/intent.rs", LashTurnState),
///     roots(path = "crates/lash-core-execution/src/engine/admission.rs", SealVerdict),
///     roots(path = "crates/lash-core-store/src/store/shift_fence.rs", AdmissionId, ShiftFence),
///     roots(path = "crates/lash-sansio/src/session_model/mod.rs", ErrorEnvelope),
///     roots(path = "crates/lash-sansio/src/session_model/message.rs", FlatPart, FlatPartRef),
///     items(TURN_OUTCOME_STATE, TURN_OUTCOME_FORMATS),
///     file(
///         path = "crates/lash-sansio/src/identity.rs",
///         cover("string_identity!", SessionId, ProcessId, TurnId),
///     ),
///     shapes(path = "crates/lash-restate/src/object_state.rs", cover(StampedValue)),
/// )
#[cfg(not(feature = "synthetic-next"))]
/// version_surface = "migrate"
/// format_manifest = "engine:restate.turn_outcome_format"
pub const LASH_TURN_OUTCOME_FORMAT_VERSION: u32 = 1;

/// Phase A's synthetic N+1 (ADR 0115 §6) moves the outcome to format 2 with
/// format 1's shape, and reads format 1 forever through its lift.
#[cfg(feature = "synthetic-next")]
/// version_surface = "migrate"
/// format_manifest = "engine:restate.turn_outcome_format"
pub const LASH_TURN_OUTCOME_FORMAT_VERSION: u32 = 2;

/// The recorded outcome's stored-format table: the registered surface.
pub(crate) const TURN_OUTCOME_FORMATS: StoredValueFormats = StoredValueFormats {
    what: "LashTurn outcome",
    surface: lash_core::surface_format!(LASH_TURN_OUTCOME_FORMAT_VERSION),
};

mod intent;
use intent::LashTurnState;
pub use intent::{
    RestateRunCloseRequest, RestateRunOutcome, RestateRunRequest, RestateSessionShiftRequest,
    recorded_turn_invocation_key, turn_invocation_key,
};
pub(crate) use intent::{admission_invocation_key, parse_turn_invocation_key};

// ---------------------------------------------------------------------------
// The engine: scheduling
// ---------------------------------------------------------------------------

/// Restate's [`SessionWorkEngine`]: a shift is a one-way send to the
/// session's `LashSession` object, and the core's `SessionShifts` lives in the
/// deployment's [`RestateSessionShiftsSlot`].
#[derive(Clone)]
pub struct RestateSessionWork {
    ingress: RestateIngressClient,
    slot: RestateSessionShiftsSlot,
    /// The scheduling engine is bound after the core registers its plugins.
    generation: lash_core::engine::EngineGeneration,
    /// The namespace the deployment's session services are named in
    /// (FIG-3898).
    namespace: crate::RestateNamespace,
    control: Arc<dyn lash_core::engine::SessionControlEngine>,
    /// Every session's shift asks from this engine: what is in flight and
    /// what is queued behind it.
    asks: Arc<asks::ShiftAsks>,
}

#[expect(
    clippy::result_large_err,
    reason = "the ingress client's RestateHttpError is unboxed across its public API"
)]
impl RestateSessionWork {
    pub(crate) fn new(
        ingress: RestateIngressClient,
        slot: RestateSessionShiftsSlot,
        generation: lash_core::engine::EngineGeneration,
        namespace: crate::RestateNamespace,
        control: Arc<dyn lash_core::engine::SessionControlEngine>,
    ) -> Self {
        Self {
            ingress,
            slot,
            generation,
            namespace,
            control,
            asks: Arc::default(),
        }
    }

    /// The slot the deployment's session handlers read the `SessionShifts` from.
    pub fn shifts_slot(&self) -> &RestateSessionShiftsSlot {
        &self.slot
    }

    /// Send `request`'s shift to `LashSession_g<G>/{session}`: the resume of
    /// a shift pinned to drain generation `G` (FIG-3795). The request is
    /// directed to `G`, which the resume-only lane holds it to, and the
    /// request id is the send's idempotency key under that service name, as
    /// on the stable lane. The drain sends it; a host never does — new work
    /// goes to the stable lane.
    pub async fn send_resume(
        &self,
        session: &SessionId,
        request: ShiftRequestId,
        generation: &BuildGeneration,
    ) -> Result<crate::RestateInvocationId, crate::RestateHttpError> {
        let route = self
            .namespace
            .generation(LashService::SessionShifts, generation.clone());
        let body = RestateSessionShiftRequest {
            request: ShiftRequest {
                session: session.clone(),
                request: request.clone(),
                intended_lane: Some(generation.clone()),
            },
            handed_off: None,
        };
        self.ingress
            .send_object_json_idempotent_bounded(
                &route.name(),
                session.as_str(),
                SHIFT_HANDLER,
                &Call::new(body),
                request.as_str(),
            )
            .await
    }
}

impl RestateSessionWork {
    /// The refusal a released run's `LashTurn` ended with, when its execution
    /// failed with one.
    async fn released_run_refusal(
        &self,
        request: &ShiftRequest,
        ordinal: u32,
    ) -> Option<lash_core::RuntimeError> {
        let key = turn_invocation_key(request, ordinal);
        match self
            .ingress
            .attach_workflow_run(&self.namespace.stable(LashService::TurnDriver).name(), &key)
            .await
        {
            Err(crate::RestateHttpError::Status { body, .. }) => decode_shift_refusal(&body),
            _ => None,
        }
    }

    /// One leg of `SessionWorkEngine::await_shift`: the attach, the
    /// released-run refusal read-back and the refusal decode.
    async fn attach_shift_leg(&self, request: &ShiftRequest) -> Result<ShiftOutcome, ShiftAbort> {
        let session = &request.session;
        let error = match self.attach_drive_request(request).await {
            Ok(outcome) => {
                for (ordinal, ran) in outcome.ran.iter().enumerate() {
                    if let lash_core::engine::RunOutcome::Released { .. } = ran
                        && let Some(refusal) =
                            self.released_run_refusal(request, ordinal as u32).await
                    {
                        return Err(classify_refusal(refusal));
                    }
                }
                return Ok(outcome);
            }
            Err(error) => error,
        };
        if let SendShiftError::Http(crate::RestateHttpError::Status { body, .. }) = &error
            && let Some(refusal) = decode_shift_refusal(body)
        {
            return Err(classify_refusal(refusal));
        }
        Err(ShiftAbort::Retry(lash_core::RuntimeError::new(
            lash_core::RuntimeErrorCode::EngineTurnTerminalAttach,
            format!(
                "attach to shift `{}` of session `{session}`: {error}",
                request.request.as_str()
            ),
        )))
    }
}

impl std::fmt::Debug for RestateSessionWork {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RestateSessionWork")
            .field("slot", &self.slot)
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl SessionWorkEngine for RestateSessionWork {
    fn control(&self) -> Arc<dyn lash_core::engine::SessionControlEngine> {
        Arc::clone(&self.control)
    }
    fn schedule_shift(&self, session: &SessionId, request: ShiftRequestId) {
        // Fire-and-forget: a shift the engine itself continues, never one an
        // admitted row owes (that one is `request_shift`'s, and the ingress
        // relay asks again for it).
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(
                session_id = session.as_str(),
                request = request.as_str(),
                "session shift not sent: scheduled outside a Tokio runtime"
            );
            return;
        };
        self.asks.join(self, &runtime, session, request);
    }

    /// Join `request` to the session's shift ([`asks`]). An ask that sends
    /// a shift answers once Restate accepted it, under the request's
    /// idempotency key; a send that did not reach Restate is retryable. An ask
    /// queued behind the shift in flight is accepted at once: the engine
    /// sends it once that shift ended. A repeated request joins the shift it
    /// joined first.
    async fn request_shift(
        &self,
        session: &SessionId,
        request: ShiftRequestId,
    ) -> Result<(), lash_core::engine::EngineRefusal> {
        let refusal = |request: &ShiftRequestId, cause| {
            crate::session_control::unaccepted_shift(session, request, cause)
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            // No runtime to pump on: send this ask alone.
            return self
                .send_shift(session, request.clone())
                .await
                .map(|_| ())
                .map_err(|error| refusal(&request, crate::session_control::refusal(error)));
        };
        let joined = self.asks.join(self, &runtime, session, request);
        if joined.queued {
            return Ok(());
        }
        match joined.shift.sent().await {
            asks::Sent::Accepted => Ok(()),
            asks::Sent::Failed(error) => Err(refusal(joined.shift.request(), error)),
        }
    }

    fn install_session_shifts(&self, shifts: Arc<dyn SessionShifts>) -> Arc<dyn SessionShifts> {
        // One recovery interval per installation: a re-install that keeps
        // the live installation starts none, and its interval ends at the
        // next tick after the core drops it.
        let weak_shifts = Arc::downgrade(&shifts);
        let (installed, new) = self.slot.install_new(shifts);
        if !new || !installed.owns_reconciliation() {
            return installed;
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(crate::session_reconcile::run(
                Arc::downgrade(&installed),
                weak_shifts,
            ));
        }
        installed
    }

    /// Attach to the shift `request` joined ([`asks`]) once the engine sent
    /// it. When this engine holds no ask of `request`, attach to `request`'s
    /// own shift by its idempotency key, starting it if no send reached
    /// Restate. A shift a handler refused terminally is decoded
    /// back to the kernel's refusal; any other failure to attach (transport,
    /// the attach ceiling) is a retry under the same key.
    ///
    /// A shift that spent its per-invocation run budget yields and continues
    /// on the derived continuation request: the waiter attaches to each leg
    /// in turn and answers only once the chain ends, with every leg's runs.
    ///
    /// A run the shift consumed as released answers the refusal its
    /// `LashTurn` ended with, as the in-process shift answers a run's
    /// terminal refusal: the shift goes on past it, but a waiter on that run
    /// learns why it did not run to its end.
    async fn await_shift(
        &self,
        session: &SessionId,
        request: &ShiftRequestId,
    ) -> Result<ShiftOutcome, ShiftAbort> {
        let leg = match self.asks.joined(session, request) {
            Some(shift) => match shift.sent().await {
                asks::Sent::Accepted => shift.request().clone(),
                asks::Sent::Failed(error) => {
                    return Err(ShiftAbort::Retry(lash_core::RuntimeError::new(
                        lash_core::RuntimeErrorCode::EngineTurnTerminalAttach,
                        format!(
                            "shift `{}` of session `{session}` was not sent: {error}",
                            shift.request().as_str()
                        ),
                    )));
                }
            },
            None => request.clone(),
        };
        self.await_drive_request(&ShiftRequest {
            session: session.clone(),
            request: leg,
            intended_lane: None,
        })
        .await
    }
}

/// The prefix of a `SessionShifts` handler's terminal error message that
/// carries the refusal as a serialized [`lash_core::RuntimeError`], so a
/// caller attached to the shift reads back the kernel's own code.
const SHIFT_REFUSAL_MARKER: &str = "lash-shift-refused:";

/// A handler's terminal refusal, carrying `error` for a caller attached to
/// the shift.
fn shift_refusal(error: &lash_core::RuntimeError) -> HandlerError {
    let encoded = serde_json::to_string(error).unwrap_or_else(|_| {
        serde_json::json!({ "code": error.code.as_str(), "message": error.message }).to_string()
    });
    TerminalError::new(format!("{SHIFT_REFUSAL_MARKER}{encoded}")).into()
}

fn classify_refusal(error: lash_core::RuntimeError) -> ShiftAbort {
    if error.is_retryable() {
        ShiftAbort::Retry(error)
    } else {
        ShiftAbort::Refused(error)
    }
}

/// The refusal a failed attach's response body carries, when a
/// `SessionShifts` handler ended the shift terminally.
fn decode_shift_refusal(body: &str) -> Option<lash_core::RuntimeError> {
    let message = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("message")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| body.to_owned());
    let (_, encoded) = message.split_once(SHIFT_REFUSAL_MARKER)?;
    let mut stream = serde_json::Deserializer::from_str(encoded).into_iter();
    stream.next()?.ok()
}

// ---------------------------------------------------------------------------
// The handlers
// ---------------------------------------------------------------------------

/// One session's shift. Every lash deployment serves it
/// (`crate::services::bind_lash_services`): a deployment that accepted input
/// without it would schedule shifts nothing runs.
#[restate_sdk::object]
pub trait LashSession {
    async fn shift(call: Call<RestateSessionShiftRequest>) -> HandlerResult<Reply<ShiftOutcome>>;
}

/// One immutable shift intent, keyed by [`turn_invocation_key`]. Its workflow
/// selects work once; repeated dispatches attach to that recorded selection.
#[restate_sdk::workflow]
pub trait LashTurn {
    async fn run(call: Call<serde_json::Value>) -> HandlerResult<Reply<RestateRunOutcome>>;

    /// The outcome `run` recorded, once it ended: what it returned, or
    /// [`RunOutcome::Released`] for a run that ended terminally without a
    /// lash outcome. `None` while `run` has not ended.
    #[shared]
    async fn outcome(call: Call<()>) -> HandlerResult<Reply<Option<RestateRunOutcome>>>;

    /// The selected admission, retained by the invocation for control and recovery.
    #[shared]
    async fn admission(call: Call<()>) -> HandlerResult<Reply<Option<Admitted>>>;

    /// The run's scope close, which `run` sends here once the run's
    /// terminal evidence is durable (FIG-4035): shared, and on a journal of
    /// its own, so neither `run` nor the session's shift waits on it and the
    /// session's next run is admitted beside it.
    #[shared]
    async fn close(call: Call<RestateRunCloseRequest>) -> HandlerResult<Reply<()>>;
}

/// The `LashSession` object over the deployment's `SessionShifts` slot, journaling
/// under the deployment's drain generation.
#[derive(Clone)]
pub(crate) struct LashSessionImpl {
    slot: RestateSessionShiftsSlot,
    authority_id: RestateAuthorityId,
    build_generation: BuildGeneration,
    /// The lane this instance serves: the binder serves one per lane of the
    /// pinned `LashSession` (FIG-3795).
    route: crate::services::ServiceRoute,
}

/// The `LashTurn` workflow over the deployment's `SessionShifts` slot, journaling
/// under the deployment's drain generation.
#[derive(Clone)]
pub(crate) struct LashTurnImpl {
    slot: RestateSessionShiftsSlot,
    authority_id: RestateAuthorityId,
    build_generation: BuildGeneration,
    /// The lane this instance serves: the binder serves one per lane of the
    /// pinned `LashTurn` (FIG-3795).
    route: crate::services::ServiceRoute,
    /// Where `run` reads the fleet epoch its recorded outcome is stamped at.
    fleet: FleetView,
}

impl LashSessionImpl {
    pub(crate) fn new(
        slot: RestateSessionShiftsSlot,
        authority_id: RestateAuthorityId,
        build_generation: BuildGeneration,
        namespace: &crate::RestateNamespace,
    ) -> Self {
        Self {
            slot,
            authority_id,
            build_generation,
            route: namespace.stable(LashService::SessionShifts),
        }
    }

    /// This object bound under `route`: the binder serves one instance per
    /// lane of the pinned `LashSession`.
    pub(crate) fn on_route(&self, route: crate::services::ServiceRoute) -> Self {
        let mut session = self.clone();
        session.route = route;
        session
    }
}

impl LashTurnImpl {
    pub(crate) fn new(
        slot: RestateSessionShiftsSlot,
        authority_id: RestateAuthorityId,
        build_generation: BuildGeneration,
        namespace: &crate::RestateNamespace,
        fleet: FleetView,
    ) -> Self {
        Self {
            slot,
            authority_id,
            build_generation,
            route: namespace.stable(LashService::TurnDriver),
            fleet,
        }
    }

    /// This workflow bound under `route`: the binder serves one instance
    /// per lane of the pinned `LashTurn`.
    pub(crate) fn on_route(&self, route: crate::services::ServiceRoute) -> Self {
        let mut turn = self.clone();
        turn.route = route;
        turn
    }
}

/// The retried end of an attempt `error` aborted.
fn retried_abort(error: &lash_core::RuntimeError) -> HandlerError {
    crate::turn_handler::retried_attempt_failure(error.attempt_failure_text())
}

/// How a handler ends an attempt the kernel aborted.
fn abort_failure(abort: ShiftAbort) -> HandlerError {
    match abort {
        // A live fault: the invocation retries, replaying what it recorded.
        ShiftAbort::Retry(error) => retried_abort(&error),
        // The park is durable; the invocation keeps its journal and pauses
        // after its attempt budget.
        ShiftAbort::Parked { error, .. } => parked_turn_failure(error),
        ShiftAbort::Refused(error) if error.is_retryable() => retried_abort(&error),
        ShiftAbort::Refused(error) => shift_refusal(&error),
    }
}

fn refused_scope(error: lash_core::RuntimeError) -> HandlerError {
    shift_refusal(&error)
}

/// A handler asked to run under a key its request does not name.
fn misaddressed(message: String) -> HandlerError {
    shift_refusal(&lash_core::RuntimeError::new(
        lash_core::RuntimeErrorCode::ExecutionScopeAdmissionRefused,
        message,
    ))
}

/// The typed refusal of a request a generation lane does not serve (FIG-3795,
/// law L11): it names another generation than the lane's, or none. Nothing
/// is journaled and nothing is stored: a misroute is the sender's error,
/// never the shift's outcome.
fn misrouted(route: &crate::services::ServiceRoute, detail: &str) -> HandlerError {
    shift_refusal(&lash_core::RuntimeError::new(
        lash_core::RuntimeErrorCode::ExecutionScopeAdmissionRefused,
        format!("misrouted: {route} serves only its own generation's work; {detail}"),
    ))
}

impl LashSession for LashSessionImpl {
    async fn shift(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<RestateSessionShiftRequest>,
    ) -> HandlerResult<Reply<ShiftOutcome>> {
        let (wire, input) = call.open()?;
        shift_session_journal(
            &self.slot,
            &self.authority_id,
            &self.build_generation,
            &self.route,
            ctx,
            input.request,
            input.handed_off,
        )
        .await
        .map(|outcome| Reply::at(wire, outcome))
    }
}

impl LashTurn for LashTurnImpl {
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        call: Call<serde_json::Value>,
    ) -> HandlerResult<Reply<RestateRunOutcome>> {
        let (wire, raw) = call.open()?;
        let input =
            intent::decode_run_intent(&self.route.name(), self.build_generation.clone(), raw)
                .await?;
        let writer = TURN_OUTCOME_FORMATS.writer(self.fleet.fleet_format());
        execute_run_journal(
            &self.slot,
            &self.authority_id,
            &self.build_generation,
            &self.route,
            ctx,
            writer,
            input,
        )
        .await
        .map(|outcome| Reply::at(wire, outcome))
    }

    async fn outcome(
        &self,
        ctx: SharedWorkflowContext<'_>,
        call: Call<()>,
    ) -> HandlerResult<Reply<Option<RestateRunOutcome>>> {
        let (wire, ()) = call.open()?;
        let recorded = read_turn_state(&ctx).await?;
        Ok(Reply::at(
            wire,
            recorded.and_then(LashTurnState::into_outcome),
        ))
    }

    async fn admission(
        &self,
        ctx: SharedWorkflowContext<'_>,
        call: Call<()>,
    ) -> HandlerResult<Reply<Option<Admitted>>> {
        let (wire, ()) = call.open()?;
        let recorded = read_turn_state(&ctx).await?;
        Ok(Reply::at(
            wire,
            recorded.and_then(LashTurnState::into_admission),
        ))
    }

    async fn close(
        &self,
        ctx: SharedWorkflowContext<'_>,
        call: Call<RestateRunCloseRequest>,
    ) -> HandlerResult<Reply<()>> {
        let (wire, input) = call.open()?;
        close_run_journal(
            &self.slot,
            &self.authority_id,
            &self.build_generation,
            &self.route,
            ctx,
            &input,
        )
        .await
        .map(|()| Reply::at(wire, ()))
    }
}

/// What `LashSession/{session}/shift` journals: the leg start, then a call
/// to each immutable intent's `LashTurn`, followed by its recorded answer,
/// until an invocation stops or a boundary hands the shift off. `handed_off`
/// is what the leg before
/// this one remembers, when this leg is a continuation.
async fn shift_session_journal(
    slot: &RestateSessionShiftsSlot,
    authority_id: &RestateAuthorityId,
    generation: &BuildGeneration,
    route: &crate::services::ServiceRoute,
    ctx: ObjectContext<'_>,
    request: ShiftRequest,
    handed_off: Option<ShiftLoop>,
) -> Result<ShiftOutcome, HandlerError> {
    if ctx.key() != request.session.as_str() {
        return Err(misaddressed(format!(
            "LashSession/{} was asked to work session `{}`",
            ctx.key(),
            request.session
        )));
    }
    let intended = match continuation_generation(&request) {
        Some(generation) => crate::services::Lane::Generation(generation),
        None => crate::services::Lane::Stable,
    };
    if route.lane() != &intended {
        return Err(misrouted(
            route,
            &format!(
                "shift `{}` of session `{}` intended lane `{intended:?}`",
                request.request.as_str(),
                request.session,
            ),
        ));
    }
    let handler = route.namespace().stable(LashService::SessionShifts).name();
    // The generation sentinel rides the leg start, the shift's first command
    // (FIG-3980): a journal of another build parks before it replays past it.
    let sentinel = FoldedSentinel::new(handler.clone(), generation.clone());
    let controller =
        RestateRuntimeEffectController::new(ctx, authority_id.clone(), generation.clone())
            .in_namespace(route.namespace().clone());
    sentinel
        .guard(shift_admissions(
            slot,
            &handler,
            &sentinel,
            &controller,
            generation,
            route,
            request,
            handed_off.unwrap_or_default(),
        ))
        .await?
}

/// The leg start, then admission `n`, the admitted run's `LashTurn` and its
/// boundary, until admission answers anything but an admitted run or a
/// boundary hands the shift off. `rules` are the kernel's stop rules as the
/// leg before this one left them, new for a shift's first leg.
#[expect(
    clippy::too_many_arguments,
    reason = "the handler's parts, expanded in place: a helper generic over the context's \
              lifetime fails the SDK's higher-ranked Send bound"
)]
async fn shift_admissions(
    slot: &RestateSessionShiftsSlot,
    handler: &str,
    sentinel: &FoldedSentinel,
    controller: &RestateRuntimeEffectController<'_, ObjectContext<'_>>,
    generation: &BuildGeneration,
    route: &crate::services::ServiceRoute,
    request: ShiftRequest,
    mut rules: ShiftLoop,
) -> Result<ShiftOutcome, HandlerError> {
    let mut ran = Vec::new();
    // `rules` are the kernel's stop rules, the same ones the in-process shift
    // keeps. Every outcome they read comes from a journaled call result or
    // from the request, so a replay rebuilds the same state.
    let mut ordinal = 0_u32;
    // Whether this attempt recorded the leg's start itself. An attempt served
    // it from the journal follows a failed attempt or a suspension.
    let fresh = Arc::new(AtomicBool::new(false));
    // The leg starts ahead of everything an attempt can fail in, its first
    // admission and the `SessionShifts` slot's read included (FIG-4556): a start
    // stored after a failure would call the attempt that outlived it fresh.
    // As the shift's first command it carries the sentinel (FIG-3980).
    let Json(leg) = {
        let fresh = Arc::clone(&fresh);
        let build_generation = sentinel.stamp();
        RunFuture::name(
            ContextSideEffects::run(controller.context(), move || async move {
                fresh.store(true, Ordering::SeqCst);
                Ok(Json(LegStart { build_generation }))
            }),
            LEG_START_STEP,
        )
        .await?
    };
    sentinel.check(leg.build_generation.as_ref()).await;
    let shifts = slot.shifts_for(handler)?;
    let shifts = shifts.as_ref();
    // This attempt's admissions, and the runs it calls when they run in this
    // process, share one runtime of the session (FIG-3825); the hold drops
    // where the attempt ends, so a replaying attempt opens its own.
    let _hold = shifts.hold_shift(&request.session);
    loop {
        let key = turn_invocation_key(&request, ordinal);
        let turn_route = route.namespace().stable(LashService::TurnDriver);
        let call = crate::services::routed_workflow::<_, _, RestateRunOutcome>(
            controller.context(),
            &turn_route,
            key.clone(),
            "run",
            RestateRunRequest {
                sender_generation: Some(generation.clone()),
                request: request.clone(),
                ordinal,
                rules: rules.clone(),
                draining: drain_answered(route, &request.request, ordinal)
                    .then(|| generation.clone()),
            },
        )
        .call();
        let invocation = call.invocation_handle().await?;
        let dispatched = call.await;
        let dispatched = match dispatched {
            Err(error) if error.code() == 409 => {
                invocation.attach::<Reply<RestateRunOutcome>>().await
            }
            result => result,
        };
        let answer = match dispatched {
            Ok(reply) => reply.into_body(),
            Err(error) => {
                let recorded =
                    crate::services::routed_workflow::<_, (), Option<RestateRunOutcome>>(
                        controller.context(),
                        &turn_route,
                        key.clone(),
                        "outcome",
                        (),
                    )
                    .call()
                    .await?
                    .into_body();
                match recorded {
                    Some(answer) => answer,
                    None => {
                        let admitted = crate::services::routed_workflow::<_, (), Option<Admitted>>(
                            controller.context(),
                            &turn_route,
                            key,
                            "admission",
                            (),
                        )
                        .call()
                        .await?
                        .into_body();
                        let Some(admitted) = admitted else {
                            return Err(error.into());
                        };
                        tracing::warn!(run = admitted.run().as_str(), error = %error, "session shift consumed a released turn invocation");
                        let outcome = RunOutcome::Released {
                            run: admitted.run().clone(),
                        };
                        RestateRunOutcome::Ran { admitted, outcome }
                    }
                }
            }
        };
        let (next, stop) = match answer {
            RestateRunOutcome::Ran { admitted, outcome } => {
                // The invocation already checked the same immutable stop rules before execution.
                rules.before(&admitted).map_err(|stop| {
                    misaddressed(format!("turn invocation bypassed shift stop {stop:?}"))
                })?;
                let work = admitted.work().clone();
                let stop = rules.after(&work, &outcome);
                let yielded_run = outcome.run().clone();
                ran.push(outcome);
                if let Some(stop) = stop {
                    return Ok(ShiftOutcome { ran, stop });
                }
                // Restate counts a handler's attempts over the invocation's
                // whole retry loop, which only a suspension or a new
                // invocation restarts: a shift that kept going would add up
                // the failed attempts of every run it runs, and pause on a
                // budget meant for one run's work (FIG-4506). So an attempt
                // that did not start the leg itself, one that follows a failed
                // attempt or a suspension, hands the rest of the shift to a
                // new invocation at the first boundary it reaches live, the
                // leg's first one included (FIG-4523). What it decided is
                // recorded, so its own replay decides the same.
                let handed_off = ran.len() == MAX_RUNS_PER_SHIFT || {
                    let fresh = Arc::clone(&fresh);
                    let Json(replayed) = RunFuture::name(
                        ContextSideEffects::run(controller.context(), move || async move {
                            Ok(Json(!fresh.load(Ordering::SeqCst)))
                        }),
                        RUN_BOUNDARY_STEP,
                    )
                    .await?;
                    replayed
                };
                if !handed_off {
                    ordinal = ordinal.checked_add(1).ok_or_else(|| {
                        misaddressed(format!(
                            "session `{}` shift `{}` exhausted its admission ordinals",
                            request.session,
                            request.request.as_str()
                        ))
                    })?;
                    continue;
                }
                // The continuation is this same shift yielding: it goes to
                // this invocation's lane, so a shift resumed on `_g<G>` stays
                // under the generation its journal family belongs to. On the
                // stable lane this is the stable name.
                (
                    Some((route.clone(), session_shift_continuation(&request, route))),
                    ShiftStop::HandedOff { run: yielded_run },
                )
            }
            // This build is draining (FIG-4639): the rest goes to the stable
            // name, the newest build's. A replay decodes the same verdict.
            RestateRunOutcome::Stopped {
                stop: ShiftStop::Draining { generation },
            } => (
                Some((
                    route.namespace().stable(LashService::SessionShifts),
                    session_shift_continuation(
                        &request,
                        &route.namespace().stable(LashService::SessionShifts),
                    ),
                )),
                ShiftStop::Draining { generation },
            ),
            RestateRunOutcome::Stopped { stop } => (None, stop),
        };
        let Some((next_route, continuation)) = next else {
            return Ok(ShiftOutcome { ran, stop });
        };
        // The send is a journaled Restate command. Its request is distinct
        // from this invocation and queues behind this object's exclusive
        // handler before we return. The request id is the send's idempotency
        // key, so a waiter that attaches under it joins this invocation
        // rather than starting a second one. It carries what the stop rules
        // remember of this leg's runs: a leg may be one run long, and a
        // run admission names again right after it ran must stop the shift
        // in the leg that meets it rather than be run there again. A leg
        // that ran no run passes on what it was handed.
        let continuation_id = continuation.request.as_str().to_owned();
        let remembered = if ran.is_empty() {
            rules
        } else {
            rules.handed_off(&ran)
        };
        crate::services::routed_object::<_, _, ()>(
            controller.context(),
            &next_route,
            request.session.as_str().to_owned(),
            "shift",
            RestateSessionShiftRequest {
                request: continuation,
                handed_off: Some(remembered),
            },
        )
        .idempotency_key(continuation_id)
        .send()
        .await?;
        return Ok(ShiftOutcome { ran, stop });
    }
}

/// What `LashTurn/{session}:{request}#{ordinal}/run` journals: the kernel's root run on
/// the selected run's scope, after its admission was recorded.
async fn execute_run_journal(
    slot: &RestateSessionShiftsSlot,
    authority_id: &RestateAuthorityId,
    generation: &BuildGeneration,
    route: &crate::services::ServiceRoute,
    ctx: WorkflowContext<'_>,
    writer: StoredValueWriter,
    request: RestateRunRequest,
) -> Result<RestateRunOutcome, HandlerError> {
    let RestateRunRequest {
        sender_generation,
        request,
        ordinal,
        mut rules,
        draining,
    } = request;
    let sender_generation = sender_generation.as_ref();
    let expected = turn_invocation_key(&request, ordinal);
    if ctx.key() != expected {
        return Err(misaddressed(format!(
            "LashTurn/{} was asked to execute `{expected}`",
            ctx.key()
        )));
    }
    // The generation lane serves a run the latest build refused, re-sent by
    // the drain under the generation the shift that admitted it ran on
    // (`sender_generation`). A request naming another generation, or none,
    // is a misroute refused before any command.
    if let crate::services::Lane::Generation(lane) = route.lane()
        && sender_generation != Some(lane)
    {
        let sender = sender_generation.map_or_else(
            || "no generation".to_string(),
            |sender| format!("generation `{sender}`"),
        );
        return Err(misrouted(
            route,
            &format!(
                "run `{}` of session `{}` was sent by {sender}",
                request.request.as_str(),
                request.session
            ),
        ));
    }
    let handler = route.namespace().stable(LashService::TurnDriver).name();
    let shifts = slot.shifts_for(&handler)?;
    // The admission records the folded sentinel before selection is decoded.
    // A predecessor journal parks before any selected run executes.
    let sentinel = Arc::new(FoldedSentinel::new(handler, generation.clone()));
    let controller = RestateRuntimeEffectController::with_options(
        ctx,
        authority_id.clone(),
        generation.clone(),
        slot.run_options(),
    )
    .in_namespace(route.namespace().clone())
    .with_folded_sentinel(Arc::clone(&sentinel));
    let admission_controller = controller
        .scoped_effect_controller(shift_admission_scope(&request.session, &request.request))
        .map_err(refused_scope)?;
    let verdict = sentinel
        .guard(shifts.admit(
            admission_controller,
            &request,
            generation,
            ordinal,
            draining.as_ref(),
        ))
        .await?
        .map_err(abort_failure)?;
    let admitted = match verdict {
        AdmitVerdict::Admit(admitted) => admitted.run_by(generation.clone()),
        verdict => {
            let stop = match verdict {
                AdmitVerdict::Idle => ShiftStop::Idle,
                AdmitVerdict::Parked(park) => ShiftStop::Parked(park),
                AdmitVerdict::SubstrateLost { run } => ShiftStop::SubstrateLost { run },
                AdmitVerdict::RunTerminal { run, kind, commit } => {
                    ShiftStop::RunTerminal { run, kind, commit }
                }
                AdmitVerdict::Draining { generation } => ShiftStop::Draining { generation },
                AdmitVerdict::Admit(_) => {
                    return Err(misaddressed(
                        "unexpected admitted verdict in stop conversion".to_owned(),
                    ));
                }
            };
            let answer = RestateRunOutcome::Stopped { stop: stop.clone() };
            object_state::set_stamped(
                controller.context(),
                TURN_OUTCOME_STATE,
                writer,
                LashTurnState::Stopped { stop },
            );
            return Ok(answer);
        }
    };
    if let Err(stop) = rules.before(&admitted) {
        let answer = RestateRunOutcome::Stopped { stop: stop.clone() };
        object_state::set_stamped(
            controller.context(),
            TURN_OUTCOME_STATE,
            writer,
            LashTurnState::Stopped { stop },
        );
        return Ok(answer);
    }
    object_state::set_stamped(
        controller.context(),
        TURN_OUTCOME_STATE,
        writer,
        LashTurnState::Selected {
            admitted: admitted.clone(),
        },
    );
    let scoped = controller
        .scoped_effect_controller(shift_run_scope(admitted.session(), admitted.run()))
        .map_err(refused_scope)?;
    let run = admitted.run().clone();
    let selected = admitted.clone();
    let RunEnd { result, owed_close } =
        sentinel.guard(shifts.execute_run(scoped, admitted)).await?;
    let (ended, result) = match result {
        Ok(outcome) => (outcome.clone(), Ok(outcome)),
        // A retryable end records nothing: the run is not over, and its
        // retry owes the run's close again.
        Err(abort @ (ShiftAbort::Retry(_) | ShiftAbort::Parked { .. })) => {
            return Err(abort_failure(abort));
        }
        Err(ShiftAbort::Refused(error)) if error.is_retryable() => {
            return Err(retried_abort(&error));
        }
        Err(abort @ ShiftAbort::Refused(_)) => {
            (RunOutcome::Released { run }, Err(abort_failure(abort)))
        }
    };
    // The run's scope close runs on the key's `close` handler, not here
    // (FIG-4035): `run` returns once the run's terminal status is handed over, so
    // the session's shift admits its next run beside the close. The send is
    // journaled, so a replay sends it once. The close is its `ScopeClose`
    // obligation's immediate delivery, so however often it runs, the scope
    // closes once.
    if let Some(owed) = owed_close {
        crate::services::routed_workflow::<_, _, ()>(
            controller.context(),
            route,
            expected,
            CLOSE_HANDLER,
            RestateRunCloseRequest {
                sender_generation: Some(generation.clone()),
                run: owed,
                scope_run: selected.run().clone(),
            },
        )
        .send()
        .await?;
    }
    // The intent records its selected run and outcome once. Readers dispatch
    // on the format stamp before decoding across builds.
    object_state::set_stamped(
        controller.context(),
        TURN_OUTCOME_STATE,
        writer,
        LashTurnState::Ran {
            admitted: selected.clone(),
            outcome: ended,
        },
    );
    result.map(|outcome| RestateRunOutcome::Ran {
        admitted: selected,
        outcome,
    })
}

async fn read_turn_state(ctx: &SharedWorkflowContext<'_>) -> HandlerResult<Option<LashTurnState>> {
    ctx.get::<Vec<u8>>(TURN_OUTCOME_STATE)
        .await?
        .map(|bytes| LashTurnState::decode(&bytes))
        .transpose()
        .map_err(Into::into)
}

/// What `LashTurn/{session}:{request}#{ordinal}/close` journals: the kernel's recorded
/// `CloseRunScope` step of `run` on the key's run scope, the scope the
/// key's `run` would have recorded it under.
async fn close_run_journal(
    slot: &RestateSessionShiftsSlot,
    authority_id: &RestateAuthorityId,
    generation: &BuildGeneration,
    route: &crate::services::ServiceRoute,
    ctx: SharedWorkflowContext<'_>,
    request: &RestateRunCloseRequest,
) -> Result<(), HandlerError> {
    let RestateRunCloseRequest {
        sender_generation,
        run,
        scope_run,
    } = request;
    let Some((session, _admission)) = parse_turn_invocation_key(ctx.key()) else {
        return Err(misaddressed(format!(
            "LashTurn/{} names no run to close `{run}` under",
            ctx.key()
        )));
    };
    // A close on a generation lane was sent by a run on that lane, which
    // names the lane's generation; anything else is a misroute.
    if let crate::services::Lane::Generation(lane) = route.lane()
        && sender_generation.as_ref() != Some(lane)
    {
        return Err(misrouted(
            route,
            &format!("the close of run `{run}` of session `{session}` was not sent by its lane"),
        ));
    }
    let handler = route.namespace().stable(LashService::TurnDriver).name();
    let shifts = slot.shifts_for(&handler)?;
    // The generation sentinel rides the close, the handler's first recorded
    // step: a journal of another build parks before it replays past it.
    let sentinel = Arc::new(FoldedSentinel::new(handler, generation.clone()));
    let controller =
        RestateRuntimeEffectController::new(ctx, authority_id.clone(), generation.clone())
            .in_namespace(route.namespace().clone())
            .with_folded_sentinel(Arc::clone(&sentinel));
    let scoped = controller
        .scoped_effect_controller(shift_run_scope(&session, scope_run))
        .map_err(refused_scope)?;
    sentinel
        .guard(shifts.close_run(scoped, &session, run))
        .await?
        .map_err(abort_failure)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct IdleShifts;

    #[async_trait::async_trait]
    impl SessionShifts for IdleShifts {
        async fn admit(
            &self,
            _controller: lash_core::ScopedEffectController<'_>,
            _request: &ShiftRequest,
            _admitting_generation: &lash_core::engine::BuildGeneration,
            _ordinal: u32,
            _draining: Option<&lash_core::engine::BuildGeneration>,
        ) -> Result<AdmitVerdict, ShiftAbort> {
            unreachable!("the slot law runs no shift")
        }

        async fn execute_run(
            &self,
            _controller: lash_core::ScopedEffectController<'_>,
            _admitted: Admitted,
        ) -> RunEnd {
            unreachable!("the slot law runs no shift")
        }

        async fn close_run(
            &self,
            _controller: lash_core::ScopedEffectController<'_>,
            _session: &SessionId,
            _run: &lash_core::TurnId,
        ) -> Result<(), ShiftAbort> {
            unreachable!("the slot law runs no shift")
        }
    }

    /// FIG-4017: the installation a core keeps decides whether an install
    /// holds, not the `SessionShifts` a shift still runs on. While the first core
    /// keeps its installation, a second install is served the first `SessionShifts`;
    /// once the first core drops it, a shift still holding the first `SessionShifts`
    /// does not stop the next install from taking its own.
    #[test]
    fn session_shifts_a_shift_still_holds_do_not_keep_their_dropped_installation() {
        let slot = RestateSessionShiftsSlot::new();
        let first: Arc<dyn SessionShifts> = Arc::new(IdleShifts);
        let second: Arc<dyn SessionShifts> = Arc::new(IdleShifts);
        let first_installation = slot.install(Arc::clone(&first));
        assert!(first_installation.runs_on(first.as_ref()));
        let in_flight = slot
            .shifts_for("shift")
            .expect("the first `SessionShifts` serves");
        assert!(
            Arc::ptr_eq(&in_flight, &first),
            "a shift runs on the `SessionShifts`"
        );
        let kept = slot.install(Arc::clone(&second));
        assert!(
            kept.runs_on(first.as_ref()),
            "a live installation keeps serving the first `SessionShifts`"
        );
        drop((kept, first_installation));

        let second_installation = slot.install(Arc::clone(&second));
        assert!(
            second_installation.runs_on(second.as_ref()),
            "the second install takes its own `SessionShifts` while a shift still holds the first"
        );
        let next = slot
            .shifts_for("shift")
            .expect("the second `SessionShifts` serves");
        assert!(Arc::ptr_eq(&next, &second));
        drop(in_flight);
    }

    /// An invocation's address round-trips arbitrary request and session ids.
    #[test]
    fn a_turn_invocation_key_round_trips_its_immutable_intent() {
        for session in ["s", "a:b", "sessé:😀", ":"] {
            for request in ["r", "b:c", "request#1", ":"] {
                for ordinal in [0, 1, u32::MAX] {
                    let request = ShiftRequest {
                        session: SessionId::from(session),
                        request: ShiftRequestId::new(request),
                        intended_lane: None,
                    };
                    let key = turn_invocation_key(&request, ordinal);
                    assert_eq!(
                        parse_turn_invocation_key(&key),
                        Some((
                            request.session.clone(),
                            lash_core::engine::AdmissionId::new(format!(
                                "{}#{ordinal}",
                                request.request.as_str()
                            )),
                        ))
                    );
                }
            }
        }
        for malformed in [
            "",
            "s:r",
            "3:ab",
            "03:abcd#0",
            "2:ab",
            ":ab",
            "x2:abc",
            "1:éx#0",
            "1:sr#00",
            "1:s#0",
        ] {
            assert_eq!(parse_turn_invocation_key(malformed), None, "{malformed}");
        }
    }

    #[test]
    fn a_shift_request_carries_no_journal_stamp() {
        let request = ShiftRequest {
            session: SessionId::from("s"),
            request: ShiftRequestId::new("r"),
            intended_lane: None,
        };
        let mut encoded = serde_json::to_value(RestateSessionShiftRequest {
            request: request.clone(),
            handed_off: None,
        })
        .expect("encode");
        assert_eq!(
            encoded
                .as_object()
                .expect("an object")
                .keys()
                .collect::<Vec<_>>(),
            ["request"],
            "a request crosses builds, so it carries no drain gate (ADR 0115 §3.1)"
        );
        // A request an older build stamped still decodes: the stamp is
        // ignored, never checked.
        encoded["shift_version"] = serde_json::json!(LASH_SESSION_SHIFT_VERSION + 1);
        let decoded: RestateSessionShiftRequest =
            serde_json::from_value(encoded).expect("a stamped request decodes");
        assert_eq!(decoded.request, request);
    }

    #[test]
    fn a_terminal_shift_refusal_decodes_back_to_its_runtime_error() {
        let error = lash_core::RuntimeError::new(
            lash_core::RuntimeErrorCode::AcceptedTurnInputCeded,
            "the input was ceded: \"quoted\" text",
        );
        let message = format!(
            "{SHIFT_REFUSAL_MARKER}{}",
            serde_json::to_string(&error).expect("encode")
        );
        for body in [
            serde_json::json!({ "message": message.clone() }).to_string(),
            message.clone(),
            format!(
                "{{\"code\":500,\"message\":{}}}",
                serde_json::json!(message)
            ),
        ] {
            let decoded = decode_shift_refusal(&body).expect("the refusal decodes");
            assert_eq!(decoded.code, error.code);
            assert_eq!(decoded.message, error.message);
        }
        assert!(decode_shift_refusal("{\"message\":\"connection reset\"}").is_none());
    }

    /// D15: the admission refusal a shift answers while a park names an
    /// unsettled redrive is retryable, so its decode classifies it as a
    /// retry — never a refused (failed-turn) row — and the handler failure
    /// it becomes keeps the attempt open.
    #[test]
    fn an_unsettled_redrive_refusal_is_classified_retry_not_refused() {
        let refusal = lash_core::RuntimeError::new(
            lash_core::RuntimeErrorCode::SessionRedriveUnsettled,
            "the parked run's redrive has not settled",
        );
        assert!(refusal.is_retryable());
        assert!(!refusal.is_terminal());
        assert!(matches!(
            classify_refusal(refusal.clone()),
            ShiftAbort::Retry(_)
        ));
        let failure = abort_failure(ShiftAbort::Refused(refusal));
        assert!(
            format!("{failure:?}").contains("Retryable"),
            "an unsettled-redrive refusal must retry, not end the shift: {failure:?}"
        );
    }

    /// Answers each request in turn from a script.
    #[derive(Debug)]
    struct Scripted {
        requests: std::sync::Mutex<Vec<lash_http_transport::HttpRequest>>,
        responses: std::sync::Mutex<std::collections::VecDeque<lash_http_transport::HttpResponse>>,
    }

    #[async_trait::async_trait]
    impl lash_http_transport::HttpTransport for Scripted {
        async fn send(
            &self,
            request: lash_http_transport::HttpRequest,
            _timeout: Option<std::time::Duration>,
        ) -> Result<lash_http_transport::HttpResponse, lash_http_transport::LlmTransportError>
        {
            self.requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(request);
            self.responses
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop_front()
                .ok_or_else(|| lash_http_transport::LlmTransportError::new("script exhausted"))
        }
    }

    fn scripted_response(status: u16, body: String) -> lash_http_transport::HttpResponse {
        lash_http_transport::HttpResponse {
            status,
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: lash_http_transport::HttpResponseBody::buffered(body),
        }
    }

    #[tokio::test]
    async fn a_transient_schedule_failure_retries_the_same_shift_request() {
        let transport = Arc::new(Scripted {
            requests: std::sync::Mutex::default(),
            responses: std::sync::Mutex::new(
                [
                    scripted_response(503, "temporary outage".to_string()),
                    scripted_response(
                        202,
                        serde_json::json!({
                            "invocationId": "accepted-shift",
                            "status": "Accepted"
                        })
                        .to_string(),
                    ),
                ]
                .into(),
            ),
        });
        let work = RestateSessionWork::new(
            crate::RestateIngressClient::new(crate::RestateConnection::with_transport(
                "https://cloud.example",
                transport.clone(),
            )),
            RestateSessionShiftsSlot::new(),
            lash_core::engine::EngineGeneration::fixed(BuildGeneration::for_test("t0")),
            crate::RestateNamespace::default(),
            Arc::new(lash_core::engine::NoEngineControl),
        );
        work.schedule_shift(
            &SessionId::from("retry-session"),
            ShiftRequestId::new("retry"),
        );
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if transport
                    .requests
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .len()
                    >= 2
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the send retries after 503");
        // The accepted send is followed by the engine's attach to learn when
        // the shift ended (FIG-4036); the first two requests are the sends.
        let requests = transport
            .requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(requests[0].url, requests[1].url);
        assert_eq!(requests[0].body, requests[1].body);
        assert_eq!(requests[0].headers, requests[1].headers);
    }

    #[tokio::test]
    async fn a_persistent_schedule_failure_stops_after_three_attempts() {
        let transport = Arc::new(Scripted {
            requests: std::sync::Mutex::default(),
            responses: std::sync::Mutex::new(
                (0..3)
                    .map(|_| scripted_response(503, "temporary outage".to_string()))
                    .collect(),
            ),
        });
        let work = RestateSessionWork::new(
            crate::RestateIngressClient::new(crate::RestateConnection::with_transport(
                "https://cloud.example",
                transport.clone(),
            )),
            RestateSessionShiftsSlot::new(),
            lash_core::engine::EngineGeneration::fixed(BuildGeneration::for_test("t0")),
            crate::RestateNamespace::default(),
            Arc::new(lash_core::engine::NoEngineControl),
        );
        work.schedule_shift(
            &SessionId::from("retry-session"),
            ShiftRequestId::new("retry"),
        );
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if transport
                    .requests
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .len()
                    >= 3
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("three attempts occur");
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert_eq!(
            transport
                .requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            3,
            "the scheduler stops retrying after the bounded attempt count"
        );
    }

    /// A run the shift consumed as released answers a waiter with the
    /// refusal its `LashTurn` run ended with, as the in-process shift answers
    /// a run's terminal refusal, rather than a stop that says nothing of why.
    #[tokio::test]
    async fn a_released_run_answers_the_refusal_its_execution_ended_with() {
        let session = SessionId::from("released-session");
        let run = lash_core::TurnId::from("released-run");
        let outcome = ShiftOutcome {
            ran: vec![lash_core::engine::RunOutcome::Released { run: run.clone() }],
            stop: lash_core::engine::ShiftStop::RunAborted { run: run.clone() },
        };
        let refusal = lash_core::RuntimeError::new(
            lash_core::RuntimeErrorCode::AcceptedTurnInputCeded,
            "the session is being deleted",
        );
        let failure = serde_json::json!({
            "message": format!(
                "{SHIFT_REFUSAL_MARKER}{}",
                serde_json::to_string(&refusal).expect("encode")
            ),
        });
        let transport = Arc::new(Scripted {
            requests: std::sync::Mutex::default(),
            responses: std::sync::Mutex::new(
                [
                    scripted_response(200, crate::wire::reply_json(&outcome)),
                    scripted_response(500, failure.to_string()),
                ]
                .into(),
            ),
        });
        let work = RestateSessionWork::new(
            crate::RestateIngressClient::new(crate::RestateConnection::with_transport(
                "https://cloud.example",
                transport.clone(),
            )),
            RestateSessionShiftsSlot::new(),
            lash_core::engine::EngineGeneration::fixed(BuildGeneration::for_test("t0")),
            crate::RestateNamespace::default(),
            Arc::new(lash_core::engine::NoEngineControl),
        );

        let answer = work
            .await_shift(&session, &ShiftRequestId::new("released-request"))
            .await;
        let Err(ShiftAbort::Refused(answered)) = answer else {
            panic!("the released run answers its refusal: {answer:?}");
        };
        assert_eq!(answered.code, refusal.code);
        assert_eq!(answered.message, refusal.message);
        let requests = transport
            .requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].method, lash_http_transport::HttpMethod::Get);
        assert!(
            requests[1].url.ends_with("/attach")
                && requests[1].url.contains("restate/workflow/LashTurn/"),
            "{}",
            requests[1].url
        );
    }

    #[tokio::test]
    async fn a_lane_refusal_on_the_transport_double_is_retried_after_release() {
        let session = SessionId::from("racing-lane-session");
        let request = ShiftRequestId::new("racing-lane-request");
        let busy = lash_core::RuntimeError::new(
            lash_core::RuntimeErrorCode::SessionExecutionLaneBusy,
            "lane release is still in flight",
        );
        let failure = serde_json::json!({
            "message": format!(
                "{SHIFT_REFUSAL_MARKER}{}",
                serde_json::to_string(&busy).expect("encode")
            ),
        });
        let completed = ShiftOutcome {
            ran: vec![],
            stop: ShiftStop::Idle,
        };
        let transport = Arc::new(Scripted {
            requests: std::sync::Mutex::default(),
            responses: std::sync::Mutex::new(
                [
                    scripted_response(500, failure.to_string()),
                    scripted_response(200, crate::wire::reply_json(&completed)),
                ]
                .into(),
            ),
        });
        let work = RestateSessionWork::new(
            crate::RestateIngressClient::new(crate::RestateConnection::with_transport(
                "https://cloud.example",
                transport.clone(),
            )),
            RestateSessionShiftsSlot::new(),
            lash_core::engine::EngineGeneration::fixed(BuildGeneration::for_test("t0")),
            crate::RestateNamespace::default(),
            Arc::new(lash_core::engine::NoEngineControl),
        );
        let first = work.await_shift(&session, &request).await;
        assert!(matches!(first, Err(ShiftAbort::Retry(ref error)) if error.code == busy.code));
        let redrive = work.await_shift(&session, &request).await;
        assert_eq!(redrive.expect("redrive after lane release"), completed);
        let requests = transport
            .requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0].url, requests[1].url,
            "redrive keeps its request identity"
        );
    }
}
