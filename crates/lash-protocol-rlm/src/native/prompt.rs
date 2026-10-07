use crate::dialect::{ExecutionSection, SessionDialect};
use crate::plugin::RlmChannel;
use lash_rlm_types::RlmTermination;

/// The native channel's execution section is the dialect's own rendering for
/// this transport: every transport-specific fragment (lead-in, response-shape
/// teaching, worked example) is authored per channel inside the dialect rather
/// than derived from the cell wording by string replacement (FIG-2881).
///
/// Only the tag-line strip remains a transform: tool-doc and host-surface
/// sections carry contributed prose, and a standalone cell-tag line in that
/// prose must not reach a channel that has no cells.
#[expect(
    clippy::expect_used,
    reason = "the dialect's authoritative execution section validates its catalog by construction"
)]
pub(crate) fn execution_section(
    dialect: &SessionDialect,
    features: crate::protocol::RlmPromptFeatures,
    catalog: &lash_core::ToolCatalog,
    discovery: Option<&lash_core::ToolDiscovery>,
) -> ExecutionSection {
    let tags = dialect.cell_tags();
    let without_tag_lines = |text: String| {
        text.lines()
            .filter(|line| ![tags.open, tags.close].contains(&line.trim()))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let section = dialect
        .execution_section(features, catalog, RlmChannel::NativeTool, discovery)
        .expect("validated dialect catalog");
    ExecutionSection {
        prose: without_tag_lines(section.prose),
        declarations: without_tag_lines(section.declarations),
    }
}

pub(crate) fn finalization(dialect: &SessionDialect, termination: &RlmTermination) -> String {
    dialect.finalization_copy(termination, RlmChannel::NativeTool)
}
