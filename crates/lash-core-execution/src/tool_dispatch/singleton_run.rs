//! The callbacks a tool call runs through, and the facts they exchange: its
//! admission (A), its attempt (X), its decision (D) and its presentation
//! (V). [`super::call_run`] runs them in memory, in that order, inside the
//! admitted execution whose records make the call durable.

use std::sync::Arc;

use lash_sansio::{ToolCallId, ToolIntentKind};
use serde::{Deserialize, Serialize};

use crate::runtime::effect::{AttemptStream, AttemptStreamRecorder};
use crate::runtime::process::{DeclaredStartObligation, DeclaredStartObligationRefusal};
use crate::store::plugin_writers::PluginRevision;
use crate::tool_run::{
    AdmissionRefusal, AdmittedBinding, AfterCheckVerdict, AttemptOrdinal, AttributedVerdict,
    CallDecision, DeclarationRefusal, ExternalCancelPolicy, HookCause, ToolDeclaration,
};
use crate::{
    EffectOpener, ProcessExecutionEnvRef, ProcessId, ProcessStartRegistration,
    RuntimeEffectControllerError, StartKey,
};

/// One tool call to run in its owning Run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SingletonToolCall {
    /// The opener of the logical Run that owns the call.
    pub owner: EffectOpener,
    pub call_id: ToolCallId,
    pub tool_name: String,
    /// The request as the model issued it.
    pub arguments: serde_json::Value,
    /// The declaration of the manifest the call is admitted under.
    pub declaration: ToolDeclaration,
    /// The executable, preparation and presentation callbacks the admission
    /// binds, each with its plugin revision.
    pub binding: AdmittedBinding,
    /// The plugin revisions this build executes. An admission bound to any
    /// other refuses, typed, before its body.
    pub available: Vec<PluginRevision>,
    /// What a cancellation of the Run after a declared start launched does
    /// to the process it launched.
    pub cancel: ExternalCancelPolicy,
    /// The execution environment the Run owns. A declared start lash
    /// executes is bound to it; a Run without one admits no such start.
    pub environment: Option<ProcessExecutionEnvRef>,
}

/// The prepared request admission made (A): the request as issued and the
/// payload preparation made of it, after argument transforms and before any
/// check.
#[derive(Clone, Debug)]
pub struct SingletonPreparedRequest {
    pub arguments: serde_json::Value,
    pub environment: Option<ProcessExecutionEnvRef>,
    pub prepared: serde_json::Value,
    /// The plugin namespace at admission.
    pub state_snapshot: Option<Arc<crate::plugin::PluginNamespaceState>>,
    /// The process route of an isolated call, never an ordinary body.
    pub isolation: Option<IsolatedBinding>,
}

/// The process implementation and canonical start admission bound to an
/// isolated call.
#[derive(Clone, Debug)]
pub struct IsolatedBinding {
    pub implementation: crate::store::plugin_writers::PluginCallbackIdentity,
    pub engine_kind: String,
    pub obligation: Arc<DeclaredStartObligation>,
}

/// An isolated call's result names the independently executing process.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IsolatedProcessDescriptor {
    pub process_id: ProcessId,
    pub start_key: StartKey,
}

/// What an attempt captured (X), or the cached success a before-check
/// supplied in its place. The capture owns the bounded stream the body
/// emitted, which the call emits when it is presented.
#[derive(Clone, Debug)]
pub enum SingletonCapture {
    /// A process bound at admission. No ordinary body produced this capture.
    Isolated {
        binding: Box<IsolatedBinding>,
    },
    Done {
        output: String,
        commands: Vec<crate::tool_run::StateCommand>,
        /// The declared Lash intents the result asks the Run to realize.
        intents: Vec<ToolIntentKind>,
        stream: AttemptStream,
        /// The process start the result declares.
        start: Option<Arc<DeclaredStartObligation>>,
    },
    /// A failure the body reported.
    Failed {
        output: String,
        suggested_delay_ms: Option<u64>,
        stream: AttemptStream,
    },
    Interrupted,
    TimedOut {
        cause: crate::tool_run::LimitCause,
        evidence: Option<String>,
    },
    Cancelled {
        evidence: Option<String>,
    },
    /// An outcome the admitted declaration does not admit, refused before
    /// anything it declared was realized.
    Refused {
        refusal: DeclarationRefusal,
    },
    /// A declared start that cannot be an obligation: keyless, or a start
    /// lash executes in a Run that owns no environment.
    StartRefused {
        refusal: DeclaredStartObligationRefusal,
    },
}

impl SingletonCapture {
    /// The result text, when the capture has one.
    #[must_use]
    pub fn output(&self) -> Option<&str> {
        match self {
            Self::Done { output, .. } | Self::Failed { output, .. } => Some(output),
            Self::TimedOut {
                evidence: Some(output),
                ..
            }
            | Self::Cancelled {
                evidence: Some(output),
            } => Some(output),
            Self::Refused { .. }
            | Self::StartRefused { .. }
            | Self::Isolated { .. }
            | Self::Interrupted
            | Self::TimedOut { .. }
            | Self::Cancelled { .. } => None,
        }
    }

