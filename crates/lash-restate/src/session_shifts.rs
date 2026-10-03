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
//! - **`LashSession/{session}`** is a virtual object keyed by the session.
//!   Its exclusive `shift` handler records the leg's start, then loops the
//!   kernel's recorded admission (`AdmitShift`, ordinal 0, 1, ..) on a
//!   controller scoped to
//!   [`shift_admission_scope`], and for every admitted run calls that
//!   run's `LashTurn` and awaits it. It returns once admission answers
//!   anything but an admitted run, or hands the rest of the shift to a
//!   continuation at a run boundary: at its run bound, and at the first
//!   boundary an attempt that replayed reaches, so the retry budget Restate
//!   counts per invocation covers one stretch of runs and never the whole
//!   backlog (FIG-4506). A shift whose build is draining hands over
//!   sooner: every admission after the run it started on reads its build's
//!   drain mark, and a marked build admits no further run and sends the
//!   rest to the stable name, the newest build's (FIG-4639, ADR 0106 §1).
//!   The object's key serializes the
//!   engine's shifts of the session (O1); an in-process `SessionShifts` beside it is
//!   serialized by the SQL session execution lease until S8.
//! - **`LashTurn/{session}:{run}`** is a workflow, one per logical run. Its
//!   `run` handler seals the admission and executes the run's turns on a
//!   controller scoped to [`shift_run_scope`], under the retry contract of a
//!   turn handler ([`turn_handler_options`](crate::turn_handler_options)): a
//!   parked run fails its attempt retryably and pauses after the budget, so
//!   its journal is kept for a restored build. A run that ended owes its
//!   scope close: `run` sends it to the same key's shared `close` handler,
//!   which records the kernel's `CloseRunScope` step on a journal of its
//!   own, and returns. `LashSession` admits the next run beside the close
//!   rather than after it (FIG-4035).
//!
//! The kernel owns what a shift admits and how a run executes; these handlers
//! only give each step its journal. The run's admission step repairs orphaned
//! inputs and records its admission. Its `InspectAdmittedHead` step records the
//! store-backed decision about the admitted head, and a follow-on recovery
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
//! carries no journal stamp: a shift pinned to one build sends its admitted
//! run to the stable `LashTurn`, which the newest build serves, so the
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
//! journaled command is the generation sentinel, a step named
//! `lash.build.generation` that records the executing build's drain
//! generation `G`. A replay that reads back another `G` (the code behind a
//! pinned deployment changed) parks its attempt, typed with the recorded `G`,
//! before any other command. A `LashTurn` request carries the `G` of the shift
//! that admitted the run (`sender_generation`), so a run the latest build
//! cannot run can be routed back to its writer's generation. The step's name
//! and output are frozen. `G` is the engine's: the facade derives it from the
//! build's drain formats and hands it in through
//! [`RestateConfig`](crate::RestateConfig) (FIG-3795 A).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use lash_core::engine::{
    AdmitVerdict, Admitted, BuildGeneration, MAX_RUNS_PER_SHIFT, RunEnd, RunOutcome, ShiftAbort,
    ShiftLoop, ShiftOutcome, ShiftRequest, ShiftRequestId, ShiftStop, shift_admission_scope,
    shift_run_scope,
};
use lash_core::{SessionId, SessionShifts, SessionWorkEngine};
use restate_sdk::context::{
    ContextReadState, ContextSideEffects, ObjectContext, RunFuture, SharedWorkflowContext,
    WorkflowContext,
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
///     shapes(cover(RestateSessionShiftRequest, RestateRunRequest)),
///     shapes(
///         path = "crates/lash-core-execution/src/engine/admission.rs",
///         path = "crates/lash-core-execution/src/engine/shift.rs",
///         path = "crates/lash-core-execution/src/engine/contracts.rs",
///         cover(
///             Admitted, AdmittedWork, AdmitRequest, AdmitVerdict, SealVerdict, RunOutcome,
///             ShiftOutcome, ShiftStop, ShiftRequest,
///         ),
///     ),
///     items(
///         SHIFT_HANDLER, TURN_OUTCOME_STATE, turn_workflow_key, shift_session_journal,
///         execute_run_journal,
///     ),
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

/// The `LashTurn` state entry `run` records the run's outcome under once it
/// ended, terminally included; `outcome` reads it back.
const TURN_OUTCOME_STATE: &str = "outcome";

/// The stored format of the run outcome `LashTurn` records under its
/// `outcome` state, in the stamped `{format, body}` envelope (ADR 0115
/// §3.4). Bump it when [`RunOutcome`]'s stored shape changes, and register
/// the previous format's lift in `lash_core::store::RECORD_UPCASTERS`. The
/// outcome is history: a finished workflow's state is never rewritten, so
/// every lift from its floor is permanent (FIG-3802).
///
/// version_guard(
///     roots(path = "crates/lash-core-execution/src/engine/shift.rs", RunOutcome),
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

/// The request `LashSession/{session}/shift` runs: one shift of the session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestateSessionShiftRequest {
    pub request: ShiftRequest,
    /// What the leg that handed this shift off remembers of its own runs
    /// ([`ShiftLoop::handed_off`]): the stop rules this leg starts from, so a
    /// run that leg ran is not run again here. Only a leg's own continuation
    /// send carries it; a host's send and a waiter's attach never do.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handed_off: Option<ShiftLoop>,
}

