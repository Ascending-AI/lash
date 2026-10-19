//! A live function as a [`FunctionValue`] (`K-EFF-002`, `K-CLO-004`).
//!
//! The root is a closure, a token holding one, or a reference to a declared
//! function. Extraction gathers the root's code with its helpers
//! ([`super::gather`]), then every declared function that code reaches, to
//! a fixed point, each with the captures it reads frozen as constants ahead
//! of its body. The entry calls the root as its [`StartAdapter`] says; a
//! declared entry is its own entry. Names are then made canonical
//! ([`super::canon`]), and the value is checked and bounded.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::ControlFlow;

use crate::heap::{Chunk, Element, EntityId, HeapFault, IntRef, PageReader, PageValue};
use crate::{
    Action, Atom, Callee, Document, Expr, Function, FunctionCatalog, FunctionId,
    FunctionRepresentation, FunctionValue, FunctionValueLimits, KernelDataLimits, Literal,
    MapEntry, Name, RecordEntry, Rhs, StartAdapter, StartConvention, Stmt, Unit,
};

use super::collect::{
    SnapshotError, SnapshotHeap, SnapshotRefusal, closure_at, gather, members, text_is, text_of,
    unreadable,
};
use super::reads::CodeReads;

/// What a crossing reads besides the heap: the running document, the
/// library it was admitted against, the bounds and the token tag the
/// document's functions are held in.
#[derive(Clone, Copy)]
pub struct Crossing<'a> {
    pub document: &'a Document,
    pub captions: &'a [crate::CaptionEntry],
    pub catalog: &'a dyn FunctionCatalog,
    pub limits: FunctionValueLimits,
    pub data: KernelDataLimits,
    /// The tag of the tokens the document holds its functions in, if it
    /// does ([`token_tag`]).
    pub tag: Option<&'a str>,
}

/// A function a value is, as it crosses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CrossingRoot {
    /// A closure, bare or held in its token.
    Closure(EntityId),
    /// A reference to a declared function, bare or held in its token.
    Declared(Name),
}

/// The tag of the tokens `document` holds its functions in: the tag of the
/// first token adapter its code carries.
#[must_use]
pub fn token_tag(document: &Document) -> Option<String> {
    let adapters = document
        .functions
        .values()
        .filter_map(|function| function.start.as_ref());
    for adapter in adapters {
        if let FunctionRepresentation::Token { tag, .. } = &adapter.representation {
            return Some(tag.clone());
        }
    }
    let mut found = None;
    let mut visit = |closure: &crate::Closure| {
        if found.is_none()
            && let Some(StartAdapter {
                representation: FunctionRepresentation::Token { tag, .. },
                ..
            }) = &closure.start
        {
            found = Some(tag.clone());
        }
    };
    for block in std::iter::once(&document.main).chain(document.functions.values().map(|f| &f.body))
    {
        each_closure(crate::Node::Block(block), &mut visit);
    }
    found
}

fn each_closure(node: crate::Node<'_>, visit: &mut dyn FnMut(&crate::Closure)) {
    if let crate::Node::Expr(Expr::Closure(closure)) = node {
        visit(closure);
    }
    for child in node.children() {
        each_closure(child, visit);
    }
}

/// The function `value` is, if it is one: a closure or a declared function
/// reference, bare or held in a token whose adapter names the slot it is
/// in.
///
/// # Errors
///
/// The heap's fault.
pub fn crossing_root<H: SnapshotHeap>(
    heap: &H,
    document: &Document,
    value: &PageValue,
) -> Result<Option<CrossingRoot>, HeapFault> {
    match value {
        PageValue::Closure(id) => Ok(Some(CrossingRoot::Closure(*id))),
        PageValue::Function(name) => Ok(Some(CrossingRoot::Declared(Name::new(text_of(
            heap.pages(),
            name,
        )?)))),
        PageValue::Tuple(tuple) => {
            let values = members(heap.pages(), tuple)?;
            for (at, member) in values.iter().enumerate() {
                let (root, start) = match member {
                    PageValue::Closure(id) => {
                        let site = heap.closure_site(*id)?;
                        let start = closure_at(document, &site).and_then(|c| c.start.as_ref());
                        (CrossingRoot::Closure(*id), start)
                    }
                    PageValue::Function(name) => {
                        let name = Name::new(text_of(heap.pages(), name)?);
                        let start = document.functions.get(&name).and_then(|f| f.start.as_ref());
                        (CrossingRoot::Declared(name), start)
                    }
                    _ => continue,
                };
                if let Some(StartAdapter {
                    representation: FunctionRepresentation::Token { tag, callable },
                    ..
                }) = start
                    && *callable as usize == at
                    && let Some(first) = values.first()
                    && text_is(heap.pages(), first, tag)?
                {
                    return Ok(Some(root));
                }
            }
            Ok(None)
        }
        _ => Ok(None),
    }
}

