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
    pub(super) fn heap_body(
        heap: &super::super::heap_impl::PagedHeap,
        entity: EntityId,
    ) -> Result<Self, HeapFault> {
        use super::super::directory::Body as StoredBody;
        let head = heap
            .current_head(entity)?
            .ok_or_else(|| invalid("source identity is absent from the heap"))?;
        let mut holding = Self::scalar(head.cost());
        let access = heap.pages();
        let mut value = |value: &PageValue| {
            stream_value_edges(access, value, &mut |target| {
                holding.add_edge(target, 1, access)
            })
        };
        match &head.body {
            StoredBody::Variable(member) => value(member)?,
            StoredBody::List { rows, .. } => visit_tree(rows, access, &mut value)?,
            StoredBody::Table { rows, .. } => visit_tree(rows, access, &mut |entry| {
                value(&entry.key)?;
                value(&entry.value)
            })?,
            StoredBody::Closure { captures, .. } => {
                visit_tree(captures, access, &mut |(_, entity)| {
                    holding.add_edge(Identity::Object(*entity), 1, access)
                })?
            }
        }
        Ok(holding)
    }
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
                .projected()
                .and_then(|projected| projected.logical_holding.get())
        {
            return Ok(holding.clone());
        }
        let mut holding = Self::scalar(arena.logical_local(value)?);
        match value {
            Value::List(handle)
            | Value::Map(handle)
            | Value::Set(handle)
            | Value::Record(handle)
            | Value::Closure(handle)
            | Value::ObjectRef(handle) => {
                let entity = arena.ledger_key(*handle).map_err(arena_fault)?;
                holding.add_edge(Identity::Object(entity), 1, access)?;
                return Ok(holding);
            }
            Value::Task(task) | Value::TaskRef(task) => {
                holding.add_edge(Identity::Task(*task), 1, access)?;
                return Ok(holding);
            }
            Value::Tuple(_) | Value::Error(_) => (),
            _ => return Ok(holding),
        }
        let mut batch = links::EdgeBatch::default();
        stream_native_edges(arena, access, value, &mut |target, count| {
            batch.push(target, count)
        })?;
        holding.links = batch.finish()?;
        // A provisional edge is rekeyed at projection: only a holding of
        // durable ids stays true for the cache's lifetime.
        if let Some(handle) = cache
            && !arena.registration_active()
            && !holding.has_provisional_edges(access)?
        {
            let _ = arena
                .facts(handle)
                .map_err(arena_fault)?
                .try_projection()?
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
        let entity = arena.ledger_key(handle).map_err(arena_fault)?;
        Self::with_edges(0, [Identity::Object(entity)], access)
    }

    /// Direct members belong to this body once. Mutable children contribute
    /// identity edges, never recursively duplicated body charges.
    pub fn native_body(
        arena: &mut Arena,
        access: &Access<'_>,
        handle: ArenaHandle,
    ) -> Result<Self, HeapFault> {
        let entity = arena.ledger_key(handle).map_err(arena_fault)?;
        if !arena.holds_resident(entity) {
            return Err(invalid("logical body is not completely resident"));
        }
        let mut holding = Self::scalar(arena.logical_body_bytes(handle)?);
        let mut members = Vec::new();
        each_member(arena.body(handle).map_err(arena_fault)?, &mut |value| {
            if arena.summary_of(value).has_edges {
                members.push(*value);
            }
        });
        if let Body::Closure { captures, .. } = arena.body(handle).map_err(arena_fault)? {
            members.extend(captures.iter().map(|cell| Value::ObjectRef(*cell)));
        }
        let mut batch = links::EdgeBatch::default();
        for member in members {
            stream_native_edges(arena, access, &member, &mut |target, count| {
                batch.push(target, count)
            })?;
        }
        holding.links = batch.finish()?;
        Ok(holding)
    }
}

// Price the occurrence separately; edge acquisition never reprices members.
fn stream_native_edges(
    arena: &mut Arena,
    access: &Access<'_>,
    value: &Value,
    visit: &mut dyn FnMut(Identity, u64) -> Result<(), HeapFault>,
) -> Result<(), HeapFault> {
    match value {
        Value::List(handle)
        | Value::Map(handle)
        | Value::Set(handle)
        | Value::Record(handle)
        | Value::Closure(handle)
        | Value::ObjectRef(handle) => {
            let entity = arena.ledger_key(*handle).map_err(arena_fault)?;
            visit(Identity::Object(entity), 1)?;
            return Ok(());
        }
        Value::Task(task) | Value::TaskRef(task) => {
            visit(Identity::Task(*task), 1)?;
            return Ok(());
        }
        Value::Tuple(_) | Value::Error(_) => (),
        _ => return Ok(()),
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
                let entity = arena.ledger_key(handle).map_err(arena_fault)?;
                visit(Identity::Object(entity), 1)?;
            }
            Value::Task(task) | Value::TaskRef(task) => {
                visit(Identity::Task(task), 1)?;
            }
            Value::Tuple(handle) | Value::Error(handle) => {
                if !arena.summary_of(&value).has_edges {
                    continue;
                }
                let facts = arena.facts(handle).map_err(arena_fault)?;
                if let Some(cached) = facts
                    .projected()
                    .and_then(|projected| projected.logical_holding.get())
                {
                    cached.each_edge(access, &mut |edge| visit(edge.target, edge.count))?;
                } else if let Some(page) = facts.page() {
                    stream_value_edges(access, page, &mut |target| visit(target, 1))?;
                } else {
                    each_member(arena.body(handle).map_err(arena_fault)?, &mut |value| {
                        pending.push(*value)
                    });
                }
            }
            _ => {}
        }
    }
    Ok(())
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

