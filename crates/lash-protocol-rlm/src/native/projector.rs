use super::history::{RlmHistoryRenderInput, build_rlm_history_messages_from_turn};
use crate::dialect::RlmDialect;
use crate::driver::{RlmPreambleConfig, SharedPromptUsage};
use crate::execution_prompt::render_system_prompt;
use crate::rlm_support::{SharedBoundVariablesPrompt, decode_rlm_options, effective_budget_tokens};
use lash_core::llm::types::{LlmRequestScope, LlmToolChoice};
use lash_core::sansio::ContextProjector;
use lash_core::{
    LlmRequest, ProjectorContext, PromptContribution, ProtocolBuildInput, TurnDriverConfig,
    TurnDriverPreamble,
};
use lash_rlm_types::{RlmFinalAnswerFormat, RlmTermination, RlmTurnOptions};
use lash_sansio::sync::RwLockExt;
use std::sync::Arc;
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
    let execution =
        super::prompt::execution_section(dialect.as_ref(), config.prompt_features, tool_catalog);
    let execution = crate::tool_catalog::with_discovery_sentence(
        execution,
        config.discovery.as_ref(),
        dialect.as_ref(),
    );
    TurnDriverPreamble {
        config: TurnDriverConfig {
            protocol: Arc::new(super::driver::NativeDriver::with_dialect(Arc::clone(
                &dialect,
            ))),
            projector: Arc::new(NativeContextProjector {
                prompt_features: config.prompt_features,
                max_output_chars: config.max_output_chars,
                max_budget_tokens: config.max_budget_tokens,
                last_prompt_usage: config.last_prompt_usage,
                bound_variables_prompt,
                dialect: Arc::clone(&dialect),
            }),
            sync_execution_environment: true,
        },
        tool_specs: Arc::new(vec![super::tool::tool_spec(dialect.as_ref())]),
        tool_names,
        tool_names_fingerprint,
        execution_prompt: Arc::from(execution),
        prompt_contributions,
    }
}

struct NativeContextProjector {
    prompt_features: crate::protocol::RlmPromptFeatures,
    max_output_chars: usize,
    max_budget_tokens: Option<usize>,
    last_prompt_usage: SharedPromptUsage,
    bound_variables_prompt: SharedBoundVariablesPrompt,
    dialect: Arc<dyn RlmDialect>,
}

impl ContextProjector<lash_core::HostTurnProtocol> for NativeContextProjector {
    fn project(&self, ctx: ProjectorContext<'_>) -> Arc<LlmRequest> {
        let options = decode_rlm_options(&ctx.config.termination)
            .expect("RLM turn options are validated before prompt projection");
        let termination = options.effective_termination();
        let finalization = super::prompt::finalization(self.dialect.as_ref(), &termination);
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
        // Both channels execute complete programs. A host's text stop must
        // not truncate a program argument or change the paired sampling cohort.
        generation.suppress_stop_sequences_for_protocol();

        Arc::new(LlmRequest {
            model: ctx.config.model.clone(),
            instructions: render_system_prompt(&ctx.config.system_prompt, self.dialect.as_ref()),
            messages,
            resolved_stored: Default::default(),
            tools: Arc::new(vec![super::tool::tool_spec(self.dialect.as_ref())]),
            tool_choice: LlmToolChoice::Auto,
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
