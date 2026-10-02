pub(crate) mod history;

use std::sync::Arc;

#[cfg(any(test, feature = "testing"))]
use lash_core::llm::types::{LlmContentBlock, LlmMessage};
use lash_core::llm::types::{LlmRequestScope, LlmToolChoice};
use lash_core::sansio::ContextProjector;
use lash_core::{
    LlmRequest, ProjectorContext, ProtocolBuildInput, TurnDriverConfig, TurnDriverPreamble,
};
use lash_lashlang_runtime::LashlangSurface;
use lash_rlm_types::{RlmFinalAnswerFormat, RlmTermination, RlmTurnOptions};

use crate::dialect::SessionDialect;
#[cfg(test)]
use crate::projection::rlm_protocol_event;
use crate::rlm_support::{decode_rlm_options, effective_budget_tokens};

#[cfg(any(test, feature = "testing"))]
use history::render_history_messages;
use history::{RlmHistoryRenderInput, build_rlm_history_messages_from_turn};

/// A prompt-only RLM preamble's configuration: the host's selected dialect
/// and the prompt knobs.
#[derive(Clone)]
pub struct RlmProjectorConfig {
    pub dialect: Arc<dyn crate::dialect::Dialect>,
    pub discovery: Option<lash_core::ToolDiscovery>,
    pub max_output_chars: usize,
    pub max_budget_tokens: Option<usize>,
    pub prompt_features: crate::protocol::RlmPromptFeatures,
    pub lashlang_surface: LashlangSurface,
}

pub(crate) struct RlmPreambleConfig {
    pub(crate) max_output_chars: usize,
    pub(crate) max_budget_tokens: Option<usize>,
    pub(crate) prompt_features: crate::protocol::RlmPromptFeatures,
}

impl RlmProjectorConfig {
    /// A preamble in `dialect`, with the default prompt knobs.
    pub fn new(dialect: Arc<dyn crate::dialect::Dialect>) -> Self {
        Self {
            dialect,
            discovery: None,
            max_output_chars: 10_000,
            max_budget_tokens: None,
            prompt_features: crate::protocol::RlmPromptFeatures::default(),
            lashlang_surface: LashlangSurface::default(),
        }
    }
}

pub fn build_rlm_preamble(
    input: ProtocolBuildInput,
    config: RlmProjectorConfig,
) -> TurnDriverPreamble {
    let dialect: Arc<SessionDialect> = Arc::new(SessionDialect::prompt_only(
        Arc::clone(&config.dialect),
        config.lashlang_surface.clone(),
    ));
    build_rlm_preamble_with_dialect(
        input,
        RlmPreambleConfig {
            max_output_chars: config.max_output_chars,
            max_budget_tokens: config.max_budget_tokens,
            prompt_features: config.prompt_features,
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
                prompt_features: config.prompt_features,
                max_output_chars: config.max_output_chars,
                max_budget_tokens: config.max_budget_tokens,
                dialect: Arc::clone(&dialect),
            }),
        },
        tool_specs: Arc::new(Vec::new()),
        tool_names,
        writer_formats: input.writer_formats,
    }
}

#[cfg(test)]
mod catalogue_tests {
    use super::*;
    use lash_lashlang_runtime::{ToolBinding, ToolDefinitionBindingExt};

    fn tool(
        name: &str,
        module: &'static str,
        operation: &'static str,
    ) -> lash_core::ToolDefinition {
        lash_core::ToolDefinition::raw(
            format!("tool:{name}"),
            name,
            format!("Tool {name}"),
            serde_json::json!({
                "type": "object",
                "properties": { "query": { "type": "string" } },
                "required": ["query"]
            }),
            serde_json::json!({ "type": "string" }),
        )
        .expect("valid declared tool schemas")
        .with_tool_binding(ToolBinding::new([module], operation))
    }

    /// The execution section a session configured as `config` renders over
    /// `catalog` on the cell channel.
    fn execution(config: &RlmProjectorConfig, catalog: &lash_core::ToolCatalog) -> String {
        let dialect = SessionDialect::prompt_only(
            Arc::clone(&config.dialect),
            config.lashlang_surface.clone(),
        );
        crate::system_prompt::execution_section(
            &dialect,
            &crate::system_prompt::RlmSystemPromptBehaviour {
                channel: crate::plugin::RlmChannel::Cell,
                prompt_features: config.prompt_features,
                discovery: config.discovery.as_ref(),
            },
            catalog,
        )
        .joined()
    }

