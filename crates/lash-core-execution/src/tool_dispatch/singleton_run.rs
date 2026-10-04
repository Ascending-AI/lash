//! A tool call admitted as a singleton round of its owning Run (K3, FIG-4877):
//! its admission (A), its attempt (X), its decision (D) and its presentation
//! with its incorporation (V) are records in the Run's opener journal.
//!
//! This is an expansion interface, not a permanent execution mode. General
//! production callers keep their existing route until the Run coordinator
//! carries them (F01-F03). The route opens no child invocation, makes no
//! group call and acquires no per-call environment: every record is one
//! journaled step of the caller's own handler, through
//! [`RuntimeEffectController::record_run_record`].
//!
//! Each record's step runs once. A replay serves the journaled record without
//! running its step, so a recorded attempt never re-executes its body, a
//! recorded check never runs again, and a recorded decision never observes
//! cancellation again. A step that did not become durable — its fault, or a
//! crash before the engine acknowledged its result — runs again with the same
//! call id and attempt ordinal. Every served record goes through the
//! [`RunLedger`](crate::tool_run::RunLedger) fold before anything acts on it,
//! and every material reference it names is verified against its canonical
//! bytes.
//!
//! [`run_singleton_tool`] runs one call alone in its Run. The
//! [`RunCoordinator`](super::RunCoordinator) runs several in one Run, ranks
//! their decisions and drains their protected work in rank order (FIG-4880).
//!
//! Reported retries (K9) are the Run coordinator's schedule (FIG-4879); a
//! singleton admits no retry policy. A Deferred attempt hands its call to the
//! source seal (FIG-4883) after its record.
//!
//! A final may declare one process start (K5, FIG-4884). Its attempt record
//! owns the start's obligation: the body's registration, bound by the Run to
//! the Run's environment when lash executes the process and to a consumer
//! hold that carries the call's recorded cancel policy, under the body's
//! stable start key. The start drains inside the final's declarations: it is
//! admitted in the record that issues them (`declare`), registered under its
//! key (`start:launch`), and discharged (`start:discharge`) — the Run's
//! cancellation read once, the recorded policy followed and the hold
//! released — before the presentation settles them. A cancellation before
//! the decision is durable withholds the final, so its start is never
//! admitted and never launches. One after it cannot forbid the start: a lost
//! launch launches again under the same key, which the registrar answers
//! with the process it registered first, and the discharge then cancels that
//! process when the recorded policy says so. No task outlives the drain, and
//! the start holds the process only until its launch is durable.
//!
//! [`RuntimeEffectController::record_run_record`]: crate::RuntimeEffectController::record_run_record
//!
//! An isolated declaration binds its registered implementation, boundary and
//! canonical start in admission. Its attempt records that binding without
//! calling `execute`. The protected start drain returns a process descriptor.
//! A hard-isolation cancel records the physical worker's termination receipt
//! before releasing the consumer hold. Ordinary bodies retain their own
//! timeout behavior and are never rerouted into this start path.

use std::sync::Arc;

use lash_sansio::{ToolCallId, ToolIntentKind};
use serde::{Deserialize, Serialize};

use crate::runtime::effect::{AttemptStream, AttemptStreamRecorder, ScopedEffectController};
use crate::runtime::process::{DeclaredStartObligation, DeclaredStartObligationRefusal};
use crate::store::plugin_writers::PluginRevision;
use crate::tool_run::{
    AdmissionRefusal, AdmittedBinding, AfterCheckVerdict, AttemptOrdinal, AttributedVerdict,
    CallDecision, DeclarationRefusal, ExternalCancelPolicy, HookCause, MaterialRef, ResultSource,
    RunEventRefusal, RunRecord, SegmentOrdinal, ToolDeclaration,
};
use crate::{
    AwaitEventKey, EffectOpener, ProcessExecutionEnvRef, ProcessId, ProcessStartRegistration,
    RuntimeEffectControllerError, StartKey,
};

