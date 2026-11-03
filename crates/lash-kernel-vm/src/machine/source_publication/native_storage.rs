//! Native storage, input/result facts and ordered scratch share one preparation.
use super::*;
use crate::compile::LibRun;
use doc::Value;
use lash_kernel_state::arena::{PreparedBatch, Registration};
use lash_kernel_state::kernel_internals::logical::Ledger;
use lash_kernel_state::kernel_internals::logical::{
    BoundaryTransaction, Dependencies, Owner, PublicationValue as PV, VirtualBody, VirtualEntity,
};
use room::{Measure, Point};

pub(in crate::machine) struct NativeStorage<'a> {
    pub(in crate::machine) contract: doc::NativeResourceContract,
    pub(in crate::machine) function: &'a crate::compile::Lib,
    pub(in crate::machine) args: Vec<Value>,
    pub(in crate::machine) output: Owner,
    pub(in crate::machine) at: Point,
    pub(in crate::machine) release: Box<[room::Owner]>,
    pub(in crate::machine) ledger: std::rc::Rc<std::cell::RefCell<Ledger>>,
    pub(in crate::machine) heap: &'a PagedHeap,
    pub(in crate::machine) charging: bool,
    pub(in crate::machine) limits: lash_kernel_value::data_limits::KernelDataLimits,
    pub(in crate::machine) room: Option<u64>,
    /// Only the checked prepared entry's actual scalar answer is supplied.
    pub(in crate::machine) known: Option<(Value, u32)>,
}
pub(in crate::machine) struct PreparedNative {
    pub(in crate::machine) value: Value,
    pub(in crate::machine) work: u64,
    pub(in crate::machine) known: bool,
    pub(in crate::machine) batch: PreparedBatch,
    pub(in crate::machine) allocated: bool,
    graph_entities: usize,
    output: Owner,
    contract: doc::NativeResourceContract,
    function: lash_kernel_value::FunctionId,
    arguments: Vec<Value>,
    given: usize,
    measurements: Vec<(crate::compile::Source, doc::Measure, u64)>,
    depth: u32,
    at: Point,
    release: Box<[room::Owner]>,
    released: u64,
}
struct DetachedNative<'a> {
    registration: Registration<'a>,
    storage: PreparedNative,
    page: doc::heap::PageValue,
    entities: Vec<VirtualEntity>,
}

/// Detached storage remains unpublished until exact source acceptance.
/// This transaction contains no event journal.
pub(in crate::machine) struct NativeBoundary<'a> {
    pub(in crate::machine) transaction: BoundaryTransaction<'a>,
    pub(in crate::machine) storage: PreparedNative,
}

