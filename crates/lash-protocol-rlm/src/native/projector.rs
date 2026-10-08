use super::history::{RlmHistoryRenderInput, build_rlm_history_messages_from_turn};
use crate::dialect::SessionDialect;
use crate::driver::RlmPreambleConfig;
use lash_core::llm::types::{LlmRequestScope, LlmToolChoice};
use lash_core::sansio::ContextProjector;
use lash_core::{
    LlmRequest, ProjectorContext, ProtocolBuildInput, TurnDriverConfig, TurnDriverPreamble,
};
use std::sync::Arc;
pub(crate) fn build_rlm_preamble_with_dialect(
    input: ProtocolBuildInput,
    config: RlmPreambleConfig,
    dialect: Arc<SessionDialect>,
) -> TurnDriverPreamble {
    let tool_catalog = input.tool_catalog.as_ref();
    let tool_names = tool_catalog.tool_names();
    TurnDriverPreamble {
        config: TurnDriverConfig {
            protocol: Arc::new(super::driver::NativeDriver::with_dialect(Arc::clone(
                &dialect,
            ))),
            projector: Arc::new(NativeContextProjector {
                max_output_chars: config.max_output_chars,
                dialect: Arc::clone(&dialect),
            }),
        },
        tool_specs: Arc::new(vec![super::tool::tool_spec(dialect.as_ref())]),
        tool_names,
        writer_formats: input.writer_formats,
    }
}

struct NativeContextProjector {
    max_output_chars: usize,
    dialect: Arc<SessionDialect>,
}

impl ContextProjector<lash_core::HostTurnProtocol> for NativeContextProjector {
    fn has_current_context_prefix(&self) -> bool {
        true
    }

    fn project(
        &self,
        ctx: ProjectorContext<'_>,
    ) -> Result<Arc<LlmRequest>, lash_core::StoredDataCorruption> {
        let mut messages = Vec::new();
        messages.extend(build_rlm_history_messages_from_turn(
            RlmHistoryRenderInput {
                dialect: self.dialect.as_ref(),
                events: ctx.events,
                turn_messages: ctx.messages,
                max_output_chars: self.max_output_chars,
                protocol_iteration: ctx.protocol_iteration + 1,
            },
        )?);

        let mut generation = ctx.config.generation.clone();
        // Both channels execute complete programs. A host's text stop must
        // not truncate a program argument or change the paired sampling cohort.
        generation.suppress_stop_sequences_for_protocol();

        Ok(Arc::new(LlmRequest {
            model: ctx.config.model.clone(),
            instructions: None,
            messages,

            tools: Arc::new(vec![super::tool::tool_spec(self.dialect.as_ref())]),
            tool_choice: LlmToolChoice::Auto,
            attachment_acceptance: Arc::clone(&ctx.config.attachment_acceptance),
            scope: LlmRequestScope::new(
                ctx.config.session_id.clone(),
                ctx.config.agent_frame_id.clone(),
                format!(
                    "{}:sansio:rlm:{}",
                    ctx.config.session_id, ctx.protocol_iteration
                ),
            ),
            output_spec: None,
            stream_events: None,
            generation,
            provider_trace: None,
        }))
    }
}

#[cfg(test)]
pub(crate) fn testing_projector(
    dialect: Arc<SessionDialect>,
) -> Arc<dyn ContextProjector<lash_core::HostTurnProtocol>> {
    Arc::new(NativeContextProjector {
        max_output_chars: 1000,
        dialect,
    })
}
