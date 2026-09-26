#![allow(
    deprecated,
    reason = "Restate SDK 0.11 retains the trait service API while its replacement is staged"
)]

//! The session driver on Restate (FIG-3600, ADR 0104 O1/O2/O6): the engine
//! that runs every session's drive.
//!
//! Two lash services split one drive across handlers, each on its own
//! journal:
//!
//! - **`LashSession/{session}`** is a virtual object keyed by the session.
//!   Its exclusive `drive` handler loops the kernel's recorded admission
//!   (`AdmitDrive`, ordinal 0, 1, ..) on a controller scoped to
//!   [`drive_admission_scope`], and for every admitted root calls that
//!   root's `LashTurn` and awaits it. It returns once admission answers
//!   anything but an admitted root. The object's key serializes the
//!   engine's drives of the session (O1); an in-process driver beside it is
//!   serialized by the SQL session execution lease until S8.
//! - **`LashTurn/{session}:{root}`** is a workflow, one per logical root. Its
//!   `run` handler seals the admission and runs the root's turns on a
//!   controller scoped to [`drive_root_scope`], under the retry contract of a
//!   turn handler ([`turn_handler_options`](crate::turn_handler_options)): a
//!   parked root fails its attempt retryably and pauses after the budget, so
//!   its journal is kept for a restored build.
//!
//! The kernel owns what a drive admits and how a root runs; these handlers
//! only give each step its journal. They journal the kernel's recorded steps
//! and nothing else, so what the kernel does between those steps is not
//! journaled either: a root still reads live store state ahead of its
//! claim and while adopting its admitted head (the orphaned-input repair,
//! `committed_turn_exists`, the pending inputs), and a replay re-evaluates
//! those reads under the session execution lease rather than reading them
//! back (FIG-3824). Rule 6 of `scripts/check-substrate-boundary.sh` pins
//! every direct store call in the session drive, tagged by whether a
//! recorded step makes it. The core installs its
//! [`SessionDriver`] on the engine ([`SessionWorkEngine::install_session_driver`]),
//! and both handlers read it from the deployment's
//! [`RestateSessionDriverSlot`], so a host wires nothing.
//!
//! **Scheduling (O2).** [`SessionWorkEngine::schedule_drive`] is a one-way
//! send to `LashSession/{session}/drive` whose idempotency key is the drive
//! request's id. Every schedule names its own request (the committed row it
//! follows, or one reconcile sweep's ask), never the session or a running
//! drive, so a schedule issued while a drive runs is never deduplicated
//! away: it queues behind the running drive on the object, and its first
//! admission is the re-check that admits whatever that drive left pending. A
//! schedule lost with its process is healed by the reconcile sweep at the
//! next boot or `drain_status`, or by the session's next schedule.
//!
//! **Generations.** Both handlers' requests carry
//! [`LASH_SESSION_DRIVE_VERSION`], the generation of the commands their
//! journals lead with. A request built for another generation decodes and
//! is refused, terminally and before the handler journals anything, so a
//! journal written under one generation is never replayed against another.
//!
//! **Drain generation (ADR 0106 §1, FIG-3795).** Each handler's first
//! journaled command is the generation sentinel, a step named
//! `lash.build.generation` that records the executing build's drain
//! generation `G`. A replay that reads back another `G` (the code behind a
//! pinned deployment changed) parks its attempt, typed with the recorded `G`,
//! before any other command. A `LashTurn` request carries the `G` of the drive
//! that admitted the root (`sender_generation`), so a root the latest build
//! cannot run can be routed back to its writer's generation. The step's name
//! and output are frozen. `G` is the engine's: the facade derives it from the
//! build's drain formats and hands it in through
//! [`RestateConfig`](crate::RestateConfig) (FIG-3795 A).

use std::sync::{Arc, Mutex, Weak};

use lash_core::engine::{
    AdmitVerdict, Admitted, BuildGeneration, DriveAbort, DriveLoop, DriveOutcome, DriveRequest,
    DriveRequestId, DriveStop, RootOutcome, drive_admission_scope, drive_root_scope,
};
use lash_core::{SessionDriver, SessionId, SessionWorkEngine};
use restate_sdk::context::{
    ContextClient, ContextReadState, ContextSideEffects, ContextWriteState, ObjectContext,
    RunFuture as _, SharedWorkflowContext, WorkflowContext,
};
use restate_sdk::errors::{HandlerError, HandlerResult, TerminalError};
use restate_sdk::serde::Json;
use serde::{Deserialize, Serialize};

use crate::{
    LashService, RestateAuthorityId, RestateIngressClient, RestateRuntimeEffectController,
    parked_turn_failure,
};

