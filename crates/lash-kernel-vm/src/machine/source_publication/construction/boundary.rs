//! Detached source-ordered storage and its single canonical ledger boundary.
use super::super::{malformed, native_fault};
use crate::machine::project::project_prepared;
use lash_kernel_doc::{self as doc, Value, arena::ArenaHandle, heap::HeapFault};
use lash_kernel_state::{
    arena::{Arena, PreparedBatch, PreparedInstallation, Registration},
    kernel_internals::{
        PagedHeap,
        logical::{
            BoundaryTransaction, Dependencies, Holding, Ledger, Owner, RootValue, VirtualBody,
            VirtualEntity,
        },
    },
};

pub(in crate::machine) struct Output {
    pub(in crate::machine) owner: Owner,
    pub(in crate::machine) value: Value,
    pub(in crate::machine) page: doc::heap::PageValue,
    pub(in crate::machine) holding: Holding,
}
/// The registration owns every reserved slot, including allocations which do
/// not survive in outputs. Bodies never become visible during preparation.
pub(in crate::machine) struct PreparedGraph<'a> {
    pub(in crate::machine) registration: Registration<'a>,
    pub(in crate::machine) batch: PreparedBatch,
    pub(in crate::machine) outputs: Vec<Output>,
    pub(in crate::machine) entities: Vec<VirtualEntity>,
}

impl<'a> PreparedGraph<'a> {
    /// Entry dependencies include discarded operands and capture cells. The
    /// source owner must authenticate the selected exit and interior peak with
    /// this transaction before it can publish either storage or resources.
    pub(in crate::machine) fn into_boundary(
        self,
        ledger: &Ledger,
        heap: &PagedHeap,
        inputs: &[Owner],
        dependencies: Dependencies,
    ) -> Result<(BoundaryTransaction<'a>, PreparedGraphStorage), HeapFault> {
        let roots: Vec<_> = self
            .outputs
            .iter()
            .map(|output| (output.owner, RootValue::Page(&output.page)))
            .collect();
        let (transaction, members) = ledger.prepare_construction_boundary(
            self.registration,
            heap,
            inputs,
            &roots,
            &self.entities,
        )?;
        Ok((
            transaction.bind_dependencies(dependencies)?,
            PreparedGraphStorage {
                batch: self.batch,
                outputs: self.outputs,
                members,
            },
        ))
    }
}
pub(in crate::machine) struct PreparedGraphStorage {
    pub(in crate::machine) batch: PreparedBatch,
    pub(in crate::machine) outputs: Vec<Output>,
    /// Canonical direct member bytes, in source allocation order.
    pub(in crate::machine) members: Vec<u64>,
}

/// Prepare an ordinary source constructor through the same canonical batch
/// interface, including the live shared empty tuple and exact key validation.
pub(in crate::machine) fn prepare_collection<'a>(
    arena: &'a mut Arena,
    heap: &PagedHeap,
    object: doc::NewObject,
    output: Owner,
    depth_limit: usize,
) -> Result<Option<PreparedGraph<'a>>, HeapFault> {
    let valid = match &object {
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
    if matches!(&object, doc::NewObject::Tuple(values) if values.is_empty()) {
        let (value, installation) = arena.prepare_empty_tuple().map_err(native_fault)?;
        let graph = finish(installation, heap, &[(output, value)], Vec::new())?;
        if matches!(&graph.outputs[0].page, doc::heap::PageValue::Tuple(tuple) if tuple.summary.depth as usize > depth_limit)
        {
            return Ok(None);
        }
        return Ok(Some(graph));
    }
    let mut nursery = doc::Nursery::unaccounted(arena.objects());
    let value = nursery.allocate(object);
    let new = nursery.into_new();
    prepare_collections(arena, heap, new, &[(output, value)], depth_limit)
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

/// `new` is the useful constructor kernel's original allocation stream, never
/// a traversal of its surviving bindings. Bind all mutable identities before
/// projecting any body, so nested and forward references retain their ordinals.
pub(in crate::machine) fn prepare_collections<'a>(
    arena: &'a mut Arena,
    heap: &PagedHeap,
    new: Vec<(ArenaHandle, doc::NewObject)>,
    outputs: &[(Owner, Value)],
    depth_limit: usize,
) -> Result<Option<PreparedGraph<'a>>, HeapFault> {
    let first = arena.next_provisional().ordinal;
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
    let allocations: Vec<_> = new
        .iter()
        .map(|(handle, object)| object.naming()(*handle))
        .collect();
    let mut installation = arena.prepare_install(new).map_err(native_fault)?;
    let mut identities = Vec::new();
    for (index, handle) in handles.iter().enumerate() {
        let entity = doc::heap::EntityId {
            family: doc::heap::FamilyId::PROVISIONAL,
            ordinal: first
                .checked_add(index as u64)
                .ok_or_else(|| malformed("construction identity overflow"))?,
        };
        installation
            .bind_entity(*handle, entity)
            .map_err(native_fault)?;
        identities.push(entity);
    }
    // Validate immutable interiors even when no output retains them.
    for value in allocations {
        if let doc::heap::PageValue::Tuple(tuple) =
            project_prepared(&mut installation, heap.pages(), &value)?
            && tuple.summary.depth as usize > depth_limit
        {
            return Ok(None);
        }
    }
    let mut entities = Vec::new();
    for (handle, entity) in handles.into_iter().zip(identities) {
        let object = installation.collection(handle).map_err(native_fault)?;
        let body = project_body(&mut installation, heap, object)?;
        entities.push(VirtualEntity { entity, body });
    }
    finish(installation, heap, outputs, entities).map(Some)
}

