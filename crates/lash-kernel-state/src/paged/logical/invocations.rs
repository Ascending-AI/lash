//! Logical frame activations survive physical stack and cache reuse.
use super::*;
use crate::arena::Arena;

#[derive(Clone, Debug)]
pub struct Invocation {
    task: TaskId,
    id: u64,
    ledger: Rc<Lineage>,
}

/// Prospective identities for a compiled prefix's synchronous activations.
/// Reserving is pure. Only a matching prepared exit consumes the completed
/// range; abandoned work and conservative admission misses consume no IDs.
/// This process-local authority is affine and is never part of a sealed root.
pub struct InvocationRange {
    stamp: Stamp,
    task: TaskId,
    first: u64,
    end: u64,
    region: Option<Rc<()>>,
}
impl InvocationRange {
    /// Authenticate a scope's task, ledger and unconsumed reservation once.
    pub fn is_current(&self, ledger: &Ledger, caller: &Invocation) -> bool {
        ledger.is_current(&self.stamp)
            && Rc::ptr_eq(&self.stamp.identity, &caller.ledger)
            && self.task == caller.task
            && self.first == ledger.state.next_invocation
    }

    pub fn len(&self) -> u64 {
        self.end - self.first
    }
    pub fn is_empty(&self) -> bool {
        self.first == self.end
    }
    pub fn invocation(&self, offset: u64) -> Option<Invocation> {
        let id = self.first.checked_add(offset)?;
        (id < self.end).then(|| Invocation {
            task: self.task,
            id,
            ledger: Rc::clone(&self.stamp.identity),
        })
    }
}

