//! The system prompt a compaction's summarizer call carries (FIG-4589).
//!
//! It is the session's prompt sections composed for the compaction purpose
//! (ADR 0133) under the session's recorded plan and config, over an empty
//! tool offer: only sections that declare the compaction purpose render, so
//! no tool or execution prose reaches a request that ships no tools. The
//! summarizer request has one instruction field, so the sections of both
//! placements fill it, initial instructions first. The render is one
//! recorded step that runs before
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
    pub(in crate::runtime) plugins: Arc<crate::plugin::PluginSession>,
    /// The configuration the compaction runs under: the running run's
    /// admitted one, or the head's for a command no run executes.
    pub(in crate::runtime) plugin_config: crate::AdmittedPluginConfig,
    /// The host's prompt plan recorded with that configuration.
    pub(in crate::runtime) prompt_plan: crate::prompt_sections::PromptPlan,
    pub(in crate::runtime) frame: Option<crate::FrameNodeId>,
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

/// The first execution of one `RenderCompactionPrompt` step: the session's
/// sections composed for the compaction purpose over an empty tool offer.
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
        use crate::plugin::prompt::{
            PromptCall, PromptCut, PromptCutParts, PromptPurpose, PromptRenderPool,
        };
        let input = &self.input;
        let cut = PromptCut::new(PromptCutParts {
            call: PromptCall {
                session_id: input.session_id.clone(),
                frame: input.frame.clone(),
                run: None,
                turn: None,
                iteration: 0,
                call: 0,
                purpose: PromptPurpose::Compaction,
            },
            config: input.plugin_config.clone(),
            session: None,
            offered: Default::default(),
            model: Default::default(),
            history: Default::default(),
            namespaces: input.plugins.committed_namespaces(),
        })
        .with_subagent(input.subagent.clone());
        let composed = input
            .plugins
            .prompt_catalog()
            .compose(
                &input.prompt_plan,
                &PromptPurpose::Compaction,
                Arc::new(cut),
                PromptRenderPool::shared(),
            )
            .await
            .map_err(|error| {
                RuntimeEffectControllerError::new(
                    RuntimeErrorCode::ContextCompaction,
                    format!("the compaction's system prompt did not compose: {error}"),
                )
            })?;
        let system_prompt = [composed.initial_instructions, composed.current_context]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join("\n\n");
        let system_prompt = system_prompt.trim();
        Ok(RuntimeEffectOutcome::RenderCompactionPrompt {
            system_prompt: (!system_prompt.is_empty()).then(|| Arc::from(system_prompt)),
        })
    }
}