/// The generation of the session driver's journaled command prefix: the
/// input stamp of every `LashSession` and `LashTurn` request (ADR 0105 §12).
///
/// It owns what the two handlers journal ahead of the kernel's own recorded
/// effects: `LashSession`'s admission steps and its `LashTurn` calls, and the
/// root start marker and seal `LashTurn` records first. Any change to those
/// commands, their order, or what they key on bumps it. The scheduler stamps
/// it on every request, and each handler refuses any other generation before
/// it journals anything. An unstamped request is generation 0, which no build
/// drives.
///
/// Generation 2 (FIG-3815): `LashTurn` records the root's start marker
/// (`drive-root-start:{admission}`) before its seal.
///
/// Generation 3 (FIG-3600 S7-A): `LashSession` reads a root's recorded
/// outcome through `LashTurn`'s `outcome` handler when its `run` call ends
/// without one.
///
/// Generation 4 (FIG-3600 S7-A): a journaled admission or drive stop that
/// names a terminal root carries the root's terminal kind and, when a head
/// commit ended it, that commit, in place of the commit alone.
pub const LASH_SESSION_DRIVE_VERSION: u32 = 4;

/// The drive handler's name on `LashSession`.
const DRIVE_HANDLER: &str = "drive";

/// The `LashTurn` state entry `run` records the root's outcome under once it
/// ended, terminally included; `outcome` reads it back.
const TURN_OUTCOME_STATE: &str = "outcome";

/// The generation an unstamped request was written by: none this build
/// drives.
fn unstamped_drive_version() -> u32 {
    0
}

/// The request `LashSession/{session}/drive` runs: one drive of the session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestateSessionDriveRequest {
    /// [`LASH_SESSION_DRIVE_VERSION`] of the build that scheduled the drive.
    #[serde(default = "unstamped_drive_version")]
    pub drive_version: u32,
    pub request: DriveRequest,
}

/// The request `LashTurn/{session}:{root}/run` runs: one admitted root.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestateTurnDriveRequest {
    /// [`LASH_SESSION_DRIVE_VERSION`] of the drive that admitted the root.
    #[serde(default = "unstamped_drive_version")]
    pub drive_version: u32,
    /// The drain generation of the build whose drive admitted the root: the
    /// generation a root the latest build refuses is routed back to
    /// (ADR 0106 §1). Unstamped, it is `None`.
    #[serde(default)]
    pub sender_generation: Option<BuildGeneration>,
    /// The recorded admission the root runs under. The root runs on the head
    /// its recorded claim was admitted on, and a replay reads that claim back;
    /// adopting that head still reads live store state (FIG-3824).
    pub admitted: Admitted,
}

/// The frozen name of the generation sentinel step, the first command every
/// session-driver journal records. Its name and its output (the executing
/// build's [`BuildGeneration`] as a JSON string) never change.
const GENERATION_SENTINEL: &str = "lash.build.generation";

/// The generation sentinel's verdict. Each handler records the executing
/// build's generation as its journal's first command
/// ([`GENERATION_SENTINEL`]); a replay that reads back another generation
/// parks its attempt, typed with the recorded generation, before any other
/// command, and keeps its journal for a build of that generation.
fn check_generation(
    service: LashService,
    recorded: &BuildGeneration,
    executing: &BuildGeneration,
) -> Result<(), HandlerError> {
    if recorded == executing {
        return Ok(());
    }
    Err(parked_turn_failure(format!(
        "RetiredGeneration: {} journal was recorded under generation `{}`; this build is \
         generation `{}` and parks it for a build of the recorded generation",
        service.name(),
        recorded.as_str(),
        executing.as_str()
    )))
}

/// The `LashTurn` workflow key of `root` in `session`: one workflow per
/// logical root, so a replay of the session's drive re-calls the same root.
///
/// The key is `{len}:{session}{root}`, where `len` is the session id's length
/// in bytes, so it parses back to exactly one `(session, root)` whatever
/// either id contains ([`parse_turn_workflow_key`]): reconciliation maps a
/// paused `LashTurn` invocation to its root by its key alone. A host finds
/// the invocation running one of its roots by this key.
pub fn turn_workflow_key(session: &SessionId, root: &lash_core::TurnId) -> String {
    format!(
        "{}:{}{}",
        session.as_str().len(),
        session.as_str(),
        root.as_str()
    )
}

/// The `(session, root)` a [`turn_workflow_key`] names, or `None` for a key
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
    let (session, root) = rest.split_at(len);
    if session.is_empty() || root.is_empty() {
        return None;
    }
    Some((SessionId::from(session), lash_core::TurnId::from(root)))
}

// ---------------------------------------------------------------------------
// The driver slot
// ---------------------------------------------------------------------------