/// The request `LashTurn/{session}:{run}/run` runs: one admitted run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestateRunRequest {
    /// The drain generation of the build whose shift admitted the run: the
    /// generation a run the latest build refuses is routed back to
    /// (ADR 0106 §1). Unstamped, it is `None`.
    #[serde(default)]
    pub sender_generation: Option<BuildGeneration>,
    /// The recorded admission the run executes under. The run executes on the head
    /// its recorded admission named, and a replay reads that admission back;
    /// adopting that head still reads live store state (FIG-3824).
    pub admitted: Admitted,
}

/// The request `LashTurn/{session}:{run}/close` runs: the scope close the
/// key's `run` owed once its run's terminal evidence was durable
/// (FIG-4035).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestateRunCloseRequest {
    /// The drain generation of the build whose run owed the close: a close
    /// sent on a generation lane names that lane's generation.
    #[serde(default)]
    pub sender_generation: Option<BuildGeneration>,
    /// The logical run whose scope closes: the key's own run, or, for a
    /// follow-on recovery, the run that owed the follow-on.
    pub run: lash_core::TurnId,
}

/// The `LashTurn` workflow key of `run` in `session`: one workflow per
/// logical run, so a replay of the session's shift re-calls the same run.
///
/// The key is `{len}:{session}{run}`, where `len` is the session id's length
/// in bytes, so it parses back to exactly one `(session, run)` whatever
/// either id contains ([`parse_turn_workflow_key`]): reconciliation maps a
/// paused `LashTurn` invocation to its run by its key alone. A host finds
/// the invocation running one of its runs by this key.
pub fn turn_workflow_key(session: &SessionId, run: &lash_core::TurnId) -> String {
    format!(
        "{}:{}{}",
        session.as_str().len(),
        session.as_str(),
        run.as_str()
    )
}

/// The `(session, run)` a [`turn_workflow_key`] names, or `None` for a key
/// no build of this generation wrote.
pub(crate) fn parse_turn_workflow_key(key: &str) -> Option<(SessionId, lash_core::TurnId)> {
    let (len, rest) = key.split_once(':')?;
    if len.is_empty()
        || !len.bytes().all(|byte| byte.is_ascii_digit())
        || (len.len() > 1 && len.starts_with('0'))
    {
        return None;
    }
    let len = len.parse::<usize>().ok()?;
    if !rest.is_char_boundary(len) {
        return None;
    }
    let (session, run) = rest.split_at(len);
    if session.is_empty() || run.is_empty() {
        return None;
    }
    Some((
        SessionId::parse(session).ok()?,
        lash_core::TurnId::parse(run).ok()?,
    ))
}

// ---------------------------------------------------------------------------
// The `SessionShifts` slot
// ---------------------------------------------------------------------------

