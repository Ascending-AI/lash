//! The scope-close seam (FIG-3607 item 7, R10): what a logical root's end
//! and a session's close tell the owner of lifetime scopes.
//!
//! A root's `Turn(root)` scope, and a session's `Session` scope, own work
//! that outlives a single step (processes started under them, among others).
//! The drive calls this seam only after the end is durable: a root's terminal
//! evidence, or a session's `CloseSession` intent. It names no engine and no
//! registry: the process registry's scope-close adapter implements it, and
//! [`NoScopeClose`] stands in until one is installed.

use crate::store::{ControlIntentId, EnginePark, ParkId, ParkReason, RootTerminal, StoreError};
use std::num::NonZeroUsize;

use serde::{Deserialize, Serialize};

use crate::{SessionId, TurnId};

/// Where the drive reports a closed root scope or session scope.
///
/// Guarantees a caller of this trait keeps:
///
/// - it is called only after the root's terminal evidence (or the session's
///   close intent) is durable;
/// - it is called at least once per terminal root: a crash between the
///   evidence and the call re-runs the recorded step that calls it;
/// - it is never called for a parked root, which holds its scope open.
///
/// An implementor must be idempotent per `(session, root)` and per intent.
#[async_trait::async_trait]
pub trait ScopeCloseSink: Send + Sync {
    /// Whether this sink owns any scope. The drive records a root's close
    /// only for a sink that does: with no owner there is nothing to close,
    /// so no step is recorded and a root's journal ends at its commit.
    fn owns_scopes(&self) -> bool {
        true
    }

    /// Close `Turn(root)` after its terminal evidence.
    async fn close_root_scope(&self, terminal: &RootTerminal) -> Result<(), StoreError>;

    /// Close `Session(session)` and the listed roots after its
    /// `CloseSession` intent.
    ///
    /// Closing the session closes every scope inside it: a closed session
    /// admits no root, so a turn id it never admitted can no longer become
    /// one, and no start may name it any more (FIG-3948).
    async fn close_session_scope(
        &self,
        session: &SessionId,
        intent: ControlIntentId,
        roots: &[TurnId],
    ) -> Result<(), StoreError>;
}

/// The sink of a host that installed no scope owner: closing is a no-op.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoScopeClose;

#[async_trait::async_trait]
impl ScopeCloseSink for NoScopeClose {
    fn owns_scopes(&self) -> bool {
        false
    }

    async fn close_root_scope(&self, _terminal: &RootTerminal) -> Result<(), StoreError> {
        Ok(())
    }

    async fn close_session_scope(
        &self,
        _session: &SessionId,
        _intent: ControlIntentId,
        _roots: &[TurnId],
    ) -> Result<(), StoreError> {
        Ok(())
    }
}

/// The replay key of a session's `BeginSessionClose` step, inside its
/// `SessionDelete` scope: one close per session, so every retry of the
/// deletion replays the recorded close.
#[must_use]
pub fn begin_session_close_replay_key(session: &SessionId) -> String {
    format!("{}:begin-close", session.as_str())
}

/// One logical root, as an engine's control verbs address it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RootRef {
    pub session: SessionId,
    pub root: TurnId,
}

/// An open logical root as the store's recovery page lists it
/// ([`DeploymentStore::non_terminal_roots_page`](crate::DeploymentStore::non_terminal_roots_page)),
/// with the execution its recorded admission names (FIG-4403).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenRoot {
    pub target: RootRef,
    /// The executor the root's admission recorded; `None` while the root
    /// has recorded no admission.
    pub executor: Option<crate::store::RootExecutor>,
}

/// The engine's evidence that an open root's execution is lost, which
/// [`DeploymentStore::end_lost_root`](crate::DeploymentStore::end_lost_root)
/// ends the root on (ADR 0104 O2, O6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootRunLoss {
    /// The root's workflow run ended with a failure and recorded no
    /// outcome: an operator's kill, or a refusal that ended nothing. The
    /// engine never runs that key again, so the root ends whether or not it
    /// had recorded its admission.
    FailedRun,
    /// No execution holds the root: the engine holds no run of the root's
    /// key on any generation lane (the run was purged or its history lost),
    /// and the execution its admission recorded runs nothing more
    /// ([`RootExecutor`](crate::store::RootExecutor)). A root that recorded its admission
    /// started, and its effects may have run, so it ends: a fresh execution
    /// must never run it again (ADR 0105 L-S8). A root that never recorded
    /// its admission started nothing; its input is still owed by its ingress
    /// obligation, which drives it, so the store leaves it open.
    NoRun,
}

