//! Comparisons of recorded process authority with a host's current grants.
//! These helpers make observations. Hosts decide whether to refuse a tool,
//! filter a list, or cancel a process; lash adds no enforcement.

pub use crate::SessionId;
pub use crate::plugins::ToolCatalog;
use crate::process::{ProcessComponentId, ProcessEnv};
pub use crate::tools::ToolId;
pub use crate::{CapabilityRef, SlotId};
use std::collections::{BTreeMap, BTreeSet};

/// One linked effect whose tool `offered` does not offer as the process
/// links it: the effect path and the linked tool, as evidence.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ExcessLink {
    /// The effect path the process performs.
    pub effect: String,
    /// The tool the effect is linked to.
    pub tool: ToolId,
}

/// The process's linked effects whose tool `offered` does not offer as the
/// link retains it. A link and an offered tool compare by tool id and
/// dispatch surface ([`crate::process::ProcessSurface`]) alone: the
/// model-facing name, the binding path and the provider key that serves it
/// are not authority, so neither makes a link excess.
pub fn tools_excess(env: &ProcessEnv, offered: &ToolCatalog) -> BTreeSet<ExcessLink> {
    let offered = offered
        .tools
        .iter()
        .map(|entry| lash_core::ProcessSurface::of_parts(&entry.manifest, &entry.contract))
        .collect::<Vec<_>>();
    env.effects()
        .iter()
        .filter(|(_, link)| !offered.contains(&link.surface))
        .map(|(effect, link)| ExcessLink {
            effect: effect.to_string(),
            tool: link.tool().clone(),
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
