//! Tool arguments that cross as function values (`K-EFF-002`,
//! `K-CLO-004`).
//!
//! An effect's arguments are copied out of the run as data. Under the
//! crossing policy for every effect, a
//! function among them crosses too: a closure, a token holding one, or a
//! reference to a declared function is cut out of the live heap as a
//! [`lash_kernel_doc::FunctionValue`] before the surrounding input is
//! copied ([`lash_kernel_doc::snapshot::extract`]). Its captures are read
//! now, so a later change to a variable it read is not in it. A task, a ref,
//! a cycle, a function no adapter says how to start and a value past its
//! bound still raise before anything is requested.

use std::collections::BTreeMap;
use std::sync::Arc;

use lash_kernel_doc::heap::{EntityId, HeapFault};
use lash_kernel_doc::snapshot::{
    Budget, Crossing, SnapshotError, SnapshotHeap, constant_of, crossing_root, extract, members,
    text_of, token_tag,
};
use lash_kernel_doc::{
    Datum, Document, ErrorDatum, Expr, FunctionCatalog, FunctionValueLimits, KernelDataLimits,
    Name, NativeHeap, Site, Unit, Value,
};
use lash_kernel_state::kernel_internals::PagedHeap;
use lash_kernel_state::kernel_internals::metadata::metadata_key;
use lash_kernel_state::kernel_internals::tree::Tree;
use lash_kernel_state::paged::{
    FunctionRecord, HeapLifecycle, HeapWriteError, KeyedRoot, SemanticHeap,
};

use crate::data::{CopyError, raised};

/// The live heap as a crossing reads it: `main`'s free names are the
/// session's bindings, a saved function's are the captures its record
/// froze, and a capture freezes to a constant.
pub(crate) struct CrossingHeap<'m> {
    pub heap: &'m mut PagedHeap,
    pub functions: &'m KeyedRoot,
    pub budget: Budget,
}

fn write_fault(error: HeapWriteError) -> HeapFault {
    match error {
        HeapWriteError::Fault(fault) => fault,
        other => HeapFault::Malformed(format!("a saved capture did not instantiate: {other}")),
    }
}

impl SnapshotHeap for CrossingHeap<'_> {
    type Frozen = Expr;

    fn native(&self) -> &dyn NativeHeap {
        self.heap
    }

    fn closure_site(&self, id: EntityId) -> Result<Arc<Site>, HeapFault> {
        self.heap.closure(id)?.site.resolve(self.heap.pages())
    }

    fn closure_capture(&mut self, id: EntityId, name: &Name) -> Result<Option<Value>, HeapFault> {
        let object = self.heap.closure(id)?;
        match object
            .captures
            .get(&metadata_key(name), self.heap.pages())?
        {
            Some((_, cell)) => self.heap.variable_get(cell).map(Some),
            None => Ok(None),
        }
    }

    fn unit_binding(&mut self, unit: &Unit, name: &Name) -> Result<Option<Value>, HeapFault> {
        match unit {
            Unit::Main => self.heap.session_binding(name),
            Unit::Function(function) => {
                let Some(record) = Tree::<FunctionRecord>::of_keyed(self.functions)
                    .get(&metadata_key(function), self.heap.pages())?
                else {
                    return Ok(None);
                };
                match record.captures.get(name) {
                    Some(frozen) => self.heap.instantiate(frozen).map(Some).map_err(write_fault),
                    None => Ok(None),
                }
            }
            Unit::Library(_) => Ok(None),
        }
    }

    fn freeze(
        &mut self,
        value: &Value,
        named: &BTreeMap<EntityId, Name>,
    ) -> Result<(Expr, std::collections::BTreeSet<Name>), SnapshotError> {
        constant_of(self.heap, value, named, &mut self.budget)
    }
}

/// What a crossing reads of the run besides its heap.
pub(crate) struct CrossingRun<'a> {
    pub document: &'a Document,
    pub catalog: &'a dyn FunctionCatalog,
    pub data: KernelDataLimits,
    /// The tag of the document's tokens, found when a function first
    /// crosses.
    pub tag: Option<Option<String>>,
}

