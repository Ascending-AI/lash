//! Adoption carries explicit OperationUse identities and immutable outcomes.
//! No global function-name replacement and no resolver is available on this path.
use lash_kernel_doc::Document;
use lash_vocabulary::{
    names::{ImportKind, ImportSlot, OperationUse},
    package::{Fill, Link},
};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AdoptionRefusal {
    #[error("live use {use_:?} has no successor")]
    MissingUse { use_: OperationUse },
    #[error("use correspondence {from:?} -> {to:?} does not name the old and successor Links")]
    WrongLink {
        from: OperationUse,
        to: OperationUse,
    },
    #[error("use correspondence {from:?} -> {to:?} is incompatible")]
    IncompatibleUse {
        from: OperationUse,
        to: OperationUse,
    },
    #[error("successor use {use_:?} would merge different committed outcomes")]
    OutcomeConflict { use_: OperationUse },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdoptedUses<T> {
    pub uses: BTreeMap<OperationUse, OperationUse>,
    /// Old rows remain retained for history and escaped callables; these are successor copies only.
    pub resolutions: BTreeMap<OperationUse, T>,
}
/// Map every live/held use explicitly. Resolved/refused/unavailable outcomes are all opaque immutable facts here.
pub fn adopt<T: Clone + Eq>(
    from: (&Link, &Document),
    to: (&Link, &Document),
    live: &BTreeSet<OperationUse>,
    correspondence: &BTreeMap<OperationUse, OperationUse>,
    outcomes: &BTreeMap<OperationUse, T>,
    successor_outcomes: &BTreeMap<OperationUse, T>,
) -> Result<AdoptedUses<T>, AdoptionRefusal> {
    let mut uses = BTreeMap::new();
    let mut resolutions = successor_outcomes.clone();
    let mut selected: BTreeMap<OperationUse, (&ImportKind, &Fill)> = BTreeMap::new();
    for old in live.iter().chain(outcomes.keys()) {
        let new = *correspondence
            .get(old)
            .ok_or(AdoptionRefusal::MissingUse { use_: *old })?;
        if old.link != from.0.identity() || new.link != to.0.identity() {
            return Err(AdoptionRefusal::WrongLink {
                from: *old,
                to: new,
            });
        }
        let before = operation(from, old.slot);
        let after = operation(to, new.slot);
        let (Some(before), Some(after)) = (before, after) else {
            return Err(AdoptionRefusal::IncompatibleUse {
                from: *old,
                to: new,
            });
        };
        // Both the signature and an ordinary pin/deferred grant must survive.
        if before != after {
            return Err(AdoptionRefusal::IncompatibleUse {
                from: *old,
                to: new,
            });
        }
        if let Some(previous) = selected.insert(new, after)
            && previous != before
        {
            return Err(AdoptionRefusal::IncompatibleUse {
                from: *old,
                to: new,
            });
        }
        uses.insert(*old, new);
        if let Some(outcome) = outcomes.get(old) {
            if resolutions
                .get(&new)
                .is_some_and(|previous| previous != outcome)
            {
                return Err(AdoptionRefusal::OutcomeConflict { use_: new });
            }
            resolutions.insert(new, outcome.clone());
        }
    }
    Ok(AdoptedUses { uses, resolutions })
}
fn operation<'a>(
    (link, document): (&'a Link, &'a Document),
    slot: ImportSlot,
) -> Option<(&'a ImportKind, &'a Fill)> {
    if link.document != document.identity() {
        return None;
    }
    let kind = &document.imports.get(&slot)?.kind;
    if !matches!(kind, ImportKind::Operation { .. }) {
        return None;
    }
    let fill = link.fills.get(&slot)?;
    matches!(fill, Fill::Operation(_) | Fill::Deferred { .. }).then_some((kind, fill))
}
#[cfg(test)]
mod laws {
    use super::*;
    use lash_kernel_doc::NumberPolicy;

    use lash_vocabulary::legacy_names::Name;
    use lash_vocabulary::types::Signature;
    use lash_vocabulary::types::Type;
    use lash_vocabulary::{
        names::{Import, PackagePath, QualifiedName, Segment},
        operations::{ContractId, OperationRequirement},
    };
    fn pair(slots: &[u32], contract: &str) -> (Document, Link) {
        let mut doc = Document::new(NumberPolicy::Float, vec![]);
        let signature = Signature {
            params: vec![],
            result: Type::Json,
        };
        for slot in slots {
            doc.imports.insert(
                ImportSlot(*slot),
                Import {
                    name: QualifiedName {
                        package: PackagePath::parse("service").unwrap(),
                        member: Segment::new("run").unwrap(),
                        type_member: None,
                    },
                    kind: ImportKind::Operation {
                        signature: signature.clone(),
                    },
                },
            );
        }
        let link = Link {
            document: doc.identity(),
            fills: slots
                .iter()
                .map(|slot| {
                    (
                        ImportSlot(*slot),
                        Fill::Operation(OperationRequirement {
                            signature: signature.clone(),
                            contract: ContractId::new(contract),
                        }),
                    )
                })
                .collect(),
        };
        (doc, link)
    }
    /// §15.1 adoption carries resolved facts by explicit uses and refuses incompatible pins or merges.
    #[test]
    fn adoption_preserves_outcomes_and_refuses_incompatible_or_conflicting_uses() {
        let (old, from) = pair(&[0, 1], "c1");
        let (new, to) = pair(&[4], "c1");
        let use_of = |link: &Link, slot| OperationUse {
            link: link.identity(),
            slot: ImportSlot(slot),
        };
        let a = use_of(&from, 0);
        let b = use_of(&from, 1);
        let successor = use_of(&to, 4);
        let correspondence = BTreeMap::from([(a, successor), (b, successor)]);
        let outcomes = BTreeMap::from([(a, Name::new("refused"))]);
        let carried = adopt(
            (&from, &old),
            (&to, &new),
            &BTreeSet::from([a]),
            &correspondence,
            &outcomes,
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(carried.resolutions.get(&successor), outcomes.get(&a));
        let conflict = BTreeMap::from([(a, Name::new("refused")), (b, Name::new("unavailable"))]);
        assert!(matches!(
            adopt(
                (&from, &old),
                (&to, &new),
                &BTreeSet::from([a, b]),
                &correspondence,
                &conflict,
                &BTreeMap::new()
            ),
            Err(AdoptionRefusal::OutcomeConflict { .. })
        ));
        let (changed, changed_link) = pair(&[4], "c2");
        let changed_use = use_of(&changed_link, 4);
        assert!(matches!(
            adopt(
                (&from, &old),
                (&changed_link, &changed),
                &BTreeSet::from([a]),
                &BTreeMap::from([(a, changed_use)]),
                &outcomes,
                &BTreeMap::new()
            ),
            Err(AdoptionRefusal::IncompatibleUse { .. })
        ));
    }
}
