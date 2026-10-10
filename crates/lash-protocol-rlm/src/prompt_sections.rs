//! The RLM protocol's keyed execution sections (ADR 0133).
//!
//! `execution` and `declarations` render in the initial instructions.
//! `bound_variables`, `finalization`, `required_output` and `context_budget`
//! render late, outside history. The host's plan may move or exclude them,
//! and trusted wrappers may replace their text. Hosts supply their own
//! identity, interaction guidance and answer presentation through sections.
//! Compaction calls run no code and select none of these turn sections.

use std::sync::Arc;

use lash_core::plugin::prompt::{
    PromptInput, PromptPlacement, PromptRenderError, PromptSection, PromptSectionKey,
    PromptSectionSpec, SectionText,
};
use lash_core::plugin::{PluginError, PluginRegistrar};

use crate::dialect::{ExecutionSection, SessionDialect};
use crate::plugin::{RLM_PROTOCOL_PLUGIN_ID, RlmChannel, RlmRecordedConfig};
use crate::rlm_support::{effective_budget_tokens, format_budget_suffix_with_vocabulary};

/// The keys the RLM protocol registers its sections under.
pub mod section_keys {
    pub const EXECUTION: &str = "execution";
    pub const DECLARATIONS: &str = "declarations";
    pub const BOUND_VARIABLES: &str = "bound_variables";
    pub const FINALIZATION: &str = "finalization";
    pub const REQUIRED_OUTPUT: &str = "required_output";
    pub const CONTEXT_BUDGET: &str = "context_budget";
}

/// The facts the RLM protocol derives from its committed execution state for
/// one call: what its programs have bound.
#[derive(Clone, Debug, Default)]
pub(crate) struct RlmPromptFacts {
    /// The built-in history binding and, when structured, its item schema.
    pub(crate) history_binding: Arc<str>,
    /// The bound-variables view under the run's recorded render.
    pub(crate) bound_variables: Arc<str>,
    /// The declaration of the session's read-only variables.
    pub(crate) read_only_variables: Option<String>,
}

/// The built-in binding and schema, derived before rendering any section.
pub(crate) fn history_binding(
    dialect: &SessionDialect,
    projection: &lash_core::facade_support::ChronologicalProjection,
    images: bool,
) -> Result<String, lash_core::StoredDataCorruption> {
    let history = crate::projection::rlm_history_projection(projection)?;
    let mut binding = format!(
        "- `history`: `{}`, read-only, {} {}",
        dialect.prompt_vocabulary().history_type,
        history.len(),
        if history.len() == 1 {
            "entry"
        } else {
            "entries"
        },
    );
    if history.history().iter().any(|item| match item {
        lash_rlm_types::RlmHistoryItem::LashVmStep { .. } => true,
        lash_rlm_types::RlmHistoryItem::Message { attachments, .. } => !attachments.is_empty(),
    }) {
        binding.push_str("\n\nSchema:\n");
        binding.push_str(&dialect.history_item_definition(images));
    }
    Ok(binding)
}

/// The session behaviour every section renders under, fixed when the
/// session's plugin was built.
pub(crate) struct RlmSectionBehaviour {
    pub(crate) dialect: Arc<SessionDialect>,
    pub(crate) channel: RlmChannel,
    pub(crate) prompt_features: crate::protocol::RlmPromptFeatures,
    pub(crate) discovery: Option<lash_core::ToolDiscovery>,
    /// The soft context budget the session's plugin was built with.
    pub(crate) budget_tokens: Option<usize>,
}

/// The execution section a session on `behaviour.channel` renders over
/// `tool_catalog`: the tools the model sees inline when the session has a
/// discovery operation, every tool otherwise.
#[expect(
    clippy::expect_used,
    reason = "catalog registration validates every tool binding against the session's dialect; the execution section only errs on an unvalidated catalog"
)]
pub(crate) fn execution_section(
    behaviour: &RlmSectionBehaviour,
    tool_catalog: &lash_core::ToolCatalog,
) -> ExecutionSection {
    let visible_catalog;
    let tool_catalog = if behaviour.discovery.is_some() {
        visible_catalog = tool_catalog.inline_tools();
        &visible_catalog
    } else {
        tool_catalog
    };
    match behaviour.channel {
        RlmChannel::Cell => behaviour
            .dialect
            .execution_section(
                behaviour.prompt_features,
                tool_catalog,
                RlmChannel::Cell,
                behaviour.discovery.as_ref(),
            )
            .expect("validated dialect surface"),
        RlmChannel::NativeTool => crate::native::prompt::execution_section(
            &behaviour.dialect,
            behaviour.prompt_features,
            tool_catalog,
            behaviour.discovery.as_ref(),
        ),
    }
}

fn text_or_omit(text: impl AsRef<str>) -> SectionText {
    let text = text.as_ref().trim();
    if text.is_empty() {
        SectionText::Omit
    } else {
        SectionText::text(text)
    }
}

/// A late block under its `=== TITLE ===` heading, or an omission.
fn late_block(title: &str, body: Option<impl AsRef<str>>) -> SectionText {
    match body {
        Some(body) if !body.as_ref().trim().is_empty() => {
            SectionText::Text(format!("=== {title} ===\n\n{}", body.as_ref()))
        }
        _ => SectionText::Omit,
    }
}

/// The run's recorded RLM options.
fn recorded(input: &PromptInput<'_>) -> Result<Option<RlmRecordedConfig>, PromptRenderError> {
    input.config::<RlmRecordedConfig>().map_err(|error| {
        PromptRenderError::new(format!("invalid recorded RLM session config: {error}"))
    })
}

