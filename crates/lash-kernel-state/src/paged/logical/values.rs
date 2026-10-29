//! Holdings from semantic values, with identity multiplicity and no body walk.
use super::*;
use crate::arena::{Arena, each_member};
use crate::paged::directory::tuple_tree;
use lash_kernel_doc::Value;
use lash_kernel_doc::arena::{ArenaHandle, Body};
use lash_kernel_doc::heap::PageValue;
use lash_kernel_doc::heap::Payload;

fn arena_fault(error: lash_kernel_doc::NativeError) -> HeapFault {
    invalid(&error.to_string())
}

impl Holding {
    /// A cold value already carries its complete occurrence price. Only
    /// immutable edges are read; scalar payload pages and entity bodies stay cold.
    pub fn from_page(access: &Access<'_>, value: &PageValue) -> Result<Self, HeapFault> {
        let mut holding = Self::scalar(Cost::local(value));
        stream_value_edges(access, value, &mut |target| {
            holding.add_edge(target, 1, access)
        })?;
        Ok(holding)
    }

    /// Source identity registration may precede projection. It does not project
    /// payloads or materialize a lazy mutable body. The caller separately admits
    /// the membership/body events before publishing the semantic owner.
    pub fn from_native(
        arena: &mut Arena,
        access: &Access<'_>,
        value: &Value,
    ) -> Result<Self, HeapFault> {
        let cache = match value {
            Value::Tuple(handle) | Value::Error(handle) => Some(*handle),
            _ => None,
        };
        if let Some(handle) = cache
            && let Some(holding) = arena
                .facts(handle)
                .map_err(arena_fault)?
                .logical_holding
                .get()
        {
            return Ok(holding.clone());
        }
        let mut holding = Self::scalar(arena.logical_local(value)?);
        if !matches!(
            value,
            Value::List(_)
                | Value::Map(_)
                | Value::Set(_)
                | Value::Record(_)
                | Value::Closure(_)
                | Value::ObjectRef(_)
                | Value::Task(_)
                | Value::TaskRef(_)
                | Value::Tuple(_)
                | Value::Error(_)
        ) {
            return Ok(holding);
        }
        let mut pending = vec![*value];
        while let Some(value) = pending.pop() {
            match value {
                Value::List(handle)
                | Value::Map(handle)
                | Value::Set(handle)
                | Value::Record(handle)
                | Value::Closure(handle)
                | Value::ObjectRef(handle) => {
                    let entity = arena.identify(handle).map_err(arena_fault)?;
                    holding.add_edge(Identity::Object(entity), 1, access)?;
                }
                Value::Task(task) | Value::TaskRef(task) => {
                    holding.add_edge(Identity::Task(task), 1, access)?;
                }
                Value::Tuple(handle) | Value::Error(handle) => {
                    if !arena.summary_of(&value).has_edges {
                        continue;
                    }
                    let facts = arena.facts(handle).map_err(arena_fault)?;
                    if let Some(cached) = facts.logical_holding.get() {
                        cached.each_edge(access, &mut |edge| {
                            holding.add_edge(edge.target, edge.count, access)
                        })?;
                    } else if let Some(page) = facts.page.get() {
                        stream_value_edges(access, page, &mut |target| {
                            holding.add_edge(target, 1, access)
                        })?;
                    } else {
                        each_member(arena.body(handle).map_err(arena_fault)?, &mut |value| {
                            pending.push(*value)
                        });
                    }
                }
                _ => {}
            }
        }
        if let Some(handle) = cache
            && !arena.registration_active()
        {
            let _ = arena
                .facts(handle)
                .map_err(arena_fault)?
                .logical_holding
                .set(holding.clone());
        }
        Ok(holding)
    }

    /// A closure frame borrows a variable cell, without another occurrence.
    /// Arbitrary mutable aliases cannot claim this ownership category.
    pub fn borrowed_cell(
        arena: &mut Arena,
        access: &Access<'_>,
        handle: ArenaHandle,
    ) -> Result<Self, HeapFault> {
        if !matches!(arena.body(handle).map_err(arena_fault)?, Body::Cell(_)) {
            return Err(invalid("a captured owner must borrow a variable cell"));
        }
        let entity = arena.identify(handle).map_err(arena_fault)?;
        Self::with_edges(0, [Identity::Object(entity)], access)
    }

    /// Direct members belong to this body once. Mutable children contribute
    /// identity edges, never recursively duplicated body charges.
    pub fn native_body(
        arena: &mut Arena,
        access: &Access<'_>,
        handle: ArenaHandle,
    ) -> Result<Self, HeapFault> {
        let entity = arena.identify(handle).map_err(arena_fault)?;
        if !arena.holds_resident(entity) {
            return Err(invalid("logical body is not completely resident"));
        }
        let mut holding = Self::scalar(arena.logical_body_bytes(handle)?);
        let mut members = Vec::new();
        each_member(arena.body(handle).map_err(arena_fault)?, &mut |value| {
            members.push(*value)
        });
        if let Body::Closure { captures, .. } = arena.body(handle).map_err(arena_fault)? {
            members.extend(captures.iter().map(|cell| Value::ObjectRef(*cell)));
        }
        for member in members {
            Self::from_native(arena, access, &member)?.each_edge(access, &mut |edge| {
                holding.add_edge(edge.target, edge.count, access)
            })?;
        }
        Ok(holding)
    }
}

/// The shared immutable edge reader. Physical census and logical holding
/// acquisition use the same frozen-family interpretation and stop on faults.
pub(crate) fn stream_value_edges(
    access: &Access<'_>,
    value: &PageValue,
    visit: &mut dyn FnMut(Identity) -> Result<(), HeapFault>,
) -> Result<(), HeapFault> {
    match value {
        PageValue::List(id)
        | PageValue::Map(id)
        | PageValue::Set(id)
        | PageValue::Record(id)
        | PageValue::Closure(id) => visit(Identity::Object(*id))?,
        PageValue::Task(task) => visit(Identity::Task(*task))?,
        PageValue::Ref(target) => visit(*target)?,
        PageValue::Tuple(tuple) if tuple.summary.has_edges => match &tuple.body {
            Payload::Resident(values) => {
                for value in values.iter() {
                    stream_value_edges(access, value, visit)?;
                }
            }
            Payload::Paged(page) => {
                let mut error = None;
                tuple_tree(tuple, **page).visit(
                    access,
                    &mut |_, value| match stream_value_edges(access, value, visit) {
                        Ok(()) => true,
                        Err(problem) => {
                            error = Some(problem);
                            false
                        }
                    },
                )?;
                if let Some(error) = error {
                    return Err(error);
                }
            }
        },
        PageValue::Error(error) if error.has_edges => {
            let data = crate::paged::stored::reads::error_data(access, error)?;
            stream_value_edges(access, &data, visit)?;
        }
        _ => {}
    }
    Ok(())
}