/// The core's [`SessionShifts`] as a deployment's session handlers see it.
///
/// The endpoint binds `LashSession` and `LashTurn` before any core over the
/// backend exists, so they read the `SessionShifts` from this slot when a shift runs.
/// The core fills it through the engine's
/// [`install_session_shifts`](SessionWorkEngine::install_session_shifts), a
/// get-or-init: while a core keeps its installation, one engine has one
/// answer to what runs its shifts, whichever core was built first.
///
/// The slot holds the installation **weakly**. The `SessionShifts` belongs to the
/// core, which owns the backend this slot lives in; a strong reference back
/// would make the three a cycle no drop ever breaks. An install wraps the
/// `SessionShifts` in an installation, and the core keeps the installation the
/// install returns for as long as it serves shifts. A shift holds the `SessionShifts`
/// it runs on, never the installation, so a shift still in flight when its
/// core is dropped runs to its end on that core's `SessionShifts` without keeping the
/// install live: a core built meanwhile installs its own `SessionShifts` (FIG-4017).
/// A shift that runs while no live installation is held (before the core is
/// built, or after it was dropped) fails its attempt retryably, naming the
/// empty slot, and a later install serves it. Clones share one slot.
#[derive(Clone, Default)]
pub struct RestateSessionShiftsSlot {
    installation: Arc<Mutex<Option<Weak<InstalledSessionShifts>>>>,
    /// The effect budget of a run's invocation, when the engine's
    /// configuration set one
    /// ([`RestateConfig::with_run_effect_budget`](crate::RestateConfig::with_run_effect_budget)).
    run_effect_budget: Option<u64>,
}

/// A `SessionShifts` as a [`RestateSessionShiftsSlot`] installed it: what its core
/// keeps, and whose life decides whether the install is live. It answers
/// every call with the `SessionShifts` it wraps.
struct InstalledSessionShifts {
    shifts: Arc<dyn SessionShifts>,
}

#[async_trait::async_trait]
impl SessionShifts for InstalledSessionShifts {
    fn owns_reconciliation(&self) -> bool {
        self.shifts.owns_reconciliation()
    }

    fn runs_on(&self, shifts: &dyn SessionShifts) -> bool {
        self.shifts.runs_on(shifts)
    }

    async fn reconcile(
        &self,
        cursor: &lash_core::engine::ReconcileCursor,
        page: std::num::NonZeroUsize,
    ) -> Result<lash_core::engine::ReconcileCursor, lash_core::StoreError> {
        self.shifts.reconcile(cursor, page).await
    }

    fn hold_shift(&self, session: &SessionId) -> lash_core::engine::ShiftHold {
        self.shifts.hold_shift(session)
    }

    async fn admit(
        &self,
        controller: lash_core::ScopedEffectController<'_>,
        request: &ShiftRequest,
        admitting_generation: &lash_core::engine::BuildGeneration,
        ordinal: u32,
        draining: Option<&BuildGeneration>,
    ) -> Result<AdmitVerdict, ShiftAbort> {
        self.shifts
            .admit(controller, request, admitting_generation, ordinal, draining)
            .await
    }

    async fn execute_run(
        &self,
        controller: lash_core::ScopedEffectController<'_>,
        admitted: Admitted,
    ) -> RunEnd {
        self.shifts.execute_run(controller, admitted).await
    }

    async fn close_run(
        &self,
        controller: lash_core::ScopedEffectController<'_>,
        session: &SessionId,
        run: &lash_core::TurnId,
    ) -> Result<(), ShiftAbort> {
        self.shifts.close_run(controller, session, run).await
    }
}

impl RestateSessionShiftsSlot {
    /// An empty slot.
    pub fn new() -> Self {
        Self::default()
    }

    /// This slot, whose runs' invocations run under `budget` effects.
    pub(crate) fn with_run_effect_budget(mut self, budget: Option<u64>) -> Self {
        self.run_effect_budget = budget;
        self
    }