impl Ledger {
    /// Prepare a native's detached canonical graph and surviving result in one
    /// boundary. The declaration is checked against actual body/member facts
    /// before any identity grant. Scratch retains its declared phase order.
    #[expect(clippy::too_many_arguments, reason = "one native storage boundary")]
    pub fn prepare_native_boundary<'a>(
        &self,
        mut registration: crate::arena::Registration<'a>,
        heap: &super::super::heap_impl::PagedHeap,
        inputs: &[Owner],
        output: Owner,
        value: &crate::arena::PreparedValueProof,
        contract: &lash_kernel_doc::NativeResourceContract,
        mut dependencies: Dependencies,
    ) -> Result<Option<BoundaryTransaction<'a>>, HeapFault> {
        use lash_kernel_doc::{NativeResourceEvent, NativeResourceExit};
        let graph = value.graph();
        if !graph.is_current(registration.arena()) {
            return Ok(None);
        }
        let Some(graph_dependencies) =
            Dependencies::capture(self, registration.arena(), graph.dependencies())
        else {
            return Ok(None);
        };
        dependencies.extend(graph_dependencies)?;
        let entities = graph.entities();
        if contract.exit != NativeResourceExit::Returned
            || !dependencies.is_current(self, registration.arena())
        {
            return Ok(None);
        }
        let construction = contract
            .peak_extra_bytes()
            .and_then(|peak| peak.checked_sub(contract.output.bytes))
            .ok_or_else(|| invalid("invalid native phase schedule"))?;
        let declarations: Vec<_> = contract
            .events
            .iter()
            .filter_map(|event| match event {
                NativeResourceEvent::Entity {
                    owner,
                    kind,
                    members,
                    ..
                } => Some((*owner, *kind, members)),
                NativeResourceEvent::Scratch { .. } => None,
            })
            .collect();
        if declarations.len() != entities.len() {
            return Ok(None);
        }
        let owners: BTreeMap<_, _> = entities
            .iter()
            .zip(&declarations)
            .map(|(entity, (owner, _, _))| (entity.entity, *owner))
            .collect();
        if owners.len() != entities.len() {
            return Ok(None);
        }
        for (index, (_, kind, members)) in declarations.iter().enumerate() {
            let Ok((actual_kind, actual_members)) = graph.native_members(index, &owners) else {
                return Ok(None);
            };
            if actual_kind != *kind || actual_members != **members {
                return Ok(None);
            }
        }
        if value.native_holding(&owners) != contract.output {
            return Ok(None);
        }
        if entities.is_empty() {
            return self
                .prepare_source_boundary(
                    registration,
                    heap,
                    inputs,
                    &[(output, RootValue::Prepared(value))],
                )?
                .bind_dependencies(dependencies)?
                .require_source_room(construction)
                .map(Some)
                .ok_or_else(|| invalid("native construction peak overflow"));
        }
        let access = heap.pages();
        let mut pending = Vec::new();
        let mut headers = Vec::with_capacity(entities.len());
        let mut bodies = Vec::with_capacity(entities.len());
        for (index, entity) in entities.iter().enumerate() {
            if self.entity_holding(entity.entity, access)?.is_some() {
                return Ok(None);
            }
            headers.push(Change::Allocate {
                entity: entity.entity,
                body: Holding::scalar(Cost::OBJECT),
            });
            let body = graph.body(index).clone();
            body.each_edge(access, &mut |edge| {
                if let Identity::Object(child) = edge.target {
                    pending.push(child);
                }
                Ok(())
            })?;
            bodies.push(Change::Mutate {
                entity: entity.entity,
                body,
            });
        }
        let roots = self.ownership_changes(
            registration.arena(),
            access,
            inputs,
            &[(output, RootValue::Prepared(value))],
            &mut pending,
        )?;
        pending.retain(|entity| !owners.contains_key(entity));
        let mut changes =
            self.allocate_membership_from(registration.arena(), access, pending, Some(heap))?;
        changes.extend(headers);
        changes.extend(bodies);
        changes.push(Change::Roots(roots));
        let prepared = self
            .prepare_event(changes, access)?
            .require_source_room(construction)
            .ok_or_else(|| invalid("native construction peak overflow"))?;
        graph.reserve_for_publication(registration.arena())?;
        Ok(Some(BoundaryTransaction::from_program(
            registration,
            dependencies,
            prepared,
            Vec::new(),
            publication::RootProof::new(inputs, &[(output, RootValue::Prepared(value))]),
        )))
    }
}
