//! The code a function value reaches and the captures it reads, gathered
//! from a live heap: the part a saved function and a crossing function
//! share. What a capture is frozen as is the heap's
//! ([`SnapshotHeap::freeze`]): frozen pages for a saved function,
//! constants for a function value.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::ControlFlow;

use serde::{Deserialize, Serialize};

use crate::Closure;
use crate::Document;
use crate::Expr;

use crate::Node;

use crate::Stmt;
use crate::ValueKind;
use crate::heap::{Chunk, Element, EntityId, HeapFault, Identity, PageReader, PageValue, TupleRef};

use lash_vocabulary::names::Name;
use lash_vocabulary::sites::Site;
use lash_vocabulary::sites::Unit;

use super::reads::CodeReads;

/// What extraction reads of a live run, and how it freezes a capture.
pub trait SnapshotHeap {
    /// A frozen capture.
    type Frozen: Clone;

    /// The heap's values, read without allocating.
    fn pages(&self) -> &dyn PageReader;

    fn closure_callable(
        &self,
        id: EntityId,
    ) -> Result<lash_vocabulary::names::CallableId, HeapFault>;

    /// The site the closure `id` was made at.
    ///
    /// # Errors
    ///
    /// The heap's fault.
    fn closure_site(&self, id: EntityId) -> Result<std::sync::Arc<Site>, HeapFault>;

    /// What the variable the closure `id` captured under `name` holds now;
    /// `None` when it captured none of that name.
    ///
    /// # Errors
    ///
    /// The heap's fault.
    fn closure_capture(
        &mut self,
        id: EntityId,
        name: &Name,
    ) -> Result<Option<PageValue>, HeapFault>;

    /// What a free name of `unit`'s code reads when no capture of a
    /// closure holds it: for `main`'s code, the session's binding; for a
    /// saved function's, the capture it froze. `None` when nothing does.
    ///
    /// # Errors
    ///
    /// The heap's fault.
    fn unit_binding(&mut self, unit: &Unit, name: &Name) -> Result<Option<PageValue>, HeapFault>;

    /// Freezes validated capture data ([`validate_capture`]): each closure
    /// of `named` as a reference to the function of its name. Also answers
    /// the names it so referenced.
    ///
    /// # Errors
    ///
    /// The heap's fault, or a capture the freezer refuses.
    fn freeze(
        &mut self,
        value: &PageValue,
        named: &BTreeSet<EntityId>,
    ) -> Result<(Self::Frozen, BTreeSet<EntityId>), SnapshotError>;
}

/// Why one captured variable cannot be frozen.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum CaptureRefusal {
    #[error("which holds a task or a pending promise")]
    Task,
    #[error("which holds data that contains itself")]
    Cyclic,
    /// Two of the functions it holds read different variables of this
    /// name.
    #[error("which names two different variables among the functions it holds")]
    Conflict,
    #[error("which holds a {kind:?} value, and no constant is written for one")]
    NoConstant { kind: ValueKind },
}

/// Why the code a root reaches cannot be gathered.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum SnapshotRefusal {
    /// A variable the code reads cannot be frozen.
    #[error("its function reads `{name}`, {why}")]
    Capture { name: Name, why: CaptureRefusal },
    /// The token a function is held in holds data about it that cannot be
    /// frozen.
    #[error("its function's value holds data about the function that is no constant")]
    Token,
    /// The run's heap did not describe the closure as its document has it.
    #[error("its function cannot be read back from the document: {problem}")]
    Unreadable { problem: String },
    /// No start adapter says how the function is started, and it is no
    /// entry: it cannot cross as a function value.
    #[error("it holds a function that states no start signature, so it cannot be started")]
    NoStartSignature,
    /// The function value would be larger than its bound.
    #[error("its function value takes more than {limit} canonical bytes ({bytes} so far)")]
    TooLarge { bytes: u64, limit: u64 },
}

