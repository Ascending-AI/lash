use lash_sansio::sync::RwLockExt;
pub(crate) mod history;

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

#[cfg(test)]
use lash_core::llm::types::LlmContentBlock;
use lash_core::llm::types::{LlmMessage, LlmRequestScope, LlmRole, LlmToolChoice};
use lash_core::sansio::ContextProjector;
use lash_core::{
    LlmRequest, ProjectorContext, PromptContribution, PromptUsage, ProtocolBuildInput,
    TurnDriverConfig, TurnDriverPreamble,
};
use lash_lashlang_runtime::LashlangSurface;
use lash_rlm_types::{RlmCreateExtras, RlmFinalAnswerFormat, RlmTermination};

use crate::dialect::{LashlangDialect, RlmDialect};
#[cfg(test)]
use crate::projection::rlm_protocol_event;
use crate::rlm_support::{SharedBoundVariablesPrompt, decode_rlm_options};

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
    pub max_output_chars: usize,
    pub max_budget_tokens: Option<usize>,
    pub last_prompt_usage: SharedPromptUsage,
    pub prompt_features: crate::protocol::RlmPromptFeatures,
    pub lashlang_surface: LashlangSurface,
    pub redaction_roots: Arc<[PathBuf]>,
}

pub(crate) struct RlmPreambleConfig {
    pub(crate) max_output_chars: usize,
    pub(crate) max_budget_tokens: Option<usize>,
    pub(crate) last_prompt_usage: SharedPromptUsage,
    pub(crate) prompt_features: crate::protocol::RlmPromptFeatures,
    pub(crate) redaction_roots: Arc<[PathBuf]>,
}

