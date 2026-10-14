//! Self-contained saved code with immutable page-backed captures.
use crate::FunctionValues;
use lash_kernel_doc::heap::*;
use lash_kernel_doc::{
    Action, Annotations, Atom, Callee, Document, EffectName, Expr, Function, FunctionId, Literal,
    Member, Name, Node, Place, Rhs, Signature, Site, Stmt, Unit, ValueKind,
};
use lash_kernel_state::paged::{
    ClosureView, FrozenRef, HeapLifecycle, HeapWriteError, PagedHeap, SemanticHeap,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::ops::ControlFlow;
mod graph;
#[cfg(test)]
mod laws;
pub use graph::{
    Bundle, BundleBody, BundleRefusal, DOCUMENT_NODE_KIND, FUNCTION_BUNDLE_NODE_KIND, MemberBody,
};

#[derive(Clone, Debug)]
pub struct SavedFunction {
    pub name: Name,
    pub document: Document,
    pub captures: BTreeMap<Name, FrozenRef>,
    pub release: PageRef,
    pub helper_owners: BTreeSet<Name>,
    pub references: BTreeSet<Name>,
    pub written: Option<Written>,
    pub token: Option<Token>,
}
#[derive(Clone, Debug)]
pub struct Token {
    pub tag: String,
    pub before: Vec<FrozenRef>,
    pub after: Vec<FrozenRef>,
}
#[derive(Clone, Debug)]
pub struct Kept {
    pub function: SavedFunction,
    pub aliases: BTreeSet<Name>,
}
/// A saved function as its dialect wrote it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Written {
    pub dialect: String,
    /// The function's signature in the dialect's own type syntax.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// The source text of the statement that bound it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The signature a host starts the function under, where every
    /// parameter is one a start can name and type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<Signature>,
    /// Dialect-owned call metadata, interpreted only by that dialect's lowerer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

/// Why a binding that holds a function was not saved.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum NotSaved {
    /// The binding reaches a task: work that is still running, or its
    /// handle.
    #[error("it holds a task or a pending promise, which ends with its cell")]
    Task,
    /// The binding is data that holds a function somewhere inside it.
    #[error(
        "it holds a function inside other data; only a function bound to a name of its own is kept"
    )]
    FunctionInData,
    /// The function was made inside a library function's body.
    #[error("its function was made by a library function, whose code is not the cell's")]
    MadeByLibrary,
    /// A variable the function reads cannot be frozen.
    #[error("its function reads `{name}`, {why}")]
    Capture { name: Name, why: CaptureRefusal },
    /// The function calls another by name that was itself not saved.
    #[error("its function uses `{function}`, which is not a saved function")]
    Needs { function: Name },
    /// The token the binding held its function in holds data about it that
    /// cannot be frozen as eligible capture data.
    #[error("its function's value holds data about the function that is no constant")]
    Token,
    /// The function performs an effect whose call ends the turn. Only a
    /// cell's `main` ends its turn, so no function is kept that would.
    #[error(
        "its function calls `{effect}`, which ends the turn; return the value and call `{effect}` from the cell's top level"
    )]
    PerformsControl { effect: EffectName },
    /// The session's dialect declares no saved function in a later cell.
    #[error("it holds a function, and this session's dialect keeps none between cells")]
    Dialect,
    /// The function was saved under an earlier kernel version, whose
    /// migration does not carry it to this one.
    #[error(
        "its function was saved under kernel version {from} and is not carried forward: {problem}"
    )]
    NotMigrated { from: u32, problem: String },
    /// The run's end did not describe the closure as its document has it.
    #[error("its function cannot be read back from the cell's document: {problem}")]
    Unreadable { problem: String },
}

/// Why one captured variable cannot be frozen.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum CaptureRefusal {
    #[error("which holds a task or a pending promise")]
    Task,
    #[error("which holds a function that is bound to no name of its own")]
    Function,
    #[error("which holds data that contains itself")]
    Cyclic,
    /// Two of the functions it holds read different variables of this
    /// name.
    #[error("which names two different variables among the functions it holds")]
    Conflict,
    #[error("which holds a {kind:?} value, and no constant is written for one")]
    NoConstant { kind: ValueKind },
}

