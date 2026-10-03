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
//! [`RuntimeEffectController::record_run_record`]: crate::RuntimeEffectController::record_run_record

use std::sync::Arc;

use lash_sansio::{ToolCallId, ToolIntentKind};
use serde::{Deserialize, Serialize};

use crate::runtime::effect::{AttemptStream, AttemptStreamRecorder, ScopedEffectController};
use crate::store::plugin_writers::PluginRevision;
use crate::tool_run::{
    AdmissionRefusal, AdmittedBinding, AfterCheckVerdict, AttemptOrdinal, AttributedVerdict,
    CallDecision, DeclarationRefusal, HookCause, ResultSource, RunEventRefusal, RunRecord,
    SegmentOrdinal, ToolDeclaration,
};
use crate::{AwaitEventKey, EffectOpener, RuntimeEffectControllerError};

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
}

/// The prepared request admission records (A): the request as issued and the
/// payload preparation made of it, after argument transforms and before any
/// check.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SingletonPreparedRequest {
    pub arguments: serde_json::Value,
    pub prepared: serde_json::Value,
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
    Done {
        output: String,
        /// The declared Lash intents the result asks the Run to realize.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        intents: Vec<ToolIntentKind>,
        #[serde(default, skip_serializing_if = "AttemptStream::is_empty")]
        stream: AttemptStream,
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
}

impl SingletonCapture {
    /// The result text, when the capture has one.
    #[must_use]
    pub fn output(&self) -> Option<&str> {
        match self {
            Self::Done { output, .. } | Self::Failed { output, .. } => Some(output),
            Self::Refused { .. } => None,
        }
    }

    /// The stream the body emitted, when it ran.
    #[must_use]
    pub fn stream(&self) -> Option<&AttemptStream> {
        match self {
            Self::Done { stream, .. } | Self::Failed { stream, .. } => Some(stream),
            Self::Refused { .. } => None,
        }
    }

    pub(super) fn intents(&self) -> &[ToolIntentKind] {
        match self {
            Self::Done { intents, .. } => intents,
            Self::Failed { .. } | Self::Refused { .. } => &[],
        }
    }
}

/// What one attempt's body returned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SingletonBodyOutcome {
    Done {
        output: String,
        intents: Vec<ToolIntentKind>,
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
}

/// Why a singleton stopped before it ended. None of these runs a body.
#[derive(Debug, thiserror::Error)]
pub enum SingletonRunError {
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
