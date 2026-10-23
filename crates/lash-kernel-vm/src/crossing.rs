//! Tool arguments that cross as function values (`K-EFF-002`,
//! `K-CLO-004`).
//!
//! An effect's arguments are copied out of the run as data, each walked
//! together with the type the effect's signature declares for it. A
//! function among them (a closure, a token holding one, or a reference to
//! a declared function) crosses exactly where the declared type at its path
//! admits a function: `Any`, a function type, or a union with one. It is
//! cut out of the live heap as a [`lash_kernel_doc::FunctionValue`] before
//! the surrounding input is copied ([`lash_kernel_doc::snapshot::extract`]);
//! its captures are read now, so a later change to a variable it read is
//! not in it. Anywhere else, a concretely typed slot such as a record field
//! or JSON data, it raises `not_data` naming its path
//! (`input.plan.steps[2].map`). A task, a ref, a cycle, a function no
//! adapter says how to start, one whose exact library origin is unavailable, and a value past its
//! bound still raise. Every refusal comes before anything is requested.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::data::{CopyError, raised};
use lash_kernel_doc::heap::{EntityId, HeapFault, PageReader, PageValue};
use lash_kernel_doc::snapshot::{members, text_of};
use lash_kernel_doc::{Datum, ErrorDatum, FunctionCatalog};

use lash_kernel_state::kernel_internals::PagedHeap;
use lash_kernel_state::paged::SemanticHeap;
use lash_vocabulary::content_ids::LinkId;
use lash_vocabulary::data_limits::KernelDataLimits;
use lash_vocabulary::names::{CallableId, DocumentId};
use lash_vocabulary::types::Type;

pub(crate) struct CrossingHeap<'m> {
    pub heap: &'m mut PagedHeap,
}

pub(crate) struct CrossingRun<'a> {
    pub function_origins: &'a crate::interface::FunctionOrigins,
    pub captions: &'a [lash_kernel_doc::CaptionEntry],
    pub documents: &'a BTreeMap<DocumentId, Arc<lash_kernel_doc::Document>>,
    pub instances: &'a BTreeMap<LinkId, crate::interface::PreparedInstance>,
    pub catalog: &'a dyn FunctionCatalog,
    pub helpers: &'a lash_kernel_doc::BuildHelpers,
    pub selection: &'a lash_kernel_doc::Selection,
    pub data: KernelDataLimits,
}

fn defining_document<'a>(
    run: &'a CrossingRun<'_>,
    callable: CallableId,
) -> Result<&'a lash_kernel_doc::Document, CopyError> {
    run.instances
        .get(&callable.link)
        .and_then(|instance| run.documents.get(&instance.document))
        .map(Arc::as_ref)
        .ok_or_else(|| CopyError::Heap(HeapFault::Malformed("callable Link is not loaded".into())))
}

fn callable(heap: &CrossingHeap<'_>, value: &PageValue) -> Result<Option<CallableId>, CopyError> {
    Ok(match value {
        PageValue::Function(callable) => Some(**callable),
        PageValue::Closure(id) => Some(heap.heap.closure(*id)?.callable),
        _ => None,
    })
}

/// Packed dialect callables cross as one FunctionValue; ordinary tuples keep their shape.
fn function_root(
    heap: &CrossingHeap<'_>,
    run: &CrossingRun<'_>,
    value: &PageValue,
) -> Result<Option<PageValue>, CopyError> {
    if callable(heap, value)?.is_some() {
        return Ok(Some(value.clone()));
    }
    let PageValue::Tuple(tuple) = value else {
        return Ok(None);
    };
    let values = members(heap.heap, tuple)?;
    for (index, member) in values.iter().enumerate() {
        let PageValue::Closure(id) = member else {
            continue;
        };
        let closure = heap.heap.closure(*id)?;
        let document = defining_document(run, closure.callable)?;
        let site = closure.site.resolve(heap.heap.pages())?;
        if let Some(start) = lash_kernel_doc::snapshot::closure_at(
            document,
            &site,
            closure.callable.link,
            run.helpers,
            run.selection,
        )
        .and_then(|c| c.presentation.as_ref())
            && let lash_kernel_doc::FunctionRepresentation::Token { tag, callable } =
                &start.representation
            && *callable as usize == index
            && let Some(PageValue::Text(first)) = values.first()
            && text_of(heap.heap, first)? == *tag
        {
            return Ok(Some(member.clone()));
        }
    }
    Ok(None)
}