    /// The options a run's controller journals under.
    fn run_options(&self) -> crate::RestateEffectControllerOptions {
        let options = crate::RestateEffectControllerOptions::default();
        match self.run_effect_budget {
            Some(budget) => options.segment_effect_budget(budget),
            None => options,
        }
    }

    /// Install `shifts` unless a live installation is held already; returns
    /// the installation the slot now serves, which the caller keeps alive for
    /// as long as it serves shifts.
    pub fn install(&self, shifts: Arc<dyn SessionShifts>) -> Arc<dyn SessionShifts> {
        self.install_new(shifts).0
    }

    /// [`Self::install`], also answering whether the slot took `shifts` —
    /// false when a live installation was already held and is kept.
    fn install_new(&self, shifts: Arc<dyn SessionShifts>) -> (Arc<dyn SessionShifts>, bool) {
        let mut slot = self
            .installation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(live) = slot.as_ref().and_then(Weak::upgrade) {
            return (live, false);
        }
        let installation = Arc::new(InstalledSessionShifts { shifts });
        *slot = Some(Arc::downgrade(&installation));
        (installation, true)
    }

    /// The installed `SessionShifts`, if its installation is still held. The `SessionShifts`
    /// returned does not keep the installation live.
    pub fn installed(&self) -> Option<Arc<dyn SessionShifts>> {
        self.installation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .and_then(Weak::upgrade)
            .map(|installation| Arc::clone(&installation.shifts))
    }

    /// The installed `SessionShifts`, or the retryable failure of a shift that ran
    /// while none was installed.
    fn shifts_for(&self, handler: &str) -> Result<Arc<dyn SessionShifts>, HandlerError> {
        self.installed().ok_or_else(|| {
            HandlerError::from(std::io::Error::other(format!(
                "{handler}: no SessionShifts is installed on this deployment; \
                 a core over this backend installs it when it is built"
            )))
        })
    }
}

impl std::fmt::Debug for RestateSessionShiftsSlot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RestateSessionShiftsSlot")
            .field("installed", &self.installed().is_some())
            .finish()
    }
}

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
        session: &SessionId,
        run: &lash_core::TurnId,
    ) -> Option<lash_core::RuntimeError> {
        let key = turn_workflow_key(session, run);
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
                for ran in &outcome.ran {
                    if let lash_core::engine::RunOutcome::Released { run } = ran
                        && let Some(refusal) = self.released_run_refusal(session, run).await
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

/// One admitted run, keyed [`turn_workflow_key`]. A workflow key runs
/// once: a shift that admits a run whose `run` already started attaches to
/// its recorded [`outcome`](LashTurn::outcome) instead.
#[restate_sdk::workflow]
pub trait LashTurn {
    async fn run(call: Call<RestateRunRequest>) -> HandlerResult<Reply<RunOutcome>>;

    /// The outcome `run` recorded, once it ended: what it returned, or
    /// [`RunOutcome::Released`] for a run that ended terminally without a
    /// lash outcome. `None` while `run` has not ended.
    #[shared]
    async fn outcome(call: Call<()>) -> HandlerResult<Reply<Option<RunOutcome>>>;

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
        call: Call<RestateRunRequest>,
    ) -> HandlerResult<Reply<RunOutcome>> {
        let (wire, input) = call.open()?;
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
    ) -> HandlerResult<Reply<Option<RunOutcome>>> {
        let (wire, ()) = call.open()?;
        let recorded = ctx
            .get::<Vec<u8>>(TURN_OUTCOME_STATE)
            .await?
            .map(|bytes| {
                object_state::decode_stamped_bytes(
                    TURN_OUTCOME_STATE,
                    &bytes,
                    &TURN_OUTCOME_FORMATS,
                )
            })
            .transpose()?;
        Ok(Reply::at(wire, recorded))
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
            input.sender_generation.as_ref(),
            &input.run,
        )
        .await
        .map(|()| Reply::at(wire, ()))
    }
}

