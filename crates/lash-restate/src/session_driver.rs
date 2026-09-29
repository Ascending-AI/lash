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
//! only give each step its journal. The root's claim step repairs orphaned
//! inputs and records its claim. Its `InspectAdmittedHead` step records the
//! store-backed decision about the claimed head. On replay both steps return
//! their recorded outcomes. Before a turn effect, a fenced live check may
//! stop a root whose claim lost authority between attempts (FIG-3824,
//! ADR 0105 §2). Rule 6 of `scripts/check-substrate-boundary.sh` pins direct
//! store calls and the repair helper in the session drive. The core installs its
//! [`SessionDriver`] on the engine ([`SessionWorkEngine::install_session_driver`]),
//! and both handlers read it from the deployment's
//! [`RestateSessionDriverSlot`], so a host wires nothing.
//!
//! **Scheduling (O2).** A drive is a send to `LashSession/{session}/drive`
//! whose idempotency key is the drive's request id. Every ask names its own
//! request: the admitted row and attempt its ingress obligation asks for,
//! `ingress:{item}:{attempt}`, or a continuation. It never names the session
//! or a running drive. The engine coalesces the asks of one session (FIG-4036,
//! [`asks`]). An ask that finds none of the session's drives in flight from
//! this process is sent at once, under its own request.
//! [`SessionWorkEngine::request_drive`] answers once Restate accepted it, and
//! `schedule_drive` is its fire-and-forget twin. An ask that finds a drive in
//! flight is never deduplicated into that drive. It joins the one drive queued
//! behind it, which the engine sends once the drive in flight has ended.
//! That drive's first admission is the re-check that admits whatever the
//! drive before it left pending. So a burst of sends queues one drive, not
//! one per send. An ask lost with its process, or a drive the engine lost
//! before it admitted the row, is the ingress relay's to ask again from the
//! row's obligation (ADR 0109 §3). Nothing scans the session catalog for
//! undriven rows.
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
    DriveRequestId, DriveStop, MAX_ROOTS_PER_DRIVE, RootOutcome, drive_admission_scope,
    drive_continuation_request, drive_root_scope,
};
use lash_core::{SessionDriver, SessionId, SessionWorkEngine};
use restate_sdk::context::{
    ContextReadState, ContextWriteState, ObjectContext, SharedWorkflowContext, WorkflowContext,
};
use restate_sdk::errors::{HandlerError, HandlerResult, TerminalError};
use restate_sdk::serde::Json;
use serde::{Deserialize, Serialize};

use crate::sentinel::FoldedSentinel;
use crate::{
    LashService, RestateAuthorityId, RestateIngressClient, RestateRuntimeEffectController,
    parked_turn_failure,
};

mod asks;

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
///
/// Generation 4 changed in place under the pre-1.0 version freeze
/// (FIG-3980): neither handler journals a separate generation sentinel step;
/// its generation rides the first recorded step, admission 0 or the root's
/// start marker.
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
/// get-or-init: while a core keeps its installation, one engine has one
/// answer to what runs its drives, whichever core was built first.
///
/// The slot holds the installation **weakly**. The driver belongs to the
/// core, which owns the backend this slot lives in; a strong reference back
/// would make the three a cycle no drop ever breaks. An install wraps the
/// driver in an installation, and the core keeps the installation the
/// install returns for as long as it serves drives. A drive holds the driver
/// it runs on, never the installation, so a drive still in flight when its
/// core is dropped runs to its end on that core's driver without keeping the
/// install live: a core built meanwhile installs its own driver (FIG-4017).
/// A drive that runs while no live installation is held (before the core is
/// built, or after it was dropped) fails its attempt retryably, naming the
/// empty slot, and a later install serves it. Clones share one slot.
#[derive(Clone, Default)]
pub struct RestateSessionDriverSlot {
    installation: Arc<Mutex<Option<Weak<InstalledSessionDriver>>>>,
}

/// A driver as a [`RestateSessionDriverSlot`] installed it: what its core
/// keeps, and whose life decides whether the install is live. It answers
/// every call with the driver it wraps.
struct InstalledSessionDriver {
    driver: Arc<dyn SessionDriver>,
}

#[async_trait::async_trait]
impl SessionDriver for InstalledSessionDriver {
    fn owns_reconciliation(&self) -> bool {
        self.driver.owns_reconciliation()
    }