use super::run_coordinator::{DecidedCall, RunCoordinator};

/// One tool call to run as a singleton in its owning Run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SingletonToolCall {
    /// The opener of the logical Run that owns the call.
    pub owner: EffectOpener,
    /// The active segment of that Run, which appends every record.
    pub segment: SegmentOrdinal,
    pub call_id: ToolCallId,
    pub tool_name: String,
    /// The request as the model issued it.
    pub arguments: serde_json::Value,
    /// The declaration of the manifest the call is admitted under. A recorded
    /// admission's declaration governs every replay; this one is admitted only
    /// on the first execution.
    pub declaration: ToolDeclaration,
    /// The executable, preparation and presentation callbacks the admission
    /// binds, each with its plugin revision.
    pub binding: AdmittedBinding,
    /// The plugin revisions this build executes. A recorded admission bound to
    /// any other refuses, typed, before its body.
    pub available: Vec<PluginRevision>,
    /// The cancel policy admission records: what a cancellation of the Run
    /// after a declared start's admission does to the process it launched.
    /// The recorded policy governs every replay.
    pub cancel: ExternalCancelPolicy,
    /// The execution environment the Run owns. A declared start lash executes
    /// is bound to it when its attempt is recorded; a Run without one admits
    /// no such start.
    pub environment: Option<ProcessExecutionEnvRef>,
}

/// The prepared request admission records (A): the request as issued and the
/// payload preparation made of it, after argument transforms and before any
/// check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SingletonPreparedRequest {
    pub arguments: serde_json::Value,
    pub environment: Option<ProcessExecutionEnvRef>,
    pub prepared: serde_json::Value,
    /// The plugin namespace at admission, fixed across crash redelivery.
    pub state_snapshot: Option<Arc<crate::plugin::PluginNamespaceState>>,
    /// The recorded process route, never an ordinary body or Deferred source.
    pub isolation: Option<RecordedIsolatedStart>,
}

/// Admission retains references; callbacks receive the hydrated read-only request.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RecordedPreparedRequest {
    pub arguments: serde_json::Value,
    pub environment: Option<ProcessExecutionEnvRef>,
    pub prepared: serde_json::Value,
    pub state_snapshot: Option<MaterialRef>,
    pub isolation: Option<RecordedIsolatedStart>,
}

/// The process implementation and canonical start material admission bound.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordedIsolatedStart {
    pub implementation: crate::store::plugin_writers::PluginCallbackIdentity,
    pub engine_kind: String,
    pub boundary: super::ProcessExecutionBoundary,
    pub start: SingletonStart,
}

/// An isolated call's result names the independently executing process.
/// Physical cancellation carries the implementation's retained reap receipt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IsolatedProcessDescriptor {
    pub process_id: ProcessId,
    pub start_key: StartKey,
    pub boundary: super::ProcessExecutionBoundary,
    pub termination: Option<super::WorkerTerminationReceipt>,
}

/// What an attempt captured (X), or the cached success a before-check
/// supplied in its place.
///
/// The capture owns the bounded stream the body emitted (FIG-4880): its
/// deltas coalesced, its shared call fields stored once and its bytes capped
/// by [`ATTEMPT_STREAM_BYTE_BUDGET`](crate::runtime::effect::ATTEMPT_STREAM_BYTE_BUDGET)
/// with a typed truncation. The Run emits it when it presents the call.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum SingletonCapture {
    /// A process bound at admission. No ordinary body produced this capture.
    Isolated { binding: Box<RecordedIsolatedStart> },
    Done {
        output: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        commands: Vec<crate::tool_run::StateCommand>,
        /// The declared Lash intents the result asks the Run to realize.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        intents: Vec<ToolIntentKind>,
        #[serde(default, skip_serializing_if = "AttemptStream::is_empty")]
        stream: AttemptStream,
        /// The process start the result declares.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        start: Option<Box<SingletonStart>>,
    },
    /// A failure the body reported.
    Failed {
        output: String,
        #[serde(default, skip_serializing_if = "AttemptStream::is_empty")]
        stream: AttemptStream,
    },
    RetryableFailure {
        output: String,
        stream: AttemptStream,
        after_ms: Option<u64>,
    },
    /// An outcome the admitted declaration does not admit, refused before
    /// anything it declared was realized.
    Refused { refusal: DeclarationRefusal },
    /// A declared start that cannot be an obligation — keyless, or a start
    /// lash executes in a Run that owns no environment — refused before it
    /// was admitted.
    StartRefused {
        refusal: DeclaredStartObligationRefusal,
    },
}

