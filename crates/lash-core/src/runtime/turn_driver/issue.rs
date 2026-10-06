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
        | RuntimeEffectCommand::RecoverFollowOn { .. }
        | RuntimeEffectCommand::TraceBoundary { .. } => cx.turn_effect(envelope, local).await,
        RuntimeEffectCommand::ResolveConfigTransaction { .. }
        | RuntimeEffectCommand::ReadSessionCommandRun { .. }
        | RuntimeEffectCommand::CloseRunScope { .. }
        | RuntimeEffectCommand::BeginSessionClose { .. } => {
            cx.session_effect(envelope, local).await
        }
        RuntimeEffectCommand::TransitionPlugins { .. }
        | RuntimeEffectCommand::AdmitShift { .. }
        | RuntimeEffectCommand::DrawRunStart { .. }
        | RuntimeEffectCommand::AcceptTurnInput { .. }
        | RuntimeEffectCommand::PluginCallbacks { .. } => cx.shift_effect(envelope, local).await,
        RuntimeEffectCommand::ToolAttempt { .. }
        | RuntimeEffectCommand::RestoreRunMaterial { .. }
        | RuntimeEffectCommand::PresentToolResult { .. }
        | RuntimeEffectCommand::Trigger { .. }
        | RuntimeEffectCommand::IngestTriggerOccurrence { .. }
        | RuntimeEffectCommand::AdmitTriggerDelivery { .. } => {
            cx.tool_effect(envelope, local).await
        }
        RuntimeEffectCommand::Sleep { .. }
        | RuntimeEffectCommand::AwaitEvent { .. }
        | RuntimeEffectCommand::PeekAwaitEvent { .. } => cx.wait_effect(envelope, local).await,
        RuntimeEffectCommand::Process { .. } | RuntimeEffectCommand::LoadExecutionEnv { .. } => {
            cx.process_effect(envelope, local).await
        }
        RuntimeEffectCommand::ExecCode { .. }
        | RuntimeEffectCommand::LanguageRuntimeValue { .. } => cx.vm_effect(envelope, local).await,
    }
}
