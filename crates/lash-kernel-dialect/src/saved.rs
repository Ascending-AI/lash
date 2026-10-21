//! Self-contained saved code with immutable page-backed captures.
//!
//! A saved function is cut out of the cell's heap by the root-based
//! snapshot every crossing function is cut by
//! ([`lash_kernel_doc::snapshot`]): this module saves every closure a
//! carried binding reaches ([`save`]) and freezes its captures as pages.
use crate::Written;
use lash_kernel_doc::Document;
use lash_kernel_doc::Expr;
use lash_kernel_doc::Function;
use lash_kernel_doc::FunctionCatalog;
use lash_kernel_doc::FunctionRepresentation;
use lash_kernel_doc::heap::*;
use lash_kernel_doc::snapshot::{
    CodeReads, FrozenToken, SnapshotError, SnapshotHeap, SnapshotRefusal, gather, members, text_is,
    text_of,
};
use lash_kernel_state::kernel_internals::PagedHeap;
use lash_kernel_state::paged::{FrozenRef, HeapWriteError, SemanticHeap};
use lash_vocabulary::legacy_names::EffectName;
use lash_vocabulary::legacy_names::FunctionId;
use lash_vocabulary::legacy_names::Name;
use lash_vocabulary::operations::OperationRequirement;
use lash_vocabulary::sites::Site;
use lash_vocabulary::sites::Unit;
use lash_vocabulary::types::Signature;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
mod carry;
mod graph;
#[cfg(test)]
mod laws;
mod relink;
#[cfg(test)]
mod relink_laws;
pub use carry::{Saved, save};
pub use graph::{
    Bundle, BundleBody, BundleRefusal, DOCUMENT_NODE_KIND, FUNCTION_BUNDLE_NODE_KIND, MemberBody,
};
pub use lash_kernel_doc::snapshot::CaptureRefusal;
pub use relink::{RelinkGap, relink_saved};

