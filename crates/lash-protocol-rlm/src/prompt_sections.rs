//! The RLM protocol's prompt sections (ADR 0133, FIG-5257).
//!
//! The protocol contributes its prompt as keyed sections under its plugin
//! id, on both channels. The default placements keep the layout a session
//! has always had; the host's plan may move any of them:
//!
//! In the initial instructions, what changes least:
//!
//! 1. `intro`: the identity statement;
//! 2. `guidance`: the behavioural bullets, with the interactive one when the
//!    offered surface has the `ask` tool;
//! 3. `execution`: the channel's prose on writing and running programs,
//!    titled by the dialect;
//! 4. `declarations`: the generated declarations over exactly the offered
//!    callable surface (tools, host surface) and the read-only variables;
//! 5. `subagent`: what a subagent session may do, and how deep it sits.
//!
//! Late, after the projected conversation and outside history, what the
//! current call needs:
//!
//! 6. `bound_variables`: the values the session's programs have bound;
//! 7. `finalization`: how to finish under the run's termination;
//! 8. `required_output`: the contract a finish value must match;
//! 9. `final_answer_format`: the presentation the answer is written in;
//! 10. `context_budget`: the soft context budget, omitted without a
//!     configured threshold or committed usage, its threshold clamped below
//!     the context window, and read against the previous turn's committed
//!     prompt usage.
//!
//! `intro` and `guidance` also render for a compaction's summarizer call,
//! which offers no tools and runs no code. The declarations never depend on
//! the prose: a wrapper that omits every built-in text leaves what the
//! session can call. Host text is the host's own sections.

use std::sync::Arc;

use lash_core::plugin::prompt::{
    PromptInput, PromptPlacement, PromptPurpose, PromptRenderError, PromptSection,
    PromptSectionKey, PromptSectionSpec, SectionText,
};
use lash_core::plugin::{PluginError, PluginRegistrar};

use crate::dialect::{ExecutionSection, SessionDialect};
use crate::plugin::{RLM_PROTOCOL_PLUGIN_ID, RlmChannel, RlmRecordedConfig};
use crate::rlm_support::{effective_budget_tokens, format_budget_suffix_with_vocabulary};

/// The identity statement a code-mode prompt opens with.
pub const RLM_BUILTIN_INTRO: &str = "You are an assistant operating the lash harness.";

const BUILTIN_GUIDANCE: &[&str] = &[
    "- Be concise; no filler, hedging, or performative tone.",
    "- Act as soon as the next step is clear; do not restate conclusions.",
    "- Prefer the simplest correct solution.",
];

/// Advice that needs a user to talk to: it renders only for a call whose
/// offered surface has the `ask` tool.
const BUILTIN_GUIDANCE_INTERACTIVE_ONLY: &str =
    "- Take initiative when the user's intent is clear. Ask only when progress is blocked.";

/// The keys the RLM protocol registers its sections under.
pub mod section_keys {
    pub const INTRO: &str = "intro";
    pub const GUIDANCE: &str = "guidance";
    pub const EXECUTION: &str = "execution";
    pub const DECLARATIONS: &str = "declarations";
    pub const SUBAGENT: &str = "subagent";
    pub const BOUND_VARIABLES: &str = "bound_variables";
    pub const FINALIZATION: &str = "finalization";
    pub const REQUIRED_OUTPUT: &str = "required_output";
    pub const FINAL_ANSWER_FORMAT: &str = "final_answer_format";
    pub const CONTEXT_BUDGET: &str = "context_budget";
}