    /// The stream the body emitted, when it ran.
    #[must_use]
    pub fn stream(&self) -> Option<&AttemptStream> {
        match self {
            Self::Done { stream, .. } | Self::Failed { stream, .. } => Some(stream),
            Self::Refused { .. }
            | Self::StartRefused { .. }
            | Self::Isolated { .. }
            | Self::Interrupted
            | Self::TimedOut { .. }
            | Self::Cancelled { .. } => None,
        }
    }

    /// The Lash intents the result declares.
    #[must_use]
    pub fn intents(&self) -> &[ToolIntentKind] {
        match self {
            Self::Done { intents, .. } => intents,
            Self::Failed { .. }
            | Self::Refused { .. }
            | Self::StartRefused { .. }
            | Self::Isolated { .. }
            | Self::Interrupted
            | Self::TimedOut { .. }
            | Self::Cancelled { .. } => &[],
        }
    }

    /// The process start the result declares.
    #[must_use]
    pub fn start(&self) -> Option<&DeclaredStartObligation> {
        match self {
            Self::Done { start, .. } => start.as_deref(),
            Self::Isolated { binding } => Some(&binding.obligation),
            Self::Failed { .. }
            | Self::Refused { .. }
            | Self::StartRefused { .. }
            | Self::Cancelled { .. }
            | Self::Interrupted
            | Self::TimedOut { .. } => None,
        }
    }

    /// Whether a final of this capture owes declarations: intents to realize
    /// or a start to launch.
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
        suggested_delay_ms: Option<u64>,
    },
    Interrupted,
    TimedOut {
        cause: crate::tool_run::LimitCause,
        evidence: Option<String>,
    },
    Cancelled {
        evidence: Option<String>,
    },
    /// The body parked on its completion wait: its pending completion, the
    /// launch receipt of the start it declared to resolve it, and that
    /// start's store-local effect, which commits with the park.
    Pending {
        completion: Box<crate::PendingCompletion>,
        launch: Option<Box<super::LaunchReceipt>>,
        store_local: Vec<crate::runtime::actor::round::StoreLocalEffect>,
    },
}

/// The attempt a body executes: its call, its ordinal and its admitted
/// request. A crash rerun of a `Repeatable` call presents the same call id
/// and ordinal.
///
/// `stream` is the attempt's own observation sink: what the body observes
/// there is captured, bounded, and emitted when the call is presented.
#[derive(Clone, Copy)]
pub struct SingletonAttempt<'a> {
    pub call_id: &'a ToolCallId,
    pub attempt: AttemptOrdinal,
    pub request: &'a SingletonPreparedRequest,
    pub stream: &'a Arc<AttemptStreamRecorder>,
}

/// A before-check's reply. A cached success is the result text only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BeforeCheckReply {
    Allow,
    Cached { output: String },
    Deny { cause: HookCause },
    Cancel { cause: HookCause },
    AbortRun { cause: HookCause },
}

/// Presentation separates a declared refusal from an invocation fault: the
/// former presents the original result, the latter faults the call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SingletonPresentationError {
    Refused { cause: HookCause },
    Fault { message: String },
}

