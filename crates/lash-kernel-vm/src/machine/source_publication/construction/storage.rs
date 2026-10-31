//! A constructor's normalized detached body is its canonical publication recipe.
use crate::compile::room::{self, Measure, Point};
use crate::machine::source_publication::{
    LeafExecution, PreparedStorage, StoragePreparation, malformed, native_fault,
};
use lash_kernel_doc::{self as doc, Value, heap::HeapFault};
use lash_kernel_state::{
    arena::{Arena, PreparedBatch},
    kernel_internals::logical::{
        Holding, Ledger, Owner, Program, PublicationValue as PV, RootValue, VirtualBody,
        VirtualEntity,
    },
};
pub(in crate::machine) struct Construction<'a> {
    pub(in crate::machine) object: doc::NewObject,
    pub(in crate::machine) output: Owner,
    pub(in crate::machine) at: Point,
    pub(in crate::machine) release: Box<[room::Owner]>,
    pub(in crate::machine) ledger: std::rc::Rc<std::cell::RefCell<Ledger>>,
    pub(in crate::machine) depth_limit: usize,
    pub(in crate::machine) heap: &'a lash_kernel_state::kernel_internals::PagedHeap,
}
pub(in crate::machine) struct Prepared {
    pub(in crate::machine) batch: PreparedBatch,
    pub(in crate::machine) value: Value,
    at: Point,
    release: Box<[room::Owner]>,
    obligation: (Measure, u64),
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
            doc::NewObject::List(_) | doc::NewObject::Tuple(_) => true,
            _ => false,
        };
        if !valid {
            return Ok(None);
        }
        let tuple = if let doc::NewObject::Tuple(values) = &self.object {
            let values = values
                .iter()
                .map(|value| crate::machine::project::project(arena, self.heap.pages(), value))
                .collect::<Result<Vec<_>, _>>()?;
            let tuple = lash_kernel_state::kernel_internals::stored_pages::tuple(
                self.heap.pages(),
                values,
            )?;
            if tuple.summary.depth as usize > self.depth_limit {
                return Ok(None);
            }
            Some(doc::heap::PageValue::Tuple(tuple))
        } else {
            None
        };
        let entity = tuple.is_none().then(|| doc::heap::EntityId {
            family: doc::heap::FamilyId::PRIMARY,
            ordinal: arena.next_ordinal(),
        });
        let (value, mut installation) = if matches!(&self.object, doc::NewObject::Tuple(values) if values.is_empty())
        {
            arena.prepare_empty_tuple().map_err(native_fault)?
        } else {
            let mut nursery = doc::Nursery::unaccounted(arena.objects());
            let value = nursery.allocate(self.object);
            let installation = arena
                .prepare_install(nursery.into_new())
                .map_err(native_fault)?;
            (value, installation)
        };
        let handle = value
            .handle()
            .ok_or_else(|| malformed("construction has no prepared handle"))?;
        let body = if let Some(entity) = entity {
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
            Some((entity, body))
        } else {
            None
        };
        let (mut registration, batch) = installation.into_parts();
        let mut event = event.clone();
        let (page, obligation, released) = if let Some((entity, body)) = body {
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
            event.entities.push(VirtualEntity { entity, body });
            (
                page,
                (Measure::Members(self.at.site.clone()), members),
                released,
            )
        } else {
            let page = tuple.ok_or_else(|| malformed("construction lost its immutable recipe"))?;
            let bytes = Holding::from_page(self.heap.pages(), &page)?.bytes();
            let released = self.ledger.borrow().release_cost_preserving(
                registration.arena(),
                self.heap,
                &event.inputs,
                &[(self.output, RootValue::Page(&page))],
            )?;
            (
                page,
                (
                    Measure::Local(room::Owner::Operand(self.at.site.clone())),
                    bytes,
                ),
                released,
            )
        };
        event.outputs.push((self.output, PV::Page(page)));
        Ok(Some(PreparedStorage {
            registration,
            storage: Prepared {
                batch,
                value,
                at: self.at,
                release: self.release,
                obligation,
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
            storage.obligation.0.clone(),
            i128::from(storage.obligation.1),
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