/// What `LashSession/{session}/shift` journals: the leg start, then
/// admission `n` on the shift-admission scope, then, for an admitted run,
/// the call to its `LashTurn`, the run boundary, then
/// admission `n + 1`, until admission answers anything but an admitted run
/// or a boundary hands the shift off. `handed_off` is what the leg before
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
    let admission_scope = shift_admission_scope(&request.session, &request.request);
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
        let scoped = controller
            .scoped_effect_controller(admission_scope.clone())
            .map_err(refused_scope)?;
        let draining = drain_answered(route, &request.request, ordinal).then_some(generation);
        let verdict = shifts
            .admit(scoped, &request, generation, ordinal, draining)
            .await
            .map_err(abort_failure)?;
        // The leg's stop, and what it hands the rest of the shift to when it
        // ends at a boundary: the route its continuation is sent under and
        // the continuation's request id.
        let (next, stop) = match verdict {
            AdmitVerdict::Admit(admitted) => {
                // A run this shift already ran, or whose execution it saw
                // released, is never called a second time: its `LashTurn`
                // key has run once.
                if let Err(stop) = rules.before(&admitted) {
                    return Ok(ShiftOutcome { ran, stop });
                }
                let run = admitted.run().clone();
                let work = admitted.work().clone();
                let key = turn_workflow_key(admitted.session(), &run);
                // A newly admitted run is new work: its `LashTurn` goes to
                // the stable lane, which Restate hands to the newest build
                // (FIG-3795), and its outcome reads back under the same route.
                let outcome = match crate::services::routed_workflow::<_, _, RunOutcome>(
                    controller.context(),
                    &route.namespace().stable(LashService::TurnDriver),
                    key.clone(),
                    "run",
                    RestateRunRequest {
                        sender_generation: Some(generation.clone()),
                        admitted,
                    },
                )
                .call()
                .await
                {
                    Ok(reply) => reply.into_body(),
                    // The call ended without a lash outcome: an earlier shift
                    // already ran this key (409), the run was refused
                    // terminally, or an operator's verb killed it. The shift
                    // attaches to what the run recorded; a run that recorded
                    // nothing is released. Either way the run is consumed,
                    // never a failure of the whole shift: the next admission
                    // reads what the store decided about it (ADR 0104 O4).
                    Err(error) => {
                        let recorded =
                            crate::services::routed_workflow::<_, (), Option<RunOutcome>>(
                                controller.context(),
                                &route.namespace().stable(LashService::TurnDriver),
                                key,
                                "outcome",
                                (),
                            )
                            .call()
                            .await
                            .map_err(HandlerError::from)?;
                        match recorded.into_body() {
                            Some(outcome) => outcome,
                            None => {
                                tracing::warn!(
                                    session_id = request.session.as_str(),
                                    run = run.as_str(),
                                    error = %error,
                                    "session shift consumed a released run execution"
                                );
                                RunOutcome::Released { run: run.clone() }
                            }
                        }
                    }
                };
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
            AdmitVerdict::Draining { generation } => (
                Some((
                    route.namespace().stable(LashService::SessionShifts),
                    session_shift_continuation(
                        &request,
                        &route.namespace().stable(LashService::SessionShifts),
                    ),
                )),
                ShiftStop::Draining { generation },
            ),
            AdmitVerdict::Idle => (None, ShiftStop::Idle),
            AdmitVerdict::Parked(park) => (None, ShiftStop::Parked(park)),
            AdmitVerdict::SubstrateLost { run } => (None, ShiftStop::SubstrateLost { run }),
            AdmitVerdict::RunTerminal { run, kind, commit } => {
                (None, ShiftStop::RunTerminal { run, kind, commit })
            }
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

/// What `LashTurn/{session}:{run}/run` journals: the kernel's root run on
/// the run's scope, which records its seal first.
async fn execute_run_journal(
    slot: &RestateSessionShiftsSlot,
    authority_id: &RestateAuthorityId,
    generation: &BuildGeneration,
    route: &crate::services::ServiceRoute,
    ctx: WorkflowContext<'_>,
    writer: StoredValueWriter,
    request: RestateRunRequest,
) -> Result<RunOutcome, HandlerError> {
    let RestateRunRequest {
        sender_generation,
        admitted,
    } = request;
    let sender_generation = sender_generation.as_ref();
    let expected = turn_workflow_key(admitted.session(), admitted.run());
    if ctx.key() != expected {
        return Err(misaddressed(format!(
            "LashTurn/{} was asked to run run `{expected}`",
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
                admitted.run(),
                admitted.session()
            ),
        ));
    }
    let handler = route.namespace().stable(LashService::TurnDriver).name();
    let shifts = slot.shifts_for(&handler)?;
    // The generation sentinel rides the run's first recorded step, its start
    // marker (FIG-3980): a journal of another build parks before it replays
    // past it.
    let sentinel = Arc::new(FoldedSentinel::new(handler, generation.clone()));
    let controller = RestateRuntimeEffectController::with_options(
        ctx,
        authority_id.clone(),
        generation.clone(),
        slot.run_options(),
    )
    .in_namespace(route.namespace().clone())
    .with_folded_sentinel(Arc::clone(&sentinel));
    let scoped = controller
        .scoped_effect_controller(shift_run_scope(admitted.session(), admitted.run()))
        .map_err(refused_scope)?;
    let run = admitted.run().clone();
    // This build executes the run, whichever build's shift admitted it: the
    // stable lane hands a new run to the newest build (FIG-4742).
    let admitted = admitted.run_by(generation.clone());
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
            },
        )
        .send()
        .await?;
    }
    // The key runs once; a later shift that admits this run reads this,
    // stamped so a reader of another build dispatches on its format.
    object_state::set_stamped(controller.context(), TURN_OUTCOME_STATE, writer, ended);
    result
}

