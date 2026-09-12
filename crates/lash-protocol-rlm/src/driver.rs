use crate::execution_prompt::render_system_prompt;
use lash_sansio::sync::RwLockExt;
pub(crate) mod history;

use std::sync::{Arc, RwLock};

#[cfg(any(test, feature = "testing"))]
use lash_core::llm::types::{LlmContentBlock, LlmMessage};
use lash_core::llm::types::{LlmRequestScope, LlmToolChoice};
use lash_core::sansio::ContextProjector;
use lash_core::{
    LlmRequest, ProjectorContext, PromptContribution, PromptUsage, ProtocolBuildInput,
    TurnDriverConfig, TurnDriverPreamble,
};
use lash_lashlang_runtime::LashlangSurface;
use lash_rlm_types::{RlmFinalAnswerFormat, RlmTermination, RlmTurnOptions};

use crate::dialect::{LashlangDialect, RlmDialect};
#[cfg(test)]
use crate::projection::rlm_protocol_event;
use crate::rlm_support::{SharedBoundVariablesPrompt, decode_rlm_options, effective_budget_tokens};

#[cfg(any(test, feature = "testing"))]
use history::render_history_messages;
use history::{RlmHistoryRenderInput, build_rlm_history_messages_from_turn};

/// Cell shared between the RLM protocol plugin's turn-prepare hook (writer)
/// and the projector (reader). The plugin's hook captures `prompt_usage`
/// from `TurnTransformContext` each turn and stores it here so the
/// projector can render the budget suffix into the volatile turn-tail
/// message — keeping the cached system prefix byte-stable.
pub type SharedPromptUsage = Arc<RwLock<Option<PromptUsage>>>;

#[derive(Clone)]
pub struct RlmProjectorConfig {
    pub discovery: Option<lash_core::ToolDiscovery>,
    pub max_output_chars: usize,
    pub max_budget_tokens: Option<usize>,
    pub last_prompt_usage: SharedPromptUsage,
    pub prompt_features: crate::protocol::RlmPromptFeatures,
    pub lashlang_surface: LashlangSurface,
}

pub(crate) struct RlmPreambleConfig {
    pub(crate) discovery: Option<lash_core::ToolDiscovery>,
    pub(crate) max_output_chars: usize,
    pub(crate) max_budget_tokens: Option<usize>,
    pub(crate) last_prompt_usage: SharedPromptUsage,
    pub(crate) prompt_features: crate::protocol::RlmPromptFeatures,
}

impl Default for RlmProjectorConfig {
    fn default() -> Self {
        Self {
            discovery: None,
            max_output_chars: 10_000,
            max_budget_tokens: None,
            last_prompt_usage: Arc::new(RwLock::new(None)),
            prompt_features: crate::protocol::RlmPromptFeatures::default(),
            lashlang_surface: LashlangSurface::default(),
        }
    }
}

pub fn build_rlm_preamble(
    input: ProtocolBuildInput,
    config: RlmProjectorConfig,
) -> TurnDriverPreamble {
    let mut cache = crate::rlm_support::BoundVariableRenderCache::default();
    let bound_variables_prompt = Arc::new(RwLock::new(crate::rlm_support::render_bound_variables(
        &mut cache,
        &[],
        // This preamble path constructs a prompt-only Lashlang dialect below.
        crate::dialect::lashlang::LASHLANG_PROMPT_VOCABULARY,
    )));
    build_rlm_preamble_with_bound_variables(input, config, bound_variables_prompt)
}

pub(crate) fn build_rlm_preamble_with_bound_variables(
    input: ProtocolBuildInput,
    config: RlmProjectorConfig,
    bound_variables_prompt: SharedBoundVariablesPrompt,
) -> TurnDriverPreamble {
    let dialect: Arc<dyn RlmDialect> = Arc::new(LashlangDialect::prompt_only(
        config.lashlang_surface.clone(),
    ));
    build_rlm_preamble_with_dialect(
        input,
        RlmPreambleConfig {
            discovery: config.discovery,
            max_output_chars: config.max_output_chars,
            max_budget_tokens: config.max_budget_tokens,
            last_prompt_usage: config.last_prompt_usage,
            prompt_features: config.prompt_features,
        },
        bound_variables_prompt,
        dialect,
    )
}