/// The terminal cell's root values, read directly from its paged heap.
#[derive(Clone, Copy)]
pub struct Left<'a> {
    pub document: &'a Document,
    pub bindings: &'a BTreeMap<Name, Value>,
    pub not_carried: &'a [Name],
    pub controls: &'a BTreeMap<EffectName, BTreeSet<crate::EffectControl>>,
    pub annotations: Option<&'a Annotations>,
    pub values: &'a FunctionValues,
    pub release: PageRef,
}
#[derive(Debug, thiserror::Error)]
enum SaveError {
    #[error(transparent)]
    Heap(#[from] HeapFault),
    #[error(transparent)]
    Refused(#[from] NotSaved),
}
fn unreadable(problem: &str) -> SaveError {
    NotSaved::Unreadable {
        problem: problem.into(),
    }
    .into()
}
fn write_fault(error: HeapWriteError) -> SaveError {
    match error {
        HeapWriteError::Fault(error) => error.into(),
        other => {
            HeapFault::Malformed(format!("validated capture failed to freeze: {other}")).into()
        }
    }
}
fn members(heap: &PagedHeap, tuple: &TupleRef) -> Result<Vec<Value>, HeapFault> {
    let mut values = Vec::new();
    heap.tuple_visit(tuple, &mut |v| {
        values.push(v.clone());
        ControlFlow::Continue(())
    })?;
    Ok(values)
}
fn text_is(heap: &PagedHeap, value: &Value, text: &str) -> Result<bool, HeapFault> {
    let Value::Text(t) = value else {
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
fn token_closure(
    heap: &PagedHeap,
    value: &Value,
    tag: Option<&str>,
) -> Result<Option<(EntityId, usize)>, HeapFault> {
    let (Some(tag), Value::Tuple(t)) = (tag, value) else {
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
        if let Value::Closure(id) = value {
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
fn closure_held(
    heap: &PagedHeap,
    value: &Value,
    tag: Option<&str>,
) -> Result<Option<EntityId>, HeapFault> {
    match value {
        Value::Closure(id) => Ok(Some(*id)),
        _ => Ok(token_closure(heap, value, tag)?.map(|(id, _)| id)),
    }
}
pub fn function_reference(
    heap: &PagedHeap,
    value: &Value,
    values: &FunctionValues,
) -> Result<Option<Name>, HeapFault> {
    if let Value::Function(name) = value {
        return Ok(Some(name.clone()));
    }
    let (Some(tag), Value::Tuple(t)) = (values.tag(), value) else {
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
    let mut names = values.iter().filter_map(|v| {
        if let Value::Function(name) = v {
            Some(name.clone())
        } else {
            None
        }
    });
    let first = names.next();
    Ok(if names.next().is_none() { first } else { None })
}
pub fn token_of(
    heap: &mut PagedHeap,
    value: &Value,
    values: &FunctionValues,
) -> Result<Option<Token>, HeapFault> {
    let (Some(tag), Value::Tuple(t)) = (values.tag(), value) else {
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
    let mut names = values
        .iter()
        .enumerate()
        .filter_map(|(i, v)| matches!(v, Value::Function(_)).then_some(i));
    let (Some(at), None) = (names.next(), names.next()) else {
        return Ok(None);
    };
    freeze_token(heap, &values, at, tag, &BTreeMap::new())
        .map(Some)
        .map_err(|error| match error {
            SaveError::Heap(e) => e,
            SaveError::Refused(e) => HeapFault::Malformed(e.to_string()),
        })
}
fn freeze_token(
    heap: &mut PagedHeap,
    values: &[Value],
    at: usize,
    tag: &str,
    named: &BTreeMap<EntityId, Name>,
) -> Result<Token, SaveError> {
    let mut each = |values: &[Value]| -> Result<Vec<FrozenRef>, SaveError> {
        values
            .iter()
            .map(|value| {
                validate_capture(heap, value, &BTreeMap::new(), &mut BTreeSet::new())
                    .map_err(|error| capture_error(&Name::new("token"), error))?;
                heap.freeze_named(value, named)
                    .map(|(frozen, _)| frozen)
                    .map_err(write_fault)
            })
            .collect()
    };
    Ok(Token {
        tag: tag.into(),
        before: each(&values[1..at])?,
        after: each(&values[at + 1..])?,
    })
}
/// The annotation key under which a dialect states, on the statement that
/// binds a function, what the source wrote of it: an object with the
/// function's `signature` as text and, where a host may start it, the
/// kernel signature it is started under as `start`.
pub const WRITTEN: &str = "function";

/// The annotation key of a statement's byte span in the source: `[start,
/// end]`.
const SPAN: &str = "span";

/// What the annotations state of the function the closure at `site` is.
fn written(annotations: &Annotations, site: &Site) -> Option<Written> {
    // The closure is the right-hand side of the statement that binds it.
    let (_, statement) = site.path.split_last()?;
    let node = annotations
        .nodes
        .iter()
        .find(|node| node.site.unit == site.unit && node.site.path == statement)?;
    let stated = node.data.get(WRITTEN);
    let source = node.data.get(SPAN).and_then(|span| {
        let start = usize::try_from(span.get(0)?.as_u64()?).ok()?;
        let end = usize::try_from(span.get(1)?.as_u64()?).ok()?;
        annotations.source.as_deref()?.get(start..end)
    });
    Some(Written {
        dialect: annotations.dialect.clone()?,
        signature: stated
            .and_then(|stated| stated.get("signature"))
            .and_then(|signature| signature.as_str())
            .map(str::to_owned),
        source: source.map(str::to_owned),
        metadata: stated.and_then(|stated| stated.get("metadata")).cloned(),
        start: stated
            .and_then(|stated| stated.get("start"))
            .and_then(|start| serde_json::from_value(start.clone()).ok()),
    })
}

/// What a body reads of the world outside it.
#[derive(Default)]
struct Reads {
    /// The variables it names and does not declare.
    free: BTreeSet<Name>,
    effects: BTreeSet<EffectName>,
    library: BTreeSet<FunctionId>,
    /// The declared functions it names.
    declared: BTreeSet<Name>,
}

impl Reads {
    fn name(&mut self, name: &Name, bound: &[Name]) {
        if !bound.contains(name) {
            self.free.insert(name.clone());
        }
    }

    /// Walks a block in a scope of its own: what it declares ends with it.
    fn block(&mut self, block: &[Stmt], bound: &mut Vec<Name>) {
        let outer = bound.len();
        for stmt in block {
            self.stmt(stmt, bound);
        }
        bound.truncate(outer);
    }

    fn scoped(&mut self, binding: &Name, block: &[Stmt], bound: &mut Vec<Name>) {
        bound.push(binding.clone());
        self.block(block, bound);
        bound.pop();
    }

    fn stmt(&mut self, stmt: &Stmt, bound: &mut Vec<Name>) {
        match stmt {
            Stmt::Let { name, value } => {
                // The right-hand side does not see the variable it binds.
                self.rhs(value, bound);
                bound.push(name.clone());
            }
            Stmt::Assign { place, value } => {
                match place {
                    Place::Variable(name) => self.name(name, bound),
                    Place::Member(member) => self.member(member, bound),
                }
                self.rhs(value, bound);
            }
            Stmt::Remove { member } => self.member(member, bound),
            Stmt::Do { action } => self.action(action, bound),
            Stmt::If {
                condition,
                then_block,
                else_block,
            } => {
                self.expr(condition, bound);
                self.block(then_block, bound);
                self.block(else_block, bound);
            }
            Stmt::For {
                binding,
                iterable,
                body,
            } => {
                self.expr(iterable, bound);
                self.scoped(binding, body, bound);
            }
            Stmt::While { condition, body } => {
                self.expr(condition, bound);
                self.block(body, bound);
            }
            Stmt::Break | Stmt::Continue => {}
            Stmt::Return { value }
            | Stmt::Throw { value }
            | Stmt::Print { value }
            | Stmt::Finish { value }
            | Stmt::Fail { value } => self.expr(value, bound),
            Stmt::Try(scope) => {
                self.block(&scope.body, bound);
                if let Some(catch) = &scope.catch {
                    self.scoped(&catch.binding, &catch.body, bound);
                }
                if let Some(finally) = &scope.finally {
                    self.block(finally, bound);
                }
            }
        }
    }

    fn rhs(&mut self, rhs: &Rhs, bound: &mut Vec<Name>) {
        match rhs {
            Rhs::Expr(expr) => self.expr(expr, bound),
            Rhs::Action(action) => self.action(action, bound),
        }
    }

    fn atom(&mut self, atom: &Atom, bound: &[Name]) {
        match atom {
            Atom::Variable(name) => self.name(name, bound),
            Atom::Literal(Literal::Function(name)) => {
                self.declared.insert(name.clone());
            }
            Atom::Literal(_) => {}
        }
    }

    fn action(&mut self, action: &Action, bound: &[Name]) {
        match action {
            Action::Call { callee, args } | Action::Spawn { callee, args } => {
                match callee {
                    Callee::Declared(name) => {
                        self.declared.insert(name.clone());
                    }
                    Callee::Value(name) => self.name(name, bound),
                    Callee::Library(function) => {
                        self.library.insert(*function);
                    }
                }
                args.iter().for_each(|arg| self.atom(arg, bound));
            }
            Action::Perform { effect, args, .. } => {
                self.effects.insert(effect.clone());
                args.iter().for_each(|arg| self.atom(arg, bound));
            }
            Action::Sleep { duration: atom }
            | Action::Join { task: atom }
            | Action::JoinMany { tasks: atom, .. }
            | Action::Cancel { task: atom } => self.atom(atom, bound),
            Action::Yield => {}
        }
    }

    fn member(&mut self, member: &Member, bound: &mut Vec<Name>) {
        match member {
            Member::Field { target, .. } => self.expr(target, bound),
            Member::Index { target, index } => {
                self.expr(target, bound);
                self.expr(index, bound);
            }
        }
    }

    fn expr(&mut self, expr: &Expr, bound: &mut Vec<Name>) {
        match expr {
            Expr::Literal(Literal::Function(name)) => {
                self.declared.insert(name.clone());
            }
            Expr::Literal(_) | Expr::Clock | Expr::Random => {}
            Expr::Variable(name) => self.name(name, bound),
            Expr::Tuple(items) | Expr::List(items) | Expr::Set(items) => {
                items.iter().for_each(|item| self.expr(item, bound));
            }
            Expr::Map(entries) => entries.iter().for_each(|entry| {
                self.expr(&entry.key, bound);
                self.expr(&entry.value, bound);
            }),
            Expr::Record(entries) => entries
                .iter()
                .for_each(|entry| self.expr(&entry.value, bound)),
            Expr::Member(member) => self.member(member, bound),
            Expr::Closure(closure) => {
                let outer = bound.len();
                bound.extend(closure.params.iter().cloned());
                self.block(&closure.body, bound);
                bound.truncate(outer);
            }
            Expr::Call { function, args } => {
                self.library.insert(*function);
                args.iter().for_each(|arg| self.expr(arg, bound));
            }
            Expr::Read(read) => {
                self.expr(&read.handle, bound);
                self.expr(&read.request, bound);
            }
        }
    }
}

/// Why a saved function cannot be used in an environment.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Unusable {
    /// It, or one it uses, names a function the session does not hold.
    #[error("the saved function `{function}` uses `{needs}`, which is not a saved function here")]
    Missing { function: Name, needs: Name },
    /// It performs an effect the environment does not offer.
    #[error("the saved function `{function}` calls `{effect}`, which this session does not offer")]
    EffectMissing { function: Name, effect: EffectName },
    /// The environment offers the effect under another signature.
    #[error(
        "the saved function `{function}` calls `{effect}` under a signature this session does not offer it under"
    )]
    EffectSignature { function: Name, effect: EffectName },
    /// The effect's call now ends the turn. A saved function was kept
    /// because none of its effects did (`NotSaved::PerformsControl`), so
    /// what the effect declares has changed since.
    #[error(
        "the saved function `{function}` calls `{effect}`, which now ends the turn; a function cannot end it"
    )]
    EffectControls { function: Name, effect: EffectName },
    /// It calls a library function the environment has not installed.
    #[error(
        "the saved function `{function}` needs the library function `{library}`, which is not installed"
    )]
    LibraryMissing { function: Name, library: String },
    /// It was written under another number policy than the document's.
    #[error("the saved function `{function}` decodes numbers by another policy than this session")]
    Numbers { function: Name },
    /// The document already declares another function under its name.
    #[error("the document already declares a function `{function}`")]
    Declared { function: Name },
    /// A capture has no constant form (a stored value this build did not
    /// write).
    #[error(
        "the saved function `{function}` holds a capture `{capture}` no constant is written for"
    )]
    Capture { function: Name, capture: Name },
    /// It was held in a function value this dialect does not make: a token
    /// in a dialect whose function values are bare, or one of another tag.
    #[error("the saved function `{function}` is held in a value this dialect does not make")]
    Token { function: Name },
}

