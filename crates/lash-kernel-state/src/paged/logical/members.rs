//! Member boundaries read the canonical original entry. Callers never provide
//! its old price, spelling, edges or a final numeric body delta.
use super::*;
use crate::arena::{Arena, Registration};
use lash_kernel_doc::{
    NativeError, Value,
    arena::{ArenaHandle, Body},
};

#[derive(Clone, Debug)]
pub enum Member {
    List(u64),
    Table(Value),
    /// A field name from the admitted source instruction, without an operand.
    Field(std::sync::Arc<str>),
    Variable,
}
impl Member {
    pub fn same(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::List(a), Self::List(b)) => a == b,
            (Self::Table(a), Self::Table(b)) => a.same(b),
            (Self::Field(a), Self::Field(b)) => a == b,
            (Self::Variable, Self::Variable) => true,
            _ => false,
        }
    }
}
#[derive(Debug, thiserror::Error)]
pub enum MemberFault {
    #[error(transparent)]
    Heap(#[from] HeapFault),
    #[error(transparent)]
    Native(#[from] NativeError),
}
pub(super) struct PreparedMember {
    pub(super) prepared: Prepared,
    pub(super) dependencies: Dependencies,
}
impl Ledger {
    /// A member write and its evaluated operands share one source frontier.
    /// Consuming an RHS transfers its occurrence into the member. A result or
    /// binding alias is separately named, with its own canonical occurrence.
    #[expect(clippy::too_many_arguments, reason = "one atomic source member event")]
    pub(super) fn prepare_member_ownership(
        &self,
        registration: &mut Registration<'_>,
        access: &Access<'_>,
        handle: ArenaHandle,
        member: Member,
        next: Option<Value>,
        inputs: &[Owner],
        outputs: &[(Owner, RootValue<'_>)],
    ) -> Result<PreparedMember, MemberFault> {
        self.prepare_members_ownership(
            registration,
            access,
            &[MemberEdit {
                handle,
                member,
                next,
            }],
            inputs,
            outputs,
            None,
        )
    }

    /// Stage final member contents and surviving roots directly, without an
    /// interior event tape. Each target occurs once: private kernels fold their
    /// repeated writes in detached storage before submitting the final edit.
    /// This is storage/graph preparation, not source or observer authorization.
    /// Core must authenticate the entry dependencies, private observers, source
    /// exit and interior peak before admitting and publishing this boundary.
    pub fn prepare_source_members_boundary<'a>(
        &self,
        mut registration: Registration<'a>,
        heap: &super::super::heap_impl::PagedHeap,
        edits: &[MemberEdit],
        inputs: &[Owner],
        outputs: &[(Owner, RootValue<'_>)],
    ) -> Result<BoundaryTransaction<'a>, MemberFault> {
        let PreparedMember {
            prepared,
            dependencies,
        } = self.prepare_members_ownership(
            &mut registration,
            heap.pages(),
            edits,
            inputs,
            outputs,
            Some(heap),
        )?;
        Ok(BoundaryTransaction::from_program(
            registration,
            dependencies,
            prepared,
            edits.to_vec(),
            publication::RootProof::new(inputs, outputs),
        ))
    }

    fn prepare_members_ownership(
        &self,
        registration: &mut Registration<'_>,
        access: &Access<'_>,
        edits: &[MemberEdit],
        inputs: &[Owner],
        outputs: &[(Owner, RootValue<'_>)],
        heap: Option<&super::super::heap_impl::PagedHeap>,
    ) -> Result<PreparedMember, MemberFault> {
        let arena = registration.arena();
        let mut targets = std::collections::BTreeSet::new();
        let mut members = Vec::with_capacity(edits.len());
        let mut pending = Vec::new();
        let mut handles = Vec::new();
        for edit in edits {
            // Independent storage reservations may be installed together.
            // Two snapshots of one body would overwrite each other. A private
            // body kernel must submit its single final storage reservation.
            if !targets.insert(arena.ledger_key(edit.handle)?) {
                return Err(invalid("member boundary needs one final edit per body").into());
            }
            let (change, edges, versions) =
                self.member_change(arena, access, edit.handle, edit.member.clone(), edit.next)?;
            members.push(change);
            pending.extend(edges);
            handles.extend(versions);
        }
        for (_, value) in outputs {
            match value {
                RootValue::Value(value) => handles.extend(value.handle()),
                RootValue::Capture(handle) => handles.push(*handle),
                _ => {}
            }
        }
        let roots = self.ownership_changes(arena, access, inputs, outputs, &mut pending)?;
        let mut changes = self.allocate_membership_from(arena, access, pending, heap)?;
        changes.extend(members);
        changes.push(Change::Roots(roots));
        let prepared = self.prepare_event(changes, access)?;
        // Bind every resident body actually projected, including fresh
        // descendants. Cold bodies come from immutable canonical heap pages.
        for entity in &prepared.projected {
            if let Some(handle) = arena.logical_handle(*entity) {
                handles.push(handle);
            }
        }
        let dependencies = Dependencies::capture(self, arena, &handles)
            .ok_or_else(|| invalid("member facts are unavailable"))?;
        Ok(PreparedMember {
            prepared,
            dependencies,
        })
    }
    pub(super) fn member_change(
        &self,
        arena: &mut Arena,
        access: &Access<'_>,
        handle: ArenaHandle,
        member: Member,
        next: Option<Value>,
    ) -> Result<(Change, Vec<EntityId>, Vec<ArenaHandle>), MemberFault> {
        let entity = arena.ledger_key(handle)?;
        if self.entity_holding(entity, access)?.is_none() {
            return Err(invalid("member owner is absent").into());
        }
        let mut handles = vec![handle];
        if let Some(handle) = next.and_then(|value| value.handle()) {
            handles.push(handle);
        }
        if let Member::Table(key) = &member
            && let Some(handle) = key.handle()
        {
            handles.push(handle);
        }
        let mut remove = Holding::scalar(0);
        let mut add = Holding::scalar(0);
        match member {
            Member::List(index) => {
                let len = match arena.body(handle)? {
                    Body::List(sequence) => sequence.len,
                    _ => return Err(NativeError::Decline.into()),
                };
                let old = arena.item(handle, index)?;
                if index > len || (old.is_none() && next.is_none()) {
                    return Err(invalid("member position is absent").into());
                }
                if let Some(value) = old {
                    remove = Holding::from_native(arena, access, &value)?;
                }
                if let Some(value) = next {
                    add = Holding::from_native(arena, access, &value)?;
                }
            }
            Member::Variable => {
                let old = arena.cell(handle)?;
                let next = next.ok_or_else(|| invalid("a variable cannot remove its member"))?;
                remove = Holding::from_native(arena, access, &old)?;
                add = Holding::from_native(arena, access, &next)?;
            }
            Member::Field(name) => {
                if !matches!(arena.body(handle)?, Body::Record(_)) {
                    return Err(NativeError::Decline.into());
                }
                let old = arena.logical_entry_key(handle, &crate::arena::Key::of_text(&name))?;
                match (old, next) {
                    (Some((key, value)), None) => remove = entry(arena, access, key, value, true)?,
                    (None, Some(value)) => {
                        let key = lash_kernel_doc::heap::PageValue::Text(
                            lash_kernel_doc::heap::TextRef {
                                normal: Default::default(),
                                summary: lash_kernel_doc::heap_values::text_summary(&name),
                                body: lash_kernel_doc::heap::TextPayload::Resident(name),
                            },
                        );
                        add = entry_holdings(
                            Holding::from_page(access, &key)?,
                            Holding::from_native(arena, access, &value)?,
                            true,
                            access,
                        )?;
                    }
                    (Some((_, old)), Some(value)) => {
                        remove = Holding::from_native(arena, access, &old)?;
                        add = Holding::from_native(arena, access, &value)?;
                    }
                    (None, None) => {}
                }
            }
            Member::Table(key) => {
                let record = match arena.body(handle)? {
                    Body::Record(_) => true,
                    Body::Map(_) | Body::Set(_) => false,
                    _ => return Err(NativeError::Decline.into()),
                };
                let old = arena.logical_entry(handle, &key)?;
                match (old, next) {
                    (Some((old_key, value)), None) => {
                        remove = entry(arena, access, old_key, value, record)?;
                    }
                    (None, Some(value)) => {
                        add = entry(arena, access, key, value, record)?;
                    }
                    (Some((_, old)), Some(value)) => {
                        remove = Holding::from_native(arena, access, &old)?;
                        add = Holding::from_native(arena, access, &value)?;
                    }
                    (None, None) => {}
                }
            }
        }
        let mut pending = Vec::new();
        add.each_edge(access, &mut |edge| {
            if let Identity::Object(entity) = edge.target {
                pending.push(entity);
            }
            Ok(())
        })?;
        Ok((
            Change::Members {
                entity,
                remove,
                add: Box::new(add),
            },
            pending,
            handles,
        ))
    }
}
fn entry(
    arena: &mut Arena,
    access: &Access<'_>,
    key: Value,
    value: Value,
    record: bool,
) -> Result<Holding, MemberFault> {
    if record && !matches!(key, Value::Text(_) | Value::LongText(_)) {
        return Err(NativeError::Decline.into());
    }
    entry_holdings(
        Holding::from_native(arena, access, &key)?,
        Holding::from_native(arena, access, &value)?,
        record,
        access,
    )
}
fn entry_holdings(
    mut holding: Holding,
    value: Holding,
    record: bool,
    access: &Access<'_>,
) -> Result<Holding, MemberFault> {
    if record {
        holding.bytes = holding
            .bytes
            .checked_sub(Cost::REFERENCE)
            .ok_or_else(|| invalid("field price underflow"))?;
    }
    holding.bytes = holding
        .bytes
        .checked_add(value.bytes)
        .ok_or_else(|| invalid("entry price overflow"))?;
    value.each_edge(access, &mut |edge| {
        holding.add_edge(edge.target, edge.count, access)
    })?;
    Ok(holding)
}
pub(super) fn occurrence(
    arena: &mut Arena,
    access: &Access<'_>,
    value: Option<crate::arena::MemberEntry>,
) -> Result<Holding, HeapFault> {
    use crate::arena::MemberEntry;
    match value {
        None => Ok(Holding::scalar(0)),
        Some(MemberEntry::Value(value)) => Holding::from_native(arena, access, &value),
        Some(MemberEntry::Pair { key, value, record }) => entry(arena, access, key, value, record)
            .map_err(|error| match error {
                MemberFault::Heap(error) => error,
                MemberFault::Native(error) => invalid(&error.to_string()),
            }),
    }
}