impl Prepared {
    /// Add completed, physically elided calls to this same atomic exit.
    /// The compiler owns their source/peak proof; the ledger owns identity
    /// allocation and rejects stale, foreign or retained activations. A stop
    /// inside a call uses ordinary frame entry for that uncompleted suffix.
    pub fn complete_invocations(
        mut self,
        range: InvocationRange,
        completed: u64,
        access: &Access<'_>,
    ) -> Result<Self, HeapFault> {
        if !Rc::ptr_eq(&self.identity, &range.stamp.identity)
            || self.revision != range.stamp.revision
            || self.next.next_invocation != range.first
            || completed > range.len()
        {
            return Err(invalid("invocation range does not belong to this exit"));
        }
        let next = range
            .first
            .checked_add(completed)
            .ok_or_else(|| invalid("logical invocation overflow"))?;
        let start = codec::invocation_key(range.task, range.first);
        let end = codec::invocation_key(range.task, range.end);
        // One ordered lookup checks the entire reserved activation range. No
        // frame insertion/deletion or per-call owner walk is needed.
        if let Some((key, _)) = self.next.execution.after(Some(start.as_slice()), access)?
            && key.as_slice() < end.as_slice()
        {
            return Err(invalid("completed invocation retains a source owner"));
        }
        self.next.next_invocation = next;
        Ok(self)
    }
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
    pub fn reserve_invocations(&self, task: TaskId, count: u64) -> Option<InvocationRange> {
        self.require_unleased().ok()?;
        Some(InvocationRange {
            stamp: self.stamp(),
            task,
            first: self.state.next_invocation,
            end: self.state.next_invocation.checked_add(count)?,
            region: None,
        })
    }
    /// A pure prospective activation. Accepted entry or a compiled call exit
    /// advances its cursor; abandoned prefixes consume no activation ID.
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
                RootValue::Prepared(_) => {
                    return Err(invalid(
                        "a detached value requires its construction boundary",
                    ));
                }
                RootValue::Page(value) => RootValue::Page(value),
                RootValue::Frozen(_) => {
                    return Err(invalid("a frame cannot borrow an incoming frozen value"));
                }
                RootValue::Existing(from) => RootValue::Existing(*from),
                RootValue::Capture(handle) => RootValue::Capture(*handle),
                RootValue::Frame => return Err(invalid("binding cannot own a frame header")),
                RootValue::Departure => return Err(invalid("binding cannot own a departure")),
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
        self.invocation_range(
            invocation,
            codec::invocation_key(invocation.task, invocation.id),
            access,
        )
    }
    pub fn frame_owners(
        &self,
        task: TaskId,
        invocation: u64,
        access: &Access<'_>,
    ) -> Result<Vec<Owner>, HeapFault> {
        self.owners_under(codec::invocation_key(task, invocation), access)
    }
    pub fn invocation_operands(
        &self,
        invocation: &Invocation,
        access: &Access<'_>,
    ) -> Result<Vec<Owner>, HeapFault> {
        self.invocation_range(
            invocation,
            codec::operand_prefix(invocation.task, invocation.id),
            access,
        )
    }
    fn invocation_range(
        &self,
        invocation: &Invocation,
        prefix: codec::Key,
        access: &Access<'_>,
    ) -> Result<Vec<Owner>, HeapFault> {
        if !Rc::ptr_eq(&self.identity, &invocation.ledger) {
            return Err(invalid("logical invocation belongs to another ledger"));
        }
        self.invocation(invocation.task, invocation.id, access)?;
        self.owners_under(prefix, access)
    }
    fn owners_under(
        &self,
        prefix: codec::Key,
        access: &Access<'_>,
    ) -> Result<Vec<Owner>, HeapFault> {
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

impl InvocationRange {
    pub(super) fn matches_region(
        &self,
        entry: &Stamp,
        token: &Rc<()>,
        reserved: Option<(TaskId, u64)>,
    ) -> bool {
        Rc::ptr_eq(&self.stamp.identity, &entry.identity)
            && self.stamp.revision == entry.revision
            && self
                .region
                .as_ref()
                .is_some_and(|actual| Rc::ptr_eq(actual, token))
            && reserved.is_some_and(|(task, bound)| task == self.task && self.len() <= bound)
    }
}
impl Ledger {
    pub(super) fn reserve_region_invocations(
        &self,
        caller: &Invocation,
        count: u64,
        token: Rc<()>,
        access: &Access<'_>,
    ) -> Result<InvocationRange, HeapFault> {
        if !Rc::ptr_eq(&self.identity, &caller.ledger)
            || self.invocation(caller.task, caller.id, access)? != *caller
        {
            return Err(invalid(
                "region caller belongs to another activation or ledger",
            ));
        }
        Ok(InvocationRange {
            stamp: self.stamp(),
            task: caller.task,
            first: self.state.next_invocation,
            end: self
                .state
                .next_invocation
                .checked_add(count)
                .ok_or_else(|| invalid("logical invocation overflow"))?,
            region: Some(token),
        })
    }
}

impl InvocationRange {
    pub(super) fn install_region(
        self,
        mut prepared: Prepared,
        outputs: &[(Owner, PublicationValue)],
        access: &Access<'_>,
    ) -> Result<Prepared, HeapFault> {
        if !Rc::ptr_eq(&prepared.identity, &self.stamp.identity)
            || prepared.revision != self.stamp.revision
            || prepared.next.next_invocation != self.first
        {
            return Err(invalid("region activation range changed"));
        }
        for (owner, _) in outputs {
            if let Owner::Frame { task, invocation }
            | Owner::Value {
                task, invocation, ..
            } = *owner
                && invocation >= self.first
            {
                if task != self.task || invocation >= self.end {
                    return Err(invalid("region retained an unreserved activation"));
                }
                let frame = Owner::Frame { task, invocation };
                if prepared
                    .next
                    .execution
                    .get(&owner_key(frame), access)?
                    .is_none()
                {
                    return Err(invalid("region retained a binding without its frame"));
                }
            }
        }
        prepared.next.next_invocation = self.end;
        Ok(prepared)
    }
}

impl InvocationRange {
    /// Consume only the reached prefix of a prospective reservation. Unused
    /// identities remain unallocated; lineage and lease qualification survive.
    pub fn prefix(mut self, count: u64) -> Option<Self> {
        if count > self.len() {
            return None;
        }
        self.end = self.first.checked_add(count)?;
        Some(self)
    }
}