#[path = "crossing/captures.rs"]
mod captures;
use captures::extract_function;

/// The declared type at one path of an effect argument: open, where every
/// value is admitted, or the members of the union it is. No member means no
/// type describes the path, so what stands there is data.
enum Slot<'t> {
    Open,
    Typed(Vec<&'t Type>),
}

/// One step from a value to a member of it.
enum Step<'s> {
    Item(usize),
    Field(&'s str),
    Key,
    Entry(Option<&'s str>),
    ErrorData,
}

impl<'t> Slot<'t> {
    fn of(ty: &'t Type) -> Self {
        let mut types = Vec::new();
        if Self::gather(ty, &mut types) {
            Self::Open
        } else {
            Self::Typed(types)
        }
    }

    /// Adds `ty`'s union members to `types`; whether one is `Any`.
    fn gather(ty: &'t Type, types: &mut Vec<&'t Type>) -> bool {
        match ty {
            Type::Any => true,
            Type::Union(members) => members.iter().any(|member| Self::gather(member, types)),
            other => {
                types.push(other);
                false
            }
        }
    }

    /// A function may cross here.
    fn admits_function(&self) -> bool {
        match self {
            Self::Open => true,
            Self::Typed(types) => types.iter().any(|ty| matches!(ty, Type::Function(_))),
        }
    }

    /// The declared type one `step` into a value standing here.
    fn child(&self, step: &Step<'_>) -> Self {
        let Self::Typed(types) = self else {
            return Self::Open;
        };
        let mut members = Vec::new();
        for ty in types {
            let member = match (ty, step) {
                (Type::Json, _) => Some(&Type::Json),
                (Type::List(item) | Type::Set(item), Step::Item(_)) => Some(&**item),
                (Type::Tuple(items), Step::Item(at)) => items.get(*at),
                (Type::Map(map), Step::Key) => Some(&map.key),
                (Type::Map(map), Step::Entry(_) | Step::Field(_)) => Some(&map.value),
                (Type::Record(record), Step::Field(name) | Step::Entry(Some(name))) => record
                    .fields
                    .iter()
                    .find(|field| field.name == *name)
                    .map(|field| &field.ty)
                    .or(record.rest.as_deref()),
                (Type::Record(_), Step::Key) => Some(&Type::Text),
                // An error's data is any value (`K-VAL-015`).
                (Type::Error, Step::ErrorData) => return Self::Open,
                _ => None,
            };
            if let Some(member) = member
                && Self::gather(member, &mut members)
            {
                return Self::Open;
            }
        }
        Self::Typed(members)
    }
}

/// The path of an argument, from its parameter's name: what a refusal
/// names. Only a typed slot records it, since an open one refuses no
/// function below it.
struct ArgumentPath {
    steps: Vec<String>,
}

impl ArgumentPath {
    fn render(&self) -> String {
        self.steps.concat()
    }
}

fn step_text(step: &Step<'_>) -> String {
    match step {
        Step::Item(at) => format!("[{at}]"),
        Step::Field(name) | Step::Entry(Some(name)) => format!(".{name}"),
        Step::Key => "[key]".to_owned(),
        Step::Entry(None) => "[entry]".to_owned(),
        Step::ErrorData => ".data".to_owned(),
    }
}

/// Copies an effect argument out of the run, walked with `ty`, the type its
/// parameter `name` declares: each function in it crosses as a function
/// value where that type admits one and raises `not_data` naming its path
/// elsewhere. The copy-out rules hold for everything else.
pub(crate) fn copy_out_tool_arg(
    heap: &mut CrossingHeap<'_>,
    run: &mut CrossingRun<'_>,
    value: &PageValue,
    name: &str,
    ty: &Type,
) -> Result<Datum, CopyError> {
    let depth = heap.heap.value_depth_limit();
    let mut at = ArgumentPath {
        steps: vec![name.to_owned()],
    };
    copy(
        heap,
        run,
        value,
        &Slot::of(ty),
        &mut at,
        depth,
        &mut Vec::new(),
    )
}

/// Copies the member one `step` into a value standing at `slot`.
#[expect(
    clippy::too_many_arguments,
    reason = "one walk carries the heap, the run, the slot, the path and the bounds"
)]
fn member(
    heap: &mut CrossingHeap<'_>,
    run: &mut CrossingRun<'_>,
    value: &PageValue,
    slot: &Slot<'_>,
    step: Step<'_>,
    at: &mut ArgumentPath,
    budget: usize,
    path: &mut Vec<EntityId>,
) -> Result<Datum, CopyError> {
    let child = slot.child(&step);
    let typed = matches!(child, Slot::Typed(_));
    if typed {
        at.steps.push(step_text(&step));
    }
    let copied = copy(heap, run, value, &child, at, budget, path);
    if typed {
        at.steps.pop();
    }
    copied
}

