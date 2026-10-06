//! The system prompt a compaction's summarizer call carries (FIG-4589).
//!
//! The prompt is the protocol plugin's: it renders it from the session's
//! recorded config, without tools or execution prose, since the summarizer
//! request ships no tools. The render is one recorded step that runs before
//! the summarizer call, on every compaction path (the administrative command,
//! context pressure and overflow recovery). A redrive of a compaction that
//! crashed between that step and its commit serves the recorded text and
//! never renders again, so a prompt command or a redeploy in between cannot
//! change what an already-journaled summarizer call was sent.

use crate::ActorContext;
use std::sync::Arc;

use crate::runtime::effect::executor::RuntimeEffectLocalRunner;
use crate::{
    EffectAddress, RuntimeAttribution, RuntimeEffectCommand, RuntimeEffectControllerError,
    RuntimeEffectEnvelope, RuntimeEffectInvocation, RuntimeEffectOutcome, RuntimeErrorCode,
};

/// What one compaction prompt renders from: recorded data only.
#[derive(Clone)]
pub(in crate::runtime) struct CompactionPromptInput {
    pub(in crate::runtime) session_id: crate::SessionId,
    pub(in crate::runtime) protocol_session: Arc<dyn crate::plugin::ProtocolSessionPlugin>,
    /// The configuration the compaction runs under: the running run's
    /// admitted one, or the head's for a command no run executes.
    pub(in crate::runtime) plugin_config: crate::AdmittedPluginConfig,
    pub(in crate::runtime) subagent: Option<crate::SubagentSessionContext>,
}

/// Which compaction of its scope a prompt belongs to.
pub(in crate::runtime) enum CompactionPromptKey<'a> {
    /// An administrative compaction: its ordinal in its run, the one its
    /// recorded base is keyed by.
    Ordinal(u32),
    /// The context-pressure step of one physical turn.
    Turn(&'a str),
}

impl CompactionPromptKey<'_> {
    fn replay_key(&self) -> String {
        match self {
            Self::Ordinal(ordinal) => format!("compaction-prompt:{ordinal}"),
            Self::Turn(turn_id) => format!("compaction-prompt:turn:{turn_id}"),
        }
    }
}

/// Render the compaction's system prompt as one recorded step under
/// `controller`, or serve the text its first execution recorded. `None` when
/// the render is empty.
pub(in crate::runtime) async fn recorded_compaction_prompt(
    controller: &ActorContext,
    key: CompactionPromptKey<'_>,
    input: CompactionPromptInput,
) -> Result<Option<Arc<str>>, RuntimeEffectControllerError> {
    let replay_key = key.replay_key();
    let session_id = input.session_id.clone();
    let invocation = RuntimeEffectInvocation::new(
        EffectAddress::new(controller.execution_scope().clone(), replay_key.clone())?,
        RuntimeAttribution::for_session(session_id.clone()),
        replay_key,
    );
    controller
        .turn_effect(
            RuntimeEffectEnvelope::new(
                invocation,
                RuntimeEffectCommand::RenderCompactionPrompt {
                    session: session_id,
                },
            ),
            lash_core_execution::core_internal::owned_runner_executor(
                Box::new(RenderCompactionPromptRunner { input }),
                None,
            ),
        )
        .await
        .and_then(RuntimeEffectOutcome::into_compaction_prompt)
}

/// The first execution of one `RenderCompactionPrompt` step: the protocol
/// plugin's render over the recorded config and an empty tool surface.
struct RenderCompactionPromptRunner {
    input: CompactionPromptInput,
}

#[async_trait::async_trait]
impl RuntimeEffectLocalRunner for RenderCompactionPromptRunner {
    async fn execute(
        self: Box<Self>,
        envelope: RuntimeEffectEnvelope,
        _effect_attempt: Option<crate::EffectAttempt>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let RuntimeEffectCommand::RenderCompactionPrompt { .. } = &envelope.command else {
            return Err(RuntimeEffectControllerError::new(
                RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                format!(
                    "compaction-prompt executor cannot execute {} command",
                    envelope.command.kind().as_str()
                ),
            ));
        };
        let tool_catalog = crate::ToolCatalog::default();
        let rendered = self
            .input
            .protocol_session
            .render_system_prompt(crate::plugin::SystemPromptContext {
                plugin_config: &self.input.plugin_config,
                tool_catalog: &tool_catalog,
                subagent: self.input.subagent.as_ref(),
                purpose: crate::plugin::SystemPromptPurpose::Compaction,
            })
            .await
            .map_err(|error| {
                RuntimeEffectControllerError::new(
                    RuntimeErrorCode::ContextCompaction,
                    format!("the compaction's system prompt did not render: {error}"),
                )
            })?;
        let system_prompt = rendered.trim();
        Ok(RuntimeEffectOutcome::RenderCompactionPrompt {
            system_prompt: (!system_prompt.is_empty()).then(|| Arc::from(system_prompt)),
        })
    }
}
