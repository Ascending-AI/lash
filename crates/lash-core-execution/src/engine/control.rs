//! The scope-close seam (FIG-3607 item 7, R10): what a logical run's end
//! and a session's close tell the owner of lifetime scopes.
//!
//! A run's `Turn(run)` scope, and a session's `Session` scope, own work
//! that outlives a single step (processes started under them, among others).
//! The shift calls this seam only after the end is durable: a run's terminal
//! evidence, or a session's `CloseSession` intent. It names no engine and no
//! registry: the process registry's scope-close adapter implements it, and
//! [`NoScopeClose`] stands in until one is installed.

use crate::store::{ControlIntentId, EnginePark, ParkId, ParkReason, RunTerminal, StoreError};
use std::num::NonZeroUsize;

use serde::{Deserialize, Serialize};

use crate::{SessionId, TurnId};

/// Where the shift reports a closed run scope or session scope.
///
/// Guarantees a caller of this trait keeps:
///
/// - it is called only after the run's terminal evidence (or the session's
///   close intent) is durable;
/// - it is called at least once per terminal run: a crash between the
///   evidence and the call re-runs the recorded step that calls it;
/// - it is never called for a parked run, which holds its scope open.
///
/// An implementor must be idempotent per `(session, run)` and per intent.
#[async_trait::async_trait]
pub trait ScopeCloseSink: Send + Sync {
    /// Whether this sink owns any scope. The shift records a run's close
    /// only for a sink that does: with no owner there is nothing to close,
    /// so no step is recorded and a run's journal ends at its commit.
    fn owns_scopes(&self) -> bool {
        true
    }

    /// Close `Turn(run)` after its terminal evidence.
    async fn close_run_scope(&self, terminal: &RunTerminal) -> Result<(), StoreError>;

    /// Close `Session(session)` and the listed runs after its
    /// `CloseSession` intent.
    ///
    /// Closing the session closes every scope inside it: a closed session
    /// admits no run, so a turn id it never admitted can no longer become
    /// one, and no start may name it any more (FIG-3948).
    async fn close_session_scope(
        &self,
        session: &SessionId,
        intent: ControlIntentId,
        runs: &[TurnId],
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

    async fn close_run_scope(&self, _terminal: &RunTerminal) -> Result<(), StoreError> {
        Ok(())
    }

    async fn close_session_scope(
        &self,
        _session: &SessionId,
        _intent: ControlIntentId,
        _runs: &[TurnId],
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

/// One logical run, as an engine's control verbs address it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RunRef {
    pub session: SessionId,
    pub run: TurnId,
}

/// An open logical run as the store's recovery page lists it
/// ([`DeploymentStore::non_terminal_runs_page`](crate::DeploymentStore::non_terminal_runs_page)),
/// with the execution its recorded admission names (FIG-4403).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenRun {
    pub target: RunRef,
    /// The executor the run's admission recorded; `None` while the run
    /// has recorded no admission.
    pub executor: Option<crate::store::RunExecutor>,
}

/// The engine's evidence that an open run's execution is lost, which
/// [`DeploymentStore::end_lost_run`](crate::DeploymentStore::end_lost_run)
/// ends the run on (ADR 0104 O2, O6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunLoss {
    /// The run's workflow run ended with a failure and recorded no
    /// outcome: an operator's kill, or a refusal that ended nothing. The
    /// engine never runs that key again, so the run ends whether or not it
    /// had recorded its admission.
    FailedRun,
    /// No execution holds the run: the engine holds no execution of the run's
    /// key on any generation lane (the run was purged or its history lost),
    /// and the execution its admission recorded runs nothing more
    /// ([`RunExecutor`](crate::store::RunExecutor)). A run that recorded its admission
    /// started, and its effects may have run, so it ends: a fresh execution
    /// must never run it again (ADR 0105 L-S8). A run that never recorded
    /// its admission started nothing; its input is still owed by its ingress
    /// obligation, which executes it, so the store leaves it open.
    NoRun,
}

/// What an engine did for a control verb.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum EngineAck {
    /// The engine resumed the execution holding the run.
    Resumed,
    /// The engine stopped the run's execution for good.
    Released,
    /// The engine held no execution for the run: nothing to do.
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

/// Why an engine could not carry out a control verb or accept a shift: its
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

