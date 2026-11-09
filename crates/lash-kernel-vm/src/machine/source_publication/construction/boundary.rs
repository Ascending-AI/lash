//! Detached source-ordered storage and its single canonical ledger boundary.
use super::super::{malformed, native_fault};
use lash_kernel_doc::{self as doc, Value, arena::ArenaHandle, heap::HeapFault};
use lash_kernel_state::{
    arena::{
        Arena, PreparedBatch, PreparedGraphProof, PreparedInstallation, PreparedValueProof,
        Registration,
    },
    kernel_internals::{
        PagedHeap,
        logical::{BoundaryTransaction, Dependencies, Holding, Ledger, Owner, RootValue},
    },
};

pub(in crate::machine) struct Output {
    pub(in crate::machine) owner: Owner,
    pub(in crate::machine) value: Value,
    pub(in crate::machine) proof: PreparedValueProof,
    pub(in crate::machine) holding: Holding,
}
/// The registration owns every reserved slot, including allocations which do
/// not survive in outputs. Bodies never become visible during preparation.
pub(in crate::machine) struct PreparedGraph<'a> {
    pub(in crate::machine) registration: Registration<'a>,
    pub(in crate::machine) batch: PreparedBatch,
    pub(in crate::machine) outputs: Vec<Output>,
    pub(in crate::machine) proof: PreparedGraphProof,
}

impl<'a> PreparedGraph<'a> {
    /// Entry dependencies include discarded operands and capture cells. The
    /// source owner must authenticate the selected exit and interior peak with
    /// this transaction before it can publish either storage or resources.
    pub(in crate::machine) fn into_boundary(
        mut self,
        ledger: &Ledger,
        heap: &PagedHeap,
        inputs: &[Owner],
        dependencies: Dependencies,
    ) -> Result<(BoundaryTransaction<'a>, PreparedGraphStorage), HeapFault> {
        if !self
            .batch
            .matches_proof(&self.proof, self.registration.arena())
        {
            return Err(malformed("construction substituted its prepared batch"));
        }
        let roots: Vec<_> = self
            .outputs
            .iter()
            .map(|output| (output.owner, RootValue::Prepared(&output.proof)))
            .collect();
        let (transaction, members) = ledger.prepare_construction_boundary(
            self.registration,
            heap,
            inputs,
            &roots,
            self.proof.entities(),
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
        if !installation.values_fit_depth(&[value], depth_limit) {
            return Ok(None);
        }
        let graph = finish(installation, heap, &[(output, value)], depth_limit, false)?;
        if graph.outputs[0].proof.depth() as usize > depth_limit {
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
    let installation = arena.prepare_install(new).map_err(native_fault)?;
    if !installation.values_fit_depth(
        &outputs.iter().map(|(_, value)| *value).collect::<Vec<_>>(),
        depth_limit,
    ) {
        return Ok(None);
    }
    finish(installation, heap, outputs, depth_limit, false).map(Some)
}

pub(in crate::machine) fn finish<'a>(
    mut installation: PreparedInstallation<'a>,
    heap: &PagedHeap,
    values: &[(Owner, Value)],
    depth_limit: usize,
    literals: bool,
) -> Result<PreparedGraph<'a>, HeapFault> {
    let owners: Vec<_> = values.iter().map(|(owner, _)| *owner).collect();
    let values: Vec<_> = values.iter().map(|(_, value)| *value).collect();
    let proof = installation.prove_values(heap.pages(), &values, depth_limit, literals)?;
    let (mut registration, batch) = installation.into_parts();
    let mut outputs = Vec::new();
    for (index, value) in values.into_iter().enumerate() {
        let value_proof = proof.value(index);
        outputs.push(Output {
            owner: owners[index],
            value,
            holding: value_proof.holding(registration.arena())?,
            proof: value_proof,
        });
    }
    Ok(PreparedGraph {
        registration,
        batch,
        outputs,
        proof,
    })
}

pub(in crate::machine) use super::super::{
    closure::prepare_closure, literal_storage::prepare_literal,
};

#[cfg(test)]
mod certificates;
// The frozen engine's laws: unreachable since the flip; L6 deletes them (FIG-7688).
#[cfg(any())]
mod laws;

#[cfg(any())]
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
    let (cell, installation) = arena.prepare_cell(value).map_err(native_fault)?;
    finish(
        installation,
        heap,
        &[(output, Value::ObjectRef(cell))],
        usize::MAX,
        false,
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