pub(in crate::machine) fn finish<'a>(
    mut installation: PreparedInstallation<'a>,
    heap: &PagedHeap,
    values: &[(Owner, Value)],
    entities: Vec<VirtualEntity>,
) -> Result<PreparedGraph<'a>, HeapFault> {
    let mut outputs = Vec::new();
    for (owner, value) in values {
        let page = project_prepared(&mut installation, heap.pages(), value)?;
        let holding = Holding::from_page(heap.pages(), &page)?;
        outputs.push(Output {
            owner: *owner,
            value: *value,
            page,
            holding,
        });
    }
    let (registration, batch) = installation.into_parts();
    Ok(PreparedGraph {
        registration,
        batch,
        outputs,
        entities,
    })
}

fn project_body(
    installation: &mut PreparedInstallation<'_>,
    heap: &PagedHeap,
    object: doc::NewObject,
) -> Result<VirtualBody, HeapFault> {
    let mut project = |value| project_prepared(installation, heap.pages(), &value);
    Ok(match object {
        doc::NewObject::List(values) => VirtualBody::List(
            values
                .into_iter()
                .map(&mut project)
                .collect::<Result<_, _>>()?,
        ),
        doc::NewObject::Set(values) => VirtualBody::Set(
            values
                .into_iter()
                .map(&mut project)
                .collect::<Result<_, _>>()?,
        ),
        doc::NewObject::Map(entries) => VirtualBody::Map(
            entries
                .into_iter()
                .map(|(key, value)| Ok((project(key)?, project(value)?)))
                .collect::<Result<_, HeapFault>>()?,
        ),
        doc::NewObject::Record(entries) => VirtualBody::Record(
            entries
                .into_iter()
                .map(|(key, value)| Ok((project(key)?, project(value)?)))
                .collect::<Result<_, HeapFault>>()?,
        ),
        _ => return Err(malformed("construction body is not mutable")),
    })
}

pub(in crate::machine) use super::super::{
    closure::prepare_closure, literal_storage::prepare_literal,
};

#[cfg(test)]
mod certificates;
#[cfg(test)]
mod laws;

#[cfg(test)]
impl crate::machine::KernelMachine {
    pub(crate) fn check_prepared_constructor_graphs(&self) {
        laws::prepared_graphs(self);
    }
    pub(crate) fn check_prepared_constructor_literals(
        &self,
        identity: std::sync::Arc<doc::arena::ClosureIdentity>,
        code: doc::arena::LocalCode,
    ) {
        laws::prepared_literals_and_captures(self, identity, code);
    }
}

/// One fresh variable cell takes the actual evaluated RHS, including aliases.
pub(in crate::machine) fn prepare_cell<'a>(
    arena: &'a mut Arena,
    heap: &PagedHeap,
    value: Value,
    output: Owner,
) -> Result<PreparedGraph<'a>, HeapFault> {
    let entity = arena.next_provisional();
    let (cell, mut installation) = arena.prepare_cell(value).map_err(native_fault)?;
    installation
        .bind_entity(cell, entity)
        .map_err(native_fault)?;
    finish(
        installation,
        heap,
        &[(output, Value::ObjectRef(cell))],
        vec![VirtualEntity {
            entity,
            body: VirtualBody::NativeVariable(value),
        }],
    )
}

#[cfg(test)]
impl crate::machine::KernelMachine {
    pub(crate) fn check_source_constructor_certificates(
        &self,
        source: &std::sync::Arc<crate::compile::room::Source>,
        link: lash_kernel_value::content_ids::LinkId,
        expression: &str,
    ) -> Vec<String> {
        certificates::source_certificates(self, source, link, expression)
    }
}

#[cfg(test)]
mod captures;
#[cfg(test)]
impl crate::machine::KernelMachine {
    pub(crate) fn check_source_closure_certificates(
        &self,
        source: &std::sync::Arc<crate::compile::room::Source>,
        identity: std::sync::Arc<doc::arena::ClosureIdentity>,
        names: Vec<lash_kernel_value::Name>,
        code: doc::arena::LocalCode,
    ) {
        captures::source_cells(self, source, identity, names, code);
    }
}
