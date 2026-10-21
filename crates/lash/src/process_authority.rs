//! Comparisons of recorded process authority with a host's current grants.
//! These helpers make observations. Hosts decide whether to refuse a tool,
//! filter a list, or cancel a process; lash adds no enforcement.

pub use crate::plugins::ToolCatalog;
use crate::process::ProcessEnv;
pub use crate::tools::ToolId;
pub use crate::{CapabilityRef, SlotId};
pub use lash_vocabulary::identity::SessionId;
use lash_vocabulary::providers::ProcessComponentId;
use std::collections::{BTreeMap, BTreeSet};

/// One linked effect whose tool `offered` does not offer as the process
/// links it: the effect path and the linked tool, as evidence.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ExcessLink {
    /// The effect path the process performs.
    pub operation: lash_vocabulary::names::OperationUse,
    /// The tool the effect is linked to.
    pub tool: ToolId,
}

/// The process's linked effects whose tool `offered` does not offer. A link
/// and an offered tool compare by tool id alone: the model-facing name, the
/// provider key and the policy are not authority, and the contract a call
/// runs under is its code's, judged at each perform by the one judge
/// ([`crate::tools::judge`]), so none makes a link excess.
pub fn tools_excess(env: &ProcessEnv, offered: &ToolCatalog) -> BTreeSet<ExcessLink> {
    env.execution()
        .uses
        .iter()
        .filter_map(|(use_, binding)| match binding {
            lash_core_store::tool_surface::UseBinding::Fixed(operation) => {
                Some((use_, &operation.route))
            }
            lash_core_store::tool_surface::UseBinding::Deferred(_) => None,
        })
        .filter(|(_, link)| {
            !offered
                .entries()
                .iter()
                .any(|entry| entry.manifest.id == link.tool)
        })
        .map(|(effect, link)| ExcessLink {
            operation: effect.clone(),
            tool: link.tool.clone(),
        })
        .collect()
}

/// Whether no linked effect is excess: exactly [`tools_excess`] is empty.
pub fn tools_within(env: &ProcessEnv, offered: &ToolCatalog) -> bool {
    tools_excess(env, offered).is_empty()
}

/// Components not named in `allowed`, in the environment's composition order.
/// A component with no tools still carries authority and is compared.
pub fn components_excess(
    env: &ProcessEnv,
    allowed: &BTreeSet<ProcessComponentId>,
) -> Vec<ProcessComponentId> {
    env.components()
        .iter()
        .filter(|entry| !allowed.contains(entry.id()))
        .map(|entry| entry.id().clone())
        .collect()
}

/// Process capability slots whose current ref is absent or unequal.
/// Equal refs are within; every other ref is excess. An opaque ref is never
/// judged narrower. Bindings, contracts and arguments are part of ref equality.
pub fn capabilities_excess(
    env: &ProcessEnv,
    current: &BTreeMap<SlotId, CapabilityRef>,
) -> BTreeMap<SlotId, CapabilityRef> {
    env.capabilities()
        .iter()
        .filter(|(slot, reference)| current.get(*slot) != Some(*reference))
        .map(|(slot, reference)| (slot.clone(), reference.clone()))
        .collect()
}

/// Named-session read grants per component (D17), including empty grant sets.
/// Provenance and originating sessions confer no grant here.
pub fn session_grants(env: &ProcessEnv) -> BTreeMap<ProcessComponentId, BTreeSet<SessionId>> {
    env.components()
        .iter()
        .map(|entry| (entry.id().clone(), entry.sessions().clone()))
        .collect()
}