#[derive(Debug)]
enum CaptureError {
    Heap(HeapFault),
    Refused(CaptureRefusal),
}
impl From<HeapFault> for CaptureError {
    fn from(e: HeapFault) -> Self {
        Self::Heap(e)
    }
}
fn capture_error(name: &Name, error: CaptureError) -> SaveError {
    match error {
        CaptureError::Heap(e) => e.into(),
        CaptureError::Refused(why) => NotSaved::Capture {
            name: name.clone(),
            why,
        }
        .into(),
    }
}
fn validate_capture(
    heap: &PagedHeap,
    value: &Value,
    named: &BTreeMap<EntityId, Name>,
    active: &mut BTreeSet<EntityId>,
) -> Result<(), CaptureError> {
    match value {
        Value::Closure(id) if !named.contains_key(id) => {
            Err(CaptureError::Refused(CaptureRefusal::Function))
        }
        Value::Task(_) | Value::Ref(Identity::Task(_)) => {
            Err(CaptureError::Refused(CaptureRefusal::Task))
        }
        Value::Timestamp(_) | Value::Error(_) | Value::Handle(_) | Value::Ref(_) => {
            Err(CaptureError::Refused(CaptureRefusal::NoConstant {
                kind: kind(value),
            }))
        }
        Value::Tuple(t) => {
            for value in members(heap, t)? {
                validate_capture(heap, &value, named, active)?;
            }
            Ok(())
        }
        Value::List(id) | Value::Map(id) | Value::Set(id) | Value::Record(id) => {
            if !active.insert(*id) {
                return Err(CaptureError::Refused(CaptureRefusal::Cyclic));
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
fn kind(value: &Value) -> ValueKind {
    match value {
        Value::Timestamp(_) => ValueKind::Timestamp,
        Value::Error(_) => ValueKind::Error,
        Value::Handle(_) => ValueKind::Handle,
        Value::Ref(_) => ValueKind::Ref,
        _ => ValueKind::Absent,
    }
}
fn closure_at<'a>(body: &'a [Stmt], site: &Site) -> Option<&'a lash_kernel_doc::Closure> {
    let mut node = Node::Block(body);
    for step in &site.path {
        node = node.children().into_iter().nth(*step as usize)?;
    }
    match node {
        Node::Expr(Expr::Closure(closure)) => Some(closure),
        _ => None,
    }
}
fn closure_code<'a>(
    left: &Left<'a>,
    object: &ClosureView,
) -> Result<&'a lash_kernel_doc::Closure, SaveError> {
    let body = match &object.site.unit {
        Unit::Main => &left.document.main,
        Unit::Function(name) => {
            &left
                .document
                .functions
                .get(name)
                .ok_or_else(|| unreadable("its unit is not declared"))?
                .body
        }
        Unit::Library(_) => return Err(NotSaved::MadeByLibrary.into()),
    };
    closure_at(body, &object.site).ok_or_else(|| unreadable("its site is not a closure expression"))
}
fn captured_value(
    heap: &PagedHeap,
    left: &Left<'_>,
    object: &ClosureView,
    free: &Name,
) -> Result<Option<Value>, SaveError> {
    if let Some((_, cell)) = object.captures.iter().find(|(name, _)| name == free) {
        return heap.variable_get(*cell).map(Some).map_err(Into::into);
    }
    if object.site.unit == Unit::Main {
        return Ok(left.bindings.get(free).cloned());
    }
    Ok(None)
}
enum Held {
    Data(FrozenRef, Box<Value>),
    Helper(EntityId),
}
struct Helper {
    name: Name,
    code: lash_kernel_doc::Closure,
    token: Option<Token>,
}
struct Frozen<'a, 'h> {
    left: &'a Left<'a>,
    heap: &'h mut PagedHeap,
    named: &'a BTreeMap<EntityId, Name>,
    reads: Reads,
    bound: BTreeMap<Name, Held>,
    helpers: Vec<Helper>,
    helper_owners: &'a BTreeMap<EntityId, BTreeSet<Name>>,
    owners: BTreeSet<Name>,
    references: BTreeSet<Name>,
}
impl<'a> Frozen<'a, '_> {
    fn absorb(&mut self, object: &ClosureView) -> Result<&'a lash_kernel_doc::Closure, SaveError> {
        let closure = closure_code(self.left, object)?;
        let mut reads = Reads::default();
        reads.block(&closure.body, &mut closure.params.clone());
        self.reads.effects.extend(reads.effects);
        self.reads.library.extend(reads.library);
        self.references.extend(reads.declared);
        for free in &reads.free {
            let Some(value) = captured_value(self.heap, self.left, object, free)? else {
                continue;
            };
            if let Some(helper) = closure_held(self.heap, &value, self.left.values.tag())?
                && !self.named.contains_key(&helper)
            {
                if let Some(owners) = self.helper_owners.get(&helper) {
                    self.owners.extend(owners.iter().cloned());
                }
                match self.bound.get(free) {
                    Some(Held::Helper(id)) if *id == helper => continue,
                    Some(_) => {
                        return Err(capture_error(
                            free,
                            CaptureError::Refused(CaptureRefusal::Conflict),
                        ));
                    }
                    None => (),
                }
                let inner = self.heap.closure(helper)?;
                let token = match (
                    &value,
                    token_closure(self.heap, &value, self.left.values.tag())?,
                ) {
                    (Value::Tuple(t), Some((_, at))) => {
                        let values = members(self.heap, t)?;
                        Some(freeze_token(
                            self.heap,
                            &values,
                            at,
                            self.left.values.tag().unwrap_or_default(),
                            self.named,
                        )?)
                    }
                    _ => None,
                };
                self.bound.insert(free.clone(), Held::Helper(helper));
                let code = self.absorb(&inner)?.clone();
                self.helpers.push(Helper {
                    name: free.clone(),
                    code,
                    token,
                });
                continue;
            }
            if let Some(Held::Data(_, held)) = self.bound.get(free)
                && self.heap.equal(held, &value)?
            {
                continue;
            }
            validate_capture(self.heap, &value, self.named, &mut BTreeSet::new())
                .map_err(|e| capture_error(free, e))?;
            let (frozen, functions) = self
                .heap
                .freeze_named(&value, self.named)
                .map_err(write_fault)?;
            self.references.extend(functions);
            match self.bound.get(free) {
                Some(Held::Data(held, _)) if held.root.id == frozen.root.id => (),
                Some(_) => {
                    return Err(capture_error(
                        free,
                        CaptureError::Refused(CaptureRefusal::Conflict),
                    ));
                }
                None => {
                    self.bound
                        .insert(free.clone(), Held::Data(frozen, Box::new(value)));
                }
            }
        }
        Ok(closure)
    }
}
fn save_closure(
    heap: &mut PagedHeap,
    left: &Left<'_>,
    name: &Name,
    id: EntityId,
    named: &BTreeMap<EntityId, Name>,
    helper_owners: &BTreeMap<EntityId, BTreeSet<Name>>,
) -> Result<SavedFunction, SaveError> {
    let object = heap.closure(id)?;
    let mut frozen = Frozen {
        left,
        heap,
        named,
        reads: Reads::default(),
        bound: BTreeMap::new(),
        helpers: Vec::new(),
        helper_owners,
        owners: BTreeSet::new(),
        references: BTreeSet::new(),
    };
    let closure = frozen.absorb(&object)?.clone();
    let mut document = Document::new(left.document.manifest.numbers, Vec::new());
    if let Some(effect) = frozen
        .reads
        .effects
        .iter()
        .find(|effect| left.controls.contains_key(*effect))
    {
        return Err(NotSaved::PerformsControl {
            effect: effect.clone(),
        }
        .into());
    }
    for effect in &frozen.reads.effects {
        document.manifest.effects.insert(
            effect.clone(),
            left.document
                .manifest
                .effects
                .get(effect)
                .ok_or_else(|| unreadable("its effect is not listed"))?
                .clone(),
        );
    }
    for function in &frozen.reads.library {
        document.manifest.functions.insert(
            *function,
            left.document
                .manifest
                .functions
                .get(function)
                .ok_or_else(|| unreadable("its library function is not listed"))?
                .clone(),
        );
    }
    let mut body = Vec::new();
    for helper in &frozen.helpers {
        body.push(Stmt::Let {
            name: helper.name.clone(),
            value: Rhs::Expr(Expr::Literal(Literal::Absent)),
        });
    }
    // Token members stay captures, not AST constants. A private helper's code
    // refers to private capture names chosen outside the document's names.
    let mut captures: BTreeMap<Name, FrozenRef> = frozen
        .bound
        .into_iter()
        .filter_map(|(name, held)| {
            if let Held::Data(value, _) = held {
                Some((name, value))
            } else {
                None
            }
        })
        .collect();
    for (index, helper) in frozen.helpers.into_iter().enumerate() {
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
                        || left.bindings.contains_key(&capture)
                        || reads_name(&closure, &capture)
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
        });
    }
    body.extend(closure.body);
    document.functions.insert(
        name.clone(),
        Function {
            params: closure.params,
            body,
        },
    );
    frozen.owners.remove(name);
    Ok(SavedFunction {
        name: name.clone(),
        document,
        captures,
        release: left.release,
        helper_owners: frozen.owners,
        references: frozen.references,
        written: left.annotations.and_then(|a| written(a, &object.site)),
        token: None,
    })
}
fn reads_name(closure: &lash_kernel_doc::Closure, name: &Name) -> bool {
    let mut reads = Reads::default();
    reads.block(&closure.body, &mut closure.params.clone());
    reads.free.contains(name)
}
/// The qualifying functions and the typed refusals of the other bindings.
pub type SavedBindings = (BTreeMap<Name, Kept>, BTreeMap<Name, NotSaved>);

