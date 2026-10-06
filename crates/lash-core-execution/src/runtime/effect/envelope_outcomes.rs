//! Recorded preparation and typed outcome consumers.

use super::*;

/// A journaled environment's preparation, render result and tool surface.
/// The preparation is journaled by digest; its consumer reads it from the
/// store set ([`TurnPreludeRef::read`]).
#[derive(Debug)]
pub struct ServedExecutionEnvironmentSync {
    pub prelude: TurnPreludeRef,
    pub result: Result<ExecutionEnvironmentSync, ExecutionEnvironmentSyncFailure>,
    pub tool_surface: Vec<crate::ToolDefinition>,
}

impl RuntimeEffectOutcome {
    pub fn into_before_llm_call(
        self,
    ) -> Result<
        Result<Option<crate::ProtocolLlmCallAction>, crate::PluginError>,
        RuntimeEffectControllerError,
    > {
        match self {
            Self::BeforeLlmCall { decision } => Ok(decision),
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::BeforeLlmCall,
                other.kind(),
            )),
        }
    }

    pub(crate) fn into_tool_attempt_effect(
        self,
    ) -> Result<ToolAttemptEffectOutcome, RuntimeEffectControllerError> {
        match self {
            Self::ToolAttempt {
                launch,
                triggers,
                capture,
            } => {
                let capture = capture.map(|capture| *capture).unwrap_or_default();
                capture.validate()?;
                Ok(ToolAttemptEffectOutcome {
                    launch: *launch,
                    triggers,
                    capture,
                })
            }
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::ToolAttempt,
                other.kind(),
            )),
        }
    }

    /// Unpacks the recorded presentation of one settled tool result.
    ///
    /// Validates the record rather than trusting it: a journal entry written
    /// by a build whose presentation format this build cannot read completely
    /// is refused here, where the outcome is consumed, instead of serving the
    /// model a prefix of what the chain produced.
    pub(crate) fn into_tool_presentation(
        self,
    ) -> Result<crate::runtime::effect::ToolPresentation, RuntimeEffectControllerError> {
        match self {
            Self::PresentToolResult { presentation } => {
                presentation.validate()?;
                Ok(*presentation)
            }
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::PresentToolResult,
                other.kind(),
            )),
        }
    }

    /// Extracts the process outcome for effect-host implementors while executing or replaying a
    /// runtime effect.
    pub fn into_process(self) -> Result<ProcessEffectOutcome, RuntimeEffectControllerError> {
        match self {
            Self::Process { result } => Ok(result),
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::Process,
                other.kind(),
            )),
        }
    }

    /// Extracts the trigger outcome for effect-host implementors while executing or replaying a
    /// runtime effect.
    pub fn into_trigger(self) -> Result<crate::TriggerEffectResult, RuntimeEffectControllerError> {
        match self {
            Self::Trigger { result } => Ok(*result),
            other => Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectWrongOutcome,
                format!("expected trigger outcome, got {}", other.kind().as_str()),
            )),
        }
    }

    /// Extracts the receipt an emission's recorded ingest was answered
    /// (FIG-4503).
    pub fn into_trigger_ingress_receipt(
        self,
    ) -> Result<crate::TriggerIngressReceipt, RuntimeEffectControllerError> {
        match self {
            Self::IngestTriggerOccurrence { receipt } => Ok(*receipt),
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::IngestTriggerOccurrence,
                other.kind(),
            )),
        }
    }

    /// Extracts the binding an emission recorded for one trigger delivery
    /// (FIG-4297).
    pub(crate) fn into_trigger_delivery_admission(
        self,
    ) -> Result<crate::TriggerDeliveryAdmission, RuntimeEffectControllerError> {
        match self {
            Self::AdmitTriggerDelivery { admission } => Ok(*admission),
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::AdmitTriggerDelivery,
                other.kind(),
            )),
        }
    }

    pub fn into_exec_code(
        self,
    ) -> Result<Result<ExecResponse, crate::ExecCodeFailure>, RuntimeEffectControllerError> {
        match self {
            Self::ExecCode { result } => Ok(*result),
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::ExecCode,
                other.kind(),
            )),
        }
    }

    pub fn into_checkpoint(
        self,
    ) -> Result<(CheckpointOutcome, CheckpointAdmittedSet), RuntimeEffectControllerError> {
        match self {
            Self::Checkpoint { result, admitted } => Ok((result, *admitted)),
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::Checkpoint,
                other.kind(),
            )),
        }
    }

    /// The immutable environment a recorded load holds.
    pub fn into_execution_env_ref(
        self,
    ) -> Result<crate::ProcessExecutionEnvRef, RuntimeEffectControllerError> {
        match self {
            Self::LoadExecutionEnv { env } => Ok(env),
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::LoadExecutionEnv,
                other.kind(),
            )),
        }
    }

    pub fn into_await_event(self) -> Result<crate::Resolution, RuntimeEffectControllerError> {
        match self {
            Self::AwaitEvent { resolution } => Ok(resolution),
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::AwaitEvent,
                other.kind(),
            )),
        }
    }

    /// Extracts the peek await event outcome for effect-host implementors while executing or
    /// replaying a runtime effect.
    pub fn into_peek_await_event(
        self,
    ) -> Result<Option<crate::Resolution>, RuntimeEffectControllerError> {
        match self {
            Self::PeekAwaitEvent { resolution } => Ok(resolution),
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::PeekAwaitEvent,
                other.kind(),
            )),
        }
    }

    /// Extracts a journaled language-runtime value.
    pub fn into_language_runtime_value(
        self,
    ) -> Result<serde_json::Value, RuntimeEffectControllerError> {
        match self {
            Self::LanguageRuntimeValue { value } => Ok(value),
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::LanguageRuntimeValue,
                other.kind(),
            )),
        }
    }

    /// Exposes kind to effect-host implementors while executing or replaying a runtime effect.
    pub fn kind(&self) -> RuntimeEffectKind {
        match self {
            Self::PluginState { kind, .. } => *kind,
            Self::TransitionPlugins { .. } => RuntimeEffectKind::TransitionPlugins,
            Self::BeforeLlmCall { .. } => RuntimeEffectKind::BeforeLlmCall,
            Self::LlmCall { .. } => RuntimeEffectKind::LlmCall,
            Self::AssistantResponseHooks { .. } => RuntimeEffectKind::AssistantResponseHooks,
            Self::Direct { .. } => RuntimeEffectKind::Direct,
            Self::ToolAttempt { .. } => RuntimeEffectKind::ToolAttempt,
            Self::PresentToolResult { .. } => RuntimeEffectKind::PresentToolResult,
            Self::Trigger { .. } => RuntimeEffectKind::Trigger,
            Self::IngestTriggerOccurrence { .. } => RuntimeEffectKind::IngestTriggerOccurrence,
            Self::AdmitTriggerDelivery { .. } => RuntimeEffectKind::AdmitTriggerDelivery,
            Self::Process { .. } => RuntimeEffectKind::Process,
            Self::ExecCode { .. } => RuntimeEffectKind::ExecCode,
            Self::AcceptTurnInput { .. } => RuntimeEffectKind::AcceptTurnInput,
            Self::PluginCallbacks { .. } => RuntimeEffectKind::PluginCallbacks,
            Self::RecoverFollowOn { .. } => RuntimeEffectKind::RecoverFollowOn,
            Self::RestoreRunMaterial { .. } => RuntimeEffectKind::RestoreRunMaterial,
            Self::AdmitShift { .. } => RuntimeEffectKind::AdmitShift,
            Self::TraceBoundary { .. } => RuntimeEffectKind::TraceBoundary,
            Self::DrawRunStart { .. } => RuntimeEffectKind::DrawRunStart,
            Self::ResolveTurnConfig { .. } => RuntimeEffectKind::ResolveTurnConfig,
            Self::RecordCompactionBase { .. } => RuntimeEffectKind::RecordCompactionBase,
            Self::RenderCompactionPrompt { .. } => RuntimeEffectKind::RenderCompactionPrompt,
            Self::ResolveConfigTransaction { .. } => RuntimeEffectKind::ResolveConfigTransaction,
            Self::ReadSessionCommandRun { .. } => RuntimeEffectKind::ReadSessionCommandRun,
            Self::CloseRunScope => RuntimeEffectKind::CloseRunScope,
            Self::Checkpoint { .. } => RuntimeEffectKind::Checkpoint,
            Self::SyncExecutionEnvironment { .. } => RuntimeEffectKind::SyncExecutionEnvironment,
            Self::LoadExecutionEnv { .. } => RuntimeEffectKind::LoadExecutionEnv,
            Self::Sleep => RuntimeEffectKind::Sleep,
            Self::AwaitEvent { .. } => RuntimeEffectKind::AwaitEvent,
            Self::PeekAwaitEvent { .. } => RuntimeEffectKind::PeekAwaitEvent,
            Self::LanguageRuntimeValue { .. } => RuntimeEffectKind::LanguageRuntimeValue,
        }
    }
}