impl CrossingRun<'_> {
    fn crossing(&mut self) -> Crossing<'_> {
        let tag = self
            .tag
            .get_or_insert_with(|| token_tag(self.document))
            .as_deref();
        Crossing {
            document: self.document,
            catalog: self.catalog,
            limits: FunctionValueLimits::standard(),
            data: self.data,
            tag,
        }
    }
}

/// Copies an effect argument out of the run, each function in it crossing
/// as a function value. The copy-out rules hold for everything else.
pub(crate) fn copy_out_tool_arg(
    heap: &mut CrossingHeap<'_>,
    run: &mut CrossingRun<'_>,
    value: &Value,
) -> Result<Datum, CopyError> {
    let depth = heap.heap.value_depth_limit();
    copy(heap, run, value, depth, &mut Vec::new())
}

fn copy(
    heap: &mut CrossingHeap<'_>,
    run: &mut CrossingRun<'_>,
    value: &Value,
    budget: usize,
    path: &mut Vec<EntityId>,
) -> Result<Datum, CopyError> {
    let Some(budget) = budget.checked_sub(1) else {
        return Err(raised(
            "too_deep",
            format!(
                "a value copied out of the run exceeds the configured {}-level value depth",
                heap.heap.value_depth_limit()
            ),
        )
        .into());
    };
    if let Some(root) = crossing_root(heap, run.document, value)? {
        let crossing = run.crossing();
        let function = extract(heap, crossing, &root).map_err(refused)?;
        return Ok(Datum::FunctionValue(Box::new(function)));
    }
    Ok(match value {
        Value::Tuple(tuple) => {
            let mut items = Vec::new();
            for member in members(heap.heap, tuple)? {
                items.push(copy(heap, run, &member, budget, path)?);
            }
            Datum::Tuple(items)
        }
        Value::List(id) | Value::Map(id) | Value::Set(id) | Value::Record(id) => {
            if path.contains(id) {
                return Err(
                    raised("cycle", "a value that holds itself cannot leave the run").into(),
                );
            }
            path.push(*id);
            // The members are read before any is copied: a crossing reads
            // the heap again.
            let mut elements = Vec::new();
            heap.heap.visit(*id, &mut |element| {
                elements.push(match element {
                    lash_kernel_doc::Element::Item(v) => (None, None, v.clone()),
                    lash_kernel_doc::Element::Entry { key, value } => {
                        (Some(key.clone()), None, value.clone())
                    }
                    lash_kernel_doc::Element::Field { name, value } => {
                        (None, Some(name.clone()), value.clone())
                    }
                });
                std::ops::ControlFlow::Continue(())
            })?;
            let mut items = Vec::new();
            let mut entries = Vec::new();
            let mut fields = Vec::new();
            for (key, field, member) in elements {
                let copied = copy(heap, run, &member, budget, path)?;
                match (key, field) {
                    (Some(key), _) => entries.push((copy(heap, run, &key, budget, path)?, copied)),
                    (None, Some(field)) => fields.push((text_of(heap.heap, &field)?, copied)),
                    (None, None) => items.push(copied),
                }
            }
            path.pop();
            match value {
                Value::List(_) => Datum::List(items),
                Value::Set(_) => Datum::Set(items),
                Value::Map(_) => Datum::Map(entries),
                _ => Datum::Record(fields),
            }
        }
        Value::Error(error) => {
            let data = heap.heap.error_data(error)?;
            Datum::Error(Box::new(ErrorDatum {
                kind: text_of(heap.heap, &error.kind)?,
                message: text_of(heap.heap, &error.message)?,
                data: copy(heap, run, &data, budget, path)?,
            }))
        }
        other => crate::data::copy_out(heap.heap, other)?,
    })
}

/// A function that cannot cross raises `not_data` at the `perform`, the
/// message naming its refusal.
fn refused(error: SnapshotError) -> CopyError {
    match error {
        SnapshotError::Heap(fault) => CopyError::Heap(fault),
        SnapshotError::Refused(refusal) => raised(
            "not_data",
            format!("a function cannot leave the run: {refusal}"),
        )
        .into(),
    }
}