/// What `LashTurn/{session}:{run}/close` journals: the kernel's recorded
/// `CloseRunScope` step of `run` on the key's run scope, the scope the
/// key's `run` would have recorded it under.
async fn close_run_journal(
    slot: &RestateSessionShiftsSlot,
    authority_id: &RestateAuthorityId,
    generation: &BuildGeneration,
    route: &crate::services::ServiceRoute,
    ctx: SharedWorkflowContext<'_>,
    sender_generation: Option<&BuildGeneration>,
    run: &lash_core::TurnId,
) -> Result<(), HandlerError> {
    let Some((session, admitted_run)) = parse_turn_workflow_key(ctx.key()) else {
        return Err(misaddressed(format!(
            "LashTurn/{} names no run to close `{run}` under",
            ctx.key()
        )));
    };
    // A close on a generation lane was sent by a run on that lane, which
    // names the lane's generation; anything else is a misroute.
    if let crate::services::Lane::Generation(lane) = route.lane()
        && sender_generation != Some(lane)
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
        .scoped_effect_controller(shift_run_scope(&session, &admitted_run))
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

    /// W5: the `LashTurn` key parses back to exactly the session and run
    /// it was built from, whatever either id contains.
    #[test]
    fn a_turn_workflow_key_round_trips_any_session_and_run() {
        let cases = [
            ("s", "r"),
            ("a:b", "c"),
            ("a", "b:c"),
            ("12:ab", ":x:"),
            ("sess\u{e9}:\u{1f600}", "run:agent-frame:2"),
            ("0", "0"),
            (":", ":"),
        ];
        for (session, run) in cases {
            let session = SessionId::from(session);
            let run = lash_core::TurnId::from(run);
            let key = turn_workflow_key(&session, &run);
            assert_eq!(
                parse_turn_workflow_key(&key),
                Some((session.clone(), run.clone())),
                "{key}"
            );
        }
        assert_ne!(
            turn_workflow_key(&SessionId::from("a:b"), &lash_core::TurnId::from("c")),
            turn_workflow_key(&SessionId::from("a"), &lash_core::TurnId::from("b:c")),
            "the pre-S7 `{{session}}:{{run}}` key was ambiguous here"
        );
        for malformed in [
            "",
            "s:r",
            "3:ab",
            "03:abcd",
            "2:ab",
            ":ab",
            "x2:abc",
            "1:\u{e9}x",
        ] {
            assert_eq!(parse_turn_workflow_key(malformed), None, "{malformed}");
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