/// Gathering failed: the heap faulted, or the code refused.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error(transparent)]
    Heap(#[from] HeapFault),
    #[error(transparent)]
    Refused(#[from] SnapshotRefusal),
}

pub(crate) fn unreadable(problem: &str) -> SnapshotError {
    SnapshotRefusal::Unreadable {
        problem: problem.into(),
    }
    .into()
}

fn capture_refused(name: &Name, why: CaptureRefusal) -> SnapshotError {
    SnapshotRefusal::Capture {
        name: name.clone(),
        why,
    }
    .into()
}

/// A token's members around its callable, frozen.
#[derive(Clone, Debug)]
pub struct FrozenToken<F> {
    pub tag: String,
    pub before: Vec<F>,
    pub after: Vec<F>,
}

/// The members of a tuple.
///
/// # Errors
///
/// The heap's fault.
pub fn members(heap: &dyn PageReader, tuple: &TupleRef) -> Result<Vec<PageValue>, HeapFault> {
    let mut values = Vec::new();
    heap.tuple_visit(tuple, &mut |v| {
        values.push(v.clone());
        ControlFlow::Continue(())
    })?;
    Ok(values)
}

/// Whether `value` is the text `text`.
///
/// # Errors
///
/// The heap's fault.
pub fn text_is(heap: &dyn PageReader, value: &PageValue, text: &str) -> Result<bool, HeapFault> {
    let PageValue::Text(t) = value else {
        return Ok(false);
    };
    if t.summary.utf8_bytes != text.len() as u64 {
        return Ok(false);
    }
    let mut found = String::new();
    heap.payload_visit(value, 0..t.summary.utf8_bytes, &mut |chunk| {
        if let Chunk::Text(s) = chunk {
            found.push_str(s);
        }
        ControlFlow::Continue(())
    })?;
    Ok(found == text)
}

/// The text `value` holds.
///
/// # Errors
///
/// The heap's fault.
pub fn text_of(heap: &dyn PageReader, text: &crate::heap::TextRef) -> Result<String, HeapFault> {
    let mut out = String::new();
    heap.payload_visit(
        &PageValue::Text(text.clone()),
        0..text.summary.utf8_bytes,
        &mut |chunk| {
            if let Chunk::Text(chunk) = chunk {
                out.push_str(chunk);
            }
            ControlFlow::Continue(())
        },
    )?;
    Ok(out)
}

/// The closure a token tagged `tag` holds and its slot: a tuple whose first
/// member is the text `tag` and that holds exactly one closure.
///
/// # Errors
///
/// The heap's fault.
pub fn token_closure(
    heap: &dyn PageReader,
    value: &PageValue,
    tag: Option<&str>,
) -> Result<Option<(EntityId, usize)>, HeapFault> {
    let (Some(tag), PageValue::Tuple(t)) = (tag, value) else {
        return Ok(None);
    };
    let values = members(heap, t)?;
    if !values
        .first()
        .map(|v| text_is(heap, v, tag))
        .transpose()?
        .unwrap_or(false)
    {
        return Ok(None);
    }
    let mut closures = values.iter().enumerate().filter_map(|(at, value)| {
        if let PageValue::Closure(id) = value {
            Some((*id, at))
        } else {
            None
        }
    });
    let first = closures.next();
    Ok(if closures.next().is_none() {
        first
    } else {
        None
    })
}

/// The closure `value` is, itself or held in a token tagged `tag`.
///
/// # Errors
///
/// The heap's fault.
pub fn closure_held(
    heap: &dyn PageReader,
    value: &PageValue,
    tag: Option<&str>,
) -> Result<Option<EntityId>, HeapFault> {
    match value {
        PageValue::Closure(id) => Ok(Some(*id)),
        _ => Ok(token_closure(heap, value, tag)?.map(|(id, _)| id)),
    }
}

/// Whether `value` can be frozen as capture data: no task, nothing with no
/// constant, no cycle. Every closure in it is one of `named`, which the
/// freeze refers to by name: the walk that named them ([`super::reach`])
/// found each.
///
/// # Errors
///
/// The heap's fault, or the refusal.
pub fn validate_capture(
    heap: &dyn PageReader,
    value: &PageValue,
    named: &BTreeSet<EntityId>,
    active: &mut BTreeSet<EntityId>,
) -> Result<(), CaptureCheck> {
    match value {
        PageValue::Closure(id) if !named.contains(id) => Err(CaptureCheck::Heap(
            HeapFault::Malformed("a captured closure was named by no walk".into()),
        )),
        PageValue::Task(_) | PageValue::Ref(Identity::Task(_)) => {
            Err(CaptureCheck::Refused(CaptureRefusal::Task))
        }
        PageValue::Timestamp(_)
        | PageValue::Error(_)
        | PageValue::Handle(_)
        | PageValue::Ref(_) => Err(CaptureCheck::Refused(CaptureRefusal::NoConstant {
            kind: no_constant_kind(value),
        })),
        PageValue::Tuple(t) => {
            for value in members(heap, t)? {
                validate_capture(heap, &value, named, active)?;
            }
            Ok(())
        }
        PageValue::List(id) | PageValue::Map(id) | PageValue::Set(id) | PageValue::Record(id) => {
            if !active.insert(*id) {
                return Err(CaptureCheck::Refused(CaptureRefusal::Cyclic));
            }
            let mut result = Ok(());
            heap.visit(*id, &mut |e| {
                let values = match e {
                    Element::Item(v) => vec![v],
                    Element::Entry { key, value } => vec![key, value],
                    Element::Field { value, .. } => vec![value],
                };
                for value in values {
                    if let Err(e) = validate_capture(heap, value, named, active) {
                        result = Err(e);
                        return ControlFlow::Break(());
                    }
                }
                ControlFlow::Continue(())
            })?;
            active.remove(id);
            result
        }
        _ => Ok(()),
    }
}

/// A capture check's failure.
#[derive(Debug)]
pub enum CaptureCheck {
    Heap(HeapFault),
    Refused(CaptureRefusal),
}

impl From<HeapFault> for CaptureCheck {
    fn from(e: HeapFault) -> Self {
        Self::Heap(e)
    }
}

impl CaptureCheck {
    fn named(self, name: &Name) -> SnapshotError {
        match self {
            Self::Heap(e) => e.into(),
            Self::Refused(why) => capture_refused(name, why),
        }
    }
}

fn no_constant_kind(value: &PageValue) -> ValueKind {
    match value {
        PageValue::Timestamp(_) => ValueKind::Timestamp,
        PageValue::Error(_) => ValueKind::Error,
        PageValue::Handle(_) => ValueKind::Handle,
        PageValue::Ref(_) => ValueKind::Ref,
        _ => ValueKind::Absent,
    }
}

/// The closure expression at `site` of `document`'s own code.
pub fn closure_at<'a>(document: &'a Document, site: &Site) -> Option<&'a Closure> {
    let body: &[Stmt] = match &site.unit {
        Unit::Main => document.main.as_deref()?,
        Unit::Library(id) => &document.functions.get(id)?.body()?.block,
        Unit::Function(_) => return None,
    };
    let mut node = Node::Block(body);
    for step in &site.path {
        node = node.children().into_iter().nth(*step as usize)?;
    }
    match node {
        Node::Expr(Expr::Closure(closure)) => Some(closure),
        _ => None,
    }
}