    fn runs_on(&self, driver: &dyn SessionDriver) -> bool {
        self.driver.runs_on(driver)
    }

    async fn reconcile(
        &self,
        cursor: &lash_core::engine::ReconcileCursor,
        page: std::num::NonZeroUsize,
    ) -> Result<lash_core::engine::ReconcileCursor, lash_core::StoreError> {
        self.driver.reconcile(cursor, page).await
    }

    fn hold_drive(&self, session: &SessionId) -> lash_core::engine::DriveHold {
        self.driver.hold_drive(session)
    }

    async fn admit(
        &self,
        controller: lash_core::ScopedEffectController<'_>,
        request: &DriveRequest,
        ordinal: u32,
    ) -> Result<AdmitVerdict, DriveAbort> {
        self.driver.admit(controller, request, ordinal).await
    }

    async fn run_root(
        &self,
        controller: lash_core::ScopedEffectController<'_>,
        admitted: Admitted,
    ) -> Result<RootOutcome, DriveAbort> {
        self.driver.run_root(controller, admitted).await
    }
}

impl RestateSessionDriverSlot {
    /// An empty slot.
    pub fn new() -> Self {
        Self::default()
    }

    /// Install `driver` unless a live installation is held already; returns
    /// the installation the slot now serves, which the caller keeps alive for
    /// as long as it serves drives.
    pub fn install(&self, driver: Arc<dyn SessionDriver>) -> Arc<dyn SessionDriver> {
        self.install_new(driver).0
    }

    /// [`Self::install`], also answering whether the slot took `driver` —
    /// false when a live installation was already held and is kept.
    fn install_new(&self, driver: Arc<dyn SessionDriver>) -> (Arc<dyn SessionDriver>, bool) {
        let mut slot = self
            .installation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(live) = slot.as_ref().and_then(Weak::upgrade) {
            return (live, false);
        }
        let installation = Arc::new(InstalledSessionDriver { driver });
        *slot = Some(Arc::downgrade(&installation));
        (installation, true)
    }

    /// The installed driver, if its installation is still held. The driver
    /// returned does not keep the installation live.
    pub fn installed(&self) -> Option<Arc<dyn SessionDriver>> {
        self.installation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .and_then(Weak::upgrade)
            .map(|installation| Arc::clone(&installation.driver))
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
    /// The namespace the deployment's session services are named in
    /// (FIG-3898).
    namespace: crate::RestateNamespace,
    control: Arc<dyn lash_core::engine::SessionControlEngine>,
    /// Every session's drive asks from this engine: what is in flight and
    /// what is queued behind it.
    asks: Arc<asks::DriveAsks>,
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
        namespace: crate::RestateNamespace,
        control: Arc<dyn lash_core::engine::SessionControlEngine>,
    ) -> Self {
        Self {
            ingress,
            slot,
            build_generation,
            namespace,
            control,
            asks: Arc::default(),
        }
    }

    /// The slot the deployment's session handlers read the driver from.
    pub fn driver_slot(&self) -> &RestateSessionDriverSlot {
        &self.slot
    }

