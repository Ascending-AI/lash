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
            Self::ToolAttempt { launch } => Ok(ToolAttemptEffectOutcome { launch: *launch }),
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::ToolAttempt,
                other.kind(),
            )),
        }
    }

    /// Unpacks the recorded presentation of one settled tool result.
    pub(crate) fn into_tool_presentation(
        self,
    ) -> Result<crate::runtime::effect::ToolPresentation, RuntimeEffectControllerError> {
        match self {
            Self::PresentToolResult { presentation } => Ok(*presentation),
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
            Self::Process { .. } => RuntimeEffectKind::Process,
            Self::ExecCode { .. } => RuntimeEffectKind::ExecCode,
            Self::AcceptTurnInput { .. } => RuntimeEffectKind::AcceptTurnInput,
            Self::PluginCallbacks { .. } => RuntimeEffectKind::PluginCallbacks,

            Self::TraceBoundary { .. } => RuntimeEffectKind::TraceBoundary,
            Self::ResolveTurnConfig { .. } => RuntimeEffectKind::ResolveTurnConfig,
            Self::RecordCompactionBase { .. } => RuntimeEffectKind::RecordCompactionBase,
            Self::ResolveConfigTransaction { .. } => RuntimeEffectKind::ResolveConfigTransaction,
            Self::ReadSessionCommandRun { .. } => RuntimeEffectKind::ReadSessionCommandRun,
            Self::CloseRunScope => RuntimeEffectKind::CloseRunScope,
            Self::Checkpoint { .. } => RuntimeEffectKind::Checkpoint,
            Self::SyncExecutionEnvironment { .. } => RuntimeEffectKind::SyncExecutionEnvironment,
            Self::LoadExecutionEnv { .. } => RuntimeEffectKind::LoadExecutionEnv,
            Self::Sleep => RuntimeEffectKind::Sleep,
            Self::LanguageRuntimeValue { .. } => RuntimeEffectKind::LanguageRuntimeValue,
        }
    }
}