/// The charge program comes from the canonical compiler at the genuine
/// unpaid suffix. Storage contributes measured arguments/results, never fees.
pub(in crate::machine) struct SourceNative<'a> {
    pub(in crate::machine) recipe: NativeStorage<'a>,
    pub(in crate::machine) charges: &'a room::charges::ChargeProgram,
    pub(in crate::machine) inputs: Vec<Owner>,
    pub(in crate::machine) operands: Vec<Value>,
}
pub(in crate::machine) struct NativePublication {
    pub(in crate::machine) storage: PreparedNative,
    pub(in crate::machine) holding: lash_kernel_state::kernel_internals::logical::Holding,
    pub(in crate::machine) units: u64,
}
impl BoundaryPreparation for SourceNative<'_> {
    type Prepared = NativePublication;
    fn prepare<'a>(
        self,
        arena: &'a mut Arena,
        entry: ExternalEntry<'_>,
    ) -> Result<Option<(room::publication::PreparedEvent<'a>, Self::Prepared)>, HeapFault> {
        use room::publication::{ExactRoots, ExternalFacts};
        let ExternalEntry {
            event,
            frontier,
            ledger,
            heap,
            mut prefix,
            resources,
        } = entry;
        let Some((
            NativeBoundary {
                transaction,
                storage,
            },
            (bound, holding, units),
        )) = self.recipe.prepare_boundary(
            arena,
            &self.inputs,
            |arena, storage, page, entities| {
                storage.bind_facts(&mut prefix.facts)?;
                let formulas: Vec<_> = self.charges.formulas().collect();
                let values = if formulas.is_empty() {
                    vec![]
                } else {
                    if formulas.len() != 1
                        || formulas[0].1 != storage.function
                        || formulas[0].2 != storage.given
                    {
                        return Ok(None);
                    }
                    vec![storage.formula_values()]
                };
                let Some(debt) = self.charges.bind_measured(&values, |_, source, measure| {
                    storage.measure(source, measure)
                }) else {
                    return Ok(None);
                };
                if let Some(canonical) = debt.native_allowance(prefix.allowance.safe_point) {
                    prefix.allowance = canonical;
                    prefix.allowance.depth = prefix.allowance.depth.max(storage.depth());
                } else {
                    prefix.allowance.units = debt.units();
                }
                let scratch = scratch_events(storage.output, storage.contract())?;
                let Ok(bound) = event.bind_external(
                    frontier,
                    ledger,
                    arena,
                    heap,
                    &mut prefix.facts,
                    ExactRoots {
                        inputs: self.inputs.clone(),
                        outputs: vec![(storage.output, PV::Page(page.clone()))],
                        ..Default::default()
                    },
                    prefix.next,
                    prefix.allowance,
                    resources,
                    ExternalFacts {
                        operands: &self.operands,
                        arguments: storage.arguments(),
                        member: None,
                        entities,
                        scratch: &scratch,
                        native: Some(storage.contract()),
                    },
                    &debt,
                ) else {
                    return Ok(None);
                };
                let holding = lash_kernel_state::kernel_internals::logical::Holding::from_page(
                    heap.pages(),
                    page,
                )?;
                Ok(Some((bound, holding, debt.units())))
            },
        )?
        else {
            return Ok(None);
        };
        match bound.finish_external(frontier, ledger, transaction) {
            Ok(prepared) => Ok(Some((
                prepared,
                NativePublication {
                    storage,
                    holding,
                    units,
                },
            ))),
            Err(PublicationError::Miss(_)) => Ok(None),
            Err(PublicationError::Heap(error)) => Err(error),
        }
    }
}

