//! The code a function value reaches and the captures it reads, gathered
//! from a live heap: the part a saved function and a crossing function
//! share. What a capture is frozen as is the heap's
//! ([`SnapshotHeap::freeze`]): frozen pages for a saved function,
//! constants for a function value.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::ControlFlow;

use serde::{Deserialize, Serialize};

use crate::AssignOrder;
use crate::Closure;
use crate::Document;
use crate::Expr;
use crate::Literal;
use crate::Node;
use crate::Place;
use crate::Rhs;
use crate::Stmt;
use crate::ValueKind;
use crate::heap::{Chunk, Element, EntityId, HeapFault, Identity, PageReader, PageValue, TupleRef};
use lash_vocabulary::legacy_names::EffectName;
use lash_vocabulary::legacy_names::FunctionId;
use lash_vocabulary::legacy_names::Name;
use lash_vocabulary::sites::Site;
use lash_vocabulary::sites::Unit;

use super::reads::CodeReads;

/// What extraction reads of a live run, and how it freezes a capture.
pub trait SnapshotHeap {
    /// A frozen capture.
    type Frozen: Clone;

    /// The heap's values, read without allocating.
    fn pages(&self) -> &dyn PageReader;

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
        named: &BTreeMap<EntityId, Name>,
    ) -> Result<(Self::Frozen, BTreeSet<Name>), SnapshotError>;
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
    /// The function was made inside a library function's body.
    #[error("its function was made by a library function, whose code is not the run's")]
    MadeByLibrary,
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
    named: &BTreeMap<EntityId, Name>,
    active: &mut BTreeSet<EntityId>,
) -> Result<(), CaptureCheck> {
    match value {
        PageValue::Closure(id) if !named.contains_key(id) => Err(CaptureCheck::Heap(
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
    let body = match &site.unit {
        Unit::Main => &document.main,
        Unit::Function(name) => &document.functions.get(name)?.body,
        Unit::Library(_) => return None,
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

enum Held<F> {
    Data(F, Box<PageValue>),
    Helper(EntityId),
}

/// A closure a gathered function reads under a name and makes again in its
/// own body.
#[derive(Clone, Debug)]
pub struct Helper<F> {
    pub name: Name,
    pub code: Closure,
    pub token: Option<FrozenToken<F>>,
}

/// The code and captures gathered from one root closure.
pub struct Gathered<F> {
    /// The root closure's code.
    pub root: Closure,
    /// Its function's body: each helper declared, made, then the root's
    /// own body. A token member is a capture of a reserved name.
    pub function: crate::Function,
    /// Each name the body reads that the run froze, frozen.
    pub captures: BTreeMap<Name, F>,
    /// The effects the code performs and the library functions it calls.
    pub effects: BTreeSet<EffectName>,
    pub library: BTreeSet<FunctionId>,
    /// The declared functions it names, in its code or in its captures.
    pub references: BTreeSet<Name>,
    /// The named functions whose helpers it shares.
    pub owners: BTreeSet<Name>,
    /// The site the root closure was made at.
    pub site: std::sync::Arc<Site>,
}

/// Gathers the code the closure `root` reaches and the captures it reads,
/// with its helpers made in its own body. `named` are the closures a name
/// of their own holds, which the code reaches by reference; a closure a
/// capture holds whole that `named` does not name is a helper.
/// `helper_owners` are the named functions that share a helper. `avoid`
/// says whether a reserved capture name is taken.
///
/// # Errors
///
/// The heap's fault, or why the code cannot be gathered.
pub fn gather<H: SnapshotHeap>(
    heap: &mut H,
    document: &Document,
    tag: Option<&str>,
    root: EntityId,
    named: &BTreeMap<EntityId, Name>,
    helper_owners: &BTreeMap<EntityId, BTreeSet<Name>>,
    avoid: &dyn Fn(&Name) -> bool,
) -> Result<Gathered<H::Frozen>, SnapshotError> {
    let mut gatherer = Gatherer {
        document,
        tag,
        heap,
        named,
        effects: BTreeSet::new(),
        library: BTreeSet::new(),
        bound: BTreeMap::new(),
        helpers: Vec::new(),
        helper_owners,
        owners: BTreeSet::new(),
        references: BTreeSet::new(),
    };
    let code = gatherer.absorb(root)?.clone();
    let site = gatherer.heap.closure_site(root)?;
    let mut body = Vec::new();
    for helper in &gatherer.helpers {
        body.push(Stmt::Let {
            name: helper.name.clone(),
            value: Rhs::Expr(Expr::Literal(Literal::Absent)),
        });
    }
    // Token members stay captures, not AST constants. A private helper's code
    // refers to private capture names chosen outside the document's names.
    let mut captures: BTreeMap<Name, H::Frozen> = gatherer
        .bound
        .into_iter()
        .filter_map(|(name, held)| match held {
            Held::Data(value, _) => Some((name, value)),
            Held::Helper(_) => None,
        })
        .collect();
    let root_reads = CodeReads::of(&code.params, &code.body);
    for (index, helper) in gatherer.helpers.into_iter().enumerate() {
        let made = Expr::Closure(Box::new(helper.code));
        let value = if let Some(token) = helper.token {
            let mut values = vec![Expr::Literal(Literal::Text(token.tag))];
            for (side, refs) in [("before", token.before), ("after", token.after)] {
                if side == "after" {
                    values.push(made.clone());
                }
                for (at, frozen) in refs.into_iter().enumerate() {
                    let mut capture = Name::new(format!("__saved_token_{index}_{side}_{at}"));
                    while captures.contains_key(&capture)
                        || avoid(&capture)
                        || root_reads.free.contains(&capture)
                    {
                        capture = Name::new(format!("_{}", capture.as_str()));
                    }
                    captures.insert(capture.clone(), frozen);
                    values.push(Expr::Variable(capture));
                }
            }
            Expr::Tuple(values)
        } else {
            made
        };
        body.push(Stmt::Assign {
            place: Place::Variable(helper.name),
            value: Rhs::Expr(value),
            order: AssignOrder::ValueFirst,
        });
    }
    body.extend(code.body.iter().cloned());
    Ok(Gathered {
        function: crate::Function {
            params: code.params.clone(),
            body,
            start: code.start.clone(),
        },
        root: code,
        captures,
        effects: gatherer.effects,
        library: gatherer.library,
        references: gatherer.references,
        owners: gatherer.owners,
        site,
    })
}

/// Freezes the members of the token `values` around its callable at `at`.
///
/// # Errors
///
/// The heap's fault, or [`SnapshotRefusal::Token`] for a member that is no
/// capture data. A closure among the members is one of `named`.
pub fn freeze_token<H: SnapshotHeap>(
    heap: &mut H,
    values: &[PageValue],
    at: usize,
    tag: &str,
    named: &BTreeMap<EntityId, Name>,
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

struct Gatherer<'a, 'h, H: SnapshotHeap> {
    document: &'a Document,
    tag: Option<&'a str>,
    heap: &'h mut H,
    named: &'a BTreeMap<EntityId, Name>,
    effects: BTreeSet<EffectName>,
    library: BTreeSet<FunctionId>,
    bound: BTreeMap<Name, Held<H::Frozen>>,
    helpers: Vec<Helper<H::Frozen>>,
    helper_owners: &'a BTreeMap<EntityId, BTreeSet<Name>>,
    owners: BTreeSet<Name>,
    references: BTreeSet<Name>,
}

impl<'a, H: SnapshotHeap> Gatherer<'a, '_, H> {
    fn code(&self, id: EntityId) -> Result<(&'a Closure, std::sync::Arc<Site>), SnapshotError> {
        let site = self.heap.closure_site(id)?;
        if let Unit::Library(_) = site.unit {
            return Err(SnapshotRefusal::MadeByLibrary.into());
        }
        if let Unit::Function(name) = &site.unit
            && !self.document.functions.contains_key(name)
        {
            return Err(unreadable("its unit is not declared"));
        }
        let closure = closure_at(self.document, &site)
            .ok_or_else(|| unreadable("its site is not a closure expression"))?;
        Ok((closure, site))
    }

    fn captured(
        &mut self,
        id: EntityId,
        site: &Site,
        free: &Name,
    ) -> Result<Option<PageValue>, SnapshotError> {
        if let Some(value) = self.heap.closure_capture(id, free)? {
            return Ok(Some(value));
        }
        // A closure `main` made reads the session's bindings.
        if site.unit == Unit::Main {
            return Ok(self.heap.unit_binding(&Unit::Main, free)?);
        }
        Ok(None)
    }

    fn absorb(&mut self, id: EntityId) -> Result<&'a Closure, SnapshotError> {
        let (closure, site) = self.code(id)?;
        let reads = CodeReads::of(&closure.params, &closure.body);
        self.effects.extend(reads.effects);
        self.library.extend(reads.library);
        self.references.extend(reads.declared);
        for free in &reads.free {
            let Some(value) = self.captured(id, &site, free)? else {
                continue;
            };
            if let Some(helper) = closure_held(self.heap.pages(), &value, self.tag)?
                && !self.named.contains_key(&helper)
            {
                if let Some(owners) = self.helper_owners.get(&helper) {
                    self.owners.extend(owners.iter().cloned());
                }
                match self.bound.get(free) {
                    Some(Held::Helper(id)) if *id == helper => continue,
                    Some(_) => return Err(capture_refused(free, CaptureRefusal::Conflict)),
                    None => (),
                }
                let token = match (&value, token_closure(self.heap.pages(), &value, self.tag)?) {
                    (PageValue::Tuple(t), Some((_, at))) => {
                        let values = members(self.heap.pages(), t)?;
                        Some(freeze_token(
                            self.heap,
                            &values,
                            at,
                            self.tag.unwrap_or_default(),
                            self.named,
                        )?)
                    }
                    _ => None,
                };
                self.bound.insert(free.clone(), Held::Helper(helper));
                let code = self.absorb(helper)?.clone();
                self.helpers.push(Helper {
                    name: free.clone(),
                    code,
                    token,
                });
                continue;
            }
            if let Some(Held::Data(_, held)) = self.bound.get(free)
                && self.heap.pages().equal(held, &value)?
            {
                continue;
            }
            validate_capture(self.heap.pages(), &value, self.named, &mut BTreeSet::new())
                .map_err(|e| e.named(free))?;
            let (frozen, functions) = self.heap.freeze(&value, self.named)?;
            self.references.extend(functions);
            match self.bound.get(free) {
                Some(Held::Data(_, held)) if self.heap.pages().equal(held, &value)? => (),
                Some(_) => return Err(capture_refused(free, CaptureRefusal::Conflict)),
                None => {
                    self.bound
                        .insert(free.clone(), Held::Data(frozen, Box::new(value)));
                }
            }
        }
        Ok(closure)
    }
}

// The capture refusal's codec, which a kind-30 bundle's refused members
// carry (FIG-5887 §2 numbers).

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