#[derive(Clone, Debug)]
pub struct SavedFunction {
    pub name: Name,
    pub document: Document,
    pub captures: BTreeMap<Name, FrozenRef>,
    /// The ExecutionClosure node of the cell that saved it.
    pub closure: PageRef,
    pub helper_owners: BTreeSet<Name>,
    pub references: BTreeSet<Name>,
    pub written: Option<Written>,
    pub token: Option<Token>,
    /// Where a carried binding held it, when no binding held it as its own
    /// value and it is saved under a generated name ([`Name::saved`]).
    pub held: Option<HeldAt>,
    /// The signature of each library function its code holds, as the
    /// set it was saved or last relinked on declared it: a relink takes a
    /// callee's new identity only under the same signature
    /// (RELINK-CLOSURE).
    pub against: BTreeMap<FunctionId, Signature>,
}
/// Where the save walk first found a saved function that no binding holds
/// as its own value: the binding, and the path to it from the binding's
/// value (`plan.map`), which names it wherever the function is refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeldAt {
    pub binding: Name,
    /// The path from the binding's name, which it starts with.
    pub path: String,
}
impl HeldAt {
    /// What a refusal calls the function.
    #[must_use]
    pub fn label(&self) -> Name {
        Name::new(self.path.clone())
    }
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
/// Why a binding that holds a function was not saved.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum NotSaved {
    /// The binding reaches a task: work that is still running, or its
    /// handle.
    #[error("it holds a task or a pending promise, which ends with its cell")]
    Task,
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
    /// The binding holds a function where no reference to a saved
    /// function can stand for it: as a map key or a set member, behind a
    /// ref, or in an error's data.
    #[error(
        "it holds a function as a key, behind a ref or in an error, where no saved function can stand for it"
    )]
    Keyed,
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
    /// The cell's ExecutionClosure node.
    pub closure: PageRef,
    /// The library the cell ran against: what the bodies its functions
    /// reach perform is read from it.
    pub catalog: &'a dyn FunctionCatalog,
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
    /// The cell's package set, or its offer, does not serve one of its
    /// callees: a library function or an operation (RELINK-CLOSURE). The
    /// function is dropped at the cell's start, named.
    #[error("the saved function `{function}` calls `{callee}`, which {gap}")]
    Relink {
        function: Name,
        callee: String,
        gap: RelinkGap,
    },
    /// It requires an effect under another requirement than the document
    /// or another saved function does (`ADM-REQUIREMENT`).
    #[error(
        "the saved function `{function}` requires `{effect}` as {saved}, and the cell requires it as {held}"
    )]
    EffectRequirement {
        function: Name,
        effect: EffectName,
        held: Box<OperationRequirement>,
        saved: Box<OperationRequirement>,
    },
    /// The effect's call now ends the turn. A saved function was kept
    /// because none of its effects did (`NotSaved::PerformsControl`), so
    /// what the effect declares has changed since.
    #[error(
        "the saved function `{function}` calls `{effect}`, which now ends the turn; a function cannot end it"
    )]
    EffectControls { function: Name, effect: EffectName },
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
    // The functions its code calls, and those the start adapters of its
    // function and closures call.
    let mut library = gathered.library.clone();
    each_start(&gathered.function, &mut |start| {
        library.extend(start.functions());
    });
    // What it performs: its own code, and the bodies it reaches
    // (`K-DOC-002`).
    let through = lash_kernel_doc::performed_through(left.catalog, library.iter().copied());
    if let Some(effect) = gathered
        .effects
        .iter()
        .chain(through.keys())
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
    for (effect, bodies) in through {
        for requirement in bodies.into_values() {
            let held = document
                .manifest
                .effects
                .entry(effect.clone())
                .or_insert_with(|| requirement.clone());
            if held != requirement {
                return Err(unreadable("it requires one effect under two requirements"));
            }
        }
    }
    let mut against = BTreeMap::new();
    for function in &library {
        against.insert(
            *function,
            left.catalog
                .header(function)
                .ok_or_else(|| unreadable("its library function is not held"))?
                .signature
                .clone(),
        );
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
        closure: left.closure,
        helper_owners: owners,
        references: gathered.references,
        written: left
            .written
            .get(&gathered.site)
            .cloned()
            .map(|mut written| {
                let prelude = gathered.function.body.len() - gathered.root.body.len();
                for (path, _) in &mut written.captions {
                    if let Some(statement) = path.first_mut() {
                        *statement += prelude as u32;
                    }
                }
                written
            }),
        token: None,
        held: None,
        against,
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
    /// What a refusal calls it: its name, or for one saved under a
    /// generated name, the path a binding held it at.
    #[must_use]
    pub fn label(&self) -> Name {
        self.held
            .as_ref()
            .map_or_else(|| self.name.clone(), HeldAt::label)
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
        use lash_kernel_state::paged::HeapLifecycle as _;
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
    pub closure: PageRef,
}
/// Declares in `document` the saved functions `roots` name and every one
/// they use, each with what it requires, refusing the first that does not
/// install. A function saved under a generated name ([`Name::saved`]) is
/// held only in carried data, which any cell may read, so each one is
/// declared too, with every function it uses; one of those that does not
/// install leaves it out, and a call through a reference to it raises.
///
/// # Errors
///
/// Why a function `roots` name, or one it uses, does not install here.
pub fn install(
    document: &mut Document,
    functions: &BTreeMap<Name, SavedFunction>,
    roots: &BTreeSet<Name>,
    values: &FunctionRepresentation,
    namespaces: &crate::Namespaces,
    library: &dyn crate::Library,
) -> Result<BTreeMap<Name, InstalledFunction>, Unusable> {
    // One effect name is one requirement across the document and every
    // function installed into it (`ADM-REQUIREMENT`).
    let required = document.manifest.effects.clone();
    let mut installing = Installing {
        document,
        values,
        namespaces,
        library,
        required,
        declared: BTreeMap::new(),
    };
    for (name, function) in closure_of(functions, roots)? {
        installing.admit(name, function)?;
    }
    for root in functions.keys().filter(|name| name.saved_index().is_some()) {
        if installing.declared.contains_key(root) {
            continue;
        }
        let Ok(closure) = closure_of(functions, &BTreeSet::from([root.clone()])) else {
            continue;
        };
        let mut attempt = installing.clone();
        let admitted = closure
            .into_iter()
            .filter(|(name, _)| !installing.declared.contains_key(name))
            .try_for_each(|(name, function)| attempt.admit(name, function));
        if admitted.is_ok() {
            installing = attempt;
        }
    }
    let Installing {
        required, declared, ..
    } = installing;
    if declared.is_empty() {
        return Ok(BTreeMap::new());
    }
    for (name, (_, declaration)) in &declared {
        document
            .functions
            .insert(name.clone(), declaration.code.clone());
    }
    document.manifest.effects = required;
    for (function, _) in declared.values() {
        document
            .manifest
            .functions
            .extend(function.document.manifest.functions.clone());
    }
    Ok(declared
        .into_iter()
        .map(|(name, (_, declaration))| (name, declaration))
        .collect())
}

/// The saved functions an [`install`] has admitted so far, and the
/// requirements they and the document hold.
#[derive(Clone)]
struct Installing<'d, 'a> {
    document: &'d Document,
    values: &'a FunctionRepresentation,
    namespaces: &'a crate::Namespaces,
    library: &'a dyn crate::Library,
    required: BTreeMap<EffectName, OperationRequirement>,
    declared: BTreeMap<Name, (&'a SavedFunction, InstalledFunction)>,
}

impl<'a> Installing<'_, 'a> {
    /// Admits `function`, declared under `name`, or answers why it does not
    /// install in the document.
    fn admit(&mut self, name: Name, function: &'a SavedFunction) -> Result<(), Unusable> {
        let label = function.label();
        if function
            .token
            .as_ref()
            .is_some_and(|t| self.values.tag() != Some(t.tag.as_str()))
        {
            return Err(Unusable::Token { function: label });
        }
        if function.document.manifest.numbers != self.document.manifest.numbers {
            return Err(Unusable::Numbers { function: label });
        }
        for (effect, requirement) in &function.document.manifest.effects {
            match self.namespaces.operation(effect) {
                Some((_, offered)) if !offered.controls.is_empty() => {
                    return Err(Unusable::EffectControls {
                        function: label,
                        effect: effect.clone(),
                    });
                }
                None => {
                    return Err(Unusable::EffectMissing {
                        function: label,
                        effect: effect.clone(),
                    });
                }
                Some((_, offered)) if offered.requirement.signature != requirement.signature => {
                    return Err(Unusable::Relink {
                        function: label,
                        callee: effect.to_string(),
                        gap: RelinkGap::Signature,
                    });
                }
                _ => (),
            }
            let held = self
                .required
                .entry(effect.clone())
                .or_insert_with(|| requirement.clone());
            if held != requirement {
                return Err(Unusable::EffectRequirement {
                    function: label,
                    effect: effect.clone(),
                    held: Box::new(held.clone()),
                    saved: Box::new(requirement.clone()),
                });
            }
        }
        // Everything one cell runs comes from its one package set
        // (RELINK-CLOSURE): each function the saved one lists is the set's
        // of its name, as relinking at the cell's start left it.
        for (listed_function, listed) in &function.document.manifest.functions {
            match self.library.resolve(listed.as_str()) {
                Some(held) if held == *listed_function => {}
                held => {
                    return Err(Unusable::Relink {
                        function: label,
                        callee: listed.to_string(),
                        gap: if held.is_some() {
                            RelinkGap::Unlinked
                        } else {
                            RelinkGap::Removed
                        },
                    });
                }
            }
        }
        if self.document.functions.contains_key(&name) {
            return Err(Unusable::Declared { function: label });
        }
        let code = function
            .definition()
            .ok_or_else(|| Unusable::Missing {
                function: label.clone(),
                needs: label.clone(),
            })?
            .clone();
        self.declared.insert(
            name,
            (
                function,
                InstalledFunction {
                    code,
                    captures: function.captures.clone(),
                    closure: function.closure,
                },
            ),
        );
        Ok(())
    }
}