/// Frozen captures of a linked closure; code remains in its defining document.
pub struct Gathered<F> {
    pub environment: EntityId,
    pub callable: lash_vocabulary::names::CallableId,
    pub captures: BTreeMap<Name, F>,
    pub references: BTreeSet<EntityId>,
    pub site: std::sync::Arc<Site>,
}

pub fn gather<H: SnapshotHeap>(
    heap: &mut H,
    catalog: &dyn crate::FunctionCatalog,
    root: EntityId,
    named: &BTreeSet<EntityId>,
) -> Result<Gathered<H::Frozen>, SnapshotError> {
    let callable = heap.closure_callable(root)?;
    let site = heap.closure_site(root)?;
    let definition = catalog
        .definition(&callable.function)
        .ok_or_else(|| unreadable("closure definition is not loaded"))?;
    let body = definition
        .body()
        .ok_or_else(|| unreadable("closure has no body"))?;
    let params: Vec<_> = definition
        .signature
        .params
        .iter()
        .map(|p| p.name.clone())
        .collect();
    let reads = CodeReads::of(&params, &body.block, catalog);
    let mut captures = BTreeMap::new();
    let mut references = BTreeSet::new();
    for name in reads.free {
        let captured = heap.closure_capture(root, &name)?;
        let value = match captured {
            Some(value) => Some(value),
            None => heap.unit_binding(&site.unit, &name)?,
        };
        if let Some(value) = value {
            validate_capture(heap.pages(), &value, named, &mut BTreeSet::new())
                .map_err(|error| error.named(&name))?;
            let (frozen, called) = heap.freeze(&value, named)?;
            captures.insert(name, frozen);
            references.extend(called);
        }
    }
    Ok(Gathered {
        environment: root,
        callable,
        captures,
        references,
        site,
    })
}

