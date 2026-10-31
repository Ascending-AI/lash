//! Every returned native graph is prepared before the source journal accepts.
use super::*;
use crate::compile::LibRun;
use doc::Value;
use lash_kernel_state::arena::PreparedBatch;
use lash_kernel_state::kernel_internals::logical::Ledger;
use lash_kernel_state::kernel_internals::logical::{
    Owner, PublicationValue as PV, VirtualBody, VirtualEntity,
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
        let (value, work, depth, new) = if let Some((value, depth)) = self.known {
            (value, 0, depth, vec![])
        } else {
            let LibRun::Native { native, .. } = &self.function.run else {
                return Ok(None);
            };
            if self.function.limit.is_some() {
                return Ok(None);
            }
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
        let first = arena.next_ordinal();
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
                family: doc::heap::FamilyId::PRIMARY,
                ordinal: first
                    .checked_add(index as u64)
                    .ok_or_else(|| malformed("native identity overflow"))?,
            };
            installation
                .bind_entity(*handle, entity)
                .map_err(native_fault)?;
            identities.push(entity);
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
            &event.inputs,
            &entities,
            &page,
        )?;
        let mut event = event.clone();
        event.native = Some(self.contract);
        event.outputs.push((self.output, PV::Page(page)));
        event.entities = entities;
        Ok(Some(PreparedStorage {
            registration,
            storage: PreparedNative {
                value,
                batch,
                allocated,
                graph_entities: event.entities.len(),
                units,
                work,
                known: self.known.is_some(),
                depth,
                at: self.at,
                release: self.release,
                released,
            },
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
        Ok(())
    }
}

impl PreparedNative {
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