impl NativeStorage<'_> {
    /// Bind source facts before the transaction takes registration's arena
    /// borrow. Returning None abandons storage and all provisional identities.
    pub(in crate::machine) fn prepare_boundary<'a, C>(
        self,
        arena: &'a mut Arena,
        inputs: &[Owner],
        bind: impl FnOnce(
            &Arena,
            &PreparedNative,
            &doc::heap::PageValue,
            &[VirtualEntity],
        ) -> Result<Option<C>, HeapFault>,
    ) -> Result<Option<(NativeBoundary<'a>, C)>, HeapFault> {
        let ledger = std::rc::Rc::clone(&self.ledger);
        let heap = self.heap;
        let mut handles: Vec<_> = self.args.iter().filter_map(Value::handle).collect();
        handles.extend(self.known.and_then(|(value, _)| value.handle()));
        let Some(dependencies) = Dependencies::capture(&ledger.borrow(), arena, &handles) else {
            return Ok(None);
        };
        let Some(DetachedNative {
            mut registration,
            storage,
            page,
            entities,
        }) = self.prepare_storage(arena, inputs)?
        else {
            return Ok(None);
        };
        let Some(bound) = bind(registration.arena(), &storage, &page, &entities)? else {
            return Ok(None);
        };
        let Some(transaction) = ledger.borrow().prepare_native_boundary(
            registration,
            heap,
            inputs,
            storage.output,
            &page,
            &entities,
            &storage.contract,
            dependencies,
        )?
        else {
            return Ok(None);
        };
        Ok(Some((
            NativeBoundary {
                transaction,
                storage,
            },
            bound,
        )))
    }

    fn prepare_storage<'a>(
        self,
        arena: &'a mut Arena,
        inputs: &[Owner],
    ) -> Result<Option<DetachedNative<'a>>, HeapFault> {
        let LibRun::Native { native, .. } = &self.function.run else {
            return Ok(None);
        };
        if !supports(&self.contract)
            || self.args.len() != self.function.arity
            || !self
                .function
                .header
                .signature
                .params
                .iter()
                .zip(&self.args)
                .all(|(param, value)| {
                    (param.optional && matches!(value, Value::Absent))
                        || resident_argument_fits(arena, &param.ty, *value)
                })
            || self.function.limit.is_some()
            || self.function.shares_params
            || self.function.specialization.resource
                != room::CallSource::Native(self.function.specialization.function)
        {
            return Ok(None);
        }
        // The installed implementation, not a caller-authored declaration,
        // authenticates the changing input facts. Unknown contracts decline.
        match native.resource_contract(doc::Args::new(
            &self.args,
            arena.objects(),
            &arena.semantics(),
        )) {
            Ok(Some(contract)) if contract == self.contract => {}
            Err(doc::NativeError::Host(error)) => return Err(error),
            Err(doc::NativeError::Malformed(reason)) => return Err(malformed(reason)),
            _ => return Ok(None),
        }
        let known = match self.known {
            // A scalar Pair/entry does not prove the installed implementation
            // has no scratch. Only its input-evaluated contract can do so.
            Some(_) if !self.contract.events.is_empty() => None,
            Some((value, depth)) => {
                if depth != 0
                    || !crate::compile::scalar(&value)
                    || !crate::compile::import_result_fits(
                        &self.function.header.signature.result,
                        value,
                    )
                {
                    return Ok(None);
                }
                match self
                    .function
                    .specialization
                    .prepared
                    .as_ref()
                    .and_then(|(entry, _)| entry.call(&self.args))
                {
                    Some(actual) if actual.same(&value) => Some(value),
                    Some(_) => return Ok(None),
                    None => None,
                }
            }
            None => None,
        };
        let (value, work, depth, new) = if let Some(value) = known {
            (value, 0, 0, vec![])
        } else {
            let semantics = arena.semantics().with_limits(self.limits);
            let mut nursery = match self.room {
                Some(room) => doc::Nursery::new(arena.objects(), doc::heap::LogicalBytes(room)),
                None => doc::Nursery::unaccounted(arena.objects()),
            };
            let mut counter = doc::WorkCounter::new(None);
            let answer = native.call(doc::NativeCall {
                args: doc::Args::new(&self.args, arena.objects(), &semantics),
                new: &mut nursery,
                counter: &mut counter,
            });
            let value = match answer {
                Ok(value) => value,
                Err(error) => {
                    nursery.discard();
                    return match error {
                        doc::NativeError::Host(error) => Err(error),
                        doc::NativeError::Malformed(reason) => Err(malformed(reason)),
                        _ => Ok(None),
                    };
                }
            };
            // Heap reads can have a checked direct answer without an immediate
            // prepared entry. Authenticate it with the installed native before
            // retaining any nursery output or accepting the source boundary.
            // Keep the nursery's slot owner until every result guard passes.
            // A speculative type miss must leave the precise retry no gaps.
            if !crate::compile::import_result_fits(&self.function.header.signature.result, value)
                || self
                    .known
                    .is_some_and(|(expected, _)| !value.same(&expected))
            {
                nursery.discard();
                return Ok(None);
            }
            if self.room.is_some_and(|room| {
                arena
                    .install_bound(nursery.built())
                    .checked_add(nursery.held().0)
                    .is_none_or(|bytes| bytes > room)
            }) {
                nursery.discard();
                return Ok(None);
            }
            (value, counter.spent(), 0, nursery.into_new())
        };
        let allocated = !new.is_empty();
        let first = arena.next_provisional().ordinal;
        let handles: Vec<_> = new
            .iter()
            .filter_map(|(handle, object)| {
                matches!(
                    object,
                    doc::NewObject::List(_)
                        | doc::NewObject::Map(_)
                        | doc::NewObject::Set(_)
                        | doc::NewObject::Record(_)
                )
                .then_some(*handle)
            })
            .collect();
        let mut installation = arena.prepare_install(new).map_err(native_fault)?;
        let mut identities = Vec::new();
        for (index, handle) in handles.iter().enumerate() {
            let entity = doc::heap::EntityId {
                family: doc::heap::FamilyId::PROVISIONAL,
                ordinal: first
                    .checked_add(index as u64)
                    .ok_or_else(|| malformed("native identity overflow"))?,
            };
            installation
                .bind_entity(*handle, entity)
                .map_err(native_fault)?;
            identities.push(entity);
        }
        // Retain the canonical changing formula inputs while the detached
        // graph is readable. A region must bind these facts to its compiled
        // plan; it cannot accept the legacy allowance's fee as authority.
        let mut measurements = Vec::new();
        for &(source, measure) in self
            .function
            .charge
            .measurements()
            .iter()
            .filter(|_| self.charging)
        {
            let operand = match source {
                crate::compile::Source::Arg(index) => self.args.get(index),
                crate::compile::Source::Result => Some(&value),
                crate::compile::Source::Nothing => None,
            };
            let measured = if let Some(value) = operand {
                let measured = match measure {
                    doc::Measure::Size => crate::data::view_size(&installation, value),
                    doc::Measure::DeepSize => crate::data::view_deep_size(&installation, value),
                    doc::Measure::NestedSize => crate::data::view_nested_size(&installation, value),
                    doc::Measure::Magnitude => {
                        Ok(crate::data::view_magnitude(&installation, value))
                    }
                };
                match measured {
                    Ok(measured) => measured,
                    Err(doc::NativeError::Host(error)) => return Err(error),
                    Err(doc::NativeError::Malformed(reason)) => return Err(malformed(reason)),
                    Err(_) => return Ok(None),
                }
            } else {
                0
            };
            measurements.push((source, measure, measured));
        }
        let mut entities = Vec::new();
        for (handle, entity) in handles.into_iter().zip(identities) {
            let body = installation.collection(handle).map_err(native_fault)?;
            let body = project_body(&mut installation, self.heap, body)?;
            entities.push(VirtualEntity { entity, body });
        }
        let page = crate::machine::project::project_prepared(
            &mut installation,
            self.heap.pages(),
            &value,
        )?;
        let (mut registration, batch) = installation.into_parts();
        let released = self.ledger.borrow().release_cost_preserving_graph(
            registration.arena(),
            self.heap,
            inputs,
            &entities,
            &page,
        )?;
        // Authenticate the result against its input-evaluated contract before
        // exact source acceptance can publish the detached storage.
        let actual = PV::Page(page.clone());
        let owners = entities
            .iter()
            .zip(self.contract.events.iter().filter_map(|event| match event {
                doc::NativeResourceEvent::Entity { owner, .. } => Some(*owner),
                _ => None,
            }))
            .map(|(entity, owner)| (entity.entity, owner))
            .collect();
        if actual.native_holding(self.heap, registration.arena(), &owners)? != self.contract.output
        {
            return Ok(None);
        }
        Ok(Some(DetachedNative {
            registration,
            storage: PreparedNative {
                value,
                batch,
                allocated,
                graph_entities: entities.len(),
                output: self.output,
                contract: self.contract,
                function: self.function.specialization.function,
                arguments: self.args,
                given: inputs.len(),
                measurements,
                work,
                known: self.known.is_some(),
                depth,
                at: self.at,
                release: self.release,
                released,
            },
            page,
            entities,
        }))
    }
}