/// The core's [`SessionDriver`] as a deployment's session handlers see it.
///
/// The endpoint binds `LashSession` and `LashTurn` before any core over the
/// backend exists, so they read the driver from this slot when a drive runs.
/// The core fills it through the engine's
/// [`install_session_driver`](SessionWorkEngine::install_session_driver), a
/// get-or-init: while an installed driver is alive, one engine has one
/// answer to what runs its drives, whichever core was built first.
///
/// The slot holds the driver **weakly**. The driver belongs to the core,
/// which owns the backend this slot lives in; a strong reference back would
/// make the three a cycle no drop ever breaks. The core keeps the driver the
/// install returns for as long as it serves drives. A drive that runs while
/// no live driver is installed (before the core is built, or after it was
/// dropped) fails its attempt retryably, naming the empty slot, and a later
/// install serves it. Clones share one slot.
#[derive(Clone, Default)]
pub struct RestateSessionDriverSlot {
    driver: Arc<Mutex<Option<Weak<dyn SessionDriver>>>>,
}

impl RestateSessionDriverSlot {
    /// An empty slot.
    pub fn new() -> Self {
        Self::default()
    }

    /// Install `driver` unless a live one is installed already; returns the
    /// driver the slot now serves, which the caller keeps alive.
    pub fn install(&self, driver: Arc<dyn SessionDriver>) -> Arc<dyn SessionDriver> {
        let mut slot = self
            .driver
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(live) = slot.as_ref().and_then(Weak::upgrade) {
            return live;
        }
        *slot = Some(Arc::downgrade(&driver));
        driver
    }

    /// The installed driver, if one is installed and still alive.
    pub fn installed(&self) -> Option<Arc<dyn SessionDriver>> {
        self.driver
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .and_then(Weak::upgrade)
    }

    /// The installed driver, or the retryable failure of a drive that ran
    /// while none was installed.
    fn driver_for(&self, handler: &str) -> Result<Arc<dyn SessionDriver>, HandlerError> {
        self.installed().ok_or_else(|| {
            HandlerError::from(std::io::Error::other(format!(
                "{handler}: no session driver is installed on this deployment; \
                 a core over this backend installs it when it is built"
            )))
        })
    }
}

impl std::fmt::Debug for RestateSessionDriverSlot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RestateSessionDriverSlot")
            .field("installed", &self.installed().is_some())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// The engine: scheduling
// ---------------------------------------------------------------------------

/// Restate's [`SessionWorkEngine`]: a drive is a one-way send to the
/// session's `LashSession` object, and the core's driver lives in the
/// deployment's [`RestateSessionDriverSlot`].
#[derive(Clone)]
pub struct RestateSessionWork {
    ingress: RestateIngressClient,
    slot: RestateSessionDriverSlot,
    /// The drain generation of the build scheduling drives: every drive
    /// request it sends is stamped with it.
    build_generation: BuildGeneration,
    control: Arc<dyn lash_core::engine::SessionControlEngine>,
}

#[expect(
    clippy::result_large_err,
    reason = "the ingress client's RestateHttpError is unboxed across its public API"
)]
impl RestateSessionWork {
    pub(crate) fn new(
        ingress: RestateIngressClient,
        slot: RestateSessionDriverSlot,
        build_generation: BuildGeneration,
        control: Arc<dyn lash_core::engine::SessionControlEngine>,
    ) -> Self {
        Self {
            ingress,
            slot,
            build_generation,
            control,
        }
    }

    /// The slot the deployment's session handlers read the driver from.
    pub fn driver_slot(&self) -> &RestateSessionDriverSlot {
        &self.slot
    }

    /// Send `request`'s drive to `LashSession/{session}`, keyed by the
    /// request id: a repeated send of one request attaches to its first
    /// invocation instead of driving twice. Resolves once Restate accepted
    /// the send, not once the drive ran.
    pub async fn send_drive(
        &self,
        session: &SessionId,
        request: DriveRequestId,
    ) -> Result<crate::RestateInvocationId, crate::RestateHttpError> {
        let body = RestateSessionDriveRequest {
            drive_version: LASH_SESSION_DRIVE_VERSION,
            request: DriveRequest {
                session: session.clone(),
                request: request.clone(),
                build_generation: self.build_generation.clone(),
            },
        };
        self.ingress
            .send_object_json_idempotent(
                LashService::SessionDriver.name(),
                session.as_str(),
                DRIVE_HANDLER,
                &body,
                request.as_str(),
            )
            .await
    }

    /// Attach to `request`'s drive of `session` and return how it ended,
    /// sending it first if nothing sent it yet: the same idempotency key as
    /// [`send_drive`](Self::send_drive), so the call and an earlier send name
    /// one invocation.
    pub async fn attach_drive(
        &self,
        session: &SessionId,
        request: DriveRequestId,
    ) -> Result<DriveOutcome, crate::RestateHttpError> {
        let body = RestateSessionDriveRequest {
            drive_version: LASH_SESSION_DRIVE_VERSION,
            request: DriveRequest {
                session: session.clone(),
                request: request.clone(),
                build_generation: self.build_generation.clone(),
            },
        };
        self.ingress
            .call_object_json_idempotent(
                LashService::SessionDriver.name(),
                session.as_str(),
                DRIVE_HANDLER,
                &body,
                request.as_str(),
            )
            .await
    }
}