/// A call's callbacks. An `Err` is a fault: the call answers nothing, and
/// the admitted execution running it ends without an outcome.
#[async_trait::async_trait]
pub trait SingletonToolHandlers: Send + Sync {
    /// The session whose read-only snapshots and declared commands this Run uses.
    fn plugin_session(&self) -> Option<Arc<crate::PluginSession>> {
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

    /// The execution policy the call is admitted under.
    fn execution_policy(&self, _call: &SingletonToolCall) -> crate::tool_run::ExecutionPolicy {
        crate::tool_run::ExecutionPolicy::Once
    }

    /// The capture of a before-check's cached success.
    fn cached_capture(&self, output: String) -> Result<SingletonCapture, String> {
        Ok(SingletonCapture::Done {
            output,
            commands: Vec::new(),
            intents: Vec::new(),
            stream: AttemptStream::default(),
            start: None,
        })
    }

    /// Prepare the request (A).
    async fn prepare(&self, call: &SingletonToolCall) -> Result<serde_json::Value, String>;

    /// Every before-check's reply on the one prepared request (A).
    async fn before_checks(
        &self,
        call: &SingletonToolCall,
        request: &SingletonPreparedRequest,
    ) -> Result<Vec<AttributedVerdict<BeforeCheckReply>>, String>;

    /// Execute the body once (X).
    async fn execute(&self, attempt: SingletonAttempt<'_>) -> Result<SingletonBodyOutcome, String>;

    /// Every after-check's reply on the result candidate (D).
    async fn after_checks(
        &self,
        call_id: &ToolCallId,
        capture: &SingletonCapture,
    ) -> Result<Vec<AttributedVerdict<AfterCheckVerdict>>, String>;

    /// Observe a call's start, once its request is prepared.
    fn observe_started(&self, _call_id: &ToolCallId, _request: &SingletonPreparedRequest) {}

    /// Project a call's activity once it ended.
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

    /// Apply a call's semantic channels once it is presented.
    fn incorporate(
        &self,
        _call_id: &ToolCallId,
        _capture: Option<&SingletonCapture>,
        _presentation: Option<&str>,
        _observe: bool,
    ) -> Result<(), RuntimeEffectControllerError> {
        Ok(())
    }

    /// Cancel the call's external work when its Run closes before it ended.
    async fn cancel_call(&self, _call_id: &ToolCallId) -> Result<(), String> {
        Err("external cancellation requires an installed handler".to_owned())
    }

    /// Realize the intents a final's capture declares: each intent's
    /// outcome, and the store-local effects that commit with the call's.
    async fn realize(
        &self,
        _call_id: &ToolCallId,
        _capture: &SingletonCapture,
    ) -> Result<super::Realization, RuntimeEffectControllerError> {
        Ok(super::Realization::default())
    }

    /// Adopt a final's realized intents before it is presented.
    fn adopt_realization(
        &self,
        _call_id: &ToolCallId,
        _receipt: &super::RealizationReceipt,
    ) -> Result<(), RuntimeEffectControllerError> {
        Ok(())
    }

    /// Commit `effects`, a call's store-local effects, at once in their own
    /// fenced transaction: what a call that is no round member does with
    /// them, since nothing else records its outcome (FIG-5225 admits a code
    /// cell's calls as round members). The default refuses: only a handler
    /// with a durable owner can commit.
    async fn commit_store_local(
        &self,
        _effects: Vec<crate::runtime::actor::round::StoreLocalEffect>,
    ) -> Result<(), RuntimeEffectControllerError> {
        Err(RuntimeEffectControllerError::new(
            crate::RuntimeErrorCode::RuntimeToolRunShape,
            "a call's store-local effects need a durable owner to commit them",
        ))
    }

    /// The model-facing presentation of a final result (V).
    async fn present(
        &self,
        call_id: &ToolCallId,
        capture: &SingletonCapture,
    ) -> Result<String, SingletonPresentationError>;

    /// Emit the captured stream once the call is presented.
    fn emit_stream(&self, call_id: &ToolCallId, stream: &AttemptStream);

    /// Stage a final's declared start under its key, with no consumer hold:
    /// its rows commit with the call's outcome, so nothing is left for a
    /// hold to release. The registrar answers the process the key holds or
    /// will, or its typed refusal of a start no retry could launch.
    async fn stage_start(
        &self,
        obligation: &DeclaredStartObligation,
    ) -> Result<StartLaunch, String>;
}

/// What a declared start's launch answered.
#[derive(Clone, Debug, PartialEq)]
pub enum StartLaunch {
    /// The start is staged under its key: the process its rows register,
    /// or the one the key already holds, with no rows.
    Staged {
        /// The process.
        handle: crate::ProcessHandleView,
        /// The rows that register it with the call's outcome.
        effect: Option<crate::runtime::actor::round::StoreLocalEffect>,
    },
    /// The registrar refused the start for good, registering nothing.
    Refused(crate::ToolIntentRefusalReason),
}

/// Why a call stopped before it ended. None of these runs a body.
#[derive(Debug, thiserror::Error)]
pub enum SingletonRunError {
    /// The Run cannot serve the call: its owner is gone.
    #[error("tool Run refused: {0}")]
    Cut(#[from] crate::tool_run::RunCutRefusal),
    #[error("isolated start refused: {0}")]
    Isolation(#[from] super::IsolatedStartRefusal),
    /// Admission refused the call, or its admission no longer binds an
    /// available plugin revision.
    #[error("admission refused call: {0}")]
    Admission(#[from] AdmissionRefusal),
    /// The engine or a handler faulted.
    #[error(transparent)]
    Controller(#[from] RuntimeEffectControllerError),
}

impl SingletonRunError {
    pub(crate) fn into_controller_error(self) -> RuntimeEffectControllerError {
        match self {
            Self::Controller(error) => error,
            Self::Admission(refusal) => {
                let mut error = RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeToolRunShape,
                    refusal.to_string(),
                );
                error.cause = Some(crate::RuntimeErrorCause::ToolRunAdmissionRefused {
                    refusal: Box::new(refusal),
                });
                error
            }
            Self::Cut(refusal) => {
                let message = refusal.to_string();
                let mut error = RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeToolRunShape,
                    message,
                );
                error.cause = Some(crate::RuntimeErrorCause::ToolRunCutRefused {
                    refusal: Box::new(refusal),
                });
                error
            }
            Self::Isolation(refusal) => {
                let message = refusal.to_string();
                let mut error = RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeToolRunShape,
                    message,
                );
                error.cause = Some(crate::RuntimeErrorCause::ToolRunIsolationRefused {
                    refusal: Box::new(refusal),
                });
                error
            }
        }
    }
}