pub(crate) fn build_rlm_preamble_with_dialect(
    input: ProtocolBuildInput,
    config: RlmPreambleConfig,
    bound_variables_prompt: SharedBoundVariablesPrompt,
    dialect: Arc<dyn RlmDialect>,
) -> TurnDriverPreamble {
    let tool_catalog = input.tool_catalog.as_ref();
    let tool_names = tool_catalog.tool_names();
    let tool_names_fingerprint = tool_catalog.tool_names_fingerprint();
    let mut prompt_contributions = Vec::new();
    let visible_catalog;
    let tool_catalog = if config.discovery.is_some() {
        visible_catalog = tool_catalog.inline_tools();
        &visible_catalog
    } else {
        tool_catalog
    };

    let tool_docs = crate::tool_catalog::rlm_prompt_tool_docs(
        tool_catalog,
        dialect.as_ref(),
        config.prompt_features,
    );
    if !dialect.renders_tool_catalogue_inline() && !tool_docs.trim().is_empty() {
        prompt_contributions.push(PromptContribution::execution(
            "Tools",
            format!(
                "Call the operations below with their declared argument records.\n\n{tool_docs}"
            ),
        ));
    }
    prompt_contributions.extend(input.extra_prompt_contributions);
    let execution = dialect
        .render_execution_section(config.prompt_features, tool_catalog)
        .expect("validated dialect surface");
    let execution = crate::tool_catalog::with_discovery_sentence(
        execution,
        config.discovery.as_ref(),
        dialect.as_ref(),
    );
    let turn_limit_dialect = Arc::clone(&dialect);
    TurnDriverPreamble {
        config: TurnDriverConfig {
            protocol: Arc::new(crate::protocol::RlmDriver::with_dialect(Arc::clone(
                &dialect,
            ))),
            projector: Arc::new(RlmContextProjector {
                prompt_features: config.prompt_features,
                max_output_chars: config.max_output_chars,
                max_budget_tokens: config.max_budget_tokens,
                last_prompt_usage: config.last_prompt_usage,
                bound_variables_prompt,
                dialect: Arc::clone(&dialect),
            }),
            sync_execution_environment: true,
            turn_limit_final_message: Arc::new(move |message_id, max_turns| {
                crate::protocol::turn_limit_final_message(
                    turn_limit_dialect.as_ref(),
                    message_id,
                    max_turns,
                )
            }),
        },
        tool_specs: Arc::new(Vec::new()),
        tool_names,
        tool_names_fingerprint,
        execution_prompt: Arc::from(execution),
        prompt_contributions,
    }
}

#[cfg(test)]
mod catalogue_tests {
    use super::*;
    use lash_core::ToolActivation;
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
        .with_activation(ToolActivation::Always)
        .with_tool_binding(ToolBinding::new([module], operation))
    }

    #[test]
    fn rlm_preamble_uses_resolved_tool_catalog_without_search_tool_special_cases() {
        let definitions = vec![
            tool("search_tools", "tools", "search"),
            tool("grep", "files", "grep"),
        ];
        let surface = lash_core::ToolCatalog::from_tool_definitions(definitions);

        let preamble = build_rlm_preamble(
            lash_core::ProtocolBuildInput {
                tool_catalog: Arc::new(surface),
                plugin_extensions: Default::default(),
                trigger_events: Default::default(),
                extra_prompt_contributions: Vec::new(),
            },
            RlmProjectorConfig {
                lashlang_surface: LashlangSurface::new(
                    lashlang::LashlangAbilities::all(),
                    lashlang::LashlangLanguageFeatures::default(),
                    lashlang::LashlangHostCatalog::tool_default(["search_tools", "grep"]),
                ),
                ..RlmProjectorConfig::default()
            },
        );

        assert_eq!(preamble.tool_names.as_ref(), &vec!["search_tools", "grep"]);
        let prompt = preamble
            .prompt_contributions
            .iter()
            .map(|contribution| contribution.content.as_ref())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(prompt.contains("tools.search"));
        assert!(prompt.contains("files.grep"));
        assert!(!prompt.contains("search_tools("));
    }

    #[test]
    fn rlm_preamble_uses_lashlang_host_environment_abilities() {
        let definitions = vec![tool("grep", "files", "grep")];
        let surface = lash_core::ToolCatalog::from_tool_definitions(definitions);

        let preamble = build_rlm_preamble(
            lash_core::ProtocolBuildInput {
                tool_catalog: Arc::new(surface),
                plugin_extensions: Default::default(),
                trigger_events: Default::default(),
                extra_prompt_contributions: Vec::new(),
            },
            RlmProjectorConfig {
                lashlang_surface: LashlangSurface::new(
                    lashlang::LashlangAbilities::default(),
                    lashlang::LashlangLanguageFeatures::default(),
                    lashlang::LashlangHostCatalog::tool_default(["grep"]),
                ),
                ..RlmProjectorConfig::default()
            },
        );

        assert!(!preamble.execution_prompt.contains("process name"));
        assert!(!preamble.execution_prompt.contains("sleep for"));
        assert!(preamble.execution_prompt.contains("- Tools:"));
    }

    #[test]
    fn finish_finalization_prompt_defaults_to_natural_guidance() {
        let prompt = rlm_finalization_prompt(&RlmTermination::default());

        assert!(prompt.contains("prose alone ends this turn as the final answer"));
        assert!(prompt.contains("write prose only when no work remains"));
        assert!(prompt.contains("otherwise perform the next step in a block"));
    }

    #[test]
    fn finish_required_schema_finalization_prompt_requires_value() {
        let prompt = rlm_finalization_prompt(&RlmTermination::FinishRequired {
            schema: Some(serde_json::json!({ "type": "object" })),
        });

        assert!(prompt.contains("finish <value>"));
        assert!(prompt.contains("REQUIRED OUTPUT"));
        assert!(prompt.contains(
            "Every response, including the last, acts inside a paired `<lashlang>...</lashlang>` block"
        ));
        assert!(prompt.contains("Do not call `finish <value>` until the answer is in hand"));
        assert!(prompt.contains("the final response's block calls `finish <value>`"));
        assert!(prompt.contains("prose alone never ends this turn"));
        assert!(prompt.contains("Never announce an action without the block that performs it"));
    }

    #[test]
    fn natural_finalization_prompt_allows_direct_prose() {
        let prompt = rlm_finalization_prompt(&RlmTermination::Natural);

        assert!(prompt.contains("Natural termination:"));
        assert!(prompt.contains("prose alone ends this turn as the final answer"));
        assert!(prompt.contains("finish <value>"));
        assert!(prompt.contains("write prose only when no work remains"));
        assert!(prompt.contains("otherwise perform the next step in a block"));
        assert!(prompt.contains("inside the program to return a computed value"));
    }
}