/// What an engine did for a control verb.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum EngineAck {
    /// The engine resumed the execution holding the root.
    Resumed,
    /// The engine stopped the root's execution for good.
    Released,
    /// The engine held no execution for the root: nothing to do.
    NothingHeld,
}

/// Whether the identical ask may succeed when it is made again.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RefusalClass {
    /// A fault of this attempt: the obligation is retried after a backoff.
    Retryable,
    /// The engine's answer to the ask: making it again is refused the same
    /// way.
    Permanent,
}

/// Why an engine could not carry out a control verb or accept a drive: its
/// retry class beside the typed code of its cause. A retryable refusal is
/// retained on the `ControlIntent` or ingress obligation and retried after a
/// backoff (ADR 0109); a permanent one refuses the intent and stalls the
/// obligation, visible to an operator under its code.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct EngineRefusal {
    pub disposition: RefusalClass,
    pub code: crate::RuntimeErrorCode,
    pub message: String,
}

impl EngineRefusal {
    /// A fault of this attempt under `code`.
    #[must_use]
    pub fn retryable(code: crate::RuntimeErrorCode, message: impl Into<String>) -> Self {
        Self {
            disposition: RefusalClass::Retryable,
            code,
            message: message.into(),
        }
    }

    /// The engine's answer for good under `code`.
    #[must_use]
    pub fn permanent(code: crate::RuntimeErrorCode, message: impl Into<String>) -> Self {
        Self {
            disposition: RefusalClass::Permanent,
            code,
            message: message.into(),
        }
    }

    /// Whether the ask is worth another attempt.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        self.disposition == RefusalClass::Retryable
    }

    /// The cause as an obligation row and a refused intent retain it.
    #[must_use]
    pub fn into_delivery_error(self) -> crate::store::DeliveryError {
        crate::store::DeliveryError::new(self.code, self.message)
    }
}

/// A store's answer to an engine's control read or write: a fault of the
/// substrate is retryable, every other variant is the store's refusal.
impl From<StoreError> for EngineRefusal {
    fn from(error: StoreError) -> Self {
        let disposition = if error.is_transient() {
            RefusalClass::Retryable
        } else {
            RefusalClass::Permanent
        };
        let crate::store::DeliveryError { code, message } = error.into();
        Self {
            disposition,
            code,
            message,
        }
    }
}

/// A process registry's answer to an engine's control verb, by the plugin
/// error's own class: only a terminal one is the registry's refusal.
impl From<crate::PluginError> for EngineRefusal {
    fn from(error: crate::PluginError) -> Self {
        let disposition = if error.is_terminal() {
            RefusalClass::Permanent
        } else {
            RefusalClass::Retryable
        };
        let message = error.to_string();
        Self {
            disposition,
            code: crate::RuntimeEffectControllerError::from(error).code,
            message,
        }
    }
}

/// The engine half of the control verbs (ADR 0104 O3/O4, FIG-3600 S7): what
/// an engine does to its executions after the store recorded an intent. It
/// names no engine; each engine implements it over its own executions.
///
/// Every method is idempotent: an intent's obligation relay re-delivers an
/// intent whose acknowledgement a crash lost.
#[async_trait::async_trait]
pub trait SessionControlEngine: Send + Sync {
    /// O3: the engine's stalled work becomes lash parks. Every execution the
    /// engine stopped retrying is recorded through `parks` with
    /// [`ParkReason::EngineRetryExhausted`] and the engine's handle; one
    /// whose target already ended is released instead. A stalled session
    /// drive is parked on its session's next root; one that stopped only
    /// behind a redrive that has since settled is resumed, and one whose
    /// session's next work names no root is released (ADR 0109 §3). Stalled
    /// work a root waits on is parked on that root, which records the work's
    /// handle; stalled work nothing waits for any more is released.
    ///
    /// Each recovery catalog inspects at most `page.limit` records under
    /// `page.budget`. The stalled-work listing resumes after `page.after`
    /// in the engine's order; the report's `next` continues it, and `None`
    /// wraps it. An engine that also repairs lost runs keeps independent
    /// process and root cursors across calls. Failed items advance their
    /// cursor and are retried when that catalog wraps.
    /// Idempotent: a second pass over the same execution records nothing new.
    async fn reconcile_parks(
        &self,
        parks: &dyn ParkRecoveryWriter,
        page: EnginePage,
    ) -> Result<ParkReconcileReport, EngineRefusal>;