impl PreparedNative {
    pub(in crate::machine) fn arguments(&self) -> &[Value] {
        &self.arguments
    }
    pub(in crate::machine) fn depth(&self) -> u64 {
        u64::from(self.depth)
    }

    pub(in crate::machine) fn bind_facts(
        &self,
        facts: &mut LeafExecution,
    ) -> Result<(), HeapFault> {
        let facts = facts
            .0
            .get_mut(&self.at)
            .ok_or_else(|| malformed("native value lost its release event"))?;
        facts.values.insert(
            Measure::Release(self.release.clone()),
            i128::from(self.released),
        );
        facts.values.insert(
            Measure::NativeScratch {
                at: self.at.site.clone(),
                function: self.function,
            },
            i128::from(
                self.contract
                    .peak_scratch_bytes()
                    .ok_or_else(|| malformed("native scratch schedule changed"))?,
            ),
        );
        facts.call = Some((
            self.at.site.clone(),
            room::CallSource::Native(self.function),
            room::CheckedRoom::native(self.function, &self.contract)
                .ok_or_else(|| malformed("native result contract changed"))?,
        ));
        Ok(())
    }
    pub(in crate::machine) fn formula_values(&self) -> room::charges::FormulaValues<'_> {
        room::charges::FormulaValues {
            arguments: &self.arguments[..self.given],
            result: self.value,
        }
    }
    pub(in crate::machine) fn measure(
        &self,
        source: crate::compile::Source,
        measure: doc::Measure,
    ) -> Option<u64> {
        self.measurements
            .iter()
            .find_map(|(s, m, value)| (*s == source && *m == measure).then_some(*value))
    }
    pub(in crate::machine) fn contract(&self) -> &doc::NativeResourceContract {
        &self.contract
    }

    pub(in crate::machine) fn known(&self) -> bool {
        self.known
    }
    pub(in crate::machine) fn allocated_immutable(&self) -> bool {
        self.allocated && self.graph_entities == 0
    }
    pub(in crate::machine) fn value(&self) -> Value {
        self.value
    }
    pub(in crate::machine) fn work(&self) -> u64 {
        self.work
    }
    pub(in crate::machine) fn publish(self, arena: &mut Arena) {
        #[cfg(any(test, feature = "testing"))]
        if self.graph_entities > 1 {
            crate::machine::islands::witness::native_graph_published();
        }
        self.batch.publish(arena);
    }
}