pub fn freeze_token<H: SnapshotHeap>(
    heap: &mut H,
    values: &[PageValue],
    at: usize,
    tag: &str,
    named: &BTreeSet<EntityId>,
) -> Result<FrozenToken<H::Frozen>, SnapshotError> {
    let mut each = |values: &[PageValue]| -> Result<Vec<H::Frozen>, SnapshotError> {
        values
            .iter()
            .map(|value| {
                validate_capture(heap.pages(), value, named, &mut BTreeSet::new()).map_err(
                    |error| match error {
                        CaptureCheck::Heap(e) => SnapshotError::Heap(e),
                        CaptureCheck::Refused(_) => SnapshotRefusal::Token.into(),
                    },
                )?;
                heap.freeze(value, named).map(|(frozen, _)| frozen)
            })
            .collect()
    };
    Ok(FrozenToken {
        tag: tag.into(),
        before: each(&values[1..at])?,
        after: each(&values[at + 1..])?,
    })
}

use lash_vocabulary::tokens::CodecVariant;
use lash_vocabulary::tokens::TokenCodec;
use lash_vocabulary::tokens::TokenError;
use lash_vocabulary::tokens::TokenReader;
use lash_vocabulary::tokens::TokenWriter;

#[doc(hidden)]
impl CodecVariant for CaptureRefusal {
    fn codec_variant(&self) -> u32 {
        match self {
            Self::Task => 0,
            Self::Cyclic => 1,
            Self::Conflict => 2,
            Self::NoConstant { .. } => 3,
        }
    }
}

impl TokenCodec for CaptureRefusal {
    fn encode(&self, out: &mut TokenWriter) {
        let variant = self.codec_variant();
        match self {
            Self::Task | Self::Cyclic | Self::Conflict => out.sum(variant, 0),
            Self::NoConstant { kind } => {
                out.sum(variant, 1);
                out.sum(value_kind_variant(*kind), 0);
            }
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let (variant, fields) = input.sum()?;
        let arity = |expected| TokenReader::expect_fields(fields, expected);
        Ok(match variant {
            0 => arity(0).map(|()| Self::Task)?,
            1 => arity(0).map(|()| Self::Cyclic)?,
            2 => arity(0).map(|()| Self::Conflict)?,
            3 => {
                arity(1)?;
                let (kind, fields) = input.sum()?;
                TokenReader::expect_fields(fields, 0)?;
                Self::NoConstant {
                    kind: value_kind(kind)?,
                }
            }
            variant => return Err(TokenError::UnsupportedVariant { variant }),
        })
    }
}

/// [`ValueKind`]'s numbers: the PageValue discriminants of FIG-5887 §2.
const VALUE_KINDS: [ValueKind; 19] = [
    ValueKind::Null,
    ValueKind::Absent,
    ValueKind::Bool,
    ValueKind::Int,
    ValueKind::Float,
    ValueKind::Text,
    ValueKind::Bytes,
    ValueKind::Timestamp,
    ValueKind::Tuple,
    ValueKind::List,
    ValueKind::Map,
    ValueKind::Set,
    ValueKind::Record,
    ValueKind::Closure,
    ValueKind::Error,
    ValueKind::Task,
    ValueKind::Function,
    ValueKind::Handle,
    ValueKind::Ref,
];

fn value_kind_variant(kind: ValueKind) -> u32 {
    match kind {
        ValueKind::Null => 0,
        ValueKind::Absent => 1,
        ValueKind::Bool => 2,
        ValueKind::Int => 3,
        ValueKind::Float => 4,
        ValueKind::Text => 5,
        ValueKind::Bytes => 6,
        ValueKind::Timestamp => 7,
        ValueKind::Tuple => 8,
        ValueKind::List => 9,
        ValueKind::Map => 10,
        ValueKind::Set => 11,
        ValueKind::Record => 12,
        ValueKind::Closure => 13,
        ValueKind::Error => 14,
        ValueKind::Task => 15,
        ValueKind::Function => 16,
        ValueKind::Handle => 17,
        ValueKind::Ref => 18,
    }
}

fn value_kind(variant: u32) -> Result<ValueKind, TokenError> {
    usize::try_from(variant)
        .ok()
        .and_then(|at| VALUE_KINDS.get(at).copied())
        .ok_or(TokenError::UnsupportedVariant { variant })
}