    /// O4 redrive: resume the execution holding the root's park, and the
    /// stopped work `children` names: the handles the root's park recorded
    /// (FIG-4630). The engine resumes exactly those and looks for no other
    /// work of the root, the session or anyone else. An engine holding none
    /// answers [`EngineAck::NothingHeld`], and the caller schedules a drive
    /// instead.
    async fn resume_root(
        &self,
        target: &RootRef,
        engine: Option<&EnginePark>,
        children: &[EnginePark],
    ) -> Result<EngineAck, EngineRefusal>;

    /// Resume a process through the engine's existing process control path.
    /// The park identity is checked before the engine resumes the invocation.
    async fn resume_process(
        &self,
        _process: &crate::ProcessId,
        _park: ParkId,
    ) -> Result<EngineAck, EngineRefusal> {
        Err(EngineRefusal::permanent(
            crate::RuntimeErrorCode::EngineControlUnsupported,
            "process redrive is not supported by this engine",
        ))
    }

    /// O4 release: stop the root's execution for good, AFTER the store
    /// recorded the root's terminal evidence. Never proof of a lash outcome
    /// (ADR 0104 O4): the evidence is the store's.
    async fn release_root(
        &self,
        target: &RootRef,
        engine: Option<&EnginePark>,
    ) -> Result<EngineAck, EngineRefusal>;
}

/// The control engine of an engine that holds no executions across calls
/// (a store-only backend's `NoSessionWork`, or a test double): there is
/// nothing to release.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoEngineControl;

#[async_trait::async_trait]
impl SessionControlEngine for NoEngineControl {
    async fn reconcile_parks(
        &self,
        _parks: &dyn ParkRecoveryWriter,
        _page: EnginePage,
    ) -> Result<ParkReconcileReport, EngineRefusal> {
        Ok(ParkReconcileReport::default())
    }

    async fn resume_root(
        &self,
        _target: &RootRef,
        _engine: Option<&EnginePark>,
        _children: &[EnginePark],
    ) -> Result<EngineAck, EngineRefusal> {
        Ok(EngineAck::NothingHeld)
    }

    async fn release_root(
        &self,
        _target: &RootRef,
        _engine: Option<&EnginePark>,
    ) -> Result<EngineAck, EngineRefusal> {
        Ok(EngineAck::NothingHeld)
    }
}

/// An engine's opaque position in its own listing of stalled executions:
/// what [`EnginePage::after`] resumes from. Meaningful only to the engine
/// that issued it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EngineCursor(String);

impl EngineCursor {
    /// The engine's position `value`.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The position as the engine wrote it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One bounded page of an engine's stalled-work listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnginePage {
    /// Resume strictly after this position; `None` starts at the beginning.
    pub after: Option<EngineCursor>,
    /// Read at most this many executions.
    pub limit: NonZeroUsize,
    /// Time available to each independent recovery page, including its
    /// store read and engine requests.
    pub budget: std::time::Duration,
}

/// The work a park holds, as the engine names it to the park writer.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ParkTarget {
    /// A session's logical root.
    Root { session: SessionId, root: TurnId },
    /// A process.
    Process { process: crate::ProcessId },
    /// A session's drive, stopped in its admission before it ran a root: it
    /// is parked on the root the session's next admission names (ADR 0109
    /// §3), and only that park's operator verb resumes it.
    Drive { session: SessionId },
    /// Work a session's logical root waits on, stopped in an execution of
    /// its own (a tool attempt's child, FIG-4607). The root's own execution
    /// is not stopped: it waits for the child. The child is parked on the
    /// root, whose park records the child's engine handle beside those of
    /// the root's other stopped children (FIG-4630), and the park's redrive
    /// resumes exactly the recorded ones.
    RootChild { session: SessionId, root: TurnId },
}

