//! Comparisons of recorded process authority with a host's current grants.
//! These helpers make observations. Hosts decide whether to refuse a tool,
//! filter a list, or cancel a process; lash adds no enforcement.

pub use crate::SessionId;
pub use crate::plugins::{SessionToolAccess, ToolCatalog};
use crate::process::{ProcessComponentId, ProcessEnv};
pub use crate::{CapabilityRef, SlotId};
use std::collections::{BTreeMap, BTreeSet};

/// Process tool names outside the session's current admitted offered names.
/// Ambient `current` admits all of `offered`, except hidden names. Restricted
/// access intersects with `offered`, also removing hidden names. Hidden process
/// names are removed too. A process tool absent from `offered` is excess even
/// when the session has Ambient access. Offered membership requires both id and
/// exact name, so a same-name tool with another id is also excess.
pub fn tools_excess(
    env: &ProcessEnv,
    current: &SessionToolAccess,
    offered: &ToolCatalog,
) -> BTreeSet<String> {
    let admitted = current.admitted_names(offered);
    let offered_members: BTreeSet<_> = offered
        .tools
        .iter()
        .map(|entry| (&entry.manifest.id, entry.manifest.name.as_str()))
        .collect();
    env.tools()
        .restricted_tools()
        .into_iter()
        .flatten()
        .filter(|tool| {
            !env.tools().hides(&tool.manifest.name)
                && (!admitted.contains(&tool.manifest.name)
                    || !offered_members.contains(&(&tool.manifest.id, tool.manifest.name.as_str())))
        })
        .map(|tool| tool.manifest.name.clone())
        .collect()
}

/// Whether no visible process tool is excess; Ambient and hidden-name behavior
/// is exactly [`tools_excess`], including tools absent from `offered`.
pub fn tools_within(env: &ProcessEnv, current: &SessionToolAccess, offered: &ToolCatalog) -> bool {
    tools_excess(env, current, offered).is_empty()
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
