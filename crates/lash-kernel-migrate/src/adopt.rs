//! Adoption carries explicit OperationUse identities and immutable outcomes.
//! No global function-name replacement and no resolver is available on this path.
use lash_kernel_doc::Document;
use std::collections::{BTreeMap, BTreeSet};
use {
    lash_kernel_value::names::ImportKind, lash_kernel_value::names::OperationSlot,
    lash_kernel_value::names::OperationUse, lash_kernel_value::package::Fill,
    lash_kernel_value::package::Link,
};

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AdoptionRefusal {
    #[error("live use {use_:?} has no successor")]
    MissingUse { use_: OperationUse },
    #[error("use correspondence {from:?} -> {to:?} does not name the old and successor Links")]
    WrongLink {
        from: Box<OperationUse>,
        to: Box<OperationUse>,
    },
    #[error("use correspondence {from:?} -> {to:?} is incompatible")]
    IncompatibleUse {
        from: Box<OperationUse>,
        to: Box<OperationUse>,
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
                from: Box::new(*old),
                to: Box::new(new),
            });
        }
        let before = operation(from, old.slot);
        let after = operation(to, new.slot);
        let (Some(before), Some(after)) = (before, after) else {
            return Err(AdoptionRefusal::IncompatibleUse {
                from: Box::new(*old),
                to: Box::new(new),
            });
        };
        // Both the signature and an ordinary pin/deferred grant must survive.
        if before != after
            || matches!(
                (old.slot, new.slot),
                (OperationSlot::Helper(_), OperationSlot::Import(_))
                    | (OperationSlot::Import(_), OperationSlot::Helper(_))
            )
            || matches!((old.slot, new.slot), (OperationSlot::Helper(a), OperationSlot::Helper(b)) if a != b)
        {
            return Err(AdoptionRefusal::IncompatibleUse {
                from: Box::new(*old),
                to: Box::new(new),
            });
        }
        if let Some(previous) = selected.insert(new, after)
            && previous != before
        {
            return Err(AdoptionRefusal::IncompatibleUse {
                from: Box::new(*old),
                to: Box::new(new),
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
    slot: OperationSlot,
) -> Option<(&'a ImportKind, &'a Fill)> {
    if link.document != document.identity() {
        return None;
    }
    let kind = &link.operation_import(&document.imports, slot)?.kind;
    if !matches!(kind, ImportKind::Operation { .. }) {
        return None;
    }
    let fill = link.operation_fill(slot)?;
    matches!(fill, Fill::Operation(_) | Fill::Deferred { .. }).then_some((kind, fill))
}
#[cfg(test)]
mod laws {
    use super::*;
    use lash_kernel_doc::NumberPolicy;

    use lash_kernel_value::names::{ImportSlot, Name};
    use lash_kernel_value::types::Signature;
    use lash_kernel_value::types::Type;
    use {
        lash_kernel_value::names::Import, lash_kernel_value::names::PackagePath,
        lash_kernel_value::names::QualifiedName, lash_kernel_value::names::Segment,
        lash_kernel_value::operations::ContractId,
        lash_kernel_value::operations::OperationRequirement,
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
            helper_fills: Default::default(),
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
            slot: (ImportSlot(slot)).into(),
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

        // Helper outcomes keep their exact declaration and pin, independently
        // of an authored operation with the same name and signature.
        let (mut before, mut from) = pair(&[0], "c1");
        let (mut after, mut to) = pair(&[4], "c1");
        let helper = lash_kernel_doc::parse_definition("function helper.operation() -> Json\nkernel 1\nmetering statements\nimport 0 operation service.run() -> Json\nbody { return perform 0() as Json }").unwrap();
        before.library.insert(helper.identity());
        after.library.insert(helper.identity());
        from.document = before.identity();
        to.document = after.identity();
        let import = helper.body().unwrap().imports[&ImportSlot(0)].clone();
        let id = lash_kernel_value::HelperOperationId::of_import(&import);
        from.helper_fills.insert(
            id,
            lash_kernel_value::HelperFill {
                import: import.clone(),
                fill: from.fills[&ImportSlot(0)].clone(),
            },
        );
        to.helper_fills.insert(
            id,
            lash_kernel_value::HelperFill {
                import,
                fill: to.fills[&ImportSlot(4)].clone(),
            },
        );
        let old = OperationUse {
            link: from.identity(),
            slot: OperationSlot::Helper(id),
        };
        let new = OperationUse {
            link: to.identity(),
            slot: OperationSlot::Helper(id),
        };
        let carried = adopt(
            (&from, &before),
            (&to, &after),
            &BTreeSet::from([old]),
            &BTreeMap::from([(old, new)]),
            &BTreeMap::from([(old, Name::new("unavailable"))]),
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(carried.resolutions[&new], Name::new("unavailable"));
        assert!(matches!(
            adopt(
                (&from, &before),
                (&to, &after),
                &BTreeSet::from([old]),
                &BTreeMap::from([(old, use_of(&to, 4))]),
                &BTreeMap::<OperationUse, Name>::new(),
                &BTreeMap::new()
            ),
            Err(AdoptionRefusal::IncompatibleUse { .. })
        ));
        let Fill::Operation(pin) = &mut to.helper_fills.get_mut(&id).unwrap().fill else {
            unreachable!()
        };
        pin.contract = ContractId::new("c2");
        let changed = OperationUse {
            link: to.identity(),
            slot: OperationSlot::Helper(id),
        };
        assert!(matches!(
            adopt(
                (&from, &before),
                (&to, &after),
                &BTreeSet::from([old]),
                &BTreeMap::from([(old, changed)]),
                &BTreeMap::<OperationUse, Name>::new(),
                &BTreeMap::new()
            ),
            Err(AdoptionRefusal::IncompatibleUse { .. })
        ));
    }
}
