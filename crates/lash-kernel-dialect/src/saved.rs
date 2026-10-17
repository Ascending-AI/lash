//! Self-contained saved code with immutable page-backed captures.
//!
//! A saved function is cut out of the cell's heap by the root-based
//! snapshot every crossing function is cut by
//! ([`lash_kernel_doc::snapshot`]): this module selects the bindings that
//! qualify and freezes their captures as pages.
use crate::Written;
use lash_kernel_doc::heap::*;
use lash_kernel_doc::snapshot::{
    CodeReads, FrozenToken, SnapshotError, SnapshotHeap, SnapshotRefusal, closure_held,
    freeze_token, gather, members, text_is, text_of, token_closure,
};
use lash_kernel_doc::{
    Action, Callee, Document, EffectName, Expr, Function, FunctionId, FunctionRepresentation, Name,
    Rhs, Signature, Site, Stmt, Unit,
};
use lash_kernel_state::kernel_internals::PagedHeap;
use lash_kernel_state::paged::{FrozenRef, HeapLifecycle, HeapWriteError, SemanticHeap};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
mod graph;
#[cfg(test)]
mod laws;
pub use graph::{
    Bundle, BundleBody, BundleRefusal, DOCUMENT_NODE_KIND, FUNCTION_BUNDLE_NODE_KIND, MemberBody,
};
pub use lash_kernel_doc::snapshot::CaptureRefusal;

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
/// The members of the token a saved function is held in, around its
/// callable, frozen.
#[derive(Clone, Debug)]
pub struct Token {
    pub tag: String,
    pub before: Vec<FrozenRef>,
    pub after: Vec<FrozenRef>,
}
impl Token {
    fn frozen(token: FrozenToken<FrozenRef>) -> Self {
        Self {
            tag: token.tag,
            before: token.before,
            after: token.after,
        }
    }
}
#[derive(Clone, Debug)]
pub struct Kept {
    pub function: SavedFunction,
    pub aliases: BTreeSet<Name>,
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

/// The terminal cell's root values, read directly from its paged heap.
#[derive(Clone, Copy)]
pub struct Left<'a> {
    pub document: &'a Document,
    pub bindings: &'a BTreeMap<Name, PageValue>,
    pub not_carried: &'a [Name],
    pub controls: &'a BTreeMap<EffectName, BTreeSet<crate::EffectControl>>,
    /// What the source wrote of each function the document makes, by the
    /// site of its closure ([`crate::Lowered::written`]).
    pub written: &'a BTreeMap<Site, Written>,
    /// How the dialect holds a function as a value.
    pub values: &'a FunctionRepresentation,
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
impl From<SnapshotError> for SaveError {
    fn from(error: SnapshotError) -> Self {
        match error {
            SnapshotError::Heap(fault) => Self::Heap(fault),
            SnapshotError::Refused(refusal) => Self::Refused(match refusal {
                SnapshotRefusal::Capture { name, why } => NotSaved::Capture { name, why },
                SnapshotRefusal::MadeByLibrary => NotSaved::MadeByLibrary,
                SnapshotRefusal::Token => NotSaved::Token,
                SnapshotRefusal::Unreadable { problem } => NotSaved::Unreadable { problem },
                other @ (SnapshotRefusal::NoStartSignature | SnapshotRefusal::TooLarge { .. }) => {
                    NotSaved::Unreadable {
                        problem: other.to_string(),
                    }
                }
            }),
        }
    }
}
fn write_fault(error: HeapWriteError) -> SnapshotError {
    match error {
        HeapWriteError::Fault(error) => error.into(),
        other => {
            HeapFault::Malformed(format!("validated capture failed to freeze: {other}")).into()
        }
    }
}
/// The cell's heap as a saving reads it: a closure `main` made reads the
/// bindings the cell left, and a capture freezes to pages.
struct Saving<'a, 'h> {
    heap: &'h mut PagedHeap,
    left: &'a Left<'a>,
}
impl SnapshotHeap for Saving<'_, '_> {
    type Frozen = FrozenRef;

    fn pages(&self) -> &dyn PageReader {
        self.heap
    }

    fn closure_site(&self, id: EntityId) -> Result<std::sync::Arc<Site>, HeapFault> {
        self.heap.closure(id)?.site.resolve(self.heap.pages())
    }

    fn closure_capture(
        &mut self,
        id: EntityId,
        name: &Name,
    ) -> Result<Option<PageValue>, HeapFault> {
        let object = self.heap.closure(id)?;
        match object.captures.get(
            &lash_kernel_state::kernel_internals::metadata::metadata_key(name),
            self.heap.pages(),
        )? {
            Some((_, cell)) => self.heap.variable_get(cell).map(Some),
            None => Ok(None),
        }
    }

    fn unit_binding(&mut self, unit: &Unit, name: &Name) -> Result<Option<PageValue>, HeapFault> {
        // The bindings the cell left, whether or not they are among the
        // bindings it drops.
        match unit {
            Unit::Main => match self.left.bindings.get(name) {
                Some(value) => Ok(Some(value.clone())),
                None => self.heap.session_binding(name),
            },
            _ => Ok(None),
        }
    }