    /// Send `request`'s drive to `LashSession/{session}`, keyed by the
    /// request id: a repeated send of one request attaches to its first
    /// invocation instead of driving twice. A transient send failure retries
    /// under the same idempotency key before the ask is given up to the
    /// ingress relay. Resolves once Restate accepted the send, not once the
    /// drive ran.
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
            .send_object_json_idempotent_bounded(
                &self.namespace.stable(LashService::SessionDriver).name(),
                session.as_str(),
                DRIVE_HANDLER,
                &body,
                request.as_str(),
            )
            .await
    }

    /// Send `request`'s drive to `LashSession_g<G>/{session}`: the resume of
    /// a drive pinned to drain generation `G` (FIG-3795). The request is
    /// stamped with `G`, which the resume-only lane holds it to, and the
    /// request id is the send's idempotency key under that service name, as
    /// on the stable lane. The drain sends it; a host never does — new work
    /// goes to the stable lane.
    pub async fn send_resume(
        &self,
        session: &SessionId,
        request: DriveRequestId,
        generation: &BuildGeneration,
    ) -> Result<crate::RestateInvocationId, crate::RestateHttpError> {
        let route = self
            .namespace
            .generation(LashService::SessionDriver, generation.clone());
        let body = RestateSessionDriveRequest {
            drive_version: LASH_SESSION_DRIVE_VERSION,
            request: DriveRequest {
                session: session.clone(),
                request: request.clone(),
                build_generation: generation.clone(),
            },
        };
        self.ingress
            .send_object_json_idempotent_bounded(
                &route.name(),
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
                &self.namespace.stable(LashService::SessionDriver).name(),
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
            .attach_workflow_run(&self.namespace.stable(LashService::TurnDriver).name(), &key)
            .await
        {
            Err(crate::RestateHttpError::Status { body, .. }) => decode_drive_refusal(&body),
            _ => None,
        }
    }

    /// One leg of `SessionWorkEngine::await_drive`: the attach, the
    /// released-root refusal read-back and the refusal decode.
    async fn attach_drive_leg(
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

    /// The leg a drive continues on after `leg` ended with `outcome`, when
    /// `leg` spent its root budget and handed off.
    fn continuation(
        &self,
        session: &SessionId,
        leg: &DriveRequestId,
        outcome: &DriveOutcome,
    ) -> Option<DriveRequestId> {
        let handed_off = matches!(outcome.stop, DriveStop::Yielded { .. })
            && outcome.ran.len() == MAX_ROOTS_PER_DRIVE;
        handed_off.then(|| {
            drive_continuation_request(&DriveRequest {
                session: session.clone(),
                request: leg.clone(),
                build_generation: self.build_generation.clone(),
            })
        })
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
        // Fire-and-forget: a drive the engine itself continues, never one an
        // admitted row owes (that one is `request_drive`'s, and the ingress
        // relay asks again for it).
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(
                session_id = session.as_str(),
                request = request.as_str(),
                "session drive not sent: scheduled outside a Tokio runtime"
            );
            return;
        };
        self.asks.join(self, &runtime, session, request);
    }

    /// Join `request` to the session's drive ([`asks`]). An ask that sends
    /// a drive answers once Restate accepted it, under the request's
    /// idempotency key; a send that did not reach Restate is retryable. An ask
    /// queued behind the drive in flight is accepted at once: the engine
    /// sends it once that drive ended. A repeated request joins the drive it
    /// joined first.
    async fn request_drive(
        &self,
        session: &SessionId,
        request: DriveRequestId,
    ) -> Result<(), lash_core::engine::EngineRefusal> {
        let refusal = |request: &DriveRequestId, error: &dyn std::fmt::Display| {
            lash_core::engine::EngineRefusal::Retryable(format!(
                "drive `{}` of session `{session}` was not accepted: {error}",
                request.as_str()
            ))
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            // No runtime to pump on: send this ask alone.
            return self
                .send_drive(session, request.clone())
                .await
                .map(|_| ())
                .map_err(|error| refusal(&request, &error));
        };
        let joined = self.asks.join(self, &runtime, session, request);
        if joined.queued {
            return Ok(());
        }
        match joined.drive.sent().await {
            asks::Sent::Accepted => Ok(()),
            asks::Sent::Failed(error) => Err(refusal(joined.drive.request(), &error)),
        }
    }

    fn install_session_driver(&self, driver: Arc<dyn SessionDriver>) -> Arc<dyn SessionDriver> {
        // One recovery interval per installed driver: a re-install that
        // keeps the live driver starts none, and the interval of a dropped
        // driver ends at its next tick.
        let (installed, new) = self.slot.install_new(driver);
        if !new || !installed.owns_reconciliation() {
            return installed;
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(crate::session_reconcile::run(Arc::downgrade(&installed)));
        }
        installed
    }

    /// Attach to the drive `request` joined ([`asks`]) once the engine sent
    /// it. When this engine holds no ask of `request`, attach to `request`'s
    /// own drive by its idempotency key, starting it if no send reached
    /// Restate. A drive a handler refused terminally is decoded
    /// back to the kernel's refusal; any other failure to attach (transport,
    /// the attach ceiling) is a retry under the same key.
    ///
    /// A drive that spent its per-invocation root budget yields and continues
    /// on the derived continuation request: the waiter attaches to each leg
    /// in turn and answers only once the chain ends, with every leg's roots.
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
        let mut leg = match self.asks.joined(session, request) {
            Some(drive) => match drive.sent().await {
                asks::Sent::Accepted => drive.request().clone(),
                asks::Sent::Failed(error) => {
                    return Err(DriveAbort::Retry(lash_core::RuntimeError::new(
                        lash_core::RuntimeErrorCode::EngineTurnTerminalAttach,
                        format!(
                            "drive `{}` of session `{session}` was not sent: {error}",
                            drive.request().as_str()
                        ),
                    )));
                }
            },
            None => request.clone(),
        };
        let mut ran = Vec::new();
        loop {
            let outcome = self.attach_drive_leg(session, &leg).await?;
            let next = self.continuation(session, &leg, &outcome);
            ran.extend(outcome.ran);
            match next {
                Some(next) => leg = next,
                None => {
                    return Ok(DriveOutcome {
                        ran,
                        stop: outcome.stop,
                    });
                }
            }
        }
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
    /// The lane this instance serves: the binder serves one per lane of the
    /// pinned `LashSession` (FIG-3795).
    route: crate::services::ServiceRoute,
}