impl RestateSessionWork {
    /// The refusal a released root's `LashTurn` ended with, when its run
    /// failed with one.
    async fn released_root_refusal(
        &self,
        session: &SessionId,
        root: &lash_core::TurnId,
    ) -> Option<lash_core::RuntimeError> {
        let key = turn_workflow_key(session, root);
        match self
            .ingress
            .attach_workflow_run(LashService::TurnDriver.name(), &key)
            .await
        {
            Err(crate::RestateHttpError::Status { body, .. }) => decode_drive_refusal(&body),
            _ => None,
        }
    }
}

impl std::fmt::Debug for RestateSessionWork {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RestateSessionWork")
            .field("slot", &self.slot)
            .field("build_generation", &self.build_generation)
            .finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl SessionWorkEngine for RestateSessionWork {
    fn control(&self) -> Arc<dyn lash_core::engine::SessionControlEngine> {
        Arc::clone(&self.control)
    }
    fn schedule_drive(&self, session: &SessionId, request: DriveRequestId) {
        // The ask is fire-and-forget by contract: the row it follows is
        // already durable, and a send that never reached Restate is healed by
        // the reconcile sweep or the session's next schedule.
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(
                session_id = session.as_str(),
                request = request.as_str(),
                "session drive not sent: scheduled outside a Tokio runtime; the reconcile sweep drives it"
            );
            return;
        };
        let engine = self.clone();
        let session = session.clone();
        runtime.spawn(async move {
            if let Err(error) = engine.send_drive(&session, request.clone()).await {
                tracing::warn!(
                    session_id = session.as_str(),
                    request = request.as_str(),
                    error = %error,
                    "session drive send failed; the reconcile sweep drives the session"
                );
            }
        });
    }

    fn install_session_driver(&self, driver: Arc<dyn SessionDriver>) -> Arc<dyn SessionDriver> {
        let installed = self.slot.install(driver);
        if !installed.owns_reconciliation() {
            return installed;
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let ingress = self.ingress.clone();
            runtime.spawn(async move {
                if let Err(error) = ingress
                    .send_object_json_idempotent(
                        crate::LashService::Reconcile.name(),
                        "recovery",
                        "tick",
                        &crate::session_reconcile::ReconcileRequest {
                            version: LASH_SESSION_DRIVE_VERSION,
                            sequence: 0,
                        },
                        &format!("reconcile-start:{LASH_SESSION_DRIVE_VERSION}"),
                    )
                    .await
                {
                    tracing::warn!(%error, "could not start session reconcile");
                }
            });
        }
        installed
    }

    /// Attach to `request`'s drive by its idempotency key, starting it if no
    /// send reached Restate. A drive a handler refused terminally is decoded
    /// back to the kernel's refusal; any other failure to attach (transport,
    /// the attach ceiling) is a retry under the same key.
    ///
    /// A root the drive consumed as released answers the refusal its
    /// `LashTurn` ended with, as the in-process drive answers a root's
    /// terminal refusal: the drive goes on past it, but a waiter on that root
    /// learns why it did not run to its end.
    async fn await_drive(
        &self,
        session: &SessionId,
        request: &DriveRequestId,
    ) -> Result<DriveOutcome, DriveAbort> {
        let error = match self.attach_drive(session, request.clone()).await {
            Ok(outcome) => {
                for ran in &outcome.ran {
                    if let lash_core::engine::RootOutcome::Released { root } = ran
                        && let Some(refusal) = self.released_root_refusal(session, root).await
                    {
                        return Err(classify_refusal(refusal));
                    }
                }
                return Ok(outcome);
            }
            Err(error) => error,
        };
        if let crate::RestateHttpError::Status { body, .. } = &error
            && let Some(refusal) = decode_drive_refusal(body)
        {
            return Err(classify_refusal(refusal));
        }
        Err(DriveAbort::Retry(lash_core::RuntimeError::new(
            lash_core::RuntimeErrorCode::EngineTurnTerminalAttach,
            format!(
                "attach to drive `{}` of session `{session}`: {error}",
                request.as_str()
            ),
        )))
    }
}

/// The prefix of a session-driver handler's terminal error message that
/// carries the refusal as a serialized [`lash_core::RuntimeError`], so a
/// caller attached to the drive reads back the kernel's own code.
const DRIVE_REFUSAL_MARKER: &str = "lash-drive-refused:";

/// A handler's terminal refusal, carrying `error` for a caller attached to
/// the drive.
fn drive_refusal(error: &lash_core::RuntimeError) -> HandlerError {
    let encoded = serde_json::to_string(error).unwrap_or_else(|_| {
        serde_json::json!({ "code": error.code.as_str(), "message": error.message }).to_string()
    });
    TerminalError::new(format!("{DRIVE_REFUSAL_MARKER}{encoded}")).into()
}

