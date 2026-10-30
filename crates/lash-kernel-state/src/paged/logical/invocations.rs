//! Logical frame activations survive physical stack and cache reuse.
use super::*;
use crate::arena::Arena;

#[derive(Clone, Debug)]
pub struct Invocation {
    task: TaskId,
    id: u64,
    ledger: Rc<()>,
}
impl PartialEq for Invocation {
    fn eq(&self, other: &Self) -> bool {
        self.task == other.task && self.id == other.id && Rc::ptr_eq(&self.ledger, &other.ledger)
    }
}
impl Eq for Invocation {}
impl Invocation {
    pub fn id(&self) -> u64 {
        self.id
    }
    pub fn frame(&self) -> Owner {
        Owner::Frame {
            task: self.task,
            invocation: self.id,
        }
    }
    pub fn owner(&self, slot: Slot) -> Owner {
        Owner::Value {
            task: self.task,
            invocation: self.id,
            slot,
        }
    }
}
impl Ledger {
    /// A pure prospective activation. Its cursor is advanced only by an
    /// accepted Enter event; abandoned prefixes consume no activation ID.
    pub fn prospective_invocation(&self, task: TaskId) -> Invocation {
        Invocation {
            task,
            id: self.state.next_invocation,
            ledger: Rc::clone(&self.identity),
        }
    }

    /// Reserve the next activation in the unpublished journal. A miss consumes
    /// no ID; concurrent preparations cannot both publish against its stamp.
    pub fn prepare_enter(
        &self,
        task: TaskId,
        arena: &mut Arena,
        access: &Access<'_>,
        inputs: &[Owner],
        bindings: &[(Slot, RootValue<'_>)],
    ) -> Result<(Invocation, Prepared), HeapFault> {
        self.prepare_enter_from(task, arena, access, inputs, bindings, None)
    }
    pub fn prepare_source_enter(
        &self,
        task: TaskId,
        arena: &mut Arena,
        heap: &super::super::heap_impl::PagedHeap,
        inputs: &[Owner],
        bindings: &[(Slot, RootValue<'_>)],
    ) -> Result<(Invocation, Prepared), HeapFault> {
        self.prepare_enter_from(task, arena, heap.pages(), inputs, bindings, Some(heap))
    }
    fn prepare_enter_from(
        &self,
        task: TaskId,
        arena: &mut Arena,
        access: &Access<'_>,
        inputs: &[Owner],
        bindings: &[(Slot, RootValue<'_>)],
        heap: Option<&super::super::heap_impl::PagedHeap>,
    ) -> Result<(Invocation, Prepared), HeapFault> {
        let id = self.state.next_invocation;
        let next = id
            .checked_add(1)
            .ok_or_else(|| invalid("logical invocation overflow"))?;
        let invocation = Invocation {
            task,
            id,
            ledger: Rc::clone(&self.identity),
        };
        let mut outputs = Vec::with_capacity(bindings.len() + 1);
        outputs.push((invocation.frame(), RootValue::Frame));
        for (slot, value) in bindings {
            if !matches!(slot, Slot::Binding(_) | Slot::Capture(_)) {
                return Err(invalid("frame entry has another owner kind"));
            }
            let value = match value {
                RootValue::Value(value) => RootValue::Value(value),
                RootValue::Page(value) => RootValue::Page(value),
                RootValue::Frozen(_) => {
                    return Err(invalid("a frame cannot borrow an incoming frozen value"));
                }
                RootValue::Existing(from) => RootValue::Existing(*from),
                RootValue::Capture(handle) => RootValue::Capture(*handle),
                RootValue::Frame => return Err(invalid("binding cannot own a frame header")),
            };
            outputs.push((invocation.owner(*slot), value));
        }
        let mut prepared = match heap {
            Some(heap) => self.prepare_source_ownership(arena, heap, inputs, &outputs)?,
            None => self.prepare_ownership(arena, access, inputs, &outputs)?,
        };
        prepared.next.next_invocation = next;
        Ok((invocation, prepared))
    }
    /// Rebind a saved activation only while its frame header still exists.
    pub fn invocation(
        &self,
        task: TaskId,
        id: u64,
        access: &Access<'_>,
    ) -> Result<Invocation, HeapFault> {
        let invocation = Invocation {
            task,
            id,
            ledger: Rc::clone(&self.identity),
        };
        if id == 0
            || id >= self.state.next_invocation
            || self.holding(invocation.frame(), access)?.is_none()
        {
            return Err(invalid("logical invocation is absent"));
        }
        Ok(invocation)
    }
    /// Only this activation's indexed key range is visited on return. Other
    /// frames and tasks remain cold. Formula mirrors have no roots here.
    pub fn invocation_owners(
        &self,
        invocation: &Invocation,
        access: &Access<'_>,
    ) -> Result<Vec<Owner>, HeapFault> {
        if !Rc::ptr_eq(&self.identity, &invocation.ledger) {
            return Err(invalid("logical invocation belongs to another ledger"));
        }
        self.invocation(invocation.task, invocation.id, access)?;
        let prefix = codec::invocation_key(invocation.task, invocation.id);
        let mut cursor = prefix.as_slice().to_vec();
        let mut owners = Vec::new();
        while let Some((key, record)) = self.state.execution.after(Some(&cursor), access)? {
            if !key.starts_with(&prefix) {
                break;
            }
            if key != owner_key(record.owner).as_slice() {
                return Err(invalid("logical owner key mismatch"));
            }
            owners.push(record.owner);
            cursor = key;
        }
        Ok(owners)
    }
    pub fn prepare_leave(
        &self,
        invocation: &Invocation,
        arena: &mut Arena,
        access: &Access<'_>,
        outputs: &[(Owner, RootValue<'_>)],
    ) -> Result<Prepared, HeapFault> {
        let inputs = self.invocation_owners(invocation, access)?;
        if outputs.iter().any(|(owner, _)| {
            matches!(owner,
            Owner::Frame { task, invocation: id } | Owner::Value { task, invocation: id, .. }
            if *task == invocation.task && *id == invocation.id)
        }) {
            return Err(invalid("leaving frame retains its own source owner"));
        }
        self.prepare_ownership(arena, access, &inputs, outputs)
    }
}