impl Default for RlmProjectorConfig {
    fn default() -> Self {
        Self {
            max_output_chars: 10_000,
            max_budget_tokens: None,
            last_prompt_usage: Arc::new(RwLock::new(None)),
            prompt_features: crate::protocol::RlmPromptFeatures::default(),
            lashlang_surface: LashlangSurface::default(),
            redaction_roots: Arc::from([]),
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
            max_output_chars: config.max_output_chars,
            max_budget_tokens: config.max_budget_tokens,
            last_prompt_usage: config.last_prompt_usage,
            prompt_features: config.prompt_features,
            redaction_roots: config.redaction_roots,
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

    let tool_docs = crate::tool_catalog::rlm_prompt_tool_docs(tool_catalog, dialect.as_ref());
    if !tool_docs.trim().is_empty() {
        prompt_contributions.push(PromptContribution::execution("Tools", tool_docs));
    }
    prompt_contributions.extend(input.extra_prompt_contributions);
    let turn_limit_dialect = Arc::clone(&dialect);
    TurnDriverPreamble {
        config: TurnDriverConfig {
            protocol: Arc::new(crate::protocol::RlmDriver::with_dialect(
                Arc::clone(&config.redaction_roots),
                Arc::clone(&dialect),
            )),
            projector: Arc::new(RlmContextProjector {
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
        execution_prompt: Arc::from(
            dialect
                .render_execution_section(config.prompt_features, tool_catalog)
                .expect("RLM tool catalog registration must validate the active dialect surface"),
        ),
        prompt_contributions,
    }
}

#[cfg(test)]
mod catalogue_tests {
    use super::*;
    use lash_core::ToolActivation;
    use lash_lashlang_runtime::{LashlangToolBinding, ToolDefinitionLashlangExt};

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
        .with_lashlang_binding(LashlangToolBinding::new([module], operation))
    }

    #[test]
    fn rlm_preamble_uses_resolved_tool_catalog_without_search_tool_special_cases() {
        let definitions = vec![
            tool("search_tools", "tools", "search"),
            tool("grep", "files", "grep"),
        ];
        let contracts = definitions
            .iter()
            .map(|tool| (tool.name().to_string(), Arc::new(tool.contract())))
            .collect();
        let surface = lash_core::ToolCatalog::from_tools(
            definitions
                .into_iter()
                .map(|tool| tool.manifest())
                .collect(),
            contracts,
        );

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
        let contracts = definitions
            .iter()
            .map(|tool| (tool.name().to_string(), Arc::new(tool.contract())))
            .collect();
        let surface = lash_core::ToolCatalog::from_tools(
            definitions
                .into_iter()
                .map(|tool| tool.manifest())
                .collect(),
            contracts,
        );

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
        assert!(preamble.execution_prompt.contains("Module operations"));
    }

    #[test]
    fn finish_finalization_prompt_defaults_to_natural_guidance() {
        let prompt = rlm_finalization_prompt(&RlmTermination::default());

        assert!(prompt.contains("prose-only response immediately ends the turn"));
        assert!(prompt.contains("If any work remains, do not write prose-only"));
        assert!(prompt.contains("without `finish` is progress"));
    }

    #[test]
    fn finish_required_schema_finalization_prompt_requires_value() {
        let prompt = rlm_finalization_prompt(&RlmTermination::FinishRequired {
            schema: Some(serde_json::json!({ "type": "object" })),
        });

        assert!(prompt.contains("finish <value>"));
        assert!(prompt.contains("REQUIRED OUTPUT"));
        assert!(prompt.contains("Every non-terminal response must contain"));
        assert!(prompt.contains("Prose-only does not end the turn"));
        assert!(prompt.contains("commentary/status only"));
    }

    #[test]
    fn natural_finalization_prompt_allows_direct_prose() {
        let prompt = rlm_finalization_prompt(&RlmTermination::Natural);

        assert!(prompt.contains("Finish with prose"));
        assert!(prompt.contains("prose-only response immediately ends the turn"));
        assert!(prompt.contains("finish <value>"));
        assert!(prompt.contains("Every message before the final answer"));
        assert!(prompt.contains("Unaccompanied prose is final-answer-only"));
        assert!(prompt.contains("Example multi-step natural turn"));
    }
}

struct RlmContextProjector {
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
        let finalization = self.dialect.finalization_copy(&options.termination);
        let required_output = required_output_block(&options.termination);
        let vocabulary = self.dialect.prompt_vocabulary();
        let final_answer_format = final_answer_format_prompt(&options, vocabulary);
        let guard = self.last_prompt_usage.read_recover();
        let budget_suffix = crate::rlm_support::format_budget_suffix_with_vocabulary(
            ctx.protocol_iteration + 1,
            guard.as_ref(),
            self.max_budget_tokens,
            vocabulary,
        );
        let bound_variables_prompt = self.bound_variables_prompt.read_recover().clone();

        let mut messages = Vec::new();
        if !ctx.config.system_prompt.trim().is_empty() {
            messages.push(LlmMessage::text(
                LlmRole::System,
                Arc::clone(&ctx.config.system_prompt),
            ));
        }
        let mut attachments = Vec::new();
        messages.extend(build_rlm_history_messages_from_turn(
            RlmHistoryRenderInput {
                dialect: self.dialect.as_ref(),
                events: ctx.events,
                turn_messages: ctx.messages,
                turn_causes: ctx.turn_causes,
                max_output_chars: self.max_output_chars,
                protocol_iteration: ctx.protocol_iteration + 1,
                finalization,
                required_output: required_output.as_deref(),
                final_answer_format: final_answer_format.as_deref(),
                budget_suffix: budget_suffix.as_deref(),
                bound_variables: &bound_variables_prompt,
            },
            &mut attachments,
        ));

        let mut generation = ctx.config.generation.clone();
        // The paired-tag grammar is RLM's response boundary. Provider wire
        // stops, including caller-supplied ones, could withhold that literal
        // boundary and leave the parser with a truncated cell. The boundary is
        // the dialect's, but no dialect hands it to the provider as a stop.
        generation.suppress_stop_sequences_for_protocol();

        Arc::new(LlmRequest {
            model: ctx.config.model.clone(),
            messages,
            attachments,
            resolved_stored: Default::default(),
            tools: Arc::new(Vec::new()),
            tool_choice: LlmToolChoice::None,
            model_variant: ctx.config.model_variant.clone(),
            model_capability: ctx.config.model_capability.clone(),
            scope: LlmRequestScope::new(
                ctx.config.session_id.clone(),
                format!("{}:frame:sansio", ctx.config.session_id),
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
    options: &RlmCreateExtras,
    vocabulary: crate::dialect::DialectPromptVocabulary,
) -> Option<String> {
    if matches!(
        options.termination,
        RlmTermination::FinishRequired { schema: Some(_) }
    ) {
        return None;
    }
    match options.final_answer_format.as_ref()? {
        RlmFinalAnswerFormat::Markdown => Some(match options.termination {
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
fn rlm_finalization_prompt(termination: &RlmTermination) -> &'static str {
    LashlangDialect::prompt_only(LashlangSurface::default()).finalization_copy(termination)
}

impl RlmContextProjector {
    /// Test helper: the history-only messages (no current-iteration tail)
    /// flattened to their text for substring assertions on the rendered format.
    #[cfg(test)]
    fn format_history(&self, events: &[lash_core::SessionHistoryRecord]) -> String {
        let mut attachments = Vec::new();
        let messages = render_history_messages(
            &RlmHistoryRenderInput {
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
            },
            &mut attachments,
        );
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
    let mut attachments = Vec::new();
    let rendered = render_history_messages(
        &RlmHistoryRenderInput {
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
        },
        &mut attachments,
    );
    match rendered.as_slice() {
        [message] if attachments.is_empty() => Ok(message.clone()),
        _ => Err(format!(
            "RLM conformance history rendered {} messages and {} attachments; expected exactly one message and no attachments",
            rendered.len(),
            attachments.len()
        )),
    }
}

#[cfg(test)]
mod tests {
    /// These fixtures cover the Lashlang wording; the cross-dialect walker in
    /// `dialect::prompt_walker_tests` covers both.
    fn final_answer_format_prompt_test(options: &RlmCreateExtras) -> Option<String> {
        final_answer_format_prompt(
            options,
            crate::dialect::lashlang::LASHLANG_PROMPT_VOCABULARY,
        )
    }

    use super::*;
    use lash_core::session_model::{ConversationRecord, MessageRole, Part, SessionHistoryRecord};
    use lash_rlm_types::{RlmProtocolEvent, RlmTrajectoryEntry};

    fn user_event(id: &str, text: &str) -> SessionHistoryRecord {
        SessionHistoryRecord::Conversation(ConversationRecord {
            id: id.to_string(),
            role: MessageRole::User,
            parts: vec![Part::text(format!("{id}.p0"), text.to_string(), None)].into(),
            origin: None,
        })
    }

    fn step_event(protocol_iteration: usize, code: &str, output: &str) -> SessionHistoryRecord {
        SessionHistoryRecord::Protocol(rlm_protocol_event(RlmProtocolEvent::RlmTrajectoryEntry(
            RlmTrajectoryEntry {
                id: format!("lashlang_step_{protocol_iteration}"),
                protocol_iteration,
                code: code.to_string(),
                output: if output.is_empty() {
                    Vec::new()
                } else {
                    vec![output.to_string()]
                },
                images: Vec::new(),
                calls: Vec::new(),
                calls_omitted: 0,
                error: None,
                final_output: None,
            },
        )))
    }

    fn terminal_step_event(
        protocol_iteration: usize,
        code: &str,
        output: Vec<String>,
        images: Vec<lash_core::AttachmentRef>,
        final_output: serde_json::Value,
    ) -> SessionHistoryRecord {
        SessionHistoryRecord::Protocol(rlm_protocol_event(RlmProtocolEvent::RlmTrajectoryEntry(
            RlmTrajectoryEntry {
                id: format!("lashlang_step_{protocol_iteration}"),
                protocol_iteration,
                code: code.to_string(),
                output,
                images,
                calls: Vec::new(),
                calls_omitted: 0,
                error: None,
                final_output: Some(final_output),
            },
        )))
    }

    fn assistant_content_event(id: &str, prose: &str) -> SessionHistoryRecord {
        SessionHistoryRecord::Protocol(rlm_protocol_event(RlmProtocolEvent::RlmAssistantContent(
            lash_rlm_types::RlmAssistantContent {
                id: id.to_string(),
                reasoning: String::new(),
                prose: prose.to_string(),
            },
        )))
    }

    fn assistant_prose_event(id: &str, text: &str) -> SessionHistoryRecord {
        SessionHistoryRecord::Conversation(ConversationRecord {
            id: id.to_string(),
            role: MessageRole::Assistant,
            parts: vec![Part::text(format!("{id}.p0"), text.to_string(), None)].into(),
            origin: None,
        })
    }

    fn projector(max_output_chars: usize) -> RlmContextProjector {
        let mut bound_variables_cache = crate::rlm_support::BoundVariableRenderCache::default();
        RlmContextProjector {
            max_output_chars,
            max_budget_tokens: None,
            last_prompt_usage: Arc::new(RwLock::new(None)),
            bound_variables_prompt: Arc::new(RwLock::new(
                crate::rlm_support::render_bound_variables(
                    &mut bound_variables_cache,
                    &[],
                    crate::dialect::lashlang::LASHLANG_PROMPT_VOCABULARY,
                ),
            )),
            dialect: Arc::new(LashlangDialect::prompt_only(LashlangSurface::default())),
        }
    }

    fn rendered_bound_variables(
        cache: &mut crate::rlm_support::BoundVariableRenderCache,
        globals: serde_json::Value,
    ) -> Arc<str> {
        let globals = globals
            .as_object()
            .expect("globals object")
            .iter()
            .map(|(name, value)| (name.clone(), lashlang::from_json(value.clone())))
            .collect::<Vec<_>>();
        crate::rlm_support::render_bound_variables(
            cache,
            &globals,
            crate::dialect::lashlang::LASHLANG_PROMPT_VOCABULARY,
        )
    }

    fn project_iteration_request(
        projector: &RlmContextProjector,
        events: &[SessionHistoryRecord],
        protocol_iteration: usize,
        model: &str,
    ) -> Arc<LlmRequest> {
        project_iteration_request_with_generation(
            projector,
            events,
            protocol_iteration,
            model,
            Default::default(),
        )
    }

    fn project_iteration_request_with_generation(
        projector: &RlmContextProjector,
        events: &[SessionHistoryRecord],
        protocol_iteration: usize,
        model: &str,
        generation: lash_core::GenerationOptions,
    ) -> Arc<LlmRequest> {
        let config = lash_core::TurnMachineConfig {
            protocol_driver: Arc::new(crate::protocol::RlmDriver::default()),
            projector: Arc::new(lash_core::sansio::ChatContextProjector),
            sync_execution_environment: true,
            model: model.to_string(),
            max_context_tokens: None,
            turn_budget: lash_core::TurnBudget::Unbounded,
            no_progress_budget: Default::default(),
            model_variant: Default::default(),
            model_capability: Default::default(),
            generation,
            autonomous: false,
            tool_specs: Arc::new(Vec::new()),
            system_prompt: Arc::from("stable RLM system prompt"),
            session_id: "prefix-stability".to_string(),
            turn_id: "prefix-stability-turn".to_string(),
            emit_llm_trace: false,
            termination: lash_core::ProtocolTurnOptions::typed(RlmCreateExtras::default())
                .expect("RLM options"),
            turn_limit_final_message: Arc::new(|message_id, max_turns| {
                let dialect = LashlangDialect::prompt_only(LashlangSurface::default());
                crate::protocol::turn_limit_final_message(&dialect, message_id, max_turns)
            }),
        };
        projector.project(ProjectorContext {
            config: &config,
            messages: &lash_core::facade_support::MessageSequence::default(),
            events,
            turn_causes: &[],
            protocol_iteration,
            use_tools: false,
        })
    }

    #[test]
    fn rlm_projector_sends_no_stop_sequence_without_caller_stops() {
        let request = project_iteration_request(&projector(100), &[], 0, "test-model");
        assert!(request.generation.stop_sequences.is_empty());
        assert!(!request.generation.stop_sequences_suppressed_by_protocol());
    }

    #[test]
    fn rlm_projector_suppresses_every_nonempty_caller_stop_list() {
        for caller_stops in [
            vec!["caller-boundary".to_string()],
            vec!["</lashlang>".to_string()],
        ] {
            let request = project_iteration_request_with_generation(
                &projector(100),
                &[],
                0,
                "test-model",
                lash_core::GenerationOptions {
                    stop_sequences: caller_stops,
                    ..Default::default()
                },
            );
            assert!(request.generation.stop_sequences.is_empty());
            assert!(request.generation.stop_sequences_suppressed_by_protocol());
        }
    }

    fn message_text(message: &LlmMessage) -> String {
        message
            .blocks
            .iter()
            .filter_map(|block| match block {
                LlmContentBlock::Text { text, .. } => Some(text.as_ref()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn chronological_history_renders_messages_and_steps_in_order() {
        let projector = projector(100);
        let events = [
            user_event("u1", "first"),
            step_event(0, "print 1", "1"),
            user_event("u2", "second"),
            step_event(1, "print 2", "2"),
        ];
        let history = projector.format_history(&events);

        // History renders in the emission grammar: prior steps are the literal
        // `<lashlang>` cell the model must emit, outputs as separate blocks.
        assert!(history.contains("first"));
        assert!(history.contains("<lashlang>\nprint 1\n</lashlang>"));
        assert!(history.contains("history[1].output[0] (1 chars):\n1"));
        assert!(history.contains("second"));
        assert!(history.contains("<lashlang>\nprint 2\n</lashlang>"));
        assert!(history.contains("history[3].output[0] (1 chars):\n2"));
        // The `--- history[N] ---` meta-format is gone entirely.
        assert!(!history.contains("--- history["));
        assert!(!history.contains("Code:"));
        assert!(!history.contains("protocol_iteration"));
        assert!(!history.contains("Task"));
        assert!(!history.contains("user_input_"));
    }

    #[test]
    fn folded_step_renders_as_emission_cell_not_history_echo() {
        let projector = projector(1000);
        // Regression for the observed glm-5.2 echo: a step preceded by assistant
        // prose folds into ONE assistant message that is the literal `<lashlang>`
        // cell — byte-identical to what the model emits — never a
        // `--- history[...] ---` meta-format the model could imitate.
        let events = [
            user_event("u1", "find it"),
            assistant_prose_event("a1", "Found it. Running it now."),
            step_event(0, "loc = run()", "ok"),
        ];
        let mut attachments = Vec::new();
        let messages = build_rlm_history_messages_from_turn(
            RlmHistoryRenderInput {
                dialect: projector.dialect.as_ref(),
                events: &events,
                turn_messages: &lash_core::facade_support::MessageSequence::default(),
                turn_causes: &[],
                max_output_chars: 1000,
                protocol_iteration: 1,
                finalization: rlm_finalization_prompt(&RlmTermination::default()),
                required_output: None,
                final_answer_format: None,
                budget_suffix: None,
                bound_variables: "",
            },
            &mut attachments,
        );

        let assistant_texts = messages
            .iter()
            .filter(|message| matches!(message.role, LlmRole::Assistant))
            .flat_map(|message| message.blocks.iter())
            .filter_map(|block| match block {
                LlmContentBlock::Text { text, .. } => Some(text.as_ref()),
                _ => None,
            })
            .collect::<Vec<_>>();

        // The prose folded into the cell: exactly one assistant message, no echo.
        assert_eq!(assistant_texts.len(), 1);
        assert!(!assistant_texts[0].contains("--- history["));
        assert!(!assistant_texts[0].contains("Code:\n"));
        assert_eq!(
            assistant_texts[0],
            projector
                .dialect
                .render_history_cell("Found it. Running it now.", "loc = run()")
        );
    }

    #[test]
    fn committed_transcript_supersedes_terminal_step_by_turn_provenance() {
        let projector = projector(1000);
        let terminal_image = lash_core::AttachmentRef {
            id: lash_core::AttachmentId::parse("terminal-image").expect("valid attachment id"),
            media_type: lash_core::MediaType::parse("image/png").unwrap(),
            byte_len: 3,
            type_metadata: Some(lash_core::AttachmentTypeMetadata::image(Some(1), Some(1))),
            label: Some("terminal.png".to_string()),
        };
        let events = [
            user_event("u1", "compute it"),
            step_event(0, "print \"observed mid-turn\"", "observed mid-turn"),
            assistant_content_event("terminal-prose", "Internal terminal commentary."),
            terminal_step_event(
                1,
                "print image\nfinish { answer: 42 }",
                vec!["terminal-only output".to_string()],
                vec![terminal_image],
                serde_json::json!({ "answer": 42 }),
            ),
            // Deliberately differs from the finish value: precedence is based
            // on same-turn provenance, never content equality.
            assistant_prose_event("committed-a1", "Host-rendered answer: forty-two."),
            user_event("u2", "continue"),
            step_event(0, "print \"next turn\"", "next turn"),
        ];
        let mut attachments = Vec::new();

        let messages = render_history_messages(
            &RlmHistoryRenderInput {
                dialect: projector.dialect.as_ref(),
                events: &events,
                turn_messages: &lash_core::facade_support::MessageSequence::default(),
                turn_causes: &[],
                max_output_chars: 1000,
                protocol_iteration: 0,
                finalization: "",
                required_output: None,
                final_answer_format: None,
                budget_suffix: None,
                bound_variables: "",
            },
            &mut attachments,
        );
        let rendered = messages
            .iter()
            .map(message_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("<lashlang>\nprint \"observed mid-turn\"\n</lashlang>"));
        assert!(rendered.contains("observed mid-turn"));
        assert!(rendered.contains("Host-rendered answer: forty-two."));
        assert!(!rendered.contains("Internal terminal commentary."));
        assert!(!rendered.contains("finish { answer: 42 }"));
        assert!(!rendered.contains("terminal-only output"));
        assert!(!rendered.contains("Final output:"));
        assert!(rendered.contains("history[4].output[0] (9 chars):\nnext turn"));
        assert!(!rendered.contains("history[6].output[0]"));
        assert!(attachments.is_empty());
    }

    #[test]
    fn terminal_step_without_committed_transcript_renders_unchanged() {
        for (code, value, expected) in [
            (
                "finish \"string answer\"",
                serde_json::json!("string answer"),
                "\"string answer\"",
            ),
            (
                "finish { answer: 42 }",
                serde_json::json!({ "answer": 42 }),
                "{\n  \"answer\": 42\n}",
            ),
        ] {
            let events = [
                user_event("u1", "compute it"),
                terminal_step_event(0, code, Vec::new(), Vec::new(), value),
            ];
            let history = projector(1000).format_history(&events);

            assert!(history.contains(&format!("<lashlang>\n{code}\n</lashlang>")));
            assert!(history.contains(&format!("Final output:\n{expected}")));
        }
    }

    #[test]
    fn later_turn_assistant_does_not_supersede_uncommitted_terminal_step() {
        let events = [
            user_event("u1", "typed value not surfaced"),
            terminal_step_event(
                0,
                "finish 42",
                Vec::new(),
                Vec::new(),
                serde_json::json!(42),
            ),
            user_event("u2", "a new turn"),
            assistant_prose_event("a2", "Natural answer from turn two."),
        ];
        let history = projector(1000).format_history(&events);

        assert!(history.contains("<lashlang>\nfinish 42\n</lashlang>"));
        assert!(history.contains("Final output:\n42"));
        assert!(history.contains("Natural answer from turn two."));
    }

    #[test]
    fn natural_prose_history_is_byte_unchanged() {
        let projector = projector(1000);
        let events = [
            user_event("u1", "Tell me naturally."),
            assistant_prose_event("a1", "A natural prose answer.\n\nSecond paragraph."),
        ];
        let mut attachments = Vec::new();
        let messages = render_history_messages(
            &RlmHistoryRenderInput {
                dialect: projector.dialect.as_ref(),
                events: &events,
                turn_messages: &lash_core::facade_support::MessageSequence::default(),
                turn_causes: &[],
                max_output_chars: 1000,
                protocol_iteration: 0,
                finalization: "",
                required_output: None,
                final_answer_format: None,
                budget_suffix: None,
                bound_variables: "",
            },
            &mut attachments,
        );

        assert_eq!(messages.len(), 2);
        assert_eq!(message_text(&messages[0]), "Tell me naturally.");
        assert_eq!(
            message_text(&messages[1]),
            "A natural prose answer.\n\nSecond paragraph."
        );
    }

    #[test]
    fn committed_transcript_remains_the_rolling_cache_fence() {
        let projector = projector(1000);
        let events = [
            user_event("u1", "compute"),
            terminal_step_event(
                0,
                "finish \"done\"",
                Vec::new(),
                Vec::new(),
                serde_json::json!("done"),
            ),
            assistant_prose_event("a1", "done"),
        ];
        let mut attachments = Vec::new();
        let messages = build_rlm_history_messages_from_turn(
            RlmHistoryRenderInput {
                dialect: projector.dialect.as_ref(),
                events: &events,
                turn_messages: &lash_core::facade_support::MessageSequence::default(),
                turn_causes: &[],
                max_output_chars: 1000,
                protocol_iteration: 0,
                finalization: "finish",
                required_output: None,
                final_answer_format: None,
                budget_suffix: None,
                bound_variables: "",
            },
            &mut attachments,
        );

        assert!(matches!(
            messages[1].blocks.first(),
            Some(LlmContentBlock::Text {
                text,
                cache_breakpoint: true,
                ..
            }) if text.as_ref() == "done"
        ));
        assert!(matches!(
            messages[2].blocks.first(),
            Some(LlmContentBlock::Text {
                cache_breakpoint: false,
                ..
            })
        ));
    }

    #[test]
    fn chronological_history_excludes_hidden_tool_events() {
        let projector = projector(1000);
        let events = [user_event("u1", "first"), step_event(0, "x = 1", "1")];
        let history = projector.format_history(&events);

        assert!(history.contains("first"));
        assert!(history.contains("<lashlang>\nx = 1\n</lashlang>"));
        assert!(!history.contains("tool_call"));
        assert!(!history.contains("--- history["));
    }

    #[test]
    fn long_user_message_gets_full_history_reference() {
        let projector = projector(10);
        let history = projector.format_history(&[user_event("u1", "abcdefghijklmnopqrstuvwxyz")]);

        assert!(history.contains("re-run `print history[0].content`"));
        assert!(history.contains("... (16 characters omitted) ..."));
        assert!(!history.contains("user_input_"));
    }

    #[test]
    fn truncated_lashlang_step_output_emits_full_reference() {
        // The render half of the re-fetch contract: a truncated step output
        // shows a preview plus a `full: history[0].output[0]` handle. The
        // resolve half — that the handle returns the full untruncated value —
        // is covered by `history_step_output_resolves_full_untruncated_value`
        // in projection::context.
        let projector = projector(10);
        let output = "x".repeat(60 * 1024);
        let history = projector.format_history(&[step_event(0, "print big", &output)]);

        assert!(history.contains("re-run `print history[0].output[0]`"));
        assert!(history.contains("full value retained"));
        assert!(history.contains("...truncated..."));
    }

    #[test]
    fn truncated_step_output_states_value_is_retained_not_lost() {
        // A truncated preview must read as display-only, not lost state — the
        // inference gpt-5.5 got wrong when it stopped a /spring-cleaning
        // mid-task ("I can't continue from the previous tool state"). The note
        // names the re-read handle, in this dialect's own print call, and
        // states the value is retained.
        let projector = projector(10);
        let output = "x".repeat(60 * 1024);
        let history = projector.format_history(&[step_event(0, "print big", &output)]);

        assert!(history.contains("full value retained"), "{history}");
        assert!(
            history.contains("re-run `print history[0].output[0]`"),
            "{history}"
        );
        // The bare, easily-misread "chars, full: <ref>" framing is gone.
        assert!(!history.contains("chars, full: history"), "{history}");
    }

    #[test]
    fn structured_lashlang_step_output_keeps_diagnostic_fields_in_projected_history() {
        let projector = projector(10);
        let raw = serde_json::json!({
            "output": "x".repeat(60 * 1024),
            "status": "failed",
            "error": "boom",
            "exit_code": 2,
            "stderr": "short stderr"
        })
        .to_string();
        let history = projector.format_history(&[step_event(0, "print result", &raw)]);

        assert!(
            history.contains("re-run `print history[0].output[0]`"),
            "{history}"
        );
        let status = history.find(r#""status":"failed""#).expect("status field");
        let error = history.find(r#""error":"boom""#).expect("error field");
        let exit = history.find(r#""exit_code":2"#).expect("exit field");
        let stderr = history
            .find(r#""stderr":"short stderr""#)
            .expect("stderr field");
        assert!(status < error && error < exit && exit < stderr, "{history}");
        assert!(history.contains("truncated"), "{history}");
    }

    #[test]
    fn plugin_origin_is_not_rendered_in_history() {
        let projector = projector(100);
        let event = SessionHistoryRecord::Conversation(ConversationRecord {
            id: "plugin".to_string(),
            role: MessageRole::User,
            parts: vec![Part::text(
                "plugin.p0".to_string(),
                "synthetic plugin message".to_string(),
                None,
            )]
            .into(),
            origin: Some(lash_core::MessageOrigin::Plugin {
                plugin_id: "test".to_string(),
                transient: false,
            }),
        });

        let history = projector.format_history(&[event]);
        assert!(history.contains("synthetic plugin message"));
        assert!(!history.contains("from plugin"));
        assert!(!history.contains("test"));
        assert!(!history.contains("--- history["));
    }

    #[test]
    fn process_wake_history_renders_as_chronological_event_context() {
        let projector = projector(1000);
        let event = SessionHistoryRecord::Conversation(ConversationRecord {
            id: "wake:abc".to_string(),
            role: MessageRole::Event,
            parts: vec![Part::text("wake:abc.p0".to_string(), "Background process wake\nProcess: process-1\nEvent: process.wake #7\nWake input:\nblue button pressed".to_string(), None)]
            .into(),
            origin: Some(lash_core::MessageOrigin::Process {
                process_id: "process-1".to_string(),
                event_type: "process.wake".to_string(),
                sequence: 7,
                wake_id: Some("wake:abc".to_string()),
                caused_by: None,
            }),
        });
        let events = [event];
        let mut attachments = Vec::new();

        let messages = build_rlm_history_messages_from_turn(
            RlmHistoryRenderInput {
                dialect: projector.dialect.as_ref(),
                events: &events,
                turn_messages: &lash_core::facade_support::MessageSequence::default(),
                turn_causes: &[],
                max_output_chars: 1000,
                protocol_iteration: 1,
                finalization: rlm_finalization_prompt(&RlmTermination::default()),
                required_output: None,
                final_answer_format: None,
                budget_suffix: None,
                bound_variables: "",
            },
            &mut attachments,
        );
        let history = projector.format_history(&events);

        assert!(history.contains("Background process wake"));
        assert!(history.contains("blue button pressed"));
        assert!(!history.contains("system message"));
        assert!(!history.contains("--- history["));
        assert!(matches!(messages[0].role, LlmRole::User));
    }

    #[test]
    fn active_turn_causes_render_in_current_turn_events_without_history_duplication() {
        let projector = projector(1000);
        let cause = lash_core::TurnCause {
            id: "wake:abc".to_string(),
            event_type: "process.wake".to_string(),
            origin: lash_core::MessageOrigin::Process {
                process_id: "process-1".to_string(),
                event_type: "process.wake".to_string(),
                sequence: 7,
                wake_id: Some("wake:abc".to_string()),
                caused_by: None,
            },
            text: "Background process wake\nProcess: process-1\nEvent: process.wake #7\nWake input:\nblue button pressed".to_string(),
        };
        let event_message = cause.to_event_message();
        let messages = lash_core::facade_support::MessageSequence::from(vec![event_message]);
        let mut attachments = Vec::new();

        let rendered = build_rlm_history_messages_from_turn(
            RlmHistoryRenderInput {
                dialect: projector.dialect.as_ref(),
                events: &[],
                turn_messages: &messages,
                turn_causes: std::slice::from_ref(&cause),
                max_output_chars: 1000,
                protocol_iteration: 0,
                finalization: rlm_finalization_prompt(&RlmTermination::default()),
                required_output: None,
                final_answer_format: None,
                budget_suffix: None,
                bound_variables: "",
            },
            &mut attachments,
        );

        let combined = rendered
            .iter()
            .flat_map(|message| message.blocks.iter())
            .filter_map(|block| match block {
                LlmContentBlock::Text { text, .. } => Some(text.as_ref()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(combined.contains("=== TURN EVENTS ==="));
        assert!(combined.contains("blue button pressed"));
        assert!(!combined.contains("--- history[0] · event message"));
        assert!(rendered.iter().any(|message| {
            message.role == LlmRole::User
                && message.blocks.iter().any(|block| {
                    matches!(
                        block,
                        LlmContentBlock::Text { text, .. }
                            if text.contains("=== TURN EVENTS ===")
                    )
                })
        }));
    }

    #[test]
    fn printed_images_render_as_llm_image_blocks() {
        let projector = projector(1000);
        let event = SessionHistoryRecord::Protocol(rlm_protocol_event(
            RlmProtocolEvent::RlmTrajectoryEntry(RlmTrajectoryEntry {
                id: "lashlang_step_1".to_string(),
                protocol_iteration: 1,
                code: "print img".to_string(),
                output: vec![r#"{"type":"image","id":"img"}"#.to_string()],
                images: vec![lash_core::AttachmentRef {
                    id: lash_core::AttachmentId::parse("img-ref").expect("valid attachment id"),
                    media_type: lash_core::MediaType::parse("image/png").unwrap(),
                    byte_len: 3,
                    type_metadata: Some(lash_core::AttachmentTypeMetadata::image(Some(1), Some(1))),
                    label: Some("img.png".to_string()),
                }],
                calls: Vec::new(),
                calls_omitted: 0,
                error: None,
                final_output: None,
            }),
        ));
        let events = [event];
        let mut attachments = Vec::new();

        let messages = build_rlm_history_messages_from_turn(
            RlmHistoryRenderInput {
                dialect: projector.dialect.as_ref(),
                events: &events,
                turn_messages: &lash_core::facade_support::MessageSequence::default(),
                turn_causes: &[],
                max_output_chars: 1000,
                protocol_iteration: 1,
                finalization: rlm_finalization_prompt(&RlmTermination::default()),
                required_output: None,
                final_answer_format: None,
                budget_suffix: None,
                bound_variables: "",
            },
            &mut attachments,
        );

        assert_eq!(attachments.len(), 1);
        assert!(matches!(
            &attachments[0],
            lash_core::AttachmentSource::Stored { attachment_ref }
                if attachment_ref.id.as_str() == "img-ref"
                    && attachment_ref.media_type.as_str() == "image/png"
        ));
        // The printed image rides the user observation message for the step.
        assert!(messages.iter().any(|message| {
            matches!(message.role, LlmRole::User)
                && message
                    .blocks
                    .iter()
                    .any(|block| matches!(block, LlmContentBlock::Attachment { attachment_idx: 0 }))
        }));
    }

    #[test]
    fn rlm_prompt_projects_history_as_chat_messages_with_rolling_cache_breakpoint() {
        let projector = projector(1000);
        let events = [user_event("u1", "first"), step_event(0, "print 1", "1")];
        let mut attachments = Vec::new();

        let messages = build_rlm_history_messages_from_turn(
            RlmHistoryRenderInput {
                dialect: projector.dialect.as_ref(),
                events: &events,
                turn_messages: &lash_core::facade_support::MessageSequence::default(),
                turn_causes: &[],
                max_output_chars: 1000,
                protocol_iteration: 2,
                finalization: rlm_finalization_prompt(&RlmTermination::default()),
                required_output: None,
                final_answer_format: None,
                budget_suffix: None,
                bound_variables: "",
            },
            &mut attachments,
        );

        // user turn, assistant cell, user observation, volatile current-iteration tail.
        assert_eq!(messages.len(), 4);
        assert!(matches!(messages[0].role, LlmRole::User));
        assert!(matches!(messages[1].role, LlmRole::Assistant));
        assert!(matches!(messages[2].role, LlmRole::User));
        assert!(matches!(messages[3].role, LlmRole::User));
        assert!(matches!(
            messages[0].blocks.first(),
            Some(LlmContentBlock::Text {
                text,
                cache_breakpoint: false,
                ..
            }) if text.as_ref() == "first"
        ));
        assert!(matches!(
            messages[1].blocks.first(),
            Some(LlmContentBlock::Text {
                text,
                cache_breakpoint: false,
                ..
            }) if text.starts_with("<lashlang>") && text.contains("print 1")
        ));
        // The last history message (the observation) carries the rolling fence.
        assert!(matches!(
            messages[2].blocks.first(),
            Some(LlmContentBlock::Text {
                text,
                cache_breakpoint: true,
                ..
            }) if text.starts_with("history[1].output[0]")
        ));
        assert!(matches!(
            messages[3].blocks.first(),
            Some(LlmContentBlock::Text {
                text,
                cache_breakpoint: false,
                ..
            }) if text.contains("=== CURRENT ITERATION: 2 ===")
        ));
    }

    #[test]
    fn rlm_system_message_is_stable_while_history_and_globals_change() {
        let previous_events = vec![user_event("u1", "inspect"), step_event(0, "value = 1", "1")];
        let mut next_events = previous_events.clone();
        next_events.push(step_event(1, "scratch_note = \"saved\"", "saved"));
        let mut cache = crate::rlm_support::BoundVariableRenderCache::default();
        let previous_bound = rendered_bound_variables(&mut cache, serde_json::json!({}));
        let next_bound =
            rendered_bound_variables(&mut cache, serde_json::json!({ "scratch_note": "saved" }));
        let projector = projector(1000);

        *projector.bound_variables_prompt.write_recover() = previous_bound;
        let previous = project_iteration_request(&projector, &previous_events, 0, "test-model");
        *projector.bound_variables_prompt.write_recover() = next_bound;
        let next = project_iteration_request(&projector, &next_events, 1, "test-model");

        assert_eq!(
            serde_json::to_vec(&previous.messages[0]).unwrap(),
            serde_json::to_vec(&next.messages[0]).unwrap()
        );
        assert!(!message_text(&previous.messages[0]).contains("Bound Variables"));
        assert!(!message_text(&next.messages[0]).contains("scratch_note"));
    }

    #[test]
    fn bound_variables_render_in_the_volatile_tail_in_name_order() {
        let events = vec![
            user_event("u1", "inspect"),
            step_event(0, "value = 1", "1"),
            step_event(1, "scratch_note = \"saved\"", "saved"),
        ];
        let mut cache = crate::rlm_support::BoundVariableRenderCache::default();
        let bound_variables = rendered_bound_variables(
            &mut cache,
            serde_json::json!({
                "zeta": 3,
                "scratch_note": "saved",
                "alpha": 1
            }),
        );
        let projector = projector(1000);
        *projector.bound_variables_prompt.write_recover() = bound_variables;
        let request = project_iteration_request(&projector, &events, 1, "test-model");
        let tail = message_text(request.messages.last().expect("volatile tail"));

        assert!(tail.contains("=== BOUND VARIABLES ==="), "{tail}");
        assert!(tail.contains("- `scratch_note` = saved"), "{tail}");
        assert!(
            tail.contains("- `history` currently has 3 entries"),
            "{tail}"
        );
        let alpha = tail.find("- `alpha` = 1").expect("alpha row");
        let scratch = tail.find("- `scratch_note` = saved").expect("scratch row");
        let zeta = tail.find("- `zeta` = 3").expect("zeta row");
        assert!(alpha < scratch && scratch < zeta, "{tail}");
    }

    #[test]
    fn rlm_prompt_renders_required_output_block_when_schema_present() {
        let projector = projector(1000);
        let events = [user_event("u1", "first")];
        let mut attachments = Vec::new();
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["call", "fold"] },
                "amount": { "type": "integer", "minimum": 0 }
            },
            "required": ["action"]
        });

        let schema_contract = render_value_schema_contract(&schema);
        let messages = build_rlm_history_messages_from_turn(
            RlmHistoryRenderInput {
                dialect: projector.dialect.as_ref(),
                events: &events,
                turn_messages: &lash_core::facade_support::MessageSequence::default(),
                turn_causes: &[],
                max_output_chars: 1000,
                protocol_iteration: 1,
                finalization: "Call finish",
                required_output: Some(&schema_contract),
                final_answer_format: None,
                budget_suffix: None,
                bound_variables: "",
            },
            &mut attachments,
        );

        let tail = messages
            .last()
            .and_then(|message| message.blocks.first())
            .and_then(|block| match block {
                LlmContentBlock::Text { text, .. } => Some(text.as_ref()),
                _ => None,
            })
            .expect("tail block");
        assert!(tail.contains("=== REQUIRED OUTPUT ==="));
        assert!(tail.contains("{ action: enum[\"call\", \"fold\"], amount?: int >= 0 }"));
        assert!(tail.contains("Fields:"));
    }

    #[test]
    fn final_answer_format_guidance_renders_markdown_for_unstructured_turns() {
        let guidance = final_answer_format_prompt_test(&RlmCreateExtras {
            dialect: None,
            termination: RlmTermination::FinishRequired { schema: None },
            final_answer_format: Some(RlmFinalAnswerFormat::Markdown),
        })
        .expect("markdown guidance");

        assert!(guidance.contains("call `finish <value>` with a nicely formatted Markdown string"));
        assert!(guidance.contains("not a raw record/list/tool-result value"));
    }

    #[test]
    fn final_answer_format_guidance_honors_custom_text_and_raw_suppression() {
        let custom = final_answer_format_prompt_test(&RlmCreateExtras {
            dialect: None,
            termination: RlmTermination::Natural,
            final_answer_format: Some(RlmFinalAnswerFormat::Custom {
                guidance: "  Finish concise release-note Markdown.  ".to_string(),
            }),
        })
        .expect("custom guidance");
        assert_eq!(custom, "Finish concise release-note Markdown.");

        assert!(
            final_answer_format_prompt_test(&RlmCreateExtras {
                dialect: None,
                termination: RlmTermination::FinishRequired { schema: None },
                final_answer_format: Some(RlmFinalAnswerFormat::RawFinalValue),
            })
            .is_none()
        );
    }

    #[test]
    fn required_output_schema_suppresses_final_answer_format_guidance() {
        let guidance = final_answer_format_prompt_test(&RlmCreateExtras {
            dialect: None,
            termination: RlmTermination::FinishRequired {
                schema: Some(serde_json::json!({ "type": "object" })),
            },
            final_answer_format: Some(RlmFinalAnswerFormat::Markdown),
        });

        assert!(guidance.is_none());
    }

    #[test]
    fn render_value_schema_contract_renders_object_shape_with_field_table() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["call", "fold"] },
                "confidence": { "type": "number", "minimum": 0, "maximum": 1 }
            },
            "required": ["action"]
        });

        let rendered = render_value_schema_contract(&schema);
        let head = rendered.lines().next().expect("at least one line");
        assert_eq!(
            head,
            "{ action: enum[\"call\", \"fold\"], confidence?: float >= 0 <= 1 }"
        );
        assert!(rendered.contains("Fields:"));
        assert!(rendered.contains("- `action: enum[\"call\", \"fold\"]`"));
        assert!(rendered.contains("- `confidence?: float >= 0 <= 1`"));
    }

    #[test]
    fn render_value_schema_contract_falls_back_to_compact_label_for_scalars() {
        let scalar = serde_json::json!({ "type": "string" });
        assert_eq!(render_value_schema_contract(&scalar), "str");

        let array = serde_json::json!({ "type": "array", "items": { "type": "integer" } });
        assert_eq!(render_value_schema_contract(&array), "list[int]");

        let nullable_string = serde_json::json!({ "type": ["string", "null"] });
        assert_eq!(render_value_schema_contract(&nullable_string), "str | null");
    }

    #[test]
    fn incremental_render_extends_cached_prefix_on_subsequent_calls() {
        let projector = projector(100);
        let initial =
            projector.format_history(&[user_event("u1", "first"), step_event(0, "print 1", "1")]);
        assert!(initial.contains("first"));
        assert!(initial.contains("<lashlang>\nprint 1\n</lashlang>"));

        let extended = projector.format_history(&[
            user_event("u1", "first"),
            step_event(0, "print 1", "1"),
            user_event("u2", "second"),
            step_event(1, "print 2", "2"),
        ]);
        // The stable prefix is byte-identical, so the cached prefix extends.
        assert!(extended.starts_with(&initial));
        assert!(extended.contains("second"));
        assert!(extended.contains("<lashlang>\nprint 2\n</lashlang>"));
    }
}