/// A declared start as its attempt recorded it: the start's stable key and
/// the canonical material of its obligation, which the attempt record owns.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SingletonStart {
    pub start_key: StartKey,
    pub obligation: MaterialRef,
}

impl SingletonCapture {
    /// The result text, when the capture has one.
    #[must_use]
    pub fn output(&self) -> Option<&str> {
        match self {
            Self::Done { output, .. }
            | Self::Failed { output, .. }
            | Self::RetryableFailure { output, .. } => Some(output),
            Self::Refused { .. } | Self::StartRefused { .. } | Self::Isolated { .. } => None,
        }
    }

    /// The stream the body emitted, when it ran.
    #[must_use]
    pub fn stream(&self) -> Option<&AttemptStream> {
        match self {
            Self::Done { stream, .. }
            | Self::Failed { stream, .. }
            | Self::RetryableFailure { stream, .. } => Some(stream),
            Self::Refused { .. } | Self::StartRefused { .. } | Self::Isolated { .. } => None,
        }
    }

    pub(super) fn intents(&self) -> &[ToolIntentKind] {
        match self {
            Self::Done { intents, .. } => intents,
            Self::Failed { .. }
            | Self::RetryableFailure { .. }
            | Self::Refused { .. }
            | Self::StartRefused { .. }
            | Self::Isolated { .. } => &[],
        }
    }

    /// The process start the result declares.
    #[must_use]
    pub fn start(&self) -> Option<&SingletonStart> {
        match self {
            Self::Done { start, .. } => start.as_deref(),
            Self::Isolated { binding } => Some(&binding.start),
            Self::Failed { .. }
            | Self::RetryableFailure { .. }
            | Self::Refused { .. }
            | Self::StartRefused { .. } => None,
        }
    }

    /// Whether a final of this capture owes declarations: intents to realize
    /// or a start to drain.
    pub(super) fn declares(&self) -> bool {
        !self.intents().is_empty() || self.start().is_some()
    }
}

/// What one attempt's body returned.
#[derive(Clone, Debug)]
pub enum SingletonBodyOutcome {
    Done {
        output: String,
        commands: crate::plugin::StateCommands,
        intents: Vec<ToolIntentKind>,
        /// One process start the result declares, under its stable start
        /// key. The Run binds its environment and consumer hold.
        start: Option<Box<ProcessStartRegistration>>,
    },
    Failed {
        output: String,
    },
    /// A reported failure the admitted retry policy may retry.
    RetryableFailure {
        output: String,
        after_ms: Option<u64>,
    },
    /// Declare one K5 start; the Run launches it and waits on its K4 terminal.
    DeferredStart {
        start: Box<ProcessStartRegistration>,
    },
    /// The production body's pending completion. Its resolver, announcement
    /// and cancellation hint are captured by X before the Run arms them.
    Pending {
        completion: Box<crate::PendingCompletion>,
    },
    /// Parked on a Deferred source; the source's seal supplies the result.
    Deferred {
        source: AwaitEventKey,
    },
}