/// Extracts the function `root` reaches as a self-contained value whose
/// one entry starts it.
///
/// # Errors
///
/// The heap's fault, or why the function cannot cross:
/// [`SnapshotRefusal::NoStartSignature`] for a function no adapter or entry
/// says how to start, a capture refusal, or
/// [`SnapshotRefusal::TooLarge`] past `crossing.limits`.
pub fn extract<H: SnapshotHeap<Frozen = Expr>>(
    heap: &mut H,
    crossing: Crossing<'_>,
    root: &CrossingRoot,
) -> Result<FunctionValue, SnapshotError> {
    let document = crossing.document;
    let fresh = |base: &str| {
        let mut name = Name::new(base);
        while document.functions.contains_key(&name) {
            name = Name::new(format!("_{}", name.as_str()));
        }
        name
    };
    let mut units: BTreeMap<Name, Function> = BTreeMap::new();
    // Each copied body keeps its original site and the number of statements
    // prepended to it. Canonicalization only changes the unit names.
    let mut origins = BTreeMap::new();
    let mut pending: Vec<Name> = Vec::new();
    let named;
    // The entry unit and the descriptor its host start uses.
    let (entry, entry_definition) = match root {
        CrossingRoot::Closure(id) => {
            let site = heap.closure_site(*id)?;
            if let Unit::Library(_) = site.unit {
                return Err(SnapshotRefusal::MadeByLibrary.into());
            }
            let start = closure_at(document, &site)
                .ok_or_else(|| unreadable("its site is not a closure expression"))?
                .start
                .clone()
                .ok_or(SnapshotRefusal::NoStartSignature)?;
            let root_name = fresh("root");
            named = BTreeMap::from([(*id, root_name.clone())]);
            let gathered = gather(
                heap,
                document,
                crossing.tag,
                *id,
                &named,
                &BTreeMap::new(),
                &|_| false,
            )?;
            pending.extend(gathered.references.iter().cloned());
            origins.insert(
                root_name.clone(),
                (
                    site.child(0),
                    gathered.captures.len() + gathered.function.body.len()
                        - gathered.root.body.len(),
                ),
            );
            units.insert(
                root_name.clone(),
                with_prelude(gathered.function, gathered.captures),
            );
            let wrapper = fresh("run");
            units.insert(wrapper.clone(), entry_wrapper(&start, &root_name));
            (
                wrapper,
                crate::Entry {
                    signature: start.signature,
                    convention: start.convention,
                },
            )
        }
        CrossingRoot::Declared(name) => {
            named = BTreeMap::new();
            let function = document
                .functions
                .get(name)
                .ok_or_else(|| unreadable("it names no declared function"))?;
            if let Some(signature) = document.entries.get(name) {
                pending.push(name.clone());
                (name.clone(), signature.clone())
            } else {
                let start = function
                    .start
                    .clone()
                    .ok_or(SnapshotRefusal::NoStartSignature)?;
                pending.push(name.clone());
                let wrapper = fresh("run");
                units.insert(wrapper.clone(), entry_wrapper(&start, name));
                (
                    wrapper,
                    crate::Entry {
                        signature: start.signature,
                        convention: start.convention,
                    },
                )
            }
        }
    };
    // Every declared function the code reaches, each with the captures it
    // froze as constants ahead of its body.
    while let Some(name) = pending.pop() {
        if units.contains_key(&name) {
            continue;
        }
        let code = document
            .functions
            .get(&name)
            .ok_or_else(|| unreadable("its code names a function the document does not declare"))?
            .clone();
        let reads = CodeReads::of(&code.params, &code.body);
        pending.extend(reads.declared.iter().cloned());
        let mut captures = BTreeMap::new();
        for free in &reads.free {
            let Some(value) = heap.unit_binding(&Unit::Function(name.clone()), free)? else {
                continue;
            };
            let (constant, references) = heap.freeze(&value, &named)?;
            pending.extend(references);
            captures.insert(free.clone(), constant);
        }
        origins.insert(
            name.clone(),
            (
                crate::Site::new(Unit::Function(name.clone()), []),
                captures.len(),
            ),
        );
        units.insert(name, with_prelude(code, captures));
    }
    let mut value = Document::new(document.manifest.numbers, Vec::new());
    let mut library: BTreeSet<FunctionId> = entry_definition
        .convention
        .functions()
        .into_iter()
        .copied()
        .collect();
    for unit in units.values() {
        let reads = CodeReads::of(&unit.params, &unit.body);
        for effect in reads.effects {
            let signature = document
                .manifest
                .effects
                .get(&effect)
                .ok_or_else(|| unreadable("its effect is not listed"))?;
            value.manifest.effects.insert(effect, signature.clone());
        }
        library.extend(reads.library);
        if let Some(start) = &unit.start {
            library.extend(start.functions());
        }
    }
    value.manifest.functions = library_closure(document, crossing.catalog, library)?;
    // What the bodies it reaches perform, it requires through them
    // (`K-DOC-002`).
    let through = crate::performed_by(crossing.catalog, value.manifest.functions.keys().copied());
    for (effect, bodies) in through {
        if let Some(requirement) = bodies.into_values().next() {
            value
                .manifest
                .effects
                .entry(effect)
                .or_insert_with(|| requirement.clone());
        }
    }
    value.functions = units;
    let public = Name::new(crate::FUNCTION_VALUE_ENTRY);
    let names = super::canon::canonical(&mut value, &entry, &public);
    let entry = public;
    value.entries.insert(entry.clone(), entry_definition);
    let mut entries = Vec::new();
    for (name, (body, prelude)) in origins {
        let Some(name) = names.get(&name) else {
            continue;
        };
        for caption in crossing.captions {
            let Some(site) = caption.target.site() else {
                continue;
            };
            if site.unit != body.unit {
                continue;
            }
            let Some(relative) = site.path.strip_prefix(body.path.as_slice()) else {
                continue;
            };
            let mut path = relative.to_vec();
            if let Some(statement) = path.first_mut() {
                *statement += prelude as u32;
            }
            entries.push(crate::CaptionEntry::site(
                crate::Site::new(Unit::Function(name.clone()), path),
                caption.caption.clone(),
            ));
        }
    }
    let captions =
        crate::Captions::new(&value, entries).map_err(|error| unreadable(&error.to_string()))?;
    let value = FunctionValue {
        document: value,
        entry,
        captions,
    };
    let bytes = value
        .counted_bytes()
        .map_err(|error| unreadable(&error.message))?;
    if bytes > crossing.limits.max_canonical_bytes {
        return Err(SnapshotRefusal::TooLarge {
            bytes,
            limit: crossing.limits.max_canonical_bytes,
        }
        .into());
    }
    crate::validate_document_with_limits(&value.document, crossing.catalog, crossing.data)
        .map_err(|error| unreadable(&error.to_string()))?;
    Ok(value)
}