/// A kind check suffices only for unconstrained containers. A resident list
/// with a supported scalar item contract checks every actual member; nested
/// or cold shapes retain the ordinary signature handler.
pub(in crate::machine) fn resident_argument_fits(
    arena: &Arena,
    ty: &lash_kernel_value::Type,
    value: Value,
) -> bool {
    if let Some(check) = crate::compile::import_result_check(ty) {
        return check(value);
    }
    match (ty, value) {
        (lash_kernel_value::Type::List(item), Value::List(handle)) => {
            let Some(check) = crate::compile::import_result_check(item) else {
                return false;
            };
            arena
                .sequence(handle)
                .is_ok_and(|values| values.iter().copied().all(check))
        }
        _ => false,
    }
}

pub(in crate::machine) fn supports(contract: &doc::NativeResourceContract) -> bool {
    contract.exit == doc::NativeResourceExit::Returned
        && contract.retained_bytes().is_some()
        && contract.events.iter().all(|event| {
            matches!(
                event,
                doc::NativeResourceEvent::Entity {
                    kind: doc::NativeResourceEntity::List
                        | doc::NativeResourceEntity::Map
                        | doc::NativeResourceEntity::Set
                        | doc::NativeResourceEntity::Record { .. },
                    ..
                } | doc::NativeResourceEvent::Scratch { .. }
            )
        })
}

