//! Adoption uses explicit import/use correspondence, never a global function-name rewrite.
use lash_kernel_migrate::{AdoptedUses, AdoptionRefusal};
use lash_kernel_package::{LinkRefusal, LinkedContent};
use lash_vocabulary::{names::*, package::*};
use std::collections::{BTreeMap, BTreeSet};
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HelperAdoptionRefusal {
    #[error(transparent)]
    Link(#[from] LinkRefusal),
    #[error(transparent)]
    Use(#[from] AdoptionRefusal),
    #[error("successor Import for {use_:?} is absent or ambiguous")]
    AmbiguousUse { use_: OperationUse },
    #[error("{message}")]
    State { message: String },
}
/// Pair by the complete scoped Import and immutable fill. Names alone cannot select an instance.
pub fn plan_adoption<T: Clone + Eq>(
    content: &LinkedContent,
    from: LinkId,
    to: LinkId,
    live: &BTreeSet<OperationUse>,
    outcomes: &BTreeMap<OperationUse, T>,
    successor_outcomes: &BTreeMap<OperationUse, T>,
) -> Result<AdoptedUses<T>, HelperAdoptionRefusal> {
    let old = content.link(from)?;
    let new = content.link(to)?;
    let old_doc = content.document(old.document)?;
    let new_doc = content.document(new.document)?;
    let mut correspondence = BTreeMap::new();
    let mut local_live = BTreeSet::new();
    let mut local_outcomes = BTreeMap::new();
    let mut retained = BTreeMap::new();
    let mut resolutions = successor_outcomes.clone();
    for use_ in live.iter().chain(outcomes.keys()) {
        if use_.link != from {
            content.link(use_.link)?;
            retained.insert(*use_, *use_);
            if let Some(outcome) = outcomes.get(use_) {
                if resolutions.get(use_).is_some_and(|known| known != outcome) {
                    return Err(AdoptionRefusal::OutcomeConflict { use_: *use_ }.into());
                }
                resolutions.insert(*use_, outcome.clone());
            }
            continue;
        }
        let import = old_doc.imports.get(&use_.slot);
        let fill = old.fills.get(&use_.slot);
        let candidates = new_doc
            .imports
            .iter()
            .filter(|(slot, found)| Some(*found) == import && new.fills.get(slot) == fill)
            .map(|(slot, _)| *slot)
            .collect::<Vec<_>>();
        let [slot] = candidates.as_slice() else {
            return Err(HelperAdoptionRefusal::AmbiguousUse { use_: *use_ });
        };
        correspondence.insert(
            *use_,
            OperationUse {
                link: to,
                slot: *slot,
            },
        );
        local_live.insert(*use_);
        if let Some(outcome) = outcomes.get(use_) {
            local_outcomes.insert(*use_, outcome.clone());
        }
    }
    let mut adopted = lash_kernel_migrate::adopt(
        (old, old_doc),
        (new, new_doc),
        &local_live,
        &correspondence,
        &local_outcomes,
        &resolutions,
    )?;
    adopted.uses.extend(retained);
    Ok(adopted)
}