    #[test]
    fn rlm_preamble_uses_resolved_tool_catalog_without_search_tool_special_cases() {
        let definitions = vec![
            tool("search_tools", "tools", "search"),
            tool("grep", "files", "grep"),
        ];
        let surface = Arc::new(lash_core::ToolCatalog::from_tool_definitions(definitions));
        let config = RlmProjectorConfig {
            lashlang_surface: LashlangSurface::new(
                lashlang::LashlangAbilities::all(),
                lashlang::LashlangLanguageFeatures::default(),
                lashlang::LashlangHostCatalog::tool_default(["search_tools", "grep"]),
            ),
            ..RlmProjectorConfig::new(Arc::new(crate::dialect::TypescriptDialect))
        };

        let preamble = build_rlm_preamble(
            lash_core::ProtocolBuildInput {
                tool_catalog: Arc::clone(&surface),
                plugin_extensions: Default::default(),
                trigger_events: Default::default(),
                writer_formats: lash_core::build_newest_writer_formats(),
            },
            config.clone(),
        );

        assert_eq!(preamble.tool_names.as_ref(), &vec!["search_tools", "grep"]);
        // The TypeScript dialect renders the tool catalogue inline in the
        // execution section.
        let prompt = execution(&config, &surface);
        assert!(prompt.contains("tools.search"));
        assert!(prompt.contains("files.grep"));
        assert!(!prompt.contains("search_tools("));
    }

    #[test]
    fn the_execution_section_uses_lashlang_host_environment_abilities() {
        let definitions = vec![tool("grep", "files", "grep")];
        let surface = lash_core::ToolCatalog::from_tool_definitions(definitions);
        let config = RlmProjectorConfig {
            lashlang_surface: LashlangSurface::new(
                lashlang::LashlangAbilities::default(),
                lashlang::LashlangLanguageFeatures::default(),
                lashlang::LashlangHostCatalog::tool_default(["grep"]),
            ),
            ..RlmProjectorConfig::new(Arc::new(crate::dialect::TypescriptDialect))
        };

        let prompt = execution(&config, &surface);
        assert!(!prompt.contains("process name"));
        assert!(!prompt.contains("sleep for"));
        assert!(prompt.contains("### Tools"));
    }

    #[test]
    fn finish_required_schema_finalization_prompt_requires_value() {
        let prompt = rlm_finalization_prompt(&RlmTermination::FinishRequired {
            schema: Some(
                lash_sansio::JsonSchema::admit(serde_json::json!({ "type": "object" }))
                    .expect("valid finish schema"),
            ),
        });

        assert!(prompt.contains("finish(value)"));
        assert!(prompt.contains("REQUIRED OUTPUT"));
        assert!(prompt.contains(
            "Every response, including the last, acts inside a paired `<typescript>...</typescript>` block"
        ));
        assert!(prompt.contains("Do not call `finish(value)` until the answer is in hand"));
        assert!(prompt.contains("the final response's block calls `finish(value)`"));
        assert!(prompt.contains("prose alone never ends this turn"));
        assert!(prompt.contains("Never announce an action without the block that performs it"));
    }

    #[test]
    fn natural_finalization_prompt_allows_direct_prose() {
        // `RlmTermination::default()` is the `Natural` variant, so the
        // default-path prompt is this same guidance.
        assert_eq!(
            rlm_finalization_prompt(&RlmTermination::default()),
            rlm_finalization_prompt(&RlmTermination::Natural)
        );
        let prompt = rlm_finalization_prompt(&RlmTermination::Natural);

        assert!(prompt.contains("Natural termination:"));
        assert!(prompt.contains("prose alone ends this turn as the final answer"));
        assert!(prompt.contains("finish(value)"));
        assert!(prompt.contains("write prose only when no work remains"));
        assert!(prompt.contains("otherwise perform the next step in a block"));
        assert!(prompt.contains("inside the program to return a computed value"));
    }
}

struct RlmContextProjector {
    prompt_features: crate::protocol::RlmPromptFeatures,
    max_output_chars: usize,
    max_budget_tokens: Option<usize>,
    dialect: Arc<SessionDialect>,
}

