//! The turn driver names the [`ActorContext`] method each effect its machine
//! yields goes through: one method per command group, chosen here by the
//! caller (I0 S1 rule 3, FIG-5194). There is no generic dispatcher on the
//! context.

use crate::ActorContext;
use crate::{
    RuntimeEffectCommand, RuntimeEffectControllerError, RuntimeEffectEnvelope,
    RuntimeEffectLocalExecutor, RuntimeEffectOutcome,
};

/// Issue `envelope` through the group method of its command.
pub(crate) async fn issue_effect(
    cx: &ActorContext,
    envelope: RuntimeEffectEnvelope,
    local: RuntimeEffectLocalExecutor<'_>,
) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
    match &envelope.command {
        RuntimeEffectCommand::BeforeLlmCall { .. }
        | RuntimeEffectCommand::LlmCall { .. }
        | RuntimeEffectCommand::AssistantResponseHooks { .. }
        | RuntimeEffectCommand::Direct { .. }
        | RuntimeEffectCommand::SyncExecutionEnvironment
        | RuntimeEffectCommand::Checkpoint { .. }
        | RuntimeEffectCommand::ResolveTurnConfig { .. }
        | RuntimeEffectCommand::RecordCompactionBase { .. }
        | RuntimeEffectCommand::RenderCompactionPrompt { .. }
        | RuntimeEffectCommand::TraceBoundary { .. } => cx.turn_effect(envelope, local).await,
        RuntimeEffectCommand::ResolveConfigTransaction { .. }
        | RuntimeEffectCommand::ReadSessionCommandRun { .. }
        | RuntimeEffectCommand::CloseRunScope { .. } => cx.session_effect(envelope, local).await,
        RuntimeEffectCommand::TransitionPlugins { .. }
        | RuntimeEffectCommand::AcceptTurnInput { .. }
        | RuntimeEffectCommand::PluginCallbacks { .. } => cx.ingress_effect(envelope, local).await,
        RuntimeEffectCommand::ToolAttempt { .. }
        | RuntimeEffectCommand::PresentToolResult { .. }
        | RuntimeEffectCommand::Trigger { .. } => cx.tool_effect(envelope, local).await,
        RuntimeEffectCommand::Sleep { .. } => cx.wait_effect(envelope, local).await,
        RuntimeEffectCommand::Process { .. } | RuntimeEffectCommand::LoadExecutionEnv { .. } => {
            cx.process_effect(envelope, local).await
        }
        RuntimeEffectCommand::ExecCode { .. }
        | RuntimeEffectCommand::LanguageRuntimeValue { .. } => cx.vm_effect(envelope, local).await,
    }
}