fn classify_refusal(error: lash_core::RuntimeError) -> DriveAbort {
    if error.is_retryable() {
        DriveAbort::Retry(error)
    } else {
        DriveAbort::Refused(error)
    }
}

/// The refusal a failed attach's response body carries, when a
/// session-driver handler ended the drive terminally.
fn decode_drive_refusal(body: &str) -> Option<lash_core::RuntimeError> {
    let message = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("message")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| body.to_owned());
    let (_, encoded) = message.split_once(DRIVE_REFUSAL_MARKER)?;
    let mut stream = serde_json::Deserializer::from_str(encoded).into_iter();
    stream.next()?.ok()
}

// ---------------------------------------------------------------------------
// The handlers
// ---------------------------------------------------------------------------

/// One session's drive. Every lash deployment serves it
/// (`crate::services::bind_lash_services`): a deployment that accepted input
/// without it would schedule drives nothing runs.
#[restate_sdk::object]
pub trait LashSession {
    async fn drive(request: Json<RestateSessionDriveRequest>) -> HandlerResult<Json<DriveOutcome>>;
}

/// One admitted root, keyed [`turn_workflow_key`]. A workflow key runs
/// once: a drive that admits a root whose `run` already started attaches to
/// its recorded [`outcome`](LashTurn::outcome) instead.
#[restate_sdk::workflow]
pub trait LashTurn {
    async fn run(request: Json<RestateTurnDriveRequest>) -> HandlerResult<Json<RootOutcome>>;

    /// The outcome `run` recorded, once it ended: what it returned, or
    /// [`RootOutcome::Released`] for a run that ended terminally without a
    /// lash outcome. `None` while `run` has not ended.
    #[shared]
    async fn outcome() -> HandlerResult<Json<Option<RootOutcome>>>;
}

/// The `LashSession` object over the deployment's driver slot, journaling
/// under the deployment's drain generation.
#[derive(Clone)]
pub(crate) struct LashSessionImpl {
    slot: RestateSessionDriverSlot,
    authority_id: RestateAuthorityId,
    build_generation: BuildGeneration,
}

/// The `LashTurn` workflow over the deployment's driver slot, journaling
/// under the deployment's drain generation.
#[derive(Clone)]
pub(crate) struct LashTurnImpl {
    slot: RestateSessionDriverSlot,
    authority_id: RestateAuthorityId,
    build_generation: BuildGeneration,
}

impl LashSessionImpl {
    pub(crate) fn new(
        slot: RestateSessionDriverSlot,
        authority_id: RestateAuthorityId,
        build_generation: BuildGeneration,
    ) -> Self {
        Self {
            slot,
            authority_id,
            build_generation,
        }
    }
}

impl LashTurnImpl {
    pub(crate) fn new(
        slot: RestateSessionDriverSlot,
        authority_id: RestateAuthorityId,
        build_generation: BuildGeneration,
    ) -> Self {
        Self {
            slot,
            authority_id,
            build_generation,
        }
    }
}

/// The terminal refusal of a request stamped for another generation. It is
/// returned before the handler journals anything.
fn retired_generation(service: LashService, found: u32) -> HandlerError {
    drive_refusal(&lash_core::RuntimeError::new(
        lash_core::RuntimeErrorCode::ExecutionScopeAdmissionRefused,
        format!(
            "{} request carries lash-session-drive-v{found}; this handler journals generation \
             {LASH_SESSION_DRIVE_VERSION}",
            service.name()
        ),
    ))
}

/// How a handler ends an attempt the kernel aborted.
fn abort_failure(abort: DriveAbort) -> HandlerError {
    match abort {
        // A live fault: the invocation retries, replaying what it recorded.
        DriveAbort::Retry(error) => HandlerError::from(error),
        // The park is durable; the invocation keeps its journal and pauses
        // after its attempt budget.
        DriveAbort::Parked { error, .. } => parked_turn_failure(error),
        DriveAbort::Refused(error) if error.is_retryable() => HandlerError::from(error),
        DriveAbort::Refused(error) => drive_refusal(&error),
    }
}

fn refused_scope(error: lash_core::RuntimeError) -> HandlerError {
    drive_refusal(&error)
}

/// A handler asked to run under a key its request does not name.
fn misaddressed(message: String) -> HandlerError {
    drive_refusal(&lash_core::RuntimeError::new(
        lash_core::RuntimeErrorCode::ExecutionScopeAdmissionRefused,
        message,
    ))
}

impl LashSession for LashSessionImpl {
    async fn drive(
        &self,
        ctx: ObjectContext<'_>,
        Json(input): Json<RestateSessionDriveRequest>,
    ) -> HandlerResult<Json<DriveOutcome>> {
        // The generation gate precedes every journaled command.
        if input.drive_version != LASH_SESSION_DRIVE_VERSION {
            return Err(retired_generation(
                LashService::SessionDriver,
                input.drive_version,
            ));
        }
        drive_session_journal(
            &self.slot,
            &self.authority_id,
            &self.build_generation,
            ctx,
            input.request,
        )
        .await
        .map(Json)
    }
}