/// The attempt a body executes: its call, its ordinal and its admitted
/// request. A crash redelivery presents the same call id and ordinal.
///
/// `stream` is the attempt's own observation sink: what the body observes
/// there is recorded, bounded, in the attempt's capture, and the Run emits it
/// when it presents the call. A redelivered attempt starts a fresh one.
#[derive(Clone, Copy)]
pub struct SingletonAttempt<'a> {
    pub call_id: &'a ToolCallId,
    pub attempt: AttemptOrdinal,
    pub request: &'a SingletonPreparedRequest,
    pub stream: &'a Arc<AttemptStreamRecorder>,
    /// The source armed by the Run before this attempt executes.
    pub completion_key: Option<&'a AwaitEventKey>,
}

/// A before-check's reply, before admission records it. A cached success is
/// the result text only; admission records it as the Run's attempt output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BeforeCheckReply {
    Allow,
    Cached { output: String },
    Deny { cause: HookCause },
    Cancel { cause: HookCause },
    AbortRun { cause: HookCause },
}

/// Presentation separates a declared refusal from an invocation fault. The
/// former records fallback text with its original cause; the latter retries
/// the uncommitted V boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SingletonPresentationError {
    Refused { cause: HookCause },
    Fault { message: String },
}

/// The callbacks a singleton's records run. Each runs inside the step of the
/// record that owns its answer and never on a replay that serves the record.
/// An `Err` is a fault: the record stays unjournaled and its step runs again.
#[async_trait::async_trait]
pub trait SingletonToolHandlers: Send + Sync {
    /// Passive original-call facts retained beside the accepted admission.
    fn admission_observation(
        &self,
        _request: &SingletonPreparedRequest,
    ) -> Result<Option<serde_json::Value>, String> {
        Ok(None)
    }

    /// Passive terminal facts; a final is observed after protected presentation.
    fn terminal_observation(
        &self,
        _call_id: &ToolCallId,
        _decision: &CallDecision,
        _cause: Option<&AttributedVerdict<HookCause>>,
        _capture: Option<&SingletonCapture>,
        _presentation: Option<&str>,
    ) -> Result<Option<serde_json::Value>, String> {
        Ok(None)
    }

    /// The session whose read-only snapshots and declared commands this Run uses.
    fn plugin_session(&self) -> Option<Arc<crate::PluginSession>> {
        None
    }

    /// Retained result material, read under the source's existing lease.
    fn tool_material_store(&self) -> Option<&dyn crate::store::ToolMaterialStore> {
        None
    }

    /// The actual process implementations installed by this host.
    fn process_engines(&self) -> Option<&crate::ProcessEngineRegistry> {
        None
    }

    /// Bind an isolated call to its process implementation and stable start.
    /// This selects data only; it must never run the ordinary tool body.
    fn isolated_start(&self, _call: &SingletonToolCall) -> Option<super::IsolatedToolStart> {
        None
    }

    /// Hydrate the recorded admission, including at successor adoption.
    /// This validates capabilities without rerunning preparation or hooks.
    fn restore_request(
        &self,
        _call_id: &ToolCallId,
        _binding: &AdmittedBinding,
        _request: &SingletonPreparedRequest,
    ) -> Result<(), RuntimeEffectControllerError> {
        Ok(())
    }

    /// Policy sealed by the same admission as the prepared request.
    fn retry_policy(
        &self,
        _call: &SingletonToolCall,
        default: crate::tool_run::RecordedRetryPolicy,
    ) -> crate::tool_run::RecordedRetryPolicy {
        default
    }

    fn cached_capture(&self, output: String) -> Result<SingletonCapture, String> {
        Ok(SingletonCapture::Done {
            output,
            commands: Vec::new(),
            intents: Vec::new(),
            stream: Default::default(),
            start: None,
        })
    }

    /// Prepare the request (A).
    async fn prepare(&self, call: &SingletonToolCall) -> Result<serde_json::Value, String>;

    /// Every before-check's reply on the one prepared request (A).
    /// A hydration or callback fault fails the invocation rather than denying the call.
    async fn before_checks(
        &self,
        call: &SingletonToolCall,
        request: &SingletonPreparedRequest,
    ) -> Result<Vec<AttributedVerdict<BeforeCheckReply>>, String>;

