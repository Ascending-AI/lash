//! Member journals read the canonical original entry. Callers never provide
//! its old price, spelling, edges or a final numeric body delta.
use super::*;
use crate::arena::{Arena, Registration};
use lash_kernel_doc::{
    NativeError, Value,
    arena::{ArenaHandle, Body},
};

#[derive(Clone, Copy, Debug)]
pub enum Member {
    List(u64),
    Table(Value),
    Variable,
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
        let arena = registration.arena();
        let entity = arena.identify(handle)?;
        if self.entity_holding(entity, access)?.is_none() {
            return Err(invalid("member owner is absent").into());
        }
        let mut handles = vec![handle];
        if let Some(handle) = next.and_then(|value| value.handle()) {
            handles.push(handle);
        }
        if let Member::Table(key) = member
            && let Some(handle) = key.handle()
        {
            handles.push(handle);
        }
        for (_, value) in outputs {
            match value {
                RootValue::Value(value) => handles.extend(value.handle()),
                RootValue::Capture(handle) => handles.push(*handle),
                RootValue::Existing(_)
                | RootValue::Frame
                | RootValue::Page(_)
                | RootValue::Frozen(_) => {}
            }
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
        let roots = self.ownership_changes(arena, access, inputs, outputs, &mut pending)?;
        let mut changes = self.allocate_membership(arena, access, pending)?;
        changes.push(Change::Members {
            entity,
            remove,
            add: Box::new(add),
        });
        changes.push(Change::Roots(roots));
        // Bind every body actually projected by this journal, including fresh
        // descendants of an immutable result. The registration borrow prevents
        // foreign mutation, but its owner can still edit staging before publish.
        for change in &changes {
            if let Change::Mutate { entity, .. } = change {
                handles.push(
                    arena
                        .logical_handle(*entity)
                        .ok_or_else(|| invalid("projected body disappeared"))?,
                );
            }
        }
        let dependencies = Dependencies::capture(self, arena, &handles)
            .ok_or_else(|| invalid("member facts are unavailable"))?;
        Ok(PreparedMember {
            prepared: self.prepare_event(changes, access)?,
            dependencies,
        })
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
    let mut holding = Holding::from_native(arena, access, &key)?;
    if record {
        holding.bytes = holding
            .bytes
            .checked_sub(Cost::REFERENCE)
            .ok_or_else(|| invalid("field price underflow"))?;
    }
    let value = Holding::from_native(arena, access, &value)?;
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