/// The facts the RLM protocol derives from its committed execution state for
/// one call: what its programs have bound.
#[derive(Clone, Debug, Default)]
pub(crate) struct RlmPromptFacts {
    /// The bound-variables view under the run's recorded render.
    pub(crate) bound_variables: Arc<str>,
    /// The declaration of the session's read-only variables.
    pub(crate) read_only_variables: Option<String>,
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

/// The built-in guidance bullets for a call with or without an `ask` tool.
fn builtin_guidance(interactive: bool) -> String {
    let mut bullets = BUILTIN_GUIDANCE.to_vec();
    if interactive {
        // After the "Be concise" lead-in, beside the other core directives.
        bullets.insert(1, BUILTIN_GUIDANCE_INTERACTIVE_ONLY);
    }
    bullets.join("\n")
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

/// The subagent description: what the session may do as a subagent and how
/// deep it sits.
fn subagent_description(subagent: &lash_core::SubagentSessionContext) -> String {
    format!(
        "Subagent capability: {}. Depth: {}/{}.",
        subagent.capability,
        subagent.depth,
        lash_core::SubagentSessionContext::MAX_DEPTH
    )
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

/// The run's recorded RLM options: its termination and answer format.
fn recorded(input: &PromptInput<'_>) -> Result<Option<RlmRecordedConfig>, PromptRenderError> {
    input.config::<RlmRecordedConfig>().map_err(|error| {
        PromptRenderError::new(format!("invalid recorded RLM session config: {error}"))
    })
}

fn intro(_: &RlmSectionBehaviour, _: &PromptInput<'_>) -> Result<SectionText, PromptRenderError> {
    Ok(SectionText::text(RLM_BUILTIN_INTRO))
}

fn guidance(
    _: &RlmSectionBehaviour,
    input: &PromptInput<'_>,
) -> Result<SectionText, PromptRenderError> {
    // The whole offered catalog decides whether a user can be asked, not
    // only the tools shown inline.
    let interactive = input
        .offered()
        .catalog
        .tool_names()
        .iter()
        .any(|name| name == "ask");
    Ok(SectionText::Text(format!(
        "## Guidance\n\n{}",
        builtin_guidance(interactive)
    )))
}

fn execution(
    behaviour: &RlmSectionBehaviour,
    input: &PromptInput<'_>,
) -> Result<SectionText, PromptRenderError> {
    let ExecutionSection { prose, .. } = execution_section(behaviour, &input.offered().catalog);
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
        execution_section(behaviour, &input.offered().catalog);
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

fn subagent(
    _: &RlmSectionBehaviour,
    input: &PromptInput<'_>,
) -> Result<SectionText, PromptRenderError> {
    Ok(input.subagent().map_or(SectionText::Omit, |subagent| {
        SectionText::Text(subagent_description(subagent))
    }))
}

fn bound_variables(
    _: &RlmSectionBehaviour,
    input: &PromptInput<'_>,
) -> Result<SectionText, PromptRenderError> {
    Ok(input
        .protocol_facts::<RlmPromptFacts>()
        .map_or(SectionText::Omit, |facts| {
            text_or_omit(&*facts.bound_variables)
        }))
}

fn finalization(
    behaviour: &RlmSectionBehaviour,
    input: &PromptInput<'_>,
) -> Result<SectionText, PromptRenderError> {
    let termination = recorded(input)?
        .map(|recorded| recorded.turn_options().effective_termination())
        .unwrap_or_default();
    let copy = match behaviour.channel {
        RlmChannel::Cell => behaviour
            .dialect
            .finalization_copy(&termination, RlmChannel::Cell),
        RlmChannel::NativeTool => {
            crate::native::prompt::finalization(&behaviour.dialect, &termination)
        }
    };
    Ok(late_block("FINALIZATION", Some(copy)))
}

fn required_output(
    behaviour: &RlmSectionBehaviour,
    input: &PromptInput<'_>,
) -> Result<SectionText, PromptRenderError> {
    let termination = recorded(input)?
        .map(|recorded| recorded.turn_options().effective_termination())
        .unwrap_or_default();
    Ok(late_block(
        "REQUIRED OUTPUT",
        crate::driver::required_output_block(&behaviour.dialect, &termination),
    ))
}

fn final_answer_format(
    behaviour: &RlmSectionBehaviour,
    input: &PromptInput<'_>,
) -> Result<SectionText, PromptRenderError> {
    let options = recorded(input)?
        .map(|recorded| recorded.turn_options())
        .unwrap_or_default();
    Ok(late_block(
        "FINAL ANSWER FORMAT",
        crate::driver::final_answer_format_prompt(&options, behaviour.dialect.prompt_vocabulary()),
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
    let sections: [(&str, PromptPlacement, bool, Render); 10] = [
        (INTRO, InitialInstructions, true, intro),
        (GUIDANCE, InitialInstructions, true, guidance),
        (EXECUTION, InitialInstructions, false, execution),
        (DECLARATIONS, InitialInstructions, false, declarations),
        (SUBAGENT, InitialInstructions, false, subagent),
        (BOUND_VARIABLES, CurrentContext, false, bound_variables),
        (FINALIZATION, CurrentContext, false, finalization),
        (REQUIRED_OUTPUT, CurrentContext, false, required_output),
        (
            FINAL_ANSWER_FORMAT,
            CurrentContext,
            false,
            final_answer_format,
        ),
        (CONTEXT_BUDGET, CurrentContext, false, context_budget),
    ];
    for (local, placement, compaction, render) in sections {
        let mut spec = PromptSectionSpec::new(key(local), placement);
        if compaction {
            spec = spec.purposes([PromptPurpose::Turn, PromptPurpose::Compaction]);
        }
        reg.prompt().section(spec, renderer(&behaviour, render))?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod testing;

#[cfg(test)]
mod tests;