    fn freeze(
        &mut self,
        value: &PageValue,
        named: &BTreeMap<EntityId, Name>,
    ) -> Result<(FrozenRef, BTreeSet<Name>), SnapshotError> {
        self.heap.freeze_named(value, named).map_err(write_fault)
    }
}
pub(crate) fn function_reference(
    heap: &PagedHeap,
    value: &PageValue,
    values: &FunctionRepresentation,
) -> Result<Option<Name>, HeapFault> {
    if let PageValue::Function(name) = value {
        return text_of(heap, name).map(|name| Some(Name::new(name)));
    }
    let (Some(tag), PageValue::Tuple(t)) = (values.tag(), value) else {
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
        if let PageValue::Function(name) = v {
            Some(name.clone())
        } else {
            None
        }
    });
    let first = names.next();
    if names.next().is_some() {
        return Ok(None);
    }
    first
        .as_ref()
        .map(|name| text_of(heap, name).map(Name::new))
        .transpose()
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
    /// It was held in a function value this dialect does not make: a token
    /// in a dialect whose function values are bare, or one of another tag.
    #[error("the saved function `{function}` is held in a value this dialect does not make")]
    Token { function: Name },
}

fn save_closure(
    heap: &mut PagedHeap,
    left: &Left<'_>,
    name: &Name,
    id: EntityId,
    named: &BTreeMap<EntityId, Name>,
    helper_owners: &BTreeMap<EntityId, BTreeSet<Name>>,
) -> Result<SavedFunction, SaveError> {
    let mut saving = Saving { heap, left };
    let gathered = gather(
        &mut saving,
        left.document,
        left.values.tag(),
        id,
        named,
        helper_owners,
        &|capture| left.bindings.contains_key(capture),
    )?;
    if let Some(effect) = gathered
        .effects
        .iter()
        .find(|effect| left.controls.contains_key(*effect))
    {
        return Err(NotSaved::PerformsControl {
            effect: effect.clone(),
        }
        .into());
    }
    let mut document = Document::new(left.document.manifest.numbers, Vec::new());
    for effect in &gathered.effects {
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
    // The functions its code calls, and those the start adapters of its
    // function and closures call.
    let mut library = gathered.library.clone();
    each_start(&gathered.function, &mut |start| {
        library.extend(start.functions());
    });
    for function in &library {
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
    document
        .functions
        .insert(name.clone(), gathered.function.clone());
    let mut owners = gathered.owners;
    owners.remove(name);
    Ok(SavedFunction {
        name: name.clone(),
        document,
        captures: gathered.captures,
        release: left.release,
        helper_owners: owners,
        references: gathered.references,
        written: left.written.get(&gathered.site).cloned(),
        token: None,
    })
}
/// Each start adapter `function` and the closures in its body carry.
fn each_start(function: &Function, visit: &mut dyn FnMut(&lash_kernel_doc::StartAdapter)) {
    fn walk(
        node: lash_kernel_doc::Node<'_>,
        visit: &mut dyn FnMut(&lash_kernel_doc::StartAdapter),
    ) {
        if let lash_kernel_doc::Node::Expr(Expr::Closure(closure)) = node
            && let Some(start) = &closure.start
        {
            visit(start);
        }
        for child in node.children() {
            walk(child, visit);
        }
    }
    if let Some(start) = &function.start {
        visit(start);
    }
    walk(lash_kernel_doc::Node::Block(&function.body), visit);
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
        let mut saving = Saving {
            heap: &mut *heap,
            left: &left,
        };
        let site = saving.closure_site(*id)?;
        let Some(closure) = lash_kernel_doc::snapshot::closure_at(left.document, &site) else {
            continue;
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
        let reads = CodeReads::of(&closure.params, &closure.body);
        if !reads.effects.is_empty()
            || !reads.declared.is_empty()
            || reads.free.len() != 1
            || !reads.free.contains(callee)
        {
            continue;
        }
        for free in &reads.free {
            let value = match saving.closure_capture(*id, free)? {
                Some(value) => Some(value),
                None if site.unit == Unit::Main => saving.unit_binding(&Unit::Main, free)?,
                None => None,
            };
            if let Some(value) = value
                && let Some(helper) = closure_held(saving.heap, &value, left.values.tag())?
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
                    && let PageValue::Tuple(t) = value
                {
                    let values = members(heap, t)?;
                    let mut saving = Saving {
                        heap: &mut *heap,
                        left: &left,
                    };
                    function.token = Some(Token::frozen(freeze_token(
                        &mut saving,
                        &values,
                        at,
                        left.values.tag().unwrap_or_default(),
                        &named,
                    )?));
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
            let reads = CodeReads::of(&code.params, &code.body);
            names.extend(reads.declared);
        }
        names
    }
    pub(crate) fn value(&self, heap: &mut PagedHeap) -> Result<PageValue, HeapWriteError> {
        let function = PageValue::Function(heap.text(self.name.as_str())?);
        let Some(token) = &self.token else {
            return Ok(function);
        };
        let mut members = vec![PageValue::Text(heap.text(&token.tag)?)];
        for frozen in &token.before {
            members.push(heap.instantiate(frozen)?);
        }
        members.push(function);
        for frozen in &token.after {
            members.push(heap.instantiate(frozen)?);
        }
        Ok(PageValue::Tuple(heap.tuple(members)?))
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
    #[cfg(test)]
    pub(crate) fn captures_for_call(
        &self,
        heap: &mut PagedHeap,
    ) -> Result<BTreeMap<Name, PageValue>, HeapWriteError> {
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
    values: &FunctionRepresentation,
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