/// `direct` and every library function their bodies reach, each with the
/// name its definition carries.
fn library_closure(
    document: &Document,
    catalog: &dyn FunctionCatalog,
    direct: BTreeSet<FunctionId>,
) -> Result<BTreeMap<FunctionId, crate::FunctionName>, SnapshotError> {
    let mut listed = BTreeMap::new();
    let mut pending: Vec<FunctionId> = direct.into_iter().collect();
    while let Some(function) = pending.pop() {
        if listed.contains_key(&function) {
            continue;
        }
        let name = match (
            document.manifest.functions.get(&function),
            catalog.definition(&function),
        ) {
            (Some(name), _) => name.clone(),
            (None, Some(definition)) => definition.name.clone(),
            (None, None) => return Err(unreadable("its library function is not listed")),
        };
        if let Some(body) = catalog.definition(&function).and_then(|d| d.body()) {
            pending.extend(body.functions.keys().copied());
        }
        listed.insert(function, name);
    }
    Ok(listed)
}

/// `function` with each capture bound to its constant ahead of its body.
fn with_prelude(function: Function, captures: BTreeMap<Name, Expr>) -> Function {
    let mut body = Vec::with_capacity(captures.len() + function.body.len());
    for (name, constant) in captures {
        body.push(Stmt::Let {
            name,
            value: Rhs::Expr(constant),
        });
    }
    body.extend(function.body);
    Function {
        params: function.params,
        body,
        start: function.start,
    }
}