    /// Execute the body once (X).
    async fn execute(&self, attempt: SingletonAttempt<'_>) -> Result<SingletonBodyOutcome, String>;

    /// Arm facts declared by a recorded pending completion. Repeated arming
    /// must be idempotent, including after successor adoption.
    async fn arm_pending(
        &self,
        _source: &crate::tool_run::SourceDescriptor,
        _completion: &crate::PendingCompletion,
    ) -> Result<(), RuntimeEffectControllerError> {
        Ok(())
    }

    /// Whether the source result needs an X finalization under this admission.
    fn finalizes_source(&self) -> bool {
        false
    }

    async fn finalize_source(
        &self,
        _call_id: &ToolCallId,
        _attempt: AttemptOrdinal,
        capture: &SingletonCapture,
        _completion: Option<&crate::PendingCompletion>,
    ) -> Result<SingletonCapture, String> {
        Ok(capture.clone())
    }

    /// Every after-check's reply on the result candidate (D).
    async fn after_checks(
        &self,
        call_id: &ToolCallId,
        capture: &SingletonCapture,
    ) -> Result<Vec<AttributedVerdict<AfterCheckVerdict>>, String>;

    /// Distinct after-check contributions recorded by D beside its verdicts.
    fn decision_contributions(&self, _call_id: &ToolCallId) -> Result<Option<String>, String> {
        Ok(None)
    }

    fn restore_decision_contributions(
        &self,
        _call_id: &ToolCallId,
        _text: &str,
    ) -> Result<(), RuntimeEffectControllerError> {
        Ok(())
    }

    /// Project a call's activity at its own acknowledged V, including while
    /// the aggregate is still pending. Replay observes through the same cursor.
    fn observe_terminal(
        &self,
        _call_id: &ToolCallId,
        _decision: &CallDecision,
        _cause: Option<&AttributedVerdict<HookCause>>,
        _capture: Option<&SingletonCapture>,
        _presentation: Option<&str>,
    ) -> Result<(), RuntimeEffectControllerError> {
        Ok(())
    }

    /// Apply recorded semantic channels only after V is acknowledged.
    fn incorporate(
        &self,
        _call_id: &ToolCallId,
        _capture: Option<&SingletonCapture>,
        _presentation: Option<&str>,
        _observe: bool,
    ) -> Result<(), RuntimeEffectControllerError> {
        Ok(())
    }

    /// Observe the owning Run's authoritative cancellation inside the body
    /// of its recorded decision (D), never from an unrecorded live flag.
    async fn run_cancel_requested(&self) -> Result<bool, String>;

    /// Wake a registered retry when its timer finishes or its owner stops.
    /// This is readiness only: the subsequent recorded D chooses cancellation.
    async fn wait_run_retry(
        &self,
        timer: crate::tool_dispatch::RunRetryTimer<'_>,
    ) -> Result<(), RuntimeEffectControllerError>;

    /// Discharge eligible external cancellation at logical Closing. The
    /// call id is the dedup key; recovery can repeat an unacknowledged call.
    /// Ignore-policy calls never invoke this callback.
    async fn cancel_call(
        &self,
        _call_id: &ToolCallId,
        _source: Option<&AwaitEventKey>,
    ) -> Result<(), String> {
        Err("external cancellation requires an installed handler".to_owned())
    }

    /// Realize a final's declared intents behind their exactly-once fences.
    /// A crash before the declarations settle realizes them again, so the
    /// fence is what makes them once.
    async fn realize_declarations(
        &self,
        call_id: &ToolCallId,
        intents: &[ToolIntentKind],
    ) -> Result<(), String>;

    async fn realize_capture(
        &self,
        call_id: &ToolCallId,
        capture: &SingletonCapture,
    ) -> Result<(), String> {
        self.realize_declarations(call_id, capture.intents()).await
    }

    /// The model-facing presentation of a final result (V). A declared
    /// refusal records the original result as fallback, with its typed cause.
    /// An invocation fault leaves V uncommitted for engine recovery.
    async fn present(
        &self,
        call_id: &ToolCallId,
        capture: &SingletonCapture,
    ) -> Result<String, SingletonPresentationError>;

    /// Emit the captured stream after V has been durably accepted. A replay
    /// that serves V emits nothing. A lost acknowledgement may omit this
    /// observation; it never permits an unaccepted proposal to publish it.
    fn emit_stream(&self, call_id: &ToolCallId, stream: &AttemptStream);

    /// Register a final's declared start under its key (K5): the
    /// registration fixes its binding, lifetime, environment and consumer
    /// hold, and arms its delivery. A crash before the launch record is
    /// durable launches again under the same key, so the registrar must
    /// answer the process it registered first.
    async fn launch_start(&self, obligation: &DeclaredStartObligation)
    -> Result<ProcessId, String>;

    /// Arm a short process-terminal subscription after the K5 launch is durable.
    async fn attach_start_terminal(
        &self,
        source: &crate::tool_run::SourceDescriptor,
        process_id: &ProcessId,
    ) -> Result<(), RuntimeEffectControllerError> {
        let _ = (source, process_id);
        Err(crate::tool_run::SourceRefusal::NotArmed.into())
    }

    /// Discharge a launched start: cancel `process_id` when `cancel`, then
    /// release the obligation's consumer hold. A crash before the discharge
    /// record is durable repeats both, so both must be idempotent.
    async fn discharge_start(
        &self,
        obligation: &DeclaredStartObligation,
        process_id: &ProcessId,
        cancel: bool,
    ) -> Result<(), String>;
}

/// How a call ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SingletonTerminal {
    /// The result is final, its declarations settled, and it is presented and
    /// incorporated.
    Final {
        source: ResultSource,
        capture: SingletonCapture,
        presentation: String,
        /// The process the final's declared start launched.
        launched: Option<ProcessId>,
    },
    /// A check or the Run's cancellation withheld the result: the decision is
    /// denied, cancelled or aborted, and the recorded check names the cause.
    Withheld { decision: CallDecision },
    /// The attempt parked on a Deferred source; the source's seal finishes
    /// the call.
    Deferred { source: AwaitEventKey },
}