/// What [`ParkRecoveryWriter::record_engine_park`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EngineParkRecorded {
    /// A new park, with reason `EngineRetryExhausted`.
    Parked(ParkId),
    /// The target already held a park: it keeps its reason and now carries
    /// the engine's handle. A park that already carried the same handle is
    /// left as it is.
    AttachedToExisting(ParkId),
    /// The target already has terminal evidence: the engine should release
    /// its execution.
    TargetTerminal,
    /// The target is gone (its session deleted, root input withdrawn, or process pruned): the
    /// engine should release its execution.
    TargetGone,
    /// A redrive owns the execution: it resumed it after the engine listed
    /// it as stopped, or is about to. Nothing was written; a later pass
    /// re-lists the execution if it stops again.
    Redriven,
    /// A stopped drive whose session's next work names no root — only
    /// queued commands, a closing session's in-flight claim, or nothing: no
    /// operator verb could resume it, so nothing was written and the engine
    /// releases the drive. What the session still holds keeps its own
    /// ingress obligation, whose relay asks for a fresh drive: the command
    /// lane drives the session again (ADR 0109 §3).
    NothingToPark,
    /// A stopped drive whose every attempt was refused only because its
    /// session's park named a redrive that had not settled (D15), and that
    /// redrive has since settled: the engine resumes the drive. Nothing was
    /// written; the redrive already was the operator's action.
    ResumeDrive,
}

/// The engine's live view of the one stalled execution a
/// [`ParkRecoveryWriter::record_engine_park`] call records.
///
/// An engine lists its stopped executions before the writer reads the
/// target's park, so a redrive can resume an execution in between. When the
/// park names a redrive that already resumed the execution, the writer asks
/// again after that read: an execution still stopped then stopped again
/// after the redrive, and one that is not is running.
#[async_trait::async_trait]
pub trait StalledExecution: Send + Sync {
    /// Whether the execution is stopped now.
    async fn still_stopped(&self) -> Result<bool, EngineRefusal>;
}

/// The park writer an engine's reconcile records stalled work through: the
/// recovery path for a park the execution could not record itself, because
/// the engine stopped running it before it reached its own park step.
/// Reconciliation is its only caller (ADR 0105 §9).
///
/// Its writes converge with the execution's own park write: both key the
/// park by its target, so a divergence park the execution recorded first
/// keeps its reason and gains the engine's handle.
#[async_trait::async_trait]
pub trait ParkRecoveryWriter: Send + Sync {
    /// Park `target` for `reason`, carrying the engine's `engine` handle. A
    /// [`ParkTarget::Drive`] park stores no handle: the engine finds a
    /// stopped drive by its session. A [`ParkTarget::RootChild`] park records
    /// the handle as one of the root's stopped children.
    /// `execution` re-reads the stalled execution when a redrive may have
    /// resumed it since the engine listed it.
    async fn record_engine_park(
        &self,
        target: &ParkTarget,
        reason: ParkReason,
        engine: EnginePark,
        execution: &dyn StalledExecution,
    ) -> Result<EngineParkRecorded, StoreError>;
}

/// What one [`SessionControlEngine::reconcile_parks`] pass did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParkReconcileReport {
    /// Targets this pass parked.
    pub parked: Vec<ParkTarget>,
    /// Stalled executions whose target already held a park.
    pub attached: usize,
    /// Roots whose execution this pass released because the store had
    /// already ended them.
    pub released: Vec<RootRef>,
    /// Stalled work this pass released because nothing waits for it any
    /// more, by its engine handle: a group child whose position its group
    /// already seated, or work of a retired group (FIG-4630).
    pub released_work: Vec<EnginePark>,
    /// Sessions whose stopped drive this pass released: the session is
    /// gone, or its next work names no root to park on, and its ingress
    /// obligations ask for a fresh drive.
    pub released_drives: Vec<SessionId>,
    /// Processes this pass ended `SubstrateLost` because the engine had
    /// finished their current segment's execution without their terminal (an
    /// operator's kill): nothing would ever run them again.
    pub ended_processes: Vec<crate::ProcessId>,
    /// Roots this pass ended `SubstrateLost` because the engine lost their
    /// execution ([`RootRunLoss`]): every run of the root failed without a
    /// Lash terminal, or the engine holds no run of a root that started.
    /// Their scope close is now owed by the terminal row.
    pub ended_roots: Vec<RootRef>,
    /// Sessions whose stopped drive this pass resumed: it stopped only behind
    /// a redrive that has since settled (D15). Any other stopped drive is
    /// never resumed here (ADR 0109 §3): it is parked, and only the park's
    /// operator verb resumes it.
    pub resumed_drives: Vec<SessionId>,
    /// Stalled executions this pass left as they were.
    pub unchanged: usize,
    /// Stalled executions this pass could not settle, each with why: one
    /// failure never fails the page, and a later pass that reaches the
    /// execution again retries it.
    pub failed: Vec<(EngineCursor, String)>,
    /// Where the next pass resumes; `None` when this one read to the end.
    pub next: Option<EngineCursor>,
}