/// The `LashTurn` workflow over the deployment's driver slot, journaling
/// under the deployment's drain generation.
#[derive(Clone)]
pub(crate) struct LashTurnImpl {
    slot: RestateSessionDriverSlot,
    authority_id: RestateAuthorityId,
    build_generation: BuildGeneration,
    /// The lane this instance serves: the binder serves one per lane of the
    /// pinned `LashTurn` (FIG-3795).
    route: crate::services::ServiceRoute,
}

impl LashSessionImpl {
    pub(crate) fn new(
        slot: RestateSessionDriverSlot,
        authority_id: RestateAuthorityId,
        build_generation: BuildGeneration,
        namespace: &crate::RestateNamespace,
    ) -> Self {
        Self {
            slot,
            authority_id,
            build_generation,
            route: namespace.stable(LashService::SessionDriver),
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
        slot: RestateSessionDriverSlot,
        authority_id: RestateAuthorityId,
        build_generation: BuildGeneration,
        namespace: &crate::RestateNamespace,
    ) -> Self {
        Self {
            slot,
            authority_id,
            build_generation,
            route: namespace.stable(LashService::TurnDriver),
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

/// The terminal refusal of a request stamped for another generation. It is
/// returned before the handler journals anything.
fn retired_generation(service: LashService, found: u32) -> HandlerError {
    drive_refusal(&lash_core::RuntimeError::new(
        lash_core::RuntimeErrorCode::ExecutionScopeAdmissionRefused,
        format!(
            "{} request carries lash-session-drive-v{found}; this handler journals generation \
             {LASH_SESSION_DRIVE_VERSION}",
            service.base_name()
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

/// The typed refusal of a request a generation lane does not serve (FIG-3795,
/// law L11): it names another generation than the lane's, or none. Nothing
/// is journaled and nothing is stored: a misroute is the sender's error,
/// never the drive's outcome.
fn misrouted(route: &crate::services::ServiceRoute, detail: &str) -> HandlerError {
    drive_refusal(&lash_core::RuntimeError::new(
        lash_core::RuntimeErrorCode::ExecutionScopeAdmissionRefused,
        format!("misrouted: {route} serves only its own generation's work; {detail}"),
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
            &self.route,
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
            &self.route,
            ctx,
            input.sender_generation.as_ref(),
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
    route: &crate::services::ServiceRoute,
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
    // The generation lane is resume-only (FIG-3795): it serves a drive whose
    // request was stamped for exactly this generation. A request naming
    // another generation, sent there by error, is refused before any command
    // — it is the sender's error, journaled nowhere.
    if let crate::services::Lane::Generation(lane) = route.lane()
        && request.build_generation != *lane
    {
        return Err(misrouted(
            route,
            &format!(
                "drive `{}` of session `{}` was stamped for generation `{}`",
                request.request.as_str(),
                request.session,
                request.build_generation
            ),
        ));
    }
    let handler = route.namespace().stable(LashService::SessionDriver).name();
    let driver = slot.driver_for(&handler)?;
    // This attempt's admissions, and the roots it calls when they run in this
    // process, share one runtime of the session (FIG-3825); the hold drops
    // where the attempt ends, so a replaying attempt opens its own.
    let _hold = driver.hold_drive(&request.session);
    // The generation sentinel rides admission 0, the drive's first command
    // (FIG-3980): a journal of another build parks before it replays past it.
    let sentinel = Arc::new(FoldedSentinel::new(handler, generation.clone()));
    let controller = RestateRuntimeEffectController::new(ctx, authority_id.clone())
        .in_namespace(route.namespace().clone())
        .with_build_generation(generation.clone())
        .with_folded_sentinel(Arc::clone(&sentinel));
    sentinel
        .guard(drive_admissions(
            driver.as_ref(),
            &controller,
            generation,
            route,
            request,
        ))
        .await?
}

/// Admission `n`, then the admitted root's `LashTurn`, until admission
/// answers anything but an admitted root.
async fn drive_admissions(
    driver: &dyn SessionDriver,
    controller: &RestateRuntimeEffectController<'_, ObjectContext<'_>>,
    generation: &BuildGeneration,
    route: &crate::services::ServiceRoute,
    request: DriveRequest,
) -> Result<DriveOutcome, HandlerError> {
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
                // A newly admitted root is new work: its `LashTurn` goes to
                // the stable lane, which Restate hands to the newest build
                // (FIG-3795), and its outcome reads back under the same route.
                let outcome = match crate::services::routed_workflow::<_, _, RootOutcome>(
                    controller.context(),
                    &route.namespace().stable(LashService::TurnDriver),
                    key.clone(),
                    "run",
                    RestateTurnDriveRequest {
                        drive_version: LASH_SESSION_DRIVE_VERSION,
                        sender_generation: Some(generation.clone()),
                        admitted,
                    },
                )
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
                        let recorded =
                            crate::services::routed_workflow::<_, (), Option<RootOutcome>>(
                                controller.context(),
                                &route.namespace().stable(LashService::TurnDriver),
                                key,
                                "outcome",
                                (),
                            )
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
                let yielded_root = outcome.root().clone();
                ran.push(outcome);
                if let Some(stop) = stop {
                    return Ok(DriveOutcome { ran, stop });
                }
                if ran.len() == MAX_ROOTS_PER_DRIVE {
                    // The send is a journaled Restate command. Its request is
                    // distinct from this invocation and queues behind this
                    // object's exclusive handler before we return. The
                    // request id is the send's idempotency key, so a waiter
                    // that attaches under it joins this invocation rather
                    // than starting a second one.
                    let continuation = DriveRequest {
                        session: request.session.clone(),
                        request: drive_continuation_request(&request),
                        build_generation: request.build_generation.clone(),
                    };
                    let continuation_id = continuation.request.as_str().to_owned();
                    // The continuation is this same drive yielding: it goes
                    // to this invocation's lane, so a drive resumed on
                    // `_g<G>` stays under the generation its journal family
                    // belongs to. On the stable lane this is the stable name.
                    crate::services::routed_object::<_, _, ()>(
                        controller.context(),
                        route,
                        request.session.as_str().to_owned(),
                        "drive",
                        RestateSessionDriveRequest {
                            drive_version: LASH_SESSION_DRIVE_VERSION,
                            request: continuation,
                        },
                    )
                    .idempotency_key(continuation_id)
                    .send()
                    .await?;
                    return Ok(DriveOutcome {
                        ran,
                        stop: DriveStop::Yielded { root: yielded_root },
                    });
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
    route: &crate::services::ServiceRoute,
    ctx: WorkflowContext<'_>,
    sender_generation: Option<&BuildGeneration>,
    admitted: Admitted,
) -> Result<RootOutcome, HandlerError> {
    let expected = turn_workflow_key(admitted.session(), admitted.root());
    if ctx.key() != expected {
        return Err(misaddressed(format!(
            "LashTurn/{} was asked to run root `{expected}`",
            ctx.key()
        )));
    }
    // The generation lane serves a root the latest build refused, re-sent by
    // the drain under the generation the drive that admitted it ran on
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
                "root `{}` of session `{}` was sent by {sender}",
                admitted.root(),
                admitted.session()
            ),
        ));
    }
    let handler = route.namespace().stable(LashService::TurnDriver).name();
    let driver = slot.driver_for(&handler)?;
    // The generation sentinel rides the root's first recorded step, its start
    // marker (FIG-3980): a journal of another build parks before it replays
    // past it.
    let sentinel = Arc::new(FoldedSentinel::new(handler, generation.clone()));
    let controller = RestateRuntimeEffectController::new(ctx, authority_id.clone())
        .in_namespace(route.namespace().clone())
        .with_build_generation(generation.clone())
        .with_folded_sentinel(Arc::clone(&sentinel));
    let scoped = controller
        .scoped_effect_controller(drive_root_scope(admitted.session(), admitted.root()))
        .map_err(refused_scope)?;
    let root = admitted.root().clone();
    let (ended, result) = match sentinel.guard(driver.run_root(scoped, admitted)).await? {
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

    struct IdleDriver;

    #[async_trait::async_trait]
    impl SessionDriver for IdleDriver {
        async fn admit(
            &self,
            _controller: lash_core::ScopedEffectController<'_>,
            _request: &DriveRequest,
            _ordinal: u32,
        ) -> Result<AdmitVerdict, DriveAbort> {
            unreachable!("the slot law runs no drive")
        }

        async fn run_root(
            &self,
            _controller: lash_core::ScopedEffectController<'_>,
            _admitted: Admitted,
        ) -> Result<RootOutcome, DriveAbort> {
            unreachable!("the slot law runs no drive")
        }
    }

    /// FIG-4017: the installation a core keeps decides whether an install
    /// holds, not the driver a drive still runs on. While the first core
    /// keeps its installation, a second install is served the first driver;
    /// once the first core drops it, a drive still holding the first driver
    /// does not stop the next install from taking its own.
    #[test]
    fn a_driver_a_drive_still_holds_does_not_keep_its_dropped_installation() {
        let slot = RestateSessionDriverSlot::new();
        let first: Arc<dyn SessionDriver> = Arc::new(IdleDriver);
        let second: Arc<dyn SessionDriver> = Arc::new(IdleDriver);
        let first_installation = slot.install(Arc::clone(&first));
        assert!(first_installation.runs_on(first.as_ref()));
        let in_flight = slot.driver_for("drive").expect("the first driver serves");
        assert!(
            Arc::ptr_eq(&in_flight, &first),
            "a drive runs on the driver"
        );
        let kept = slot.install(Arc::clone(&second));
        assert!(
            kept.runs_on(first.as_ref()),
            "a live installation keeps serving the first driver"
        );
        drop((kept, first_installation));

        let second_installation = slot.install(Arc::clone(&second));
        assert!(
            second_installation.runs_on(second.as_ref()),
            "the second install takes its own driver while a drive still holds the first"
        );
        let next = slot.driver_for("drive").expect("the second driver serves");
        assert!(Arc::ptr_eq(&next, &second));
        drop(in_flight);
    }

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

    /// D15: the admission refusal a drive answers while a park names an
    /// unsettled redrive is retryable, so its decode classifies it as a
    /// retry — never a refused (failed-turn) row — and the handler failure
    /// it becomes keeps the attempt open.
    #[test]
    fn an_unsettled_redrive_refusal_is_classified_retry_not_refused() {
        let refusal = lash_core::RuntimeError::new(
            lash_core::RuntimeErrorCode::SessionRedriveUnsettled,
            "the parked root's redrive has not settled",
        );
        assert!(refusal.is_retryable());
        assert!(!refusal.is_terminal());
        assert!(matches!(
            classify_refusal(refusal.clone()),
            DriveAbort::Retry(_)
        ));
        let failure = abort_failure(DriveAbort::Refused(refusal));
        assert!(
            format!("{failure:?}").contains("Retryable"),
            "an unsettled-redrive refusal must retry, not end the drive: {failure:?}"
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
    async fn a_transient_schedule_failure_retries_the_same_drive_request() {
        let transport = Arc::new(Scripted {
            requests: std::sync::Mutex::default(),
            responses: std::sync::Mutex::new(
                [
                    scripted_response(503, "temporary outage".to_string()),
                    scripted_response(
                        202,
                        serde_json::json!({
                            "invocationId": "accepted-drive",
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
            RestateSessionDriverSlot::new(),
            BuildGeneration::for_test("t0"),
            crate::RestateNamespace::default(),
            Arc::new(lash_core::engine::NoEngineControl),
        );
        work.schedule_drive(
            &SessionId::from("retry-session"),
            DriveRequestId::new("retry"),
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
        // the drive ended (FIG-4036); the first two requests are the sends.
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
            RestateSessionDriverSlot::new(),
            BuildGeneration::for_test("t0"),
            crate::RestateNamespace::default(),
            Arc::new(lash_core::engine::NoEngineControl),
        );
        work.schedule_drive(
            &SessionId::from("retry-session"),
            DriveRequestId::new("retry"),
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
            crate::RestateNamespace::default(),
            Arc::new(lash_core::engine::NoEngineControl),
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
            crate::RestateNamespace::default(),
            Arc::new(lash_core::engine::NoEngineControl),
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
}
