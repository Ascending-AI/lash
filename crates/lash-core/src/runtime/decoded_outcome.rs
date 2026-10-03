//! Decoding a recorded [`RuntimeEffectOutcome`] into the shapes the turn
//! driver and direct completions consume.
//!
//! These readers are the runtime's own: no effect-host implementor needs
//! them, so they live beside their only callers rather than on the
//! integrator-facing outcome type.

use lash_core_execution::runtime::effect::{
    RuntimeAssistantResponseHooksOutcome, RuntimeDirectLlmOutcome, RuntimeLlmCallOutcome,
    ServedExecutionEnvironmentSync,
};

use crate::{RuntimeEffectControllerError, RuntimeEffectKind, RuntimeEffectOutcome};

pub(crate) trait DecodedEffectOutcome: Sized {
    fn into_llm_call(self) -> Result<RuntimeLlmCallOutcome, RuntimeEffectControllerError>;

    fn into_assistant_response_hooks(
        self,
    ) -> Result<RuntimeAssistantResponseHooksOutcome, RuntimeEffectControllerError>;

    fn into_direct_response(self) -> Result<RuntimeDirectLlmOutcome, RuntimeEffectControllerError>;

    /// The sync's result and the tool surface its record names.
    fn into_sync_execution_environment(
        self,
    ) -> Result<ServedExecutionEnvironmentSync, RuntimeEffectControllerError>;
}

impl DecodedEffectOutcome for RuntimeEffectOutcome {
    fn into_llm_call(self) -> Result<RuntimeLlmCallOutcome, RuntimeEffectControllerError> {
        match self {
            Self::LlmCall {
                result,
                text_streamed,
                call_record,
                stream,
            } => Ok(RuntimeLlmCallOutcome {
                result: *result,
                text_streamed,
                call_record,
                stream: *stream,
            }),
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::LlmCall,
                other.kind(),
            )),
        }
    }

    fn into_assistant_response_hooks(
        self,
    ) -> Result<RuntimeAssistantResponseHooksOutcome, RuntimeEffectControllerError> {
        match self {
            Self::AssistantResponseHooks { response, events } => Ok((*response, events)),
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::AssistantResponseHooks,
                other.kind(),
            )),
        }
    }

    fn into_direct_response(self) -> Result<RuntimeDirectLlmOutcome, RuntimeEffectControllerError> {
        match self {
            Self::Direct {
                result,
                call_record,
            } => Ok((*result, call_record)),
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::Direct,
                other.kind(),
            )),
        }
    }

    /// The sync's result and the tool surface its record names.
    fn into_sync_execution_environment(
        self,
    ) -> Result<ServedExecutionEnvironmentSync, RuntimeEffectControllerError> {
        match self {
            Self::SyncExecutionEnvironment {
                prelude,
                result,
                tool_surface,
            } => Ok(ServedExecutionEnvironmentSync {
                prelude,
                result: *result,
                tool_surface,
            }),
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::SyncExecutionEnvironment,
                other.kind(),
            )),
        }
    }
}
