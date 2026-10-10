pub(crate) mod history;

use std::sync::Arc;

use crate::rlm_support::RlmCompletion;
#[cfg(any(test, feature = "testing"))]
use lash_core::llm::types::{LlmContentBlock, LlmMessage};
use lash_core::llm::types::{LlmRequestScope, LlmToolChoice};
use lash_core::sansio::ContextProjector;
use lash_core::{
    LlmRequest, ProjectorContext, ProtocolBuildInput, TurnDriverConfig, TurnDriverPreamble,
};

use crate::dialect::SessionDialect;
#[cfg(test)]
use crate::projection::rlm_protocol_event;

#[cfg(any(test, feature = "testing"))]
use history::render_history_messages;
use history::{RlmHistoryRenderInput, build_rlm_history_messages_from_turn};

/// A prompt-only RLM preamble's configuration: the host's selected dialect
/// and the prompt knobs.
#[derive(Clone)]
pub struct RlmProjectorConfig {
    pub dialect: crate::dialect::CellDialect,
    pub max_output_chars: usize,
}

pub(crate) struct RlmPreambleConfig {
    pub(crate) max_output_chars: usize,
}

impl RlmProjectorConfig {
    /// A preamble in `dialect`, with the default prompt knobs.
    pub fn new(dialect: crate::dialect::CellDialect) -> Self {
        Self {
            dialect,
            max_output_chars: 10_000,
        }
    }
}

pub fn build_rlm_preamble(
    input: ProtocolBuildInput,
    config: RlmProjectorConfig,
) -> TurnDriverPreamble {
    let dialect: Arc<SessionDialect> =
        Arc::new(SessionDialect::prompt_only(config.dialect.clone()));
    build_rlm_preamble_with_dialect(
        input,
        RlmPreambleConfig {
            max_output_chars: config.max_output_chars,
        },
        dialect,
    )
}

pub(crate) fn build_rlm_preamble_with_dialect(
    input: ProtocolBuildInput,
    config: RlmPreambleConfig,
    dialect: Arc<SessionDialect>,
) -> TurnDriverPreamble {
    let tool_catalog = input.tool_catalog.as_ref();
    let tool_names = tool_catalog.tool_names();
    TurnDriverPreamble {
        config: TurnDriverConfig {
            protocol: Arc::new(crate::protocol::RlmDriver::with_dialect(Arc::clone(
                &dialect,
            ))),
            projector: Arc::new(RlmContextProjector {
                max_output_chars: config.max_output_chars,
                dialect: Arc::clone(&dialect),
            }),
        },
        tool_specs: Arc::new(Vec::new()),
        tool_names,
        writer_formats: input.writer_formats,
    }
}

struct RlmContextProjector {
    max_output_chars: usize,
    dialect: Arc<SessionDialect>,
}

impl ContextProjector<lash_core::HostTurnProtocol> for RlmContextProjector {
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
        // The paired-tag grammar is RLM's response boundary. Provider wire
        // stops, including caller-supplied ones, could withhold that literal
        // boundary and leave the parser with a truncated cell. The boundary is
        // the dialect's, but no dialect hands it to the provider as a stop.
        generation.suppress_stop_sequences_for_protocol();

        Ok(Arc::new(LlmRequest {
            instructions: None,
            model: ctx.config.model.clone(),
            messages,

            tools: Arc::new(Vec::new()),
            tool_choice: LlmToolChoice::None,
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

/// The REQUIRED OUTPUT block: the contract a finish value must match, under
/// either termination that states one.
pub(crate) fn required_output_block(
    dialect: &SessionDialect,
    termination: &RlmCompletion,
) -> Option<String> {
    termination
        .finish_schema()
        .map(|schema| dialect.required_output_contract(schema.as_value()))
}

impl RlmContextProjector {
    #[cfg(test)]
    fn format_history(&self, events: &[lash_core::SessionHistoryRecord]) -> String {
        let messages = render_history_messages(&RlmHistoryRenderInput {
            dialect: self.dialect.as_ref(),
            events,
            turn_messages: &lash_core::facade_support::MessageSequence::default(),
            max_output_chars: self.max_output_chars,
            protocol_iteration: 0,
        })
        .expect("valid history fixture");
        messages
            .iter()
            .flat_map(|message| message.blocks.iter())
            .filter_map(|block| match block {
                LlmContentBlock::Text { text, .. } => Some(text.as_ref()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

/// Render one durable assistant message through the production RLM history
/// renderer for provider conformance tests.
#[cfg(feature = "testing")]
pub(crate) fn render_conformance_history_message(
    message: lash_core::Message,
) -> Result<LlmMessage, String> {
    let dialect = SessionDialect::prompt_only(crate::dialect::CellDialect::typescript());
    let events = [lash_core::SessionHistoryRecord::Conversation(
        lash_core::session_model::ConversationRecord::from_message(message),
    )];

    let rendered = render_history_messages(&RlmHistoryRenderInput {
        dialect: &dialect,
        events: &events,
        turn_messages: &lash_core::facade_support::MessageSequence::default(),
        max_output_chars: 10_000,
        protocol_iteration: 0,
    })
    .map_err(|error| error.to_string())?;
    let attachment_count = rendered
        .iter()
        .flat_map(|message| message.blocks.iter())
        .flat_map(LlmContentBlock::attachments)
        .count();
    match rendered.as_slice() {
        [message] if attachment_count == 0 => Ok(message.clone()),
        _ => Err(format!(
            "RLM conformance history rendered {} messages and {} attachments; expected exactly one message and no attachments",
            rendered.len(),
            attachment_count
        )),
    }
}

#[cfg(test)]
pub(crate) mod tests;
