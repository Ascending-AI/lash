//! The standard protocol's prompt sections (ADR 0133).
//!
//! The protocol contributes its prompt as keyed sections under its plugin
//! id, each placed in the initial instructions unless the host's plan says
//! otherwise:
//!
//! - `intro`: the identity statement;
//! - `execution`: how to call tools, naming `batch` only when it is offered;
//! - `guidance`: the behavioural bullets, with the interactive one when the
//!   offered surface has the `ask` tool.
//!
//! A tool's own guidance is a section its plugin registers. `intro` and
//! `guidance` also render for a compaction's summarizer call, which offers
//! no tools; `execution` does not. Host text is the host's
//! own sections, and a host replaces or omits a built-in one by wrapping it.

use std::sync::Arc;

use lash_core::plugin::prompt::{
    PromptInput, PromptPlacement, PromptPurpose, PromptRenderError, PromptSection,
    PromptSectionKey, PromptSectionSpec, SectionText,
};
use lash_core::plugin::{PluginError, PluginRegistrar};

use crate::{BatchSugar, standard_execution_section};

/// The identity statement the prompt opens with.
pub use lash_core::facade_support::PROTOCOL_INTRO as STANDARD_INTRO;

/// The keys the standard protocol registers its sections under.
pub mod section_keys {
    pub const INTRO: &str = "intro";
    pub const EXECUTION: &str = "execution";
    pub const GUIDANCE: &str = "guidance";
}

/// What the sections render under: the behaviour the session's plugin was
/// built with.
#[derive(Clone, Debug)]
pub(crate) struct StandardPromptBehaviour {
    pub(crate) batch: BatchSugar,
    pub(crate) discovery: bool,
}

impl StandardPromptBehaviour {
    /// The tools the model sees inline: every offered tool, or only the
    /// inline ones when the session discovers the rest.
    fn visible(&self, input: &PromptInput<'_>) -> lash_core::ToolCatalog {
        let catalog = input.offered().catalog().as_ref();
        if self.discovery {
            catalog.inline_tools()
        } else {
            catalog.clone()
        }
    }
}

fn key(key: &str) -> PromptSectionKey {
    #[expect(
        clippy::expect_used,
        reason = "the protocol's section keys are constants within the key alphabet"
    )]
    PromptSectionKey::new(key).expect("a valid section key")
}

fn titled(title: &str, parts: impl IntoIterator<Item = String>) -> SectionText {
    let parts = parts
        .into_iter()
        .filter_map(|part| {
            let text = part.trim();
            (!text.is_empty()).then(|| text.to_string())
        })
        .collect::<Vec<_>>();
    if parts.is_empty() {
        SectionText::Omit
    } else {
        SectionText::Text(format!("## {title}\n\n{}", parts.join("\n\n")))
    }
}

fn section(
    behaviour: &Arc<StandardPromptBehaviour>,
    render: fn(&StandardPromptBehaviour, &PromptInput<'_>) -> SectionText,
) -> Arc<dyn PromptSection> {
    let behaviour = Arc::clone(behaviour);
    Arc::new(
        move |input: &PromptInput<'_>| -> Result<SectionText, PromptRenderError> {
            Ok(render(&behaviour, input))
        },
    )
}

fn intro(_: &StandardPromptBehaviour, _: &PromptInput<'_>) -> SectionText {
    SectionText::text(STANDARD_INTRO)
}

fn execution(behaviour: &StandardPromptBehaviour, _: &PromptInput<'_>) -> SectionText {
    titled("Execution", [standard_execution_section(behaviour.batch)])
}

fn guidance(behaviour: &StandardPromptBehaviour, input: &PromptInput<'_>) -> SectionText {
    let interactive = behaviour
        .visible(input)
        .tools
        .iter()
        .any(|tool| tool.manifest.name == "ask");
    titled(
        "Guidance",
        [lash_core::facade_support::protocol_guidance(interactive)],
    )
}

/// Register the protocol's sections.
pub(crate) fn register_sections(
    reg: &mut PluginRegistrar,
    behaviour: StandardPromptBehaviour,
) -> Result<(), PluginError> {
    let behaviour = Arc::new(behaviour);
    let both = [PromptPurpose::Turn, PromptPurpose::Compaction];
    let placement = PromptPlacement::InitialInstructions;
    reg.prompt().section(
        PromptSectionSpec::new(key(section_keys::INTRO), placement).purposes(both.clone()),
        section(&behaviour, intro),
    )?;
    reg.prompt().section(
        PromptSectionSpec::new(key(section_keys::EXECUTION), placement),
        section(&behaviour, execution),
    )?;
    reg.prompt().section(
        PromptSectionSpec::new(key(section_keys::GUIDANCE), placement).purposes(both),
        section(&behaviour, guidance),
    )
}
