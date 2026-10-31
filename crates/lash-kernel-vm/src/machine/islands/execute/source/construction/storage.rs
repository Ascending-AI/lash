//! A constructor's normalized detached body is its canonical publication recipe.
use super::super::publication::{PreparedStorage, StoragePreparation, native_fault};
use super::*;
use lash_kernel_state::{arena::PreparedBatch, kernel_internals::logical::Ledger};

pub(super) struct Construction<'a> {
    pub(super) object: doc::NewObject,
    pub(super) output: Owner,
    pub(super) at: Point,
    pub(super) release: Box<[room::Owner]>,
    pub(super) ledger: std::rc::Rc<std::cell::RefCell<Ledger>>,
    pub(super) heap: &'a lash_kernel_state::kernel_internals::PagedHeap,
}
pub(super) struct Prepared {
    pub(super) batch: PreparedBatch,
    pub(super) value: Value,
    at: Point,
    release: Box<[room::Owner]>,
    members: u64,
    released: u64,
}
impl StoragePreparation for Construction<'_> {
    type Prepared = Prepared;
    const MATERIALIZES: bool = true;
    fn prepare<'a>(
        self,
        arena: &'a mut Arena,
        actual: Program,
    ) -> Result<Option<PreparedStorage<'a, Prepared>>, HeapFault> {
        let [event] = actual.events() else {
            return Ok(None);
        };
        if !event.outputs.is_empty()
            || !event.entities.is_empty()
            || event.frame.is_some()
            || event.member.is_some()
            || !event.scratch.is_empty()
        {
            return Ok(None);
        }
        // A source key error remains at the precise constructor. This uses the
        // same key validator as ordinary construction, before reserving a slot.
        let valid = match &self.object {
            doc::NewObject::Map(entries) | doc::NewObject::Record(entries) => {
                valid_keys(arena, entries.iter().map(|(key, _)| key))?
            }
            doc::NewObject::Set(values) => valid_keys(arena, values.iter())?,
            doc::NewObject::List(_) => true,
            _ => false,
        };
        if !valid {
            return Ok(None);
        }
        let entity = doc::heap::EntityId {
            family: doc::heap::FamilyId::PRIMARY,
            ordinal: arena.next_ordinal(),
        };
        let mut nursery = doc::Nursery::unaccounted(arena.objects());
        let value = nursery.allocate(self.object);
        let handle = value
            .handle()
            .ok_or_else(|| malformed("construction has no fresh handle"))?;
        let mut installation = arena
            .prepare_install(nursery.into_new())
            .map_err(native_fault)?;
        let body = match installation.collection(handle).map_err(native_fault)? {
            doc::NewObject::List(values) => VirtualBody::NativeList(values),
            doc::NewObject::Map(entries) => VirtualBody::NativeMap(entries),
            doc::NewObject::Set(values) => VirtualBody::NativeSet(values),
            doc::NewObject::Record(entries) => VirtualBody::NativeRecord(entries),
            _ => return Err(malformed("construction has another body kind")),
        };
        installation
            .bind_entity(handle, entity)
            .map_err(native_fault)?;
        let (mut registration, batch) = installation.into_parts();
        let members = body.member_bytes(self.heap, registration.arena())?;
        let released = self.ledger.borrow().release_cost_preserving_body(
            registration.arena(),
            self.heap,
            &event.inputs,
            &body,
        )?;
        let page = match value {
            Value::List(_) => doc::heap::PageValue::List(entity),
            Value::Map(_) => doc::heap::PageValue::Map(entity),
            Value::Set(_) => doc::heap::PageValue::Set(entity),
            Value::Record(_) => doc::heap::PageValue::Record(entity),
            _ => return Err(malformed("construction has another body kind")),
        };
        let mut event = event.clone();
        event.outputs.push((self.output, PV::Page(page)));
        event.entities.push(VirtualEntity { entity, body });
        Ok(Some(PreparedStorage {
            registration,
            storage: Prepared {
                batch,
                value,
                at: self.at,
                release: self.release,
                members,
                released,
            },
            actual: Program::new(vec![event]),
        }))
    }
    fn facts(storage: &Prepared, facts: &mut LeafExecution) -> Result<(), HeapFault> {
        let facts = facts
            .0
            .get_mut(&storage.at)
            .ok_or_else(|| malformed("construction lost its source event"))?;
        facts.values.insert(
            Measure::Members(storage.at.site.clone()),
            i128::from(storage.members),
        );
        facts.values.insert(
            Measure::Release(storage.release.clone()),
            i128::from(storage.released),
        );
        Ok(())
    }
}

fn valid_keys<'a>(arena: &Arena, keys: impl Iterator<Item = &'a Value>) -> Result<bool, HeapFault> {
    for key in keys {
        match arena.key(key) {
            Ok(_) => {}
            Err(doc::NativeError::Host(error)) => return Err(error),
            Err(_) => return Ok(false),
        }
    }
    Ok(true)
}