/// The entry that starts the declared function `root` as `start` says.
fn entry_wrapper(start: &StartAdapter, root: &Name) -> Function {
    let source_params: Vec<Name> = start
        .signature
        .params
        .iter()
        .map(|p| p.name.clone())
        .collect();
    let local = |base: &str| {
        let mut name = Name::new(base);
        while source_params.contains(&name) {
            name = Name::new(format!("_{}", name.as_str()));
        }
        name
    };
    let returned = local("returned");
    let call = |args| {
        Rhs::Action(Action::Call {
            callee: Callee::Declared(root.clone()),
            args,
        })
    };
    let (params, mut body, args) = match &start.convention {
        // The cleanup is the extracted entry's own convention.
        StartConvention::Direct | StartConvention::PyDirect { .. } => (
            source_params.clone(),
            Vec::new(),
            source_params.iter().cloned().map(Atom::Variable).collect(),
        ),
        StartConvention::TsDirect { .. } => {
            let args = local("args");
            let body = vec![Stmt::Let {
                name: args.clone(),
                value: Rhs::Expr(Expr::List(
                    source_params.iter().cloned().map(Expr::Variable).collect(),
                )),
            }];
            (
                source_params.clone(),
                body,
                vec![Atom::Literal(Literal::Absent), Atom::Variable(args)],
            )
        }
        StartConvention::TsArgsList { .. } => {
            let this = local("this");
            let args = local("args");
            (
                vec![this.clone(), args.clone()],
                Vec::new(),
                vec![Atom::Variable(this), Atom::Variable(args)],
            )
        }
    };
    body.push(Stmt::Let {
        name: returned.clone(),
        value: call(args),
    });
    let value = match &start.convention {
        StartConvention::Direct | StartConvention::PyDirect { .. } => returned,
        StartConvention::TsDirect { await_fn } | StartConvention::TsArgsList { await_fn, .. } => {
            let value = local("value");
            body.push(Stmt::Let {
                name: value.clone(),
                value: Rhs::Action(Action::Await {
                    policy: *await_fn,
                    value: Atom::Variable(returned),
                }),
            });
            value
        }
    };
    body.push(Stmt::Return {
        value: Expr::Variable(value),
    });
    Function {
        params,
        body,
        start: None,
    }
}

/// The constant `value` is written as, spending its encoded size from
/// `budget`: each closure of `named` and each function reference as a
/// reference to the declared function of its name, which it also answers.
///
/// # Errors
///
/// The heap's fault; [`SnapshotRefusal::TooLarge`] once `budget` is spent;
/// and an unreadable value for anything [`super::validate_capture`] refuses.
pub fn constant_of(
    heap: &dyn PageReader,
    value: &PageValue,
    named: &BTreeMap<EntityId, Name>,
    budget: &mut Budget,
) -> Result<(Expr, BTreeSet<Name>), SnapshotError> {
    let mut references = BTreeSet::new();
    let expr = constant(heap, value, named, budget, &mut references)?;
    Ok((expr, references))
}

/// What a crossing may still write.
#[derive(Clone, Copy, Debug)]
pub struct Budget {
    spent: u64,
    limit: u64,
}

impl Budget {
    #[must_use]
    pub const fn new(limit: u64) -> Self {
        Self { spent: 0, limit }
    }

    fn spend(&mut self, bytes: u64) -> Result<(), SnapshotError> {
        self.spent = self.spent.saturating_add(bytes);
        if self.spent > self.limit {
            return Err(SnapshotRefusal::TooLarge {
                bytes: self.spent,
                limit: self.limit,
            }
            .into());
        }
        Ok(())
    }
}

/// The bytes one constant node takes at least, beside its payload.
const NODE_BYTES: u64 = 8;