struct RlmContextProjector {
    prompt_features: crate::protocol::RlmPromptFeatures,
    max_output_chars: usize,
    max_budget_tokens: Option<usize>,
    last_prompt_usage: SharedPromptUsage,
    bound_variables_prompt: SharedBoundVariablesPrompt,
    dialect: Arc<dyn RlmDialect>,
}

impl ContextProjector<lash_core::HostTurnProtocol> for RlmContextProjector {
    fn project(&self, ctx: ProjectorContext<'_>) -> Arc<LlmRequest> {
        let options = decode_rlm_options(&ctx.config.termination)
            .expect("RLM turn options are validated before prompt projection");
        let termination = options.effective_termination();
        let finalization = self.dialect.finalization_copy(&termination);
        let required_output = required_output_block(&termination);
        let vocabulary = self.dialect.prompt_vocabulary();
        let final_answer_format = final_answer_format_prompt(&options, vocabulary);
        let guard = self.last_prompt_usage.read_recover();
        let budget_suffix = crate::rlm_support::format_budget_suffix_with_vocabulary(
            ctx.protocol_iteration + 1,
            guard.as_ref(),
            effective_budget_tokens(self.max_budget_tokens, ctx.config.max_context_tokens),
            vocabulary,
            self.prompt_features.decomposition,
        );
        let bound_variables_prompt = self.bound_variables_prompt.read_recover().clone();

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
                bound_variables: &bound_variables_prompt,
            },
        ));

        let mut generation = ctx.config.generation.clone();
        // The paired-tag grammar is RLM's response boundary. Provider wire
        // stops, including caller-supplied ones, could withhold that literal
        // boundary and leave the parser with a truncated cell. The boundary is
        // the dialect's, but no dialect hands it to the provider as a stop.
        generation.suppress_stop_sequences_for_protocol();

        Arc::new(LlmRequest {
            instructions: render_system_prompt(&ctx.config.system_prompt, self.dialect.as_ref()),
            model: ctx.config.model.clone(),
            messages,
            resolved_stored: Default::default(),
            tools: Arc::new(Vec::new()),
            tool_choice: LlmToolChoice::None,
            model_variant: ctx.config.model_variant.clone(),
            model_capability: ctx.config.model_capability.clone(),
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

fn required_output_block(termination: &RlmTermination) -> Option<String> {
    match termination {
        RlmTermination::FinishRequired {
            schema: Some(schema),
        } => Some(render_value_schema_contract(schema)),
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

fn render_value_schema_contract(schema: &serde_json::Value) -> String {
    let input_contract = lash_core::ToolDefinition::raw(
        "tool:finish",
        "finish",
        "",
        schema.clone(),
        serde_json::json!({}),
    )
    .compact_contract();

    if input_contract.parameters.is_empty() {
        return lash_core::ToolDefinition::raw(
            "tool:finish",
            "finish",
            "",
            lash_core::ToolDefinition::default_input_schema(),
            schema.clone(),
        )
        .compact_contract()
        .returns;
    }

    let head = format!(
        "{{ {} }}",
        input_contract
            .parameters
            .iter()
            .filter_map(|value| value.get("signature").and_then(serde_json::Value::as_str))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let lines = input_contract
        .parameters
        .iter()
        .filter_map(compact_doc_line)
        .collect::<Vec<_>>();

    if lines.is_empty() {
        head
    } else {
        format!("{head}\nFields:\n{}", lines.join("\n"))
    }
}

fn compact_doc_line(value: &serde_json::Value) -> Option<String> {
    let signature = value.get("signature")?.as_str()?.trim();
    if signature.is_empty() {
        return None;
    }
    let description = value
        .get("description")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    Some(match description {
        Some(description) => format!("- `{signature}` — {description}"),
        None => format!("- `{signature}`"),
    })
}

#[cfg(test)]
fn rlm_finalization_prompt(termination: &RlmTermination) -> String {
    LashlangDialect::prompt_only(LashlangSurface::default()).finalization_copy(termination)
}

impl RlmContextProjector {
    /// Test helper: the history-only messages (no current-iteration tail)
    /// flattened to their text for substring assertions on the rendered format.
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
    let dialect = LashlangDialect::prompt_only(LashlangSurface::default());
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
        .filter(|block| matches!(block, LlmContentBlock::Attachment { .. }))
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
