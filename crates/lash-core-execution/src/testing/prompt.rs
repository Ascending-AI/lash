//! Prompt composition outside a model call, for tests (ADR 0133). The
//! runtime builds a call's cut and composes it only at the call's admission;
//! these builders let a test drive one plugin's sections over a cut it
//! states, on the same composer.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::plugin::prompt::{CommittedPluginNamespace, PromptCatalog, PromptCompositionError};
pub use crate::plugin::prompt::{
    ComposedPrompt, ComposedSection, PromptCut, PromptCutParts, PromptRenderPool,
};
use crate::prompt_sections::{PromptPlan, PromptPurpose};

/// The cut `parts` state, with no protocol facts.
pub fn cut(parts: PromptCutParts) -> PromptCut {
    PromptCut::new(parts)
}

/// The cut `parts` state, for a protocol stating `protocol` facts.
pub fn cut_with(
    parts: PromptCutParts,
    protocol: Option<crate::plugin::prompt::ProtocolPromptFacts>,
) -> PromptCut {
    crate::core_internal::prompt_cut(parts, protocol)
}

/// A namespace frozen at `generation` with `values`.
pub fn namespace(
    generation: u64,
    values: BTreeMap<String, serde_json::Value>,
) -> CommittedPluginNamespace {
    CommittedPluginNamespace::new(generation, values)
}

/// Compose `catalog`'s sections for a `purpose` call over `cut` under `plan`,
/// on the process's shared render pool.
///
/// # Errors
///
/// The composer's [`PromptCompositionError`].
pub async fn compose(
    catalog: &PromptCatalog,
    plan: &PromptPlan,
    purpose: &PromptPurpose,
    cut: PromptCut,
) -> Result<ComposedPrompt, PromptCompositionError> {
    catalog
        .compose(plan, purpose, Arc::new(cut), PromptRenderPool::shared())
        .await
}

/// Each section `plan` resolves for a `purpose` call over `cut`, composed in
/// plan order on the caller's thread: its base text, wrapper outputs and
/// final text.
///
/// # Errors
///
/// The first section's [`PromptCompositionError`] in plan order.
///
/// # Panics
///
/// When the plan does not resolve.
pub fn compose_sections(
    catalog: &PromptCatalog,
    plan: &PromptPlan,
    purpose: &PromptPurpose,
    cut: &PromptCut,
) -> Result<Vec<ComposedSection>, PromptCompositionError> {
    let resolved = catalog
        .resolve(plan, purpose, cut.offered())
        .expect("the plan resolves");
    (0..resolved.record().sections.len())
        .map(|index| resolved.compose_section(index, cut))
        .collect()
}