/// A store's answer keeps the retry class of its canonical runtime code.
impl From<StoreError> for EngineRefusal {
    fn from(error: StoreError) -> Self {
        let disposition = if error.runtime_code().is_retryable() {
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

/// A plugin's answer keeps its class. A redrive needs fresh authority, so
/// repeating the identical control request cannot repair it.
impl From<crate::PluginError> for EngineRefusal {
    fn from(error: crate::PluginError) -> Self {
        let disposition = match error.class() {
            crate::PluginErrorClass::Retryable => RefusalClass::Retryable,
            crate::PluginErrorClass::Redrivable | crate::PluginErrorClass::Terminal => {
                RefusalClass::Permanent
            }
        };
        let message = error.to_string();
        Self {
            disposition,
            code: crate::RuntimeEffectControllerError::from(error).code,
            message,
        }
    }
}

#[cfg(test)]
mod refusal_classification_tests {
    use super::*;

    #[test]
    fn engine_refusal_retries_only_retryable_plugin_operations() {
        for (error, plugin_error) in StoreError::samples_for_testing()
            .into_iter()
            .zip(StoreError::samples_for_testing())
        {
            let code = error.runtime_code();
            let plugin = crate::PluginError::from(plugin_error);
            let plugin_code = crate::RuntimeEffectControllerError::from(plugin.clone()).code;
            let through_plugin = EngineRefusal::from(plugin);
            let refusal = EngineRefusal::from(error);
            assert_eq!(refusal.code, code);
            assert_eq!(refusal.is_retryable(), code.is_retryable(), "{refusal:?}");
            assert_eq!(refusal.disposition, through_plugin.disposition);
            assert_eq!(through_plugin.code, plugin_code);
        }
        for plugin in [
            crate::PluginError::from(StoreError::Contended),
            crate::PluginError::from(StoreError::StoredDataCorrupt {
                record_kind: "process record",
                message: "invalid JSON".into(),
            }),
            crate::PluginError::SessionExecutionLeaseLost {
                session_id: SessionId::fixture("lost-authority"),
            },
            crate::PluginError::ProcessExecutionSuperseded {
                process_id: crate::process_id_for_test("superseded-process"),
            },
        ] {
            let expected = plugin.class() == crate::PluginErrorClass::Retryable;
            let code = crate::RuntimeEffectControllerError::from(plugin.clone()).code;
            let refusal = EngineRefusal::from(plugin);
            assert_eq!(refusal.code, code);
            assert_eq!(refusal.is_retryable(), expected, "{refusal:?}");
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
    /// shift is parked on its session's next run; one that stopped only
    /// behind a redrive that has since settled is resumed, and one whose
    /// session's next work names no run is released (ADR 0109 §3). Stalled
    /// work a run waits on is parked on that run, which records the work's
    /// handle; stalled work nothing waits for any more is released.
    ///
    /// Each recovery catalog inspects at most `page.limit` records under
    /// `page.budget`. The stalled-work listing resumes after `page.after`
    /// in the engine's order; the report's `next` continues it, and `None`
    /// wraps it. An engine that also repairs lost runs keeps independent
    /// process and run cursors across calls. Failed items advance their
    /// cursor and are retried when that catalog wraps.
    /// Idempotent: a second pass over the same execution records nothing new.
    async fn reconcile_parks(
        &self,
        parks: &dyn ParkRecoveryWriter,
        page: EnginePage,
    ) -> Result<ParkReconcileReport, EngineRefusal>;

    /// O4 redrive: resume the execution holding the run's park, and the
    /// stopped work `children` names: the handles the run's park recorded
    /// (FIG-4630). The engine resumes exactly those and looks for no other
    /// work of the run, the session or anyone else. An engine holding none
    /// answers [`EngineAck::NothingHeld`], and the caller schedules a shift
    /// instead.
    async fn resume_run(
        &self,
        target: &RunRef,
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

    /// The drain of `generation` asking `session`'s parked turn to hand over
    /// (FIG-4739): a turn of the session that runs on a build of
    /// `generation` and is parked on a durable wait its Run's successor
    /// segment may take over ends at a segment boundary there, with the wait
    /// left open, and the Run goes on in a new execution on the newest
    /// build. A turn parked on no such wait, or running on another
    /// generation, is left as it is: it hands over at its next quiet point
    /// by itself.
    ///
    /// Idempotent: a wait already handed over is not parked any more. An
    /// engine whose turns hold no execution across a wait has nothing to
    /// wake.
    async fn hand_over_turns(
        &self,
        session: &crate::SessionId,
        generation: &super::BuildGeneration,
    ) -> Result<(), EngineRefusal> {
        let _ = (session, generation);
        Ok(())
    }

    /// O4 release: stop the run's execution for good, AFTER the store
    /// recorded the run's terminal evidence. Never proof of a lash outcome
    /// (ADR 0104 O4): the evidence is the store's.
    async fn release_run(
        &self,
        target: &RunRef,
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

    async fn resume_run(
        &self,
        _target: &RunRef,
        _engine: Option<&EnginePark>,
        _children: &[EnginePark],
    ) -> Result<EngineAck, EngineRefusal> {
        Ok(EngineAck::NothingHeld)
    }

    async fn release_run(
        &self,
        _target: &RunRef,
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
    /// A session's logical run.
    Run { session: SessionId, run: TurnId },
    /// A process.
    Process { process: crate::ProcessId },
    /// A session's shift, stopped in its admission before it ran a run: it
    /// is parked on the run the session's next admission names (ADR 0109
    /// §3), and only that park's operator verb resumes it.
    Shift { session: SessionId },
    /// Work a session's logical run waits on, stopped in an execution of
    /// its own (a tool attempt's child, FIG-4607). The run's own execution
    /// is not stopped: it waits for the child. The child is parked on the
    /// run, whose park records the child's engine handle beside those of
    /// the run's other stopped children (FIG-4630), and the park's redrive
    /// resumes exactly the recorded ones.
    RunChild { session: SessionId, run: TurnId },
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
    /// The target is gone (its session deleted, run input withdrawn, or process pruned): the
    /// engine should release its execution.
    TargetGone,
    /// A redrive owns the execution: it resumed it after the engine listed
    /// it as stopped, or is about to. Nothing was written; a later pass
    /// re-lists the execution if it stops again.
    Redriven,
    /// A stopped shift whose session's next work names no run — only
    /// queued commands, a closing session's in-flight claim, or nothing: no
    /// operator verb could resume it, so nothing was written and the engine
    /// releases the shift. What the session still holds keeps its own
    /// ingress obligation, whose relay asks for a fresh shift: the command
    /// lane works the session again (ADR 0109 §3).
    NothingToPark,
    /// A stopped shift whose every attempt was refused only because its
    /// session's park named a redrive that had not settled (D15), and that
    /// redrive has since settled: the engine resumes the shift. Nothing was
    /// written; the redrive already was the operator's action.
    ResumeShift,
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
    /// [`ParkTarget::Shift`] park stores no handle: the engine finds a
    /// stopped shift by its session. A [`ParkTarget::RunChild`] park records
    /// the handle as one of the run's stopped children.
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
    /// Runs whose execution this pass released because the store had
    /// already ended them.
    pub released: Vec<RunRef>,
    /// Stalled work this pass released because nothing waits for it any
    /// more, by its engine handle: a group child whose position its group
    /// already seated, or work of a retired group (FIG-4630).
    pub released_work: Vec<EnginePark>,
    /// Sessions whose stopped shift this pass released: the session is
    /// gone, or its next work names no run to park on, and its ingress
    /// obligations ask for a fresh shift.
    pub released_shifts: Vec<SessionId>,
    /// Processes this pass ended `SubstrateLost` because the engine had
    /// finished their current segment's execution without their terminal (an
    /// operator's kill): nothing would ever run them again.
    pub ended_processes: Vec<crate::ProcessId>,
    /// Runs this pass ended `SubstrateLost` because the engine lost their
    /// execution ([`RunLoss`]): every execution of the run failed without a
    /// Lash terminal, or the engine holds no execution of a run that started.
    /// Their scope close is now owed by the terminal row.
    pub ended_runs: Vec<RunRef>,
    /// Sessions whose stopped shift this pass resumed: it stopped only behind
    /// a redrive that has since settled (D15). Any other stopped shift is
    /// never resumed here (ADR 0109 §3): it is parked, and only the park's
    /// operator verb resumes it.
    pub resumed_shifts: Vec<SessionId>,
    /// Stalled executions this pass left as they were.
    pub unchanged: usize,
    /// Stalled executions this pass could not settle, each with why: one
    /// failure never fails the page, and a later pass that reaches the
    /// execution again retries it.
    pub failed: Vec<(EngineCursor, String)>,
    /// Where the next pass resumes; `None` when this one read to the end.
    pub next: Option<EngineCursor>,
}
