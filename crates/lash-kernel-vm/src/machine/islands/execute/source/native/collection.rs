//! A native's fresh shallow collection installs with its canonical source graph.
use super::*;
use lash_kernel_state::{
    arena::PreparedBatch,
    kernel_internals::logical::{Ledger, VirtualBody, VirtualEntity},
};
use publication::{PreparedStorage, StoragePreparation, native_fault};

pub(super) fn supports(contract: &doc::NativeResourceContract) -> bool {
    let doc::NativeResourceOrigin::Fresh(output) = contract.output.origin else {
        return false;
    };
    if contract.exit != doc::NativeResourceExit::Returned {
        return false;
    }
    let mut entity = false;
    for event in &contract.events {
        match event {
            doc::NativeResourceEvent::Entity {
                owner,
                kind:
                    doc::NativeResourceEntity::List
                    | doc::NativeResourceEntity::Map
                    | doc::NativeResourceEntity::Set
                    | doc::NativeResourceEntity::Record { .. },
                release: None,
                members,
            } if !entity
                && *owner == output
                && members.iter().all(|member| {
                    !matches!(member.origin, doc::NativeResourceOrigin::Fresh(_))
                }) =>
            {
                entity = true;
            }
            doc::NativeResourceEvent::Scratch {
                release: doc::NativeResourceRelease::Exit,
                ..
            } => {}
            _ => return false,
        }
    }
    entity
}

pub(super) struct CollectionNative<'a> {
    pub(super) function: &'a crate::compile::Lib,
    pub(super) args: Vec<Value>,
    pub(super) output: Owner,
    pub(super) at: Point,
    pub(super) release: Box<[room::Owner]>,
    pub(super) ledger: std::rc::Rc<std::cell::RefCell<Ledger>>,
    pub(super) heap: &'a lash_kernel_state::kernel_internals::PagedHeap,
    pub(super) charging: bool,
    pub(super) limits: lash_kernel_value::data_limits::KernelDataLimits,
    pub(super) room: Option<u64>,
}

pub(super) struct PreparedCollection {
    pub(super) batch: PreparedBatch,
    pub(super) value: Value,
    units: u64,
    released: u64,
    at: Point,
    release: Box<[room::Owner]>,
}

impl StoragePreparation for CollectionNative<'_> {
    type Prepared = PreparedCollection;
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
        let value = match native.call(doc::NativeCall {
            args: doc::Args::new(&self.args, arena.objects(), &semantics),
            new: &mut nursery,
            counter: &mut counter,
        }) {
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
        let (body, len) = match (value, nursery.built()) {
            (Value::List(handle), [(target, doc::NewObject::List(values))])
                if handle == *target =>
            {
                (VirtualBody::NativeList(values.clone()), values.len())
            }
            (Value::Map(handle), [(target, doc::NewObject::Map(values))]) if handle == *target => {
                (VirtualBody::NativeMap(values.clone()), values.len())
            }
            (Value::Set(handle), [(target, doc::NewObject::Set(values))]) if handle == *target => {
                (VirtualBody::NativeSet(values.clone()), values.len())
            }
            (Value::Record(handle), [(target, doc::NewObject::Record(values))])
                if handle == *target =>
            {
                (VirtualBody::NativeRecord(values.clone()), values.len())
            }
            _ => {
                nursery.discard();
                return Ok(None);
            }
        };
        let units = if self.charging {
            let Some(units) = crate::machine::islands::cx::measured_collection_formula(
                arena,
                &self.function.charge,
                &self.args,
                value,
                len as u64,
            ) else {
                nursery.discard();
                return Ok(None);
            };
            units
        } else {
            0
        };
        let entity = doc::heap::EntityId {
            family: doc::heap::FamilyId::PRIMARY,
            ordinal: arena.next_ordinal(),
        };
        let mut installation = arena
            .prepare_install(nursery.into_new())
            .map_err(native_fault)?;
        installation
            .bind_entity(
                value
                    .handle()
                    .ok_or_else(|| malformed("native collection lost its handle"))?,
                entity,
            )
            .map_err(native_fault)?;
        let (mut registration, batch) = installation.into_parts();
        let released = self.ledger.borrow().release_cost_preserving_body(
            registration.arena(),
            self.heap,
            &event.inputs,
            &body,
        )?;
        let mut event = event.clone();
        let page = match value {
            Value::List(_) => doc::heap::PageValue::List(entity),
            Value::Map(_) => doc::heap::PageValue::Map(entity),
            Value::Set(_) => doc::heap::PageValue::Set(entity),
            Value::Record(_) => doc::heap::PageValue::Record(entity),
            _ => return Err(malformed("native collection lost its kind")),
        };
        event.outputs.push((self.output, PV::Page(page)));
        event.entities.push(VirtualEntity { entity, body });
        Ok(Some(PreparedStorage {
            registration,
            storage: PreparedCollection {
                batch,
                value,
                units,
                released,
                at: self.at,
                release: self.release,
            },
            actual: Program::new(vec![event]),
        }))
    }

    fn allowance(storage: &PreparedCollection, allowance: &mut Allowance) {
        allowance.units = storage.units;
    }

    fn facts(storage: &PreparedCollection, facts: &mut LeafExecution) -> Result<(), HeapFault> {
        let facts = facts
            .0
            .get_mut(&storage.at)
            .ok_or_else(|| malformed("native collection lost its release event"))?;
        facts.values.insert(
            Measure::Release(storage.release.clone()),
            i128::from(storage.released),
        );
        Ok(())
    }
}
