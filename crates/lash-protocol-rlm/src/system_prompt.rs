//! The code-mode system prompt (FIG-4588).
//!
//! The RLM protocol renders its own system prompt from recorded data: the
//! session's recorded prompt config ([`RlmPrompt`]), its recorded tool
//! catalog, its bindings and its subagent authority. The order is fixed, with
//! what changes least first:
//!
//! 1. the intro: the built-in statement, the host's, or none;
//! 2. `## Guidance`: the built-in bullets unless the config leaves them out,
//!    then the host's instructions;
//! 3. the execution section, titled by the dialect: the built-in prose
//!    unless the config leaves it out, then the generated declarations
//!    (tools with each module's instructions once, the host surface, the
//!    read-only variables and the subagent description);
//! 4. `## Context`: the host's context, last.
//!
//! The declarations never depend on the prose: a config that omits every
//! built-in text still renders what the session can call.

use lash_rlm_types::{RlmPrompt, RlmPromptIntro};

use crate::dialect::{ExecutionSection, SessionDialect};
use crate::plugin::RlmChannel;
use crate::projection::RlmProjectedBindings;

/// The identity statement a code-mode prompt opens with unless the session's
/// prompt config replaces or omits it.
pub const RLM_BUILTIN_INTRO: &str = "You are an assistant operating the lash harness.";

const BUILTIN_GUIDANCE: &[&str] = &[
    "- Be concise; no filler, hedging, or performative tone.",
    "- Act as soon as the next step is clear; do not restate conclusions.",
    "- Prefer the simplest correct solution.",
];

/// Advice that needs a user to talk to: it renders only for a session whose
/// catalog has the `ask` tool.
const BUILTIN_GUIDANCE_INTERACTIVE_ONLY: &str =
    "- Take initiative when the user's intent is clear. Ask only when progress is blocked.";

/// The built-in guidance bullets for a session with or without an `ask` tool.
fn builtin_guidance(interactive: bool) -> String {
    let mut bullets = BUILTIN_GUIDANCE.to_vec();
    if interactive {
        // After the "Be concise" lead-in, beside the other core directives.
        bullets.insert(1, BUILTIN_GUIDANCE_INTERACTIVE_ONLY);
    }
    bullets.join("\n")
}

/// What one render of a code-mode system prompt reads, all of it recorded.
#[derive(Clone, Copy)]
pub struct RlmSystemPromptInput<'a> {
    /// The prompt config the session's RLM namespace recorded.
    pub prompt: &'a RlmPrompt,
    /// The session's recorded tool catalog.
    pub tool_catalog: &'a lash_core::ToolCatalog,
    /// The read-only values bound into the session's programs.
    pub bindings: &'a RlmProjectedBindings,
    /// The session's subagent authority, when it is a subagent.
    pub subagent: Option<&'a lash_core::SubagentSessionContext>,
}

/// The session behaviour a render reads beside its input.
pub(crate) struct RlmSystemPromptBehaviour<'a> {
    pub(crate) channel: RlmChannel,
    pub(crate) prompt_features: crate::protocol::RlmPromptFeatures,
    pub(crate) discovery: Option<&'a lash_core::ToolDiscovery>,
}

/// The execution section a session on `behaviour.channel` renders over
/// `tool_catalog`: the tools the model sees inline when the session has a
/// discovery operation, every tool otherwise.
#[expect(
    clippy::expect_used,
    reason = "catalog registration validates every tool binding against the session's dialect; the execution section only errs on an unvalidated catalog"
)]
pub(crate) fn execution_section(
    dialect: &SessionDialect,
    behaviour: &RlmSystemPromptBehaviour<'_>,
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
        RlmChannel::Cell => dialect
            .execution_section(
                behaviour.prompt_features,
                tool_catalog,
                RlmChannel::Cell,
                behaviour.discovery,
            )
            .expect("validated dialect surface"),
        RlmChannel::NativeTool => crate::native::prompt::execution_section(
            dialect,
            behaviour.prompt_features,
            tool_catalog,
            behaviour.discovery,
        ),
    }
}

/// The subagent description: what the session may do as a subagent and how
/// deep it sits.
fn subagent_description(subagent: &lash_core::SubagentSessionContext) -> String {
    format!(
        "Subagent capability: {}. Depth: {}/{}.",
        subagent.capability, subagent.depth, subagent.max_depth
    )
}

fn push_text(parts: &mut Vec<String>, text: &str) {
    let text = text.trim();
    if !text.is_empty() {
        parts.push(text.to_string());
    }
}

/// One titled section, or nothing when it has no parts.
fn section(title: &str, parts: Vec<String>) -> Option<String> {
    (!parts.is_empty()).then(|| format!("## {title}\n\n{}", parts.join("\n\n")))
}

/// Render the system prompt of a session in `dialect` from recorded data.
pub(crate) fn render_system_prompt(
    dialect: &SessionDialect,
    behaviour: &RlmSystemPromptBehaviour<'_>,
    input: RlmSystemPromptInput<'_>,
) -> String {
    let RlmSystemPromptInput {
        prompt,
        tool_catalog,
        bindings,
        subagent,
    } = input;
    let mut sections = Vec::new();

    match &prompt.intro {
        RlmPromptIntro::Builtin => sections.push(RLM_BUILTIN_INTRO.to_string()),
        RlmPromptIntro::Host { text } => push_text(&mut sections, text),
        RlmPromptIntro::Omitted => {}
    }

    let mut guidance = Vec::new();
    if !prompt.omit_builtin_guidance {
        // The whole catalog decides whether a user can be asked, not only the
        // tools shown inline.
        let interactive = tool_catalog.tool_names().iter().any(|name| name == "ask");
        guidance.push(builtin_guidance(interactive));
    }
    for instruction in &prompt.instructions {
        push_text(&mut guidance, instruction);
    }
    sections.extend(section("Guidance", guidance));

    let ExecutionSection {
        prose,
        declarations,
    } = execution_section(dialect, behaviour, tool_catalog);
    let vocabulary = dialect.prompt_vocabulary();
    let mut execution = Vec::new();
    if !prompt.omit_builtin_execution {
        push_text(&mut execution, &prose);
    }
    push_text(&mut execution, &declarations);
    if let Some(variables) = crate::projection::read_only_variables_prompt(bindings, vocabulary) {
        execution.push(format!(
            "### {}\n\n{}",
            crate::projection::READ_ONLY_VARIABLES_TITLE,
            variables.trim()
        ));
    }
    if let Some(subagent) = subagent {
        execution.push(subagent_description(subagent));
    }
    sections.extend(section(vocabulary.execution_title, execution));

    let mut context = Vec::new();
    for entry in &prompt.context {
        push_text(&mut context, entry);
    }
    sections.extend(section("Context", context));

    sections.join("\n\n")
}

/// Render a code-mode system prompt without a session behind it: the prompt
/// a session configured as `config` renders over `input`, on the cell
/// channel. Hosts use it to preview a prompt config; a session renders
/// through its own recorded behaviour.
pub fn render_rlm_system_prompt(
    config: &crate::RlmProjectorConfig,
    input: RlmSystemPromptInput<'_>,
) -> String {
    let dialect = SessionDialect::prompt_only(
        std::sync::Arc::clone(&config.dialect),
        config.lashlang_surface.clone(),
    );
    render_system_prompt(
        &dialect,
        &RlmSystemPromptBehaviour {
            channel: RlmChannel::Cell,
            prompt_features: config.prompt_features,
            discovery: config.discovery.as_ref(),
        },
        input,
    )
}

#[cfg(test)]
mod tests;