/// Qualifies closures against semantic names and freezes their captures once.
pub fn save(
    heap: &mut PagedHeap,
    left: Left<'_>,
    held: &BTreeSet<Name>,
) -> Result<SavedBindings, HeapFault> {
    let mut named = BTreeMap::new();
    for (name, value) in left.bindings {
        if let Some(id) = closure_held(heap, value, left.values.tag())? {
            named.entry(id).or_insert_with(|| name.clone());
        }
    }
    let mut helper_owners: BTreeMap<EntityId, BTreeSet<Name>> = BTreeMap::new();
    for (id, owner) in &named {
        let object = heap.closure(*id)?;
        let closure = match closure_code(&left, &object) {
            Ok(c) => c,
            Err(SaveError::Refused(_)) => continue,
            Err(SaveError::Heap(e)) => return Err(e),
        };
        let [
            ..,
            Stmt::Let {
                name: result,
                value:
                    Rhs::Action(Action::Call {
                        callee: Callee::Value(callee),
                        ..
                    }),
            },
            Stmt::Return {
                value: Expr::Variable(returned),
            },
        ] = closure.body.as_slice()
        else {
            continue;
        };
        if result != returned {
            continue;
        }
        let mut reads = Reads::default();
        reads.block(&closure.body, &mut closure.params.clone());
        if !reads.effects.is_empty()
            || !reads.declared.is_empty()
            || reads.free.len() != 1
            || !reads.free.contains(callee)
        {
            continue;
        }
        for free in &reads.free {
            if let Some(value) =
                captured_value(heap, &left, &object, free).map_err(|e| match e {
                    SaveError::Heap(e) => e,
                    SaveError::Refused(e) => HeapFault::Malformed(e.to_string()),
                })?
                && let Some(helper) = closure_held(heap, &value, left.values.tag())?
                && !named.contains_key(&helper)
            {
                helper_owners
                    .entry(helper)
                    .or_default()
                    .insert(owner.clone());
            }
        }
    }
    let mut saved = BTreeMap::new();
    let mut refused = BTreeMap::new();
    let mut aliases: BTreeMap<Name, BTreeSet<Name>> = BTreeMap::new();
    for name in left.not_carried {
        let Some(value) = left.bindings.get(name) else {
            refused.insert(name.clone(), NotSaved::Task);
            continue;
        };
        let Some(id) = closure_held(heap, value, left.values.tag())? else {
            refused.insert(name.clone(), NotSaved::FunctionInData);
            continue;
        };
        let known = named
            .get(&id)
            .ok_or_else(|| HeapFault::Malformed("named closure is missing".into()))?;
        if name != known {
            aliases
                .entry(known.clone())
                .or_default()
                .insert(name.clone());
            continue;
        }
        let result =
            save_closure(heap, &left, name, id, &named, &helper_owners).and_then(|mut function| {
                if let Some((_, at)) = token_closure(heap, value, left.values.tag())?
                    && let Value::Tuple(t) = value
                {
                    let values = members(heap, t)?;
                    function.token = Some(freeze_token(
                        heap,
                        &values,
                        at,
                        left.values.tag().unwrap_or_default(),
                        &named,
                    )?);
                }
                Ok(function)
            });
        match result {
            Ok(function) => {
                saved.insert(
                    name.clone(),
                    Kept {
                        function,
                        aliases: BTreeSet::new(),
                    },
                );
            }
            Err(SaveError::Refused(why)) => {
                refused.insert(name.clone(), why);
            }
            Err(SaveError::Heap(e)) => return Err(e),
        }
    }
    for (name, names) in aliases {
        if let Some(kept) = saved.get_mut(&name) {
            kept.aliases = names;
        } else if let Some(why) = refused.get(&name).cloned() {
            refused.extend(names.into_iter().map(|n| (n, why.clone())));
        }
    }
    loop {
        let mut missing = None;
        for (name, kept) in &saved {
            for needed in kept.function.needs() {
                let still_held = held.contains(&needed)
                    && left
                        .bindings
                        .get(&needed)
                        .map(|v| function_reference(heap, v, left.values))
                        .transpose()?
                        .flatten()
                        == Some(needed.clone());
                if !saved.contains_key(&needed) && !still_held {
                    missing = Some((name.clone(), needed));
                    break;
                }
            }
            if missing.is_some() {
                break;
            }
        }
        let Some((name, function)) = missing else {
            break;
        };
        if let Some(kept) = saved.remove(&name) {
            for alias in kept.aliases {
                refused.insert(
                    alias,
                    NotSaved::Needs {
                        function: function.clone(),
                    },
                );
            }
        }
        refused.insert(name, NotSaved::Needs { function });
    }
    Ok((saved, refused))
}
impl SavedFunction {
    pub fn definition(&self) -> Option<&Function> {
        self.document.functions.get(&self.name)
    }
    pub fn needs(&self) -> BTreeSet<Name> {
        let mut names: BTreeSet<Name> = self
            .references
            .union(&self.helper_owners)
            .cloned()
            .collect();
        if let Some(code) = self.definition() {
            let mut reads = Reads::default();
            reads.block(&code.body, &mut code.params.clone());
            names.extend(reads.declared);
        }
        names
    }
    pub fn value(&self, heap: &mut PagedHeap) -> Result<Value, HeapWriteError> {
        let function = Value::Function(self.name.clone());
        let Some(token) = &self.token else {
            return Ok(function);
        };
        let mut members = vec![Value::Text(heap.text(&token.tag).map_err(native_write)?)];
        for frozen in &token.before {
            members.push(heap.instantiate(frozen)?);
        }
        members.push(function);
        for frozen in &token.after {
            members.push(heap.instantiate(frozen)?);
        }
        Ok(Value::Tuple(heap.tuple(members).map_err(native_write)?))
    }
    pub fn renamed(&self, name: &Name) -> Self {
        let mut value = self.clone();
        if let Some(code) = value.document.functions.remove(&value.name) {
            value.document.functions.insert(name.clone(), code);
        }
        value.name = name.clone();
        value
    }
    /// Each invocation receives fresh identities over the saved immutable pages.
    pub fn captures_for_call(
        &self,
        heap: &mut PagedHeap,
    ) -> Result<BTreeMap<Name, Value>, HeapWriteError> {
        self.captures
            .iter()
            .map(|(name, frozen)| Ok((name.clone(), heap.instantiate(frozen)?)))
            .collect()
    }
}
pub fn closure_of<'a>(
    functions: &'a BTreeMap<Name, SavedFunction>,
    roots: &BTreeSet<Name>,
) -> Result<BTreeMap<Name, &'a SavedFunction>, Unusable> {
    let mut used = BTreeMap::new();
    let mut pending = roots
        .iter()
        .map(|n| (n.clone(), n.clone()))
        .collect::<Vec<_>>();
    while let Some((by, name)) = pending.pop() {
        let function = functions.get(&name).ok_or(Unusable::Missing {
            function: by,
            needs: name,
        })?;
        if used.contains_key(&function.name) {
            continue;
        }
        pending.extend(
            function
                .needs()
                .into_iter()
                .map(|n| (function.name.clone(), n)),
        );
        used.insert(function.name.clone(), function);
    }
    Ok(used)
}
/// Declarations and their immutable captures, ready for the cell compiler.
#[derive(Clone, Debug)]
pub struct InstalledFunction {
    pub code: Function,
    pub captures: BTreeMap<Name, FrozenRef>,
    pub release: PageRef,
}
pub fn install(
    document: &mut Document,
    functions: &BTreeMap<Name, SavedFunction>,
    roots: &BTreeSet<Name>,
    values: &FunctionValues,
    effects: &BTreeMap<EffectName, Signature>,
    controls: &BTreeMap<EffectName, BTreeSet<crate::EffectControl>>,
    installed: &dyn Fn(&FunctionId) -> bool,
) -> Result<BTreeMap<Name, InstalledFunction>, Unusable> {
    let mut declarations = BTreeMap::new();
    for (name, function) in closure_of(functions, roots)? {
        if function
            .token
            .as_ref()
            .is_some_and(|t| values.tag() != Some(t.tag.as_str()))
        {
            return Err(Unusable::Token { function: name });
        }
        if function.document.manifest.numbers != document.manifest.numbers {
            return Err(Unusable::Numbers { function: name });
        }
        for (effect, signature) in &function.document.manifest.effects {
            if effects.contains_key(effect) && controls.contains_key(effect) {
                return Err(Unusable::EffectControls {
                    function: name,
                    effect: effect.clone(),
                });
            }
            match effects.get(effect) {
                None => {
                    return Err(Unusable::EffectMissing {
                        function: name,
                        effect: effect.clone(),
                    });
                }
                Some(v) if v != signature => {
                    return Err(Unusable::EffectSignature {
                        function: name,
                        effect: effect.clone(),
                    });
                }
                _ => (),
            }
        }
        for (library, listed) in &function.document.manifest.functions {
            if !installed(library) {
                return Err(Unusable::LibraryMissing {
                    function: name,
                    library: listed.to_string(),
                });
            }
        }
        if document.functions.contains_key(&name) {
            return Err(Unusable::Declared { function: name });
        }
        let code = function
            .definition()
            .ok_or_else(|| Unusable::Missing {
                function: name.clone(),
                needs: name.clone(),
            })?
            .clone();
        declarations.insert(
            name,
            InstalledFunction {
                code,
                captures: function.captures.clone(),
                release: function.release,
            },
        );
    }
    for (name, declaration) in &declarations {
        document
            .functions
            .insert(name.clone(), declaration.code.clone());
    }
    for function in closure_of(functions, roots)?.values() {
        document
            .manifest
            .effects
            .extend(function.document.manifest.effects.clone());
        document
            .manifest
            .functions
            .extend(function.document.manifest.functions.clone());
    }
    Ok(declarations)
}

fn native_write(error: NativeError) -> HeapWriteError {
    match error {
        NativeError::Heap(e) => HeapWriteError::Fault(e),
        NativeError::Memory => HeapWriteError::Memory,
        other => HeapWriteError::Fault(HeapFault::Malformed(format!(
            "saved token construction failed: {other}"
        ))),
    }
}