impl ContextProjector<lash_core::HostTurnProtocol> for RlmContextProjector {
    #[expect(
        clippy::expect_used,
        reason = "recorded turn options are validated by the plugin at session open; decode_rlm_options only errs on options that validation already refused"
    )]
    fn project(&self, ctx: ProjectorContext<'_>) -> Arc<LlmRequest> {
        let options = decode_rlm_options(&ctx.config.termination)
            .expect("RLM turn options are validated before prompt projection");
        let termination = options.effective_termination();
        let finalization = self
            .dialect
            .finalization_copy(&termination, crate::plugin::RlmChannel::Cell);
        let required_output = required_output_block(&self.dialect, &termination);
        let vocabulary = self.dialect.prompt_vocabulary();
        let final_answer_format = final_answer_format_prompt(&options, vocabulary);
        let budget_suffix = crate::rlm_support::format_budget_suffix_with_vocabulary(
            ctx.protocol_iteration + 1,
            ctx.environment.projector_turn_inputs.prompt_usage.as_ref(),
            effective_budget_tokens(
                self.max_budget_tokens,
                Some(ctx.config.model.context_window_tokens()),
            ),
            vocabulary,
            self.prompt_features.decomposition,
        );
        let bound_variables_prompt = ctx
            .environment
            .projector_turn_inputs
            .bound_variables_prompt
            .as_deref()
            .unwrap_or("");

        let mut messages = Vec::new();

        messages.extend(build_rlm_history_messages_from_turn(
            RlmHistoryRenderInput {
                images: self.prompt_features.images,
                dialect: self.dialect.as_ref(),
                events: ctx.events,
                turn_messages: ctx.messages,
                turn_causes: ctx.turn_causes,
                max_output_chars: self.max_output_chars,
                protocol_iteration: ctx.protocol_iteration + 1,
                finalization: &finalization,
                required_output: required_output.as_deref(),
                final_answer_format: final_answer_format.as_deref(),
                budget_suffix: budget_suffix.as_deref(),
                bound_variables: bound_variables_prompt,
            },
        ));

        let mut generation = ctx.config.generation.clone();
        // The paired-tag grammar is RLM's response boundary. Provider wire
        // stops, including caller-supplied ones, could withhold that literal
        // boundary and leave the parser with a truncated cell. The boundary is
        // the dialect's, but no dialect hands it to the provider as a stop.
        generation.suppress_stop_sequences_for_protocol();

        Arc::new(LlmRequest {
            instructions: (!ctx.environment.system_prompt.trim().is_empty())
                .then(|| Arc::from(ctx.environment.system_prompt.trim())),
            model: ctx.config.model.clone(),
            messages,
            resolved_stored: Default::default(),
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
        })
    }
}

fn required_output_block(dialect: &SessionDialect, termination: &RlmTermination) -> Option<String> {
    match termination {
        RlmTermination::FinishRequired {
            schema: Some(schema),
        } => Some(dialect.required_output_contract(schema.as_value())),
        _ => None,
    }
}

fn final_answer_format_prompt(
    options: &RlmTurnOptions,
    vocabulary: crate::dialect::DialectPromptVocabulary,
) -> Option<String> {
    let termination = options.effective_termination();
    if matches!(
        termination,
        RlmTermination::FinishRequired { schema: Some(_) }
    ) {
        return None;
    }
    match options.final_answer_format.as_ref()? {
        RlmFinalAnswerFormat::Markdown => Some(match termination {
            RlmTermination::FinishRequired { schema: None } => format!(
                "When finishing, call `{}` with a nicely formatted Markdown string, not a raw record/list/tool-result value.",
                vocabulary.finish_statement
            ),
            RlmTermination::Natural => format!(
                "Write prose-only final answers as nicely formatted Markdown. If you intentionally use `{}`, use a Markdown string for user-facing answers, not a raw record/list/tool-result value.",
                vocabulary.finish_statement
            ),
            RlmTermination::FinishRequired { schema: Some(_) } => unreachable!(),
        }),
        RlmFinalAnswerFormat::Custom { guidance } => {
            let guidance = guidance.trim();
            (!guidance.is_empty()).then(|| guidance.to_string())
        }
        RlmFinalAnswerFormat::RawFinalValue => None,
    }
}

#[cfg(test)]
fn rlm_finalization_prompt(termination: &RlmTermination) -> String {
    SessionDialect::prompt_only(
        Arc::new(crate::dialect::TypescriptDialect),
        LashlangSurface::default(),
    )
    .finalization_copy(termination, crate::plugin::RlmChannel::Cell)
}

impl RlmContextProjector {
    #[cfg(test)]
    fn format_history(&self, events: &[lash_core::SessionHistoryRecord]) -> String {
        let messages = render_history_messages(&RlmHistoryRenderInput {
            images: true,
            dialect: self.dialect.as_ref(),
            events,
            turn_messages: &lash_core::facade_support::MessageSequence::default(),
            turn_causes: &[],
            max_output_chars: self.max_output_chars,
            protocol_iteration: 0,
            finalization: "",
            required_output: None,
            final_answer_format: None,
            budget_suffix: None,
            bound_variables: "",
        });
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
    let dialect = SessionDialect::prompt_only(
        Arc::new(crate::dialect::TypescriptDialect),
        LashlangSurface::default(),
    );
    let events = [lash_core::SessionHistoryRecord::Conversation(
        lash_core::session_model::ConversationRecord::from_message(message),
    )];

    let rendered = render_history_messages(&RlmHistoryRenderInput {
        images: true,
        dialect: &dialect,
        events: &events,
        turn_messages: &lash_core::facade_support::MessageSequence::default(),
        turn_causes: &[],
        max_output_chars: 10_000,
        protocol_iteration: 0,
        finalization: "",
        required_output: None,
        final_answer_format: None,
        budget_suffix: None,
        bound_variables: "",
    });
    let attachment_count = rendered
        .iter()
        .flat_map(|message| message.blocks.iter())
        .flat_map(LlmContentBlock::attachment_sources)
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
mod tests;

#[cfg(test)]
#[path = "driver_runtime_feedback_tests.rs"]
mod runtime_feedback_tests;
