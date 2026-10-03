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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SingletonPreparedRequest {
    pub arguments: serde_json::Value,
    pub prepared: serde_json::Value,
    /// The recorded process route, never an ordinary body or Deferred source.
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
            Self::Done { output, .. } | Self::Failed { output, .. } => Some(output),
            Self::Refused { .. } | Self::StartRefused { .. } | Self::Isolated { .. } => None,
        }
    }

    /// The stream the body emitted, when it ran.
    #[must_use]
    pub fn stream(&self) -> Option<&AttemptStream> {
        match self {
            Self::Done { stream, .. } | Self::Failed { stream, .. } => Some(stream),
            Self::Refused { .. } | Self::StartRefused { .. } | Self::Isolated { .. } => None,
        }
    }

    pub(super) fn intents(&self) -> &[ToolIntentKind] {
        match self {
            Self::Done { intents, .. } => intents,
            Self::Failed { .. }
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
            Self::Failed { .. } | Self::Refused { .. } | Self::StartRefused { .. } => None,
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
        intents: Vec<ToolIntentKind>,
        /// One process start the result declares, under its stable start
        /// key. The Run binds its environment and consumer hold.
        start: Option<Box<ProcessStartRegistration>>,
    },
    Failed {
        output: String,
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

/// The callbacks a singleton's records run. Each runs inside the step of the
/// record that owns its answer and never on a replay that serves the record.
/// An `Err` is a fault: the record stays unjournaled and its step runs again.
#[async_trait::async_trait]
pub trait SingletonToolHandlers: Send + Sync {
    /// The actual process implementations installed by this host.
    fn process_engines(&self) -> Option<&crate::ProcessEngineRegistry> {
        None
    }

    /// Bind an isolated call to its process implementation and stable start.
    /// This selects data only; it must never run the ordinary tool body.
    fn isolated_start(&self, _call: &SingletonToolCall) -> Option<super::IsolatedToolStart> {
        None
    }

    /// Prepare the request (A).
    async fn prepare(&self, call: &SingletonToolCall) -> Result<serde_json::Value, String>;

    /// Every before-check's reply on the one prepared request (A).
    async fn before_checks(
        &self,
        call: &SingletonToolCall,
        request: &SingletonPreparedRequest,
    ) -> Vec<AttributedVerdict<BeforeCheckReply>>;

    /// Execute the body once (X).
    async fn execute(&self, attempt: SingletonAttempt<'_>) -> Result<SingletonBodyOutcome, String>;

    /// Every after-check's reply on the result candidate (D).
    async fn after_checks(
        &self,
        call_id: &ToolCallId,
        capture: &SingletonCapture,
    ) -> Vec<AttributedVerdict<AfterCheckVerdict>>;

    /// Whether the owning Run's cancellation is requested, read once inside
    /// the decision's step (D).
    fn run_cancel_requested(&self) -> bool;

    /// Realize a final's declared intents behind their exactly-once fences.
    /// A crash before the declarations settle realizes them again, so the
    /// fence is what makes them once.
    async fn realize_declarations(
        &self,
        call_id: &ToolCallId,
        intents: &[ToolIntentKind],
    ) -> Result<(), String>;

    /// The model-facing presentation of a final result (V).
    async fn present(
        &self,
        call_id: &ToolCallId,
        capture: &SingletonCapture,
    ) -> Result<String, String>;

    /// Emit the stream a decided call's attempt captured to the host, once
    /// its declarations settled and before its presentation (V). A replay
    /// that serves the presentation emits nothing again.
    fn emit_stream(&self, call_id: &ToolCallId, stream: &AttemptStream);

    /// Register a final's declared start under its key (K5): the
    /// registration fixes its binding, lifetime, environment and consumer
    /// hold, and arms its delivery. A crash before the launch record is
    /// durable launches again under the same key, so the registrar must
    /// answer the process it registered first.
    async fn launch_start(&self, obligation: &DeclaredStartObligation)
    -> Result<ProcessId, String>;

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

/// What a recorded admission names differently from the call replaying it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SingletonDrift {
    CallId,
    ToolName,
    Arguments,
    IsolationBinding,
}

/// Why a singleton stopped before it ended. None of these runs a body.
#[derive(Debug, thiserror::Error)]
pub enum SingletonRunError {
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