fn copy(
    heap: &mut CrossingHeap<'_>,
    run: &mut CrossingRun<'_>,
    value: &PageValue,
    slot: &Slot<'_>,
    at: &mut ArgumentPath,
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
    if function_root(heap, run, value)?.is_some() {
        if !slot.admits_function() {
            return Err(raised(
                "not_data",
                format!(
                    "a function cannot leave the run: `{}` is declared as data, which admits no function",
                    at.render()
                ),
            )
            .into());
        }
        let function =
            extract_function(heap, run, value, budget, path).map_err(|error| match error {
                CopyError::Raised(mut raised) => {
                    raised.message = format!("a function cannot leave the run: {}", raised.message);
                    CopyError::Raised(raised)
                }
                other => other,
            })?;
        return Ok(Datum::FunctionValue(Box::new(function)));
    }
    Ok(match value {
        PageValue::Tuple(tuple) => {
            let mut items = Vec::new();
            for (index, item) in members(heap.heap, tuple)?.iter().enumerate() {
                items.push(member(
                    heap,
                    run,
                    item,
                    slot,
                    Step::Item(index),
                    at,
                    budget,
                    path,
                )?);
            }
            Datum::Tuple(items)
        }
        PageValue::List(id) | PageValue::Map(id) | PageValue::Set(id) | PageValue::Record(id) => {
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
                    lash_kernel_doc::heap::Element::Item(v) => (None, None, v.clone()),
                    lash_kernel_doc::heap::Element::Entry { key, value } => {
                        (Some(key.clone()), None, value.clone())
                    }
                    lash_kernel_doc::heap::Element::Field { name, value } => {
                        (None, Some(name.clone()), value.clone())
                    }
                });
                std::ops::ControlFlow::Continue(())
            })?;
            let mut items = Vec::new();
            let mut entries = Vec::new();
            let mut fields = Vec::new();
            for (index, (key, field, element)) in elements.into_iter().enumerate() {
                match (key, field) {
                    (Some(key), _) => {
                        // A text key names its entry, as a field names
                        // its value.
                        let name = match (&key, slot) {
                            (PageValue::Text(text), Slot::Typed(_)) => {
                                Some(text_of(heap.heap, text)?)
                            }
                            _ => None,
                        };
                        let copied = member(
                            heap,
                            run,
                            &element,
                            slot,
                            Step::Entry(name.as_deref()),
                            at,
                            budget,
                            path,
                        )?;
                        let key = member(heap, run, &key, slot, Step::Key, at, budget, path)?;
                        entries.push((key, copied));
                    }
                    (None, Some(field)) => {
                        let name = text_of(heap.heap, &field)?;
                        let copied = member(
                            heap,
                            run,
                            &element,
                            slot,
                            Step::Field(&name),
                            at,
                            budget,
                            path,
                        )?;
                        fields.push((name, copied));
                    }
                    (None, None) => items.push(member(
                        heap,
                        run,
                        &element,
                        slot,
                        Step::Item(index),
                        at,
                        budget,
                        path,
                    )?),
                }
            }
            path.pop();
            match value {
                PageValue::List(_) => Datum::List(items),
                PageValue::Set(_) => Datum::Set(items),
                PageValue::Map(_) => Datum::Map(entries),
                _ => Datum::Record(fields),
            }
        }
        PageValue::Error(error) => {
            let data = heap.heap.error_data(error)?;
            Datum::Error(Box::new(ErrorDatum {
                kind: text_of(heap.heap, &error.kind)?,
                message: text_of(heap.heap, &error.message)?,
                data: member(heap, run, &data, slot, Step::ErrorData, at, budget, path)?,
            }))
        }
        other => crate::data::copy_page_out(heap.heap, other)?,
    })
}
