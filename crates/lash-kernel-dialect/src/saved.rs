//! Saved values retain their defining callable and immutable capture pages.
//! Saving and fresh fulfilment never reconstruct or rename executable code.
use crate::Written;

use lash_kernel_doc::heap::*;
use lash_kernel_doc::snapshot::{FrozenToken, SnapshotError, SnapshotHeap, SnapshotRefusal};
use lash_kernel_doc::{Document, FunctionCatalog, FunctionDefinition, FunctionRepresentation};

use lash_kernel_state::kernel_internals::PagedHeap;
use lash_kernel_state::paged::{FrozenRef, HeapWriteError, SemanticHeap};
use lash_vocabulary::sites::{Site, Unit};
use lash_vocabulary::{CallableId, EffectName, LinkId, Name};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
mod carry;
mod graph;
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
    pub link: LinkId,
    pub callable: CallableId,
    pub environment: EntityId,
    pub document: lash_kernel_doc::IdentifiedDocument,
    pub captures: BTreeMap<Name, FrozenRef>,
    pub closure: PageRef,
    pub references: BTreeSet<lash_kernel_doc::heap::EntityId>,
    pub written: Option<Written>,
    pub token: Option<Token>,
    pub held: Option<HeldAt>,
}
impl SavedFunction {
    pub fn definition(&self) -> Option<&FunctionDefinition> {
        self.document.functions.get(&self.callable.function)
    }
    pub fn label(&self) -> Name {
        self.held
            .as_ref()
            .map_or_else(|| self.name.clone(), HeldAt::label)
    }
    pub fn renamed(&self, name: &Name) -> Self {
        let mut saved = self.clone();
        saved.name = name.clone();
        saved
    }
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
    pub link: LinkId,
    pub documents: &'a BTreeMap<LinkId, Document>,
    pub bindings: &'a BTreeMap<Name, PageValue>,
    pub not_carried: &'a [Name],
    pub controls: &'a BTreeMap<lash_vocabulary::OperationUse, BTreeSet<crate::EffectControl>>,
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

    fn closure_callable(&self, id: EntityId) -> Result<CallableId, HeapFault> {
        Ok(self.heap.closure(id)?.callable)
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
            None => {
                let site = object.site.resolve(self.heap.pages())?;
                if object.callable.link != self.left.link || site.unit != Unit::Main {
                    return Ok(None);
                }
                let value = match self.left.bindings.get(name) {
                    Some(value) => Some(value.clone()),
                    None => self.heap.session_binding(name)?,
                };
                if let Some(value) = &value {
                    self.heap
                        .bind_closure_capture(id, name, value.clone())
                        .map_err(|error| match error {
                            HeapWriteError::Fault(fault) => fault,
                            other => HeapFault::Malformed(format!(
                                "saved Main capture cannot be retained: {other}"
                            )),
                        })?;
                }
                Ok(value)
            }
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
        named: &BTreeSet<EntityId>,
    ) -> Result<(FrozenRef, BTreeSet<EntityId>), SnapshotError> {
        self.heap.freeze_named(value, named).map_err(write_fault)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Unusable {
    #[error("saved function `{function}` refers to unavailable callable {needs:?}")]
    Missing { function: Name, needs: EntityId },
    #[error("saved function `{function}` calls `{callee}`, which {gap}")]
    Relink {
        function: Name,
        callee: String,
        gap: RelinkGap,
    },
    #[error("saved function `{function}` has inconsistent defining Link")]
    Link { function: Name },
}
pub fn closure_of<'a>(
    functions: &'a BTreeMap<Name, SavedFunction>,
    roots: &BTreeSet<Name>,
) -> Result<BTreeMap<Name, &'a SavedFunction>, Unusable> {
    let mut held = BTreeMap::new();
    let mut pending = roots
        .iter()
        .filter_map(|name| functions.get(name))
        .collect::<Vec<_>>();
    while let Some(function) = pending.pop() {
        if held.insert(function.name.clone(), function).is_some() {
            continue;
        }
        for needed in &function.references {
            let next = functions
                .values()
                .find(|candidate| candidate.environment == *needed)
                .ok_or_else(|| Unusable::Missing {
                    function: function.label(),
                    needs: *needed,
                })?;
            pending.push(next);
        }
    }
    Ok(held)
}

/// A closure cannot end a cell through a known control operation. Follow
/// immutable local identities; diagnostic spellings do not select controls.
fn check_controls(
    left: &Left<'_>,
    callable: CallableId,
    document: &Document,
) -> Result<(), SaveError> {
    let mut pending = vec![callable.function];
    let mut seen = BTreeSet::new();
    while let Some(id) = pending.pop() {
        if !seen.insert(id) {
            continue;
        }
        let definition = document
            .functions
            .get(&id)
            .or_else(|| left.catalog.definition(&id))
            .ok_or_else(|| unreadable("the saved callable definition is not loaded"))?;
        if let Some(body) = definition.body() {
            pending.extend(lash_kernel_doc::Node::Block(&body.block).functions_within());
            for slot in crate::authoring::used_imports(&body.block) {
                let use_ = lash_vocabulary::OperationUse {
                    link: callable.link,
                    slot,
                };
                if left
                    .controls
                    .get(&use_)
                    .is_some_and(|controls| !controls.is_empty())
                {
                    let import = document
                        .imports
                        .get(&slot)
                        .or_else(|| body.imports.get(&slot))
                        .ok_or_else(|| unreadable("the saved operation import is not declared"))?;
                    return Err(NotSaved::PerformsControl {
                        effect: lash_vocabulary::EffectName::new(import.name.to_string()).map_err(
                            |_| unreadable("control operation has invalid diagnostic name"),
                        )?,
                    }
                    .into());
                }
            }
        }
    }
    Ok(())
}