impl LashTurn for LashTurnImpl {
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(input): Json<RestateTurnDriveRequest>,
    ) -> HandlerResult<Json<RootOutcome>> {
        // The generation gate precedes every journaled command.
        if input.drive_version != LASH_SESSION_DRIVE_VERSION {
            return Err(retired_generation(
                LashService::TurnDriver,
                input.drive_version,
            ));
        }
        run_root_journal(
            &self.slot,
            &self.authority_id,
            &self.build_generation,
            ctx,
            input.admitted,
        )
        .await
        .map(Json)
    }

    async fn outcome(
        &self,
        ctx: SharedWorkflowContext<'_>,
    ) -> HandlerResult<Json<Option<RootOutcome>>> {
        let recorded = ctx
            .get::<Json<RootOutcome>>(TURN_OUTCOME_STATE)
            .await?
            .map(|Json(outcome)| outcome);
        Ok(Json(recorded))
    }
}

/// What `LashSession/{session}/drive` journals: admission `n` on the
/// drive-admission scope, then, for an admitted root, the call to its
/// `LashTurn`, then admission `n + 1`, until admission answers anything but
/// an admitted root.
async fn drive_session_journal(
    slot: &RestateSessionDriverSlot,
    authority_id: &RestateAuthorityId,
    generation: &BuildGeneration,
    ctx: ObjectContext<'_>,
    request: DriveRequest,
) -> Result<DriveOutcome, HandlerError> {
    if ctx.key() != request.session.as_str() {
        return Err(misaddressed(format!(
            "LashSession/{} was asked to drive session `{}`",
            ctx.key(),
            request.session
        )));
    }
    let Json(recorded) = ctx
        .run(|| {
            let executing = generation.clone();
            async move { Ok(Json(executing)) }
        })
        .name(GENERATION_SENTINEL)
        .await?;
    check_generation(LashService::SessionDriver, &recorded, generation)?;
    let driver = slot.driver_for(LashService::SessionDriver.name())?;
    let controller = RestateRuntimeEffectController::new(ctx, authority_id.clone());
    let admission_scope = drive_admission_scope(&request.session, &request.request);
    let mut ran = Vec::new();
    // The kernel's stop rules, the same ones the in-process drive keeps.
    // Every outcome they read comes from a journaled call result, so a
    // replay rebuilds the same state.
    let mut rules = DriveLoop::new();
    let mut ordinal = 0_u32;
    loop {
        let scoped = controller
            .scoped_effect_controller(admission_scope.clone())
            .map_err(refused_scope)?;
        let verdict = driver
            .admit(scoped, &request, ordinal)
            .await
            .map_err(abort_failure)?;
        let stop = match verdict {
            AdmitVerdict::Admit(admitted) => {
                // A root this drive already ran, or whose execution it saw
                // released, is never called a second time: its `LashTurn`
                // key has run once.
                if let Err(stop) = rules.before(&admitted) {
                    return Ok(DriveOutcome { ran, stop });
                }
                let root = admitted.root().clone();
                let work = admitted.work().clone();
                let key = turn_workflow_key(admitted.session(), &root);
                let outcome = match controller
                    .context()
                    .workflow_client::<LashTurnClient>(key.clone())
                    .run(Json(RestateTurnDriveRequest {
                        drive_version: LASH_SESSION_DRIVE_VERSION,
                        sender_generation: Some(generation.clone()),
                        admitted,
                    }))
                    .call()
                    .await
                {
                    Ok(Json(outcome)) => outcome,
                    // The call ended without a lash outcome: an earlier drive
                    // already ran this key (409), the run was refused
                    // terminally, or an operator's verb killed it. The drive
                    // attaches to what the run recorded; a run that recorded
                    // nothing is released. Either way the root is consumed,
                    // never a failure of the whole drive: the next admission
                    // reads what the store decided about it (ADR 0104 O4).
                    Err(error) => {
                        let recorded = controller
                            .context()
                            .workflow_client::<LashTurnClient>(key)
                            .outcome()
                            .call()
                            .await
                            .map_err(HandlerError::from)?;
                        match recorded {
                            Json(Some(outcome)) => outcome,
                            Json(None) => {
                                tracing::warn!(
                                    session_id = request.session.as_str(),
                                    root = root.as_str(),
                                    error = %error,
                                    "session drive consumed a released root execution"
                                );
                                RootOutcome::Released { root }
                            }
                        }
                    }
                };
                let stop = rules.after(&work, &outcome);
                ran.push(outcome);
                if let Some(stop) = stop {
                    return Ok(DriveOutcome { ran, stop });
                }
                ordinal = ordinal.checked_add(1).ok_or_else(|| {
                    misaddressed(format!(
                        "session `{}` drive `{}` exhausted its admission ordinals",
                        request.session,
                        request.request.as_str()
                    ))
                })?;
                continue;
            }
            AdmitVerdict::Idle => DriveStop::Idle,
            AdmitVerdict::Parked(park) => DriveStop::Parked(park),
            AdmitVerdict::SubstrateLost { root } => DriveStop::SubstrateLost { root },
            AdmitVerdict::RootTerminal { root, kind, commit } => {
                DriveStop::RootTerminal { root, kind, commit }
            }
        };
        return Ok(DriveOutcome { ran, stop });
    }
}

