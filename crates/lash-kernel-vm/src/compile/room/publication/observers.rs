//! A source activation's observer footprint is compiled apart from storage.
use super::*;
use crate::compile::room::placement::{BindingPlacement, DenseActivation};
use crate::machine::SlotState;
use lash_kernel_doc::arena::ArenaHandle;
use logical::{BoundaryTransaction, Dependencies, Holding, ObserverExclusion, Slot};

struct BindingObserver {
    slot: Slot,
    shared: bool,
}

/// The finite source roots through which a private kernel may observe its
/// members. This proves exclusion only; the kernel still proves source order,
/// all legal exits, charges, logical peaks and synchronous call composition.
pub(crate) struct ObserverScope {
    placement: Arc<BindingPlacement>,
    bindings: Box<[BindingObserver]>,
    operands: Box<[Site]>,
}

/// Opaque current observer evidence. It cannot outlive a new alias, ledger
/// revision, cold reopen or actual input content change, nor be serialized.
pub(crate) struct PrivateObservers {
    source: Arc<Source>,
    invocation: Invocation,
    exclusion: ObserverExclusion,
    dependencies: Dependencies,
}

impl ObserverScope {
    /// Physical binding indices are validated once against immutable source
    /// placement. Evaluated operands name genuine source owner selectors.
    pub(crate) fn compile(
        placement: Arc<BindingPlacement>,
        physical_bindings: &[u32],
        operands: &[Site],
    ) -> Result<Self, Refused> {
        let source = placement.source();
        source.verify()?;
        let owners = source.owner_slots()?;
        let mut bindings = Vec::new();
        let mut seen = BTreeSet::new();
        for physical in physical_bindings {
            let mut selected = source.bindings.iter().filter_map(|binding| {
                let logical::Owner::Value { slot, .. } =
                    source.binding_root(binding.index, TaskId::MAIN, 0)?
                else {
                    return None;
                };
                (placement.physical(slot) == Some(*physical)).then_some((binding, slot))
            });
            let (binding, slot) = selected.next().ok_or(Refused::Source)?;
            if selected.next().is_some() || !seen.insert(slot) {
                return Err(Refused::Source);
            }
            bindings.push(BindingObserver {
                slot,
                shared: binding.shared,
            });
        }
        let mut seen = BTreeSet::new();
        for site in operands {
            if owners.get(&Owner::Operand(site.clone())).is_none() || !seen.insert(site.clone()) {
                return Err(Refused::Source);
            }
        }
        if bindings.is_empty() && operands.is_empty() {
            return Err(Refused::Retained);
        }
        Ok(Self {
            placement,
            bindings: bindings.into(),
            operands: operands.into(),
        })
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "one observer footprint and its actual activation"
    )]
    pub(crate) fn bind(
        &self,
        activation: &DenseActivation<'_>,
        invocation: &Invocation,
        ledger: &Ledger,
        arena: &mut Arena,
        heap: &PagedHeap,
        operands: &[(Site, doc::Value)],
        targets: &[ArenaHandle],
    ) -> Result<Option<PrivateObservers>, HeapFault> {
        let source = self.placement.source();
        if !activation.current(source, ledger, invocation)
            || operands.len() != self.operands.len()
            || targets.is_empty()
        {
            return Ok(None);
        }
        let mut roots = Vec::new();
        let mut handles = targets.to_vec();
        for binding in &self.bindings {
            let Some(state) = activation.state(binding.slot) else {
                return Ok(None);
            };
            let holding = match (binding.shared, state) {
                (false, SlotState::Value(value)) => {
                    handles.extend(value.handle());
                    Holding::from_native(arena, heap.pages(), &value)?
                }
                (true, SlotState::Cell(handle)) => {
                    handles.push(handle);
                    if matches!(binding.slot, Slot::Capture(_)) {
                        Holding::borrowed_cell(arena, heap.pages(), handle)?
                    } else {
                        Holding::from_native(arena, heap.pages(), &doc::Value::ObjectRef(handle))?
                    }
                }
                _ => return Ok(None),
            };
            roots.push((invocation.owner(binding.slot), holding));
        }
        let logical::Owner::Frame { task, .. } = invocation.frame() else {
            return Ok(None);
        };
        let owners = source
            .owner_slots()
            .map_err(|_| malformed("observer source owners changed"))?;
        for (site, (actual, value)) in self.operands.iter().zip(operands) {
            if actual != site {
                return Ok(None);
            }
            let Some(owner) = owners.root(&Owner::Operand(site.clone()), task, invocation.id())
            else {
                return Ok(None);
            };
            handles.extend(value.handle());
            roots.push((owner, Holding::from_native(arena, heap.pages(), value)?));
        }
        let mut entities = Vec::new();
        for handle in targets {
            entities.push(
                arena
                    .ledger_key(*handle)
                    .map_err(|error| malformed(&error.to_string()))?,
            );
        }
        let Some(exclusion) =
            ledger.exclude_observers(invocation, &roots, &entities, heap.pages())?
        else {
            return Ok(None);
        };
        let Some(dependencies) = Dependencies::capture(ledger, arena, &handles) else {
            return Ok(None);
        };
        Ok(Some(PrivateObservers {
            source: Arc::clone(source),
            invocation: invocation.clone(),
            exclusion,
            dependencies,
        }))
    }
}

impl PrivateObservers {
    pub(crate) fn permits(
        &self,
        source: &Arc<Source>,
        invocation: &Invocation,
        ledger: &Ledger,
        arena: &Arena,
        target: ArenaHandle,
    ) -> bool {
        Arc::ptr_eq(source, &self.source)
            && invocation == &self.invocation
            && self.dependencies.is_current(ledger, arena)
            && arena
                .facts(target)
                .ok()
                .and_then(|facts| facts.ledger_entity())
                .is_some_and(|entity| self.exclusion.permits(ledger, entity))
    }

    pub(crate) fn finish<'a>(
        self,
        transaction: BoundaryTransaction<'a>,
    ) -> Result<BoundaryTransaction<'a>, HeapFault> {
        transaction
            .bind_dependencies(self.dependencies)?
            .with_private_observers(self.exclusion)
    }
}

fn malformed(reason: &str) -> HeapFault {
    HeapFault::Malformed(reason.into())
}
