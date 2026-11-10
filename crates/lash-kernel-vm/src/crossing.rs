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

use crate::data::CopyError;
use lash_kernel_doc::heap::HeapFault;
use lash_kernel_doc::{Datum, FunctionCatalog};

use lash_kernel_value::content_ids::LinkId;
use lash_kernel_value::data_limits::KernelDataLimits;
use lash_kernel_value::types::Type;
use {lash_kernel_value::names::CallableId, lash_kernel_value::names::DocumentId};

pub(crate) struct CrossingRun<'a> {
    pub function_origins: &'a crate::interface::FunctionOrigins,
    pub captions: &'a [lash_kernel_doc::CaptionEntry],
    pub documents: &'a BTreeMap<DocumentId, Arc<lash_kernel_doc::Document>>,
    pub instances: &'a BTreeMap<LinkId, crate::interface::PreparedInstance>,
    pub catalog: &'a dyn FunctionCatalog,
    pub helpers: &'a lash_kernel_doc::BuildHelpers,
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

#[path = "crossing/captures.rs"]
mod captures;

#[path = "crossing/arena.rs"]
pub(crate) mod arena;

/// The bytes a crossed argument's function values add to its boundary
/// charge: each one's canonical form.
pub(crate) fn crossed_bytes(datum: &Datum) -> u64 {
    match datum {
        Datum::FunctionValue(value) => value
            .canonical_tagged_bytes()
            .map_or(0, |bytes| bytes.len() as u64),
        Datum::Tuple(items) | Datum::List(items) | Datum::Set(items) => {
            items.iter().map(crossed_bytes).sum()
        }
        Datum::Map(entries) => entries
            .iter()
            .map(|(key, value)| crossed_bytes(key) + crossed_bytes(value))
            .sum(),
        Datum::Record(fields) => fields.iter().map(|(_, value)| crossed_bytes(value)).sum(),
        Datum::Error(error) => crossed_bytes(error.data()),
        _ => 0,
    }
}

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
                    .or(record.rest.kept_type()),
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