/// What `LashTurn/{session}:{root}/run` journals: the kernel's root run on
/// the root's scope, which records its seal first.
async fn run_root_journal(
    slot: &RestateSessionDriverSlot,
    authority_id: &RestateAuthorityId,
    generation: &BuildGeneration,
    ctx: WorkflowContext<'_>,
    admitted: Admitted,
) -> Result<RootOutcome, HandlerError> {
    let expected = turn_workflow_key(admitted.session(), admitted.root());
    if ctx.key() != expected {
        return Err(misaddressed(format!(
            "LashTurn/{} was asked to run root `{expected}`",
            ctx.key()
        )));
    }
    let Json(recorded) = ctx
        .run(|| {
            let executing = generation.clone();
            async move { Ok(Json(executing)) }
        })
        .name(GENERATION_SENTINEL)
        .await?;
    check_generation(LashService::TurnDriver, &recorded, generation)?;
    let driver = slot.driver_for(LashService::TurnDriver.name())?;
    let controller = RestateRuntimeEffectController::new(ctx, authority_id.clone());
    let scoped = controller
        .scoped_effect_controller(drive_root_scope(admitted.session(), admitted.root()))
        .map_err(refused_scope)?;
    let root = admitted.root().clone();
    let (ended, result) = match driver.run_root(scoped, admitted).await {
        Ok(outcome) => (outcome.clone(), Ok(outcome)),
        // A retryable end records nothing: the run is not over.
        Err(abort @ (DriveAbort::Retry(_) | DriveAbort::Parked { .. })) => {
            return Err(abort_failure(abort));
        }
        Err(DriveAbort::Refused(error)) if error.is_retryable() => {
            return Err(HandlerError::from(error));
        }
        Err(abort @ DriveAbort::Refused(_)) => {
            (RootOutcome::Released { root }, Err(abort_failure(abort)))
        }
    };
    // The key runs once; a later drive that admits this root reads this.
    controller.context().set(TURN_OUTCOME_STATE, Json(ended));
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// W5: the `LashTurn` key parses back to exactly the session and root
    /// it was built from, whatever either id contains.
    #[test]
    fn a_turn_workflow_key_round_trips_any_session_and_root() {
        let cases = [
            ("s", "r"),
            ("a:b", "c"),
            ("a", "b:c"),
            ("12:ab", ":x:"),
            ("sess\u{e9}:\u{1f600}", "root:agent-frame:2"),
            ("0", "0"),
            (":", ":"),
        ];
        for (session, root) in cases {
            let session = SessionId::from(session);
            let root = lash_core::TurnId::from(root);
            let key = turn_workflow_key(&session, &root);
            assert_eq!(
                parse_turn_workflow_key(&key),
                Some((session.clone(), root.clone())),
                "{key}"
            );
        }
        assert_ne!(
            turn_workflow_key(&SessionId::from("a:b"), &lash_core::TurnId::from("c")),
            turn_workflow_key(&SessionId::from("a"), &lash_core::TurnId::from("b:c")),
            "the pre-S7 `{{session}}:{{root}}` key was ambiguous here"
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
    fn a_request_decodes_whatever_generation_it_carries() {
        let request = DriveRequest {
            session: SessionId::from("s"),
            request: DriveRequestId::new("r"),
            build_generation: BuildGeneration::for_test("t0"),
        };
        let mut stamped = serde_json::to_value(RestateSessionDriveRequest {
            drive_version: LASH_SESSION_DRIVE_VERSION,
            request,
        })
        .expect("encode");
        assert_eq!(
            stamped["drive_version"],
            serde_json::json!(LASH_SESSION_DRIVE_VERSION)
        );
        stamped["drive_version"] = serde_json::json!(LASH_SESSION_DRIVE_VERSION + 1);
        let successor: RestateSessionDriveRequest =
            serde_json::from_value(stamped.clone()).expect("a successor's stamp decodes");
        assert_eq!(successor.drive_version, LASH_SESSION_DRIVE_VERSION + 1);
        stamped
            .as_object_mut()
            .expect("an object")
            .remove("drive_version");
        let unstamped: RestateSessionDriveRequest =
            serde_json::from_value(stamped).expect("an unstamped request decodes");
        assert_eq!(unstamped.drive_version, 0);
    }

    #[test]
    fn the_sentinel_admits_only_its_own_generation() {
        let own = BuildGeneration::for_test("t0");
        assert!(check_generation(LashService::TurnDriver, &own, &own).is_ok());
        let other = BuildGeneration::for_test("t1");
        let refusal = check_generation(LashService::TurnDriver, &other, &own)
            .expect_err("another generation parks");
        let message = format!("{refusal:?}");
        assert!(message.contains("RetiredGeneration"), "{message}");
        assert!(message.contains(other.as_str()), "{message}");
    }

    #[test]
    fn a_terminal_drive_refusal_decodes_back_to_its_runtime_error() {
        let error = lash_core::RuntimeError::new(
            lash_core::RuntimeErrorCode::AcceptedTurnInputCeded,
            "the input was ceded: \"quoted\" text",
        );
        let message = format!(
            "{DRIVE_REFUSAL_MARKER}{}",
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
            let decoded = decode_drive_refusal(&body).expect("the refusal decodes");
            assert_eq!(decoded.code, error.code);
            assert_eq!(decoded.message, error.message);
        }
        assert!(decode_drive_refusal("{\"message\":\"connection reset\"}").is_none());
    }

    #[test]
    fn a_retryable_lane_refusal_keeps_the_handler_attempt_open() {
        let busy = lash_core::RuntimeError::new(
            lash_core::RuntimeErrorCode::SessionExecutionLaneBusy,
            "another execution holds the lane",
        );
        assert!(busy.is_retryable());
        assert!(matches!(
            classify_refusal(busy.clone()),
            DriveAbort::Retry(_)
        ));
        let failure = abort_failure(DriveAbort::Refused(busy));
        assert!(
            format!("{failure:?}").contains("Retryable"),
            "a racing lane release must not end the workflow: {failure:?}"
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

    /// A root the drive consumed as released answers a waiter with the
    /// refusal its `LashTurn` run ended with, as the in-process drive answers
    /// a root's terminal refusal, rather than a stop that says nothing of why.
    #[tokio::test]
    async fn a_released_root_answers_the_refusal_its_run_ended_with() {
        let session = SessionId::from("released-session");
        let root = lash_core::TurnId::from("released-root");
        let outcome = DriveOutcome {
            ran: vec![lash_core::engine::RootOutcome::Released { root: root.clone() }],
            stop: lash_core::engine::DriveStop::RootAborted { root: root.clone() },
        };
        let refusal = lash_core::RuntimeError::new(
            lash_core::RuntimeErrorCode::AcceptedTurnInputCeded,
            "the session is being deleted",
        );
        let failure = serde_json::json!({
            "message": format!(
                "{DRIVE_REFUSAL_MARKER}{}",
                serde_json::to_string(&refusal).expect("encode")
            ),
        });
        let transport = Arc::new(Scripted {
            requests: std::sync::Mutex::default(),
            responses: std::sync::Mutex::new(
                [
                    scripted_response(200, serde_json::to_string(&outcome).expect("encode")),
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
            RestateSessionDriverSlot::new(),
            BuildGeneration::for_test("t0"),
        );

        let answer = work
            .await_drive(&session, &DriveRequestId::new("released-request"))
            .await;
        let Err(DriveAbort::Refused(answered)) = answer else {
            panic!("the released root answers its refusal: {answer:?}");
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
        let request = DriveRequestId::new("racing-lane-request");
        let busy = lash_core::RuntimeError::new(
            lash_core::RuntimeErrorCode::SessionExecutionLaneBusy,
            "lane release is still in flight",
        );
        let failure = serde_json::json!({
            "message": format!(
                "{DRIVE_REFUSAL_MARKER}{}",
                serde_json::to_string(&busy).expect("encode")
            ),
        });
        let completed = DriveOutcome {
            ran: vec![],
            stop: DriveStop::Idle,
        };
        let transport = Arc::new(Scripted {
            requests: std::sync::Mutex::default(),
            responses: std::sync::Mutex::new(
                [
                    scripted_response(500, failure.to_string()),
                    scripted_response(200, serde_json::to_string(&completed).expect("encode")),
                ]
                .into(),
            ),
        });
        let work = RestateSessionWork::new(
            crate::RestateIngressClient::new(crate::RestateConnection::with_transport(
                "https://cloud.example",
                transport.clone(),
            )),
            RestateSessionDriverSlot::new(),
            BuildGeneration::for_test("t0"),
        );
        let first = work.await_drive(&session, &request).await;
        assert!(matches!(first, Err(DriveAbort::Retry(ref error)) if error.code == busy.code));
        let redrive = work.await_drive(&session, &request).await;
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

    #[test]
    fn the_sentinel_step_is_frozen() {
        assert_eq!(GENERATION_SENTINEL, "lash.build.generation");
        let generation = BuildGeneration::for_test("t0");
        assert_eq!(
            serde_json::to_string(&generation).expect("encode"),
            format!("\"{}\"", generation.as_str())
        );
    }
}