/// A finished singleton: how it ended and the records its Run holds.
#[derive(Clone, Debug)]
pub struct SingletonRunOutcome {
    pub terminal: SingletonTerminal,
    pub records: Vec<RunRecord>,
}

pub use crate::tool_run::SingletonDrift;

/// Why a singleton stopped before it ended. None of these runs a body.
#[derive(Debug, thiserror::Error)]
pub enum SingletonRunError {
    #[error(transparent)]
    Continuation(#[from] crate::tool_run::ContinuationRefusal),
    #[error(transparent)]
    Cut(#[from] super::run_coordinator::RunCutRefusal),
    /// A source seal named material whose retention or authority refused the read.
    #[error(transparent)]
    Material(#[from] crate::tool_run::MaterialRetentionError),
    #[error("isolated start refused: {0}")]
    Isolation(#[from] super::IsolatedStartRefusal),
    /// Admission refused the call, or a recorded admission no longer binds an
    /// available plugin revision.
    #[error("admission refused call: {0}")]
    Admission(#[from] AdmissionRefusal),
    /// The recorded admission belongs to another request under this call id.
    #[error("the recorded admission of call {call_id} names another {drift:?}")]
    Drift {
        call_id: ToolCallId,
        drift: SingletonDrift,
    },
    /// A served record breaks the Run's event contract.
    #[error("a Run record was refused: {0}")]
    Ledger(#[from] RunEventRefusal),
    /// The engine, or a material read, refused; a material refusal carries
    /// its typed cause.
    #[error(transparent)]
    Controller(#[from] RuntimeEffectControllerError),
}

/// Run `call` alone in its owning Run, recording A, X, D and V in the opener
/// journal `scoped` serves.
///
/// # Errors
///
/// A typed [`SingletonRunError`]; none of them executes a body.
pub async fn run_singleton_tool(
    scoped: &ScopedEffectController<'_>,
    call: &SingletonToolCall,
    handlers: &dyn SingletonToolHandlers,
) -> Result<SingletonRunOutcome, SingletonRunError> {
    let mut run = RunCoordinator::open(
        scoped,
        call.owner.clone(),
        call.segment,
        call.available.clone(),
    );
    let terminal = match run.decide(call, handlers).await? {
        DecidedCall::Deferred { source } => SingletonTerminal::Deferred { source },
        DecidedCall::Ranked { .. } => run
            .drain()
            .await?
            .pop()
            .map(|(_, terminal)| terminal)
            .ok_or_else(|| RunEventRefusal::BoundaryOrder {
                call_id: call.call_id.clone(),
            })?,
    };
    Ok(SingletonRunOutcome {
        terminal,
        records: run.into_records(),
    })
}

/// The body of one independently recorded X receipt.
pub type RunAttemptStep<'run> = std::pin::Pin<
    Box<
        dyn std::future::Future<Output = Result<crate::tool_run::RunAttemptEntry, String>>
            + Send
            + 'run,
    >,
>;

/// An independently registered X; awaiting it does not register another command.
pub type RunAttemptHandle<'run> = std::pin::Pin<
    Box<
        dyn std::future::Future<
                Output = Result<crate::tool_run::RunAttemptEntry, RuntimeEffectControllerError>,
            > + Send
            + 'run,
    >,
>;

/// A durable backoff registered before its result is awaited.
pub type RunRetryTimer<'run> = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<(), RuntimeEffectControllerError>> + Send + 'run>,
>;

impl SingletonRunError {
    pub(crate) fn into_controller_error(self) -> RuntimeEffectControllerError {
        match self {
            Self::Controller(error) => error,
            Self::Continuation(refusal) => refusal.into(),
            Self::Material(refusal) => match refusal {
                crate::tool_run::MaterialRetentionError::Refused(refusal) => refusal.into(),
                crate::tool_run::MaterialRetentionError::Controller(error) => *error,
                crate::tool_run::MaterialRetentionError::Store(error) => error.into(),
                crate::tool_run::MaterialRetentionError::HolderEnded { holder } => {
                    crate::RuntimeError::artifact_referrer_ended(holder.referrer()).into()
                }
            },
            Self::Ledger(cause) => crate::tool_run::ContinuationRefusal::Records { cause }.into(),
            Self::Admission(refusal) => {
                let mut error = RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeEffectGroupShape,
                    refusal.to_string(),
                );
                error.cause = Some(crate::RuntimeErrorCause::ToolRunAdmissionRefused {
                    refusal: Box::new(refusal),
                });
                error
            }
            Self::Cut(refusal) => run_refusal(
                crate::RuntimeErrorCause::ToolRunCutRefused {
                    refusal: Box::new(refusal),
                },
                refusal.to_string(),
            ),
            Self::Isolation(refusal) => {
                let message = refusal.to_string();
                run_refusal(
                    crate::RuntimeErrorCause::ToolRunIsolationRefused {
                        refusal: Box::new(refusal),
                    },
                    message,
                )
            }
            Self::Drift { call_id, drift } => run_refusal(
                crate::RuntimeErrorCause::ToolRunDrift {
                    call_id: Box::new(call_id),
                    drift: Box::new(drift),
                },
                format!("recorded admission drifted in {drift:?}"),
            ),
        }
    }
}

fn run_refusal(cause: crate::RuntimeErrorCause, message: String) -> RuntimeEffectControllerError {
    let mut error = RuntimeEffectControllerError::new(
        crate::RuntimeErrorCode::RuntimeEffectGroupShape,
        message,
    );
    error.cause = Some(cause);
    error
}
