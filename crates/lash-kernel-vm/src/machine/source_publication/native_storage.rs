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
    pub(in crate::machine) units: u64,
    pub(in crate::machine) work: u64,
    pub(in crate::machine) known: bool,
    pub(in crate::machine) batch: PreparedBatch,
    pub(in crate::machine) allocated: bool,
    graph_entities: usize,
    output: Owner,
    contract: doc::NativeResourceContract,
    function: lash_kernel_value::FunctionId,
    #[allow(
        dead_code,
        reason = "core charge binding consumes the retained actual operands"
    )]
    arguments: Vec<Value>,
    #[allow(
        dead_code,
        reason = "core must extend FormulaValues for detached heap results"
    )]
    measurements: Vec<(crate::compile::Source, doc::Measure, u64)>,
    depth: u32,
    at: Point,
    release: Box<[room::Owner]>,
    released: u64,
}
impl StoragePreparation for NativeStorage<'_> {
    type Prepared = PreparedNative;
    const MATERIALIZES: bool = true;
    fn prepare<'a>(
        self,
        arena: &'a mut Arena,
        actual: Program,
    ) -> Result<Option<PreparedStorage<'a, Self::Prepared>>, HeapFault> {
        let [event] = actual.events() else {
            return Ok(None);
        };
        if !event.outputs.is_empty()
            || !event.entities.is_empty()
            || event.frame.is_some()
            || event.member.is_some()
        {
            return Ok(None);
        }
        let Some(detached) = self.prepare_storage(arena, &event.inputs)? else {
            return Ok(None);
        };
        let DetachedNative {
            registration,
            storage,
            page,
            entities,
        } = detached;
        let mut event = event.clone();
        event.native = Some(storage.contract().clone());
        event.outputs.push((storage.output, PV::Page(page)));
        event.entities = entities;
        Ok(Some(PreparedStorage {
            registration,
            storage,
            actual: Program::new(vec![event]),
        }))
    }
    fn allowance(storage: &PreparedNative, allowance: &mut Allowance) {
        allowance.units = storage.units;
        allowance.depth = allowance.depth.max(u64::from(storage.depth));
    }
    fn facts(storage: &PreparedNative, facts: &mut LeafExecution) -> Result<(), HeapFault> {
        let facts = facts
            .0
            .get_mut(&storage.at)
            .ok_or_else(|| malformed("native value lost its release event"))?;
        facts.values.insert(
            Measure::Release(storage.release.clone()),
            i128::from(storage.released),
        );
        facts.values.insert(
            Measure::NativeScratch {
                at: storage.at.site.clone(),
                function: storage.function,
            },
            i128::from(
                storage
                    .contract
                    .peak_scratch_bytes()
                    .ok_or_else(|| malformed("native scratch schedule changed"))?,
            ),
        );
        facts.call = Some((
            storage.at.site.clone(),
            room::CallSource::Native(storage.function),
            room::CheckedRoom::native(storage.function, &storage.contract)
                .ok_or_else(|| malformed("native result contract changed"))?,
        ));
        Ok(())
    }
}

struct DetachedNative<'a> {
    registration: Registration<'a>,
    storage: PreparedNative,
    page: doc::heap::PageValue,
    entities: Vec<VirtualEntity>,
}

/// Core authenticates source exit/entry/charges before accepting this boundary.
/// No event journal is evaluated or published by this path.
#[allow(dead_code, reason = "core owns authenticated region entrypoint wiring")]
pub(in crate::machine) struct NativeBoundary<'a> {
    pub(in crate::machine) transaction: BoundaryTransaction<'a>,
    pub(in crate::machine) storage: PreparedNative,
}

impl NativeStorage<'_> {
    #[allow(dead_code, reason = "core owns authenticated region entrypoint wiring")]
    pub(in crate::machine) fn prepare_boundary<'a>(
        self,
        arena: &'a mut Arena,
        inputs: &[Owner],
    ) -> Result<Option<NativeBoundary<'a>>, HeapFault> {
        let ledger = std::rc::Rc::clone(&self.ledger);
        let heap = self.heap;
        let mut handles: Vec<_> = self.args.iter().filter_map(Value::handle).collect();
        handles.extend(self.known.and_then(|(value, _)| value.handle()));
        let Some(dependencies) = Dependencies::capture(&ledger.borrow(), arena, &handles) else {
            return Ok(None);
        };
        let Some(DetachedNative {
            registration,
            storage,
            page,
            entities,
        }) = self.prepare_storage(arena, inputs)?
        else {
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
        Ok(Some(NativeBoundary {
            transaction,
            storage,
        }))
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
            || self.args.len() > self.function.arity
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
        let (value, work, depth, new) = if let Some((value, depth)) = self.known {
            if !crate::compile::scalar(&value)
                || !crate::compile::import_result_fits(
                    &self.function.header.signature.result,
                    value,
                )
            {
                return Ok(None);
            }
            (value, 0, depth, vec![])
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
        let units = if self.charging {
            let Some(units) = crate::machine::islands::cx::measured_prepared_formula(
                &installation,
                &self.function.charge,
                &self.args,
                &value,
            ) else {
                return Ok(None);
            };
            units
        } else {
            0
        };
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
        // either the old source consumer or the region consumer can publish.
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
                measurements,
                units,
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
    #[allow(dead_code, reason = "core owns region charge compiler wiring")]
    pub(in crate::machine) fn formula_values(&self) -> room::charges::FormulaValues<'_> {
        room::charges::FormulaValues {
            arguments: &self.arguments,
            result: self.value,
        }
    }
    #[allow(
        dead_code,
        reason = "core must extend FormulaValues for detached heap results"
    )]
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
    pub(in crate::machine) fn units(&self) -> u64 {
        self.units
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