fn project_body(
    installation: &mut lash_kernel_state::arena::PreparedInstallation<'_>,
    heap: &PagedHeap,
    body: doc::NewObject,
) -> Result<VirtualBody, HeapFault> {
    let project = |installation: &mut lash_kernel_state::arena::PreparedInstallation<'_>, value| {
        crate::machine::project::project_prepared(installation, heap.pages(), &value)
    };
    Ok(match body {
        doc::NewObject::List(values) => VirtualBody::List(
            values
                .into_iter()
                .map(|value| project(installation, value))
                .collect::<Result<_, _>>()?,
        ),
        doc::NewObject::Set(values) => VirtualBody::Set(
            values
                .into_iter()
                .map(|value| project(installation, value))
                .collect::<Result<_, _>>()?,
        ),
        doc::NewObject::Map(values) => VirtualBody::Map(
            values
                .into_iter()
                .map(|(key, value)| {
                    Ok((project(installation, key)?, project(installation, value)?))
                })
                .collect::<Result<_, HeapFault>>()?,
        ),
        doc::NewObject::Record(values) => VirtualBody::Record(
            values
                .into_iter()
                .map(|(key, value)| {
                    Ok((project(installation, key)?, project(installation, value)?))
                })
                .collect::<Result<_, HeapFault>>()?,
        ),
        _ => return Err(malformed("native mutable recipe has another body kind")),
    })
}

/// Retain the canonical lease boundaries, including a release before an entity
/// allocation. Equal peaks do not imply interchangeable phase schedules.
pub(in crate::machine) fn scratch_events(
    output: Owner,
    contract: &doc::NativeResourceContract,
) -> Result<Vec<lash_kernel_state::kernel_internals::logical::ScratchEvent>, HeapFault> {
    use doc::{NativeResourceEvent as Event, NativeResourceRelease as Release};
    use lash_kernel_state::kernel_internals::logical::{ScratchEvent, Slot};
    let Owner::Value {
        task,
        invocation,
        slot: Slot::Operand(_),
    } = output
    else {
        return Err(malformed("native output has no operand owner"));
    };
    if contract.peak_scratch_bytes().is_none() {
        return Err(malformed("native scratch contract is invalid"));
    }
    let owner = |index: doc::NativeResourceOwner| Owner::Value {
        task,
        invocation,
        slot: Slot::Scratch(index.0),
    };
    let mut active = Vec::new();
    let mut events = Vec::new();
    for event in &contract.events {
        active.retain(|(index, release)| {
            if *release == Release::Before(event.owner()) {
                events.push(ScratchEvent::End {
                    owner: owner(*index),
                });
                false
            } else {
                true
            }
        });
        if let Event::Scratch {
            owner: index,
            bytes,
            release,
        } = event
        {
            events.push(ScratchEvent::Begin {
                owner: owner(*index),
                bytes: *bytes,
            });
            active.push((*index, *release));
        }
    }
    events.extend(active.into_iter().map(|(index, _)| ScratchEvent::End {
        owner: owner(index),
    }));
    Ok(events)
}

#[cfg(test)]
thread_local! { static NATIVE_PUBLICATIONS: Cell<[u64; 5]> = const { Cell::new([0; 5]) }; }
#[cfg(test)]
pub(in crate::machine) fn published(
    packet: bool,
    contract: &doc::NativeResourceContract,
    optional: bool,
) {
    NATIVE_PUBLICATIONS.with(|counts| {
        let mut next = counts.get();
        next[usize::from(packet)] += 1;
        next[2] += u64::from(
            contract
                .events
                .iter()
                .any(|event| matches!(event, doc::NativeResourceEvent::Entity { .. })),
        );
        next[3] += u64::from(
            contract
                .events
                .iter()
                .any(|event| matches!(event, doc::NativeResourceEvent::Scratch { .. })),
        );
        next[4] += u64::from(optional);
        counts.set(next);
    });
}
#[cfg(test)]
impl crate::machine::KernelMachine {
    pub(crate) fn take_native_publications(&self) -> [u64; 5] {
        NATIVE_PUBLICATIONS.with(|counts| counts.replace([0; 5]))
    }
}