fn execution(
    behaviour: &RlmSectionBehaviour,
    input: &PromptInput<'_>,
) -> Result<SectionText, PromptRenderError> {
    let ExecutionSection { prose, .. } = execution_section(behaviour, input.offered().catalog());
    let title = behaviour.dialect.prompt_vocabulary().execution_title;
    Ok(match text_or_omit(prose) {
        SectionText::Text(prose) => SectionText::Text(format!("## {title}\n\n{prose}")),
        SectionText::Omit => SectionText::Omit,
    })
}

fn declarations(
    behaviour: &RlmSectionBehaviour,
    input: &PromptInput<'_>,
) -> Result<SectionText, PromptRenderError> {
    let ExecutionSection { declarations, .. } =
        execution_section(behaviour, input.offered().catalog());
    let mut parts = Vec::new();
    let declarations = declarations.trim();
    if !declarations.is_empty() {
        parts.push(declarations.to_string());
    }
    if let Some(variables) = input
        .protocol_facts::<RlmPromptFacts>()
        .and_then(|facts| facts.read_only_variables.as_deref())
    {
        parts.push(format!(
            "### {}\n\n{}",
            crate::projection::READ_ONLY_VARIABLES_TITLE,
            variables.trim()
        ));
    }
    Ok(text_or_omit(parts.join("\n\n")))
}

fn bound_variables(
    _: &RlmSectionBehaviour,
    input: &PromptInput<'_>,
) -> Result<SectionText, PromptRenderError> {
    Ok(input
        .protocol_facts::<RlmPromptFacts>()
        .map_or(SectionText::Omit, |facts| {
            let mut bindings = facts.history_binding.to_string();
            if !facts.bound_variables.is_empty() {
                if !bindings.is_empty() {
                    bindings.push_str("\n\n");
                }
                bindings.push_str(&facts.bound_variables);
            }
            late_block("BOUND VARIABLES", Some(bindings))
        }))
}

fn finalization(
    behaviour: &RlmSectionBehaviour,
    input: &PromptInput<'_>,
) -> Result<SectionText, PromptRenderError> {
    let termination = recorded(input)?
        .map(|recorded| crate::rlm_support::RlmCompletion::from(recorded.turn_options()))
        .unwrap_or_default();
    let copy = match behaviour.channel {
        RlmChannel::Cell => behaviour.dialect.finalization_copy(
            termination.mode,
            termination.finish_schema(),
            RlmChannel::Cell,
        ),
        RlmChannel::NativeTool => {
            crate::native::prompt::finalization(&behaviour.dialect, &termination)
        }
    };
    // The original tail separates finalization from bindings with three newlines.
    Ok(SectionText::Text(format!(
        "\n=== FINALIZATION ===\n\n{copy}"
    )))
}

fn required_output(
    behaviour: &RlmSectionBehaviour,
    input: &PromptInput<'_>,
) -> Result<SectionText, PromptRenderError> {
    let termination = recorded(input)?
        .map(|recorded| crate::rlm_support::RlmCompletion::from(recorded.turn_options()))
        .unwrap_or_default();
    Ok(late_block(
        "REQUIRED OUTPUT",
        crate::driver::required_output_block(&behaviour.dialect, &termination),
    ))
}

fn context_budget(
    behaviour: &RlmSectionBehaviour,
    input: &PromptInput<'_>,
) -> Result<SectionText, PromptRenderError> {
    let model = input.model();
    let window = model
        .context_window_tokens
        .and_then(|window| usize::try_from(window).ok());
    Ok(late_block(
        "CONTEXT BUDGET",
        format_budget_suffix_with_vocabulary(
            input.call().iteration as usize + 1,
            model.committed_usage.as_ref(),
            effective_budget_tokens(behaviour.budget_tokens, window),
            behaviour.dialect.prompt_vocabulary(),
            behaviour.prompt_features.decomposition,
        ),
    ))
}

type Render = fn(&RlmSectionBehaviour, &PromptInput<'_>) -> Result<SectionText, PromptRenderError>;

fn renderer(behaviour: &Arc<RlmSectionBehaviour>, render: Render) -> Arc<dyn PromptSection> {
    let behaviour = Arc::clone(behaviour);
    Arc::new(move |input: &PromptInput<'_>| render(&behaviour, input))
}

#[expect(
    clippy::expect_used,
    reason = "the protocol's section keys are constants within the key alphabet"
)]
fn key(key: &str) -> PromptSectionKey {
    PromptSectionKey::new(key).expect("a valid section key")
}

/// The id of the RLM section registered under `local`.
pub fn section_id(local: &str) -> lash_core::prompt_sections::PromptSectionId {
    lash_core::prompt_sections::PromptSectionId::new(RLM_PROTOCOL_PLUGIN_ID, key(local))
}

/// Register the protocol's sections, in the order the module states.
pub(crate) fn register_sections(
    reg: &mut PluginRegistrar,
    behaviour: RlmSectionBehaviour,
) -> Result<(), PluginError> {
    use PromptPlacement::{CurrentContext, InitialInstructions};
    use section_keys::*;
    let behaviour = Arc::new(behaviour);
    let sections: [(&str, PromptPlacement, Render); 6] = [
        (EXECUTION, InitialInstructions, execution),
        (DECLARATIONS, InitialInstructions, declarations),
        (BOUND_VARIABLES, CurrentContext, bound_variables),
        (FINALIZATION, CurrentContext, finalization),
        (REQUIRED_OUTPUT, CurrentContext, required_output),
        (CONTEXT_BUDGET, CurrentContext, context_budget),
    ];
    for (local, placement, render) in sections {
        let spec = PromptSectionSpec::new(key(local), placement);
        reg.prompt().section(spec, renderer(&behaviour, render))?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod testing;

#[cfg(test)]
mod tests;
