//! The standard protocol's keyed execution section (ADR 0133).
//! Hosts supply identity, interaction guidance and presentation as their own
//! sections. The execution section renders for turns only.

use std::sync::Arc;

use lash_core::plugin::prompt::{
    PromptInput, PromptPlacement, PromptSectionKey, PromptSectionSpec, SectionText,
};
use lash_core::plugin::{PluginError, PluginRegistrar};

use crate::{BatchSugar, standard_execution_section};

pub mod section_keys {
    pub const EXECUTION: &str = "execution";
}

pub(crate) struct StandardPromptBehaviour {
    pub(crate) batch: BatchSugar,
    pub(crate) termination: lash_core::TerminationMode,
}

#[expect(
    clippy::expect_used,
    reason = "the section key is a validated constant"
)]
pub(crate) fn register_sections(
    reg: &mut PluginRegistrar,
    behaviour: StandardPromptBehaviour,
) -> Result<(), PluginError> {
    reg.prompt().section(
        PromptSectionSpec::new(
            PromptSectionKey::new(section_keys::EXECUTION).expect("valid section key"),
            PromptPlacement::InitialInstructions,
        ),
        Arc::new(move |_: &PromptInput<'_>| {
            Ok(SectionText::Text(format!(
                "## Execution\n\n{}",
                standard_execution_section(behaviour.batch, behaviour.termination)
            )))
        }),
    )
}