fn constant(
    heap: &dyn PageReader,
    value: &PageValue,
    named: &BTreeMap<EntityId, Name>,
    budget: &mut Budget,
    references: &mut BTreeSet<Name>,
) -> Result<Expr, SnapshotError> {
    budget.spend(NODE_BYTES)?;
    Ok(match value {
        PageValue::Null => Expr::Literal(Literal::Null),
        PageValue::Absent => Expr::Literal(Literal::Absent),
        PageValue::Bool(flag) => Expr::Literal(Literal::Bool(*flag)),
        PageValue::Float(float) => Expr::Literal(Literal::Float(*float)),
        PageValue::Int(IntRef::Inline(integer)) => Expr::Literal(Literal::Int(integer.clone())),
        PageValue::Int(int @ IntRef::Paged { summary, .. }) => {
            let mut bytes = Vec::new();
            heap.payload_visit(
                &PageValue::Int(int.clone()),
                0..summary.bits.div_ceil(8),
                &mut |chunk| {
                    if let Chunk::Bytes(b) = chunk {
                        bytes.extend_from_slice(b);
                    }
                    ControlFlow::Continue(())
                },
            )?;
            budget.spend(bytes.len() as u64)?;
            Expr::Literal(Literal::Int(crate::heap_values::integer_from_digits(
                summary.negative,
                &bytes,
            )))
        }
        PageValue::Text(text) => {
            budget.spend(text.summary.utf8_bytes)?;
            Expr::Literal(Literal::Text(text_of(heap, text)?))
        }
        PageValue::Bytes(bytes) => {
            budget.spend(bytes.len.saturating_mul(2))?;
            let mut out = Vec::new();
            heap.payload_visit(value, 0..bytes.len, &mut |chunk| {
                if let Chunk::Bytes(b) = chunk {
                    out.extend_from_slice(b);
                }
                ControlFlow::Continue(())
            })?;
            Expr::Literal(Literal::Bytes(crate::Bytes::new(out)))
        }
        PageValue::Function(name) => {
            let name = Name::new(text_of(heap, name)?);
            references.insert(name.clone());
            Expr::Literal(Literal::Function(name))
        }
        PageValue::Closure(id) => {
            let name = named
                .get(id)
                .ok_or_else(|| unreadable("a captured closure is bound to no name"))?;
            references.insert(name.clone());
            Expr::Literal(Literal::Function(name.clone()))
        }
        PageValue::Tuple(tuple) => {
            let mut items = Vec::new();
            for member in members(heap, tuple)? {
                items.push(constant(heap, &member, named, budget, references)?);
            }
            Expr::Tuple(items)
        }
        PageValue::List(id) | PageValue::Set(id) | PageValue::Map(id) | PageValue::Record(id) => {
            let mut items = Vec::new();
            let mut entries = Vec::new();
            let mut fields = Vec::new();
            let mut failed = None;
            heap.visit(*id, &mut |element| {
                let done = match element {
                    Element::Item(item) => {
                        constant(heap, item, named, budget, references).map(|e| items.push(e))
                    }
                    Element::Entry { key, value } => constant(heap, key, named, budget, references)
                        .and_then(|key| {
                            constant(heap, value, named, budget, references)
                                .map(|value| entries.push(MapEntry { key, value }))
                        }),
                    Element::Field { name, value } => text_of(heap, name)
                        .map_err(SnapshotError::from)
                        .and_then(|field| {
                            budget.spend(field.len() as u64)?;
                            constant(heap, value, named, budget, references)
                                .map(|value| fields.push(RecordEntry { field, value }))
                        }),
                };
                match done {
                    Ok(()) => ControlFlow::Continue(()),
                    Err(error) => {
                        failed = Some(error);
                        ControlFlow::Break(())
                    }
                }
            })?;
            if let Some(error) = failed {
                return Err(error);
            }
            match value {
                PageValue::List(_) => Expr::List(items),
                PageValue::Set(_) => Expr::Set(items),
                PageValue::Map(_) => Expr::Map(entries),
                _ => Expr::Record(fields),
            }
        }
        PageValue::Timestamp(_)
        | PageValue::Error(_)
        | PageValue::Handle(_)
        | PageValue::Ref(_)
        | PageValue::Task(_) => {
            return Err(unreadable("a capture with no constant was not refused"));
        }
    })
}
