//! Editing a workflow document through typed transactions.
//!
//! A [`WorkflowDraft`] is an editing handle over one [`WorkflowGraph`]. It
//! names every node and process container by a [`WorkflowDraftHandle`] that
//! survives the edits the node survives, applies [`WorkflowEdit`]s in atomic
//! transactions against a base revision, and answers each transaction with
//! the [`WorkflowCorrespondence`] from the nodes it started with to the nodes
//! it left. Its export, [`WorkflowDraft::document`], is the one graph
//! document; the handles and the record behind the correspondence are
//! private to the draft and are never part of the program.
//!
//! No dialect is involved. An edit changes the document's authoritative
//! fields; the draft then normalizes: it reconstructs the IR the document
//! spells, validates it, and projects it again, so every derived view is
//! recomputed and every construct lands in its canonical node kind. Edges are
//! one of those views: an edit changes order or a typed use of a binding and
//! the edges follow ([`WorkflowDraft::edit_for_edge_drag`]).
//!
//! A refused transaction leaves the draft as it was and names what it
//! refused at a node and expression path ([`WorkflowEditDiagnostic`]). Each
//! edit must leave a structurally valid program. Binding resolution is
//! checked once, when the transaction commits, so a transaction may remove a
//! producer and reconnect its uses in any order.
//!
//! Bindings resolve lexically, by the rules in `draft/scope.rs`: a variable
//! is a name in a frame, and a read needs a binder that precedes it. Names
//! the opened document already read without binding are the document's
//! ambient names (session globals, for `main`) and stay readable.

use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;

use crate::ast::{
    AstPath, AstRoot, AstString, Declaration, Expr, InvalidAst, Program, StructuralRole,
    validate_ast,
};

use super::projection::{expr_at, statement_addresses};
use super::reconstruction::{Reconstruction, reconstruct};
use super::{
    WorkflowBodySlot, WorkflowContainer, WorkflowDeclaration, WorkflowGraph, WorkflowGraphError,
    WorkflowNode, WorkflowNodeId, WorkflowNodeKind, WorkflowProcess, WorkflowSlotPath,
    WorkflowSubgraph, statement_list, workflow_graph_from_program, workflow_node_statement,
};

mod correspondence;
mod edit;
mod scope;
#[cfg(test)]
mod tests;

pub use correspondence::{WorkflowCorrespondence, WorkflowCorrespondenceEntry, WorkflowNodeSource};
pub use edit::{WorkflowBindingRef, WorkflowEdgeDrag, WorkflowEdit};

use correspondence::Journal;
use scope::{FrameRoot, Lexical, Role};

/// The prefix of the id a node carries while the draft works on it.
const HANDLE_ID_PREFIX: &str = "draft:";

/// A node or a process container of a draft, stable for as long as edits
/// keep it: replacing an expression, renaming a binding or moving the node
/// keeps its handle, inserting or cloning mints one, and removing retires it.
///
/// A handle means nothing outside the draft that minted it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkflowDraftHandle(u64);

impl WorkflowDraftHandle {
    fn id(self) -> WorkflowNodeId {
        WorkflowNodeId::new(format!("{HANDLE_ID_PREFIX}{}", self.0))
    }

    fn of(id: &WorkflowNodeId) -> Option<Self> {
        id.as_str()
            .strip_prefix(HANDLE_ID_PREFIX)?
            .parse()
            .ok()
            .map(Self)
    }
}

impl std::fmt::Display for WorkflowDraftHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{HANDLE_ID_PREFIX}{}", self.0)
    }
}

/// One committed state of a draft. A transaction names the revision it was
/// written against.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkflowDraftRevision(u64);

impl std::fmt::Display for WorkflowDraftRevision {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// An ordered body of statements in a draft.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkflowBodyRef {
    Main,
    /// The body of a process container.
    Process(WorkflowDraftHandle),
    /// A child body of a container node.
    Child {
        node: WorkflowDraftHandle,
        slot: WorkflowBodySlot,
    },
}

/// Edits to apply together, or not at all.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkflowEditTransaction {
    /// The revision the edits were written against.
    pub base: WorkflowDraftRevision,
    pub edits: Vec<WorkflowEdit>,
}

/// Where in a draft a diagnostic points.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkflowEditLocation {
    Document,
    /// An expression of a node: `slot` is the path from the statement
    /// [`workflow_node_statement`] spells, empty for the statement itself.
    Node {
        node: WorkflowDraftHandle,
        slot: WorkflowSlotPath,
    },
    Process {
        process: WorkflowDraftHandle,
    },
    Function {
        name: AstString,
    },
}

/// Why an edit or a transaction was refused.
#[derive(Clone, Debug, Error, PartialEq)]
#[non_exhaustive]
pub enum WorkflowEditDiagnosticKind {
    #[error("the transaction was written against revision {base}; the draft is at {current}")]
    StaleRevision {
        base: WorkflowDraftRevision,
        current: WorkflowDraftRevision,
    },
    #[error("`{handle}` names no node or process of the draft")]
    UnknownHandle { handle: WorkflowDraftHandle },
    #[error("the draft has no such body")]
    UnknownBody,
    #[error("`{anchor}` is not a statement of the body")]
    AnchorOutsideBody { anchor: WorkflowDraftHandle },
    /// The order a tree of bodies cannot hold: a statement inside itself. A
    /// loop is a container, never a back edge.
    #[error("a node cannot move into its own subtree; repetition is a loop container")]
    MoveIntoOwnSubtree,
    #[error("the node has no expression at that slot")]
    UnknownSlot,
    #[error("the slot is inside a child body, whose statements are nodes of their own")]
    SlotInChildBody,
    #[error("the edit applies to {expected}")]
    EditDoesNotApply { expected: &'static str },
    /// A read no binder reaches: its producer was removed, or the read moved
    /// to where the binder does not precede it.
    #[error("`{name}` is read where nothing binds it")]
    UnresolvedBinding { name: AstString },
    /// A node moved between `main` and a process, or between processes, uses
    /// a name the destination already binds: the move would silently make it
    /// read or overwrite a different variable.
    #[error("`{name}` names a different variable where the node was moved")]
    BindingCaptured { name: AstString },
    #[error("the document binds no variable `{name}` there")]
    UnknownBinding { name: AstString },
    #[error("`{name}` is already a name there")]
    BindingNameTaken { name: AstString },
    #[error("no process `{name}` is declared")]
    UnknownProcess { name: AstString },
    /// An edit named a function the document does not declare, or a call
    /// names one: its declaration was removed, or never was.
    #[error("no function `{name}` is declared")]
    UnknownFunction { name: AstString },
    /// A lifted process is derived from the literal that carries it: its name
    /// is a digest and its declaration follows the literal.
    #[error("a lifted process is derived from its literal")]
    DerivedProcess,
    #[error(transparent)]
    InvalidDocument(WorkflowGraphError),
    #[error(transparent)]
    InvalidProgram(InvalidAst),
}

impl WorkflowEditDiagnosticKind {
    /// A stable identifier for the refusal, independent of its message.
    pub fn code(&self) -> &'static str {
        match self {
            Self::StaleRevision { .. } => "stale_revision",
            Self::UnknownHandle { .. } => "unknown_handle",
            Self::UnknownBody => "unknown_body",
            Self::AnchorOutsideBody { .. } => "anchor_outside_body",
            Self::MoveIntoOwnSubtree => "move_into_own_subtree",
            Self::UnknownSlot => "unknown_slot",
            Self::SlotInChildBody => "slot_in_child_body",
            Self::EditDoesNotApply { .. } => "edit_does_not_apply",
            Self::UnresolvedBinding { .. } => "unresolved_binding",
            Self::BindingCaptured { .. } => "binding_captured",
            Self::UnknownBinding { .. } => "unknown_binding",
            Self::BindingNameTaken { .. } => "binding_name_taken",
            Self::UnknownProcess { .. } => "unknown_process",
            Self::UnknownFunction { .. } => "unknown_function",
            Self::DerivedProcess => "derived_process",
            Self::InvalidDocument(_) => "invalid_document",
            Self::InvalidProgram(_) => "invalid_program",
        }
    }
}

/// One refusal, at the place it concerns.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkflowEditDiagnostic {
    /// The position in the transaction of the edit that was refused, or
    /// `None` for a check of the transaction's whole result.
    pub edit: Option<usize>,
    pub location: WorkflowEditLocation,
    pub kind: WorkflowEditDiagnosticKind,
}

/// A transaction the draft did not apply. The draft is unchanged.
#[derive(Clone, Debug, Error, PartialEq)]
#[error("the workflow edit transaction was refused with {} diagnostic(s)", .diagnostics.len())]
pub struct WorkflowEditRefusal {
    pub diagnostics: Vec<WorkflowEditDiagnostic>,
}

/// Why a document does not open as a draft.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum WorkflowDraftOpenError {
    #[error(transparent)]
    Document(#[from] WorkflowGraphError),
    #[error(transparent)]
    Program(#[from] InvalidAst),
}

type Refused = (WorkflowEditLocation, WorkflowEditDiagnosticKind);

/// What owns a top-level frame: the key of a document's ambient names.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Owner {
    Main,
    Process(WorkflowDraftHandle),
    Function(AstString),
}

type Ambient = BTreeMap<Owner, BTreeSet<AstString>>;

/// An editing handle over one workflow document.
#[derive(Clone, Debug)]
pub struct WorkflowDraft {
    state: State,
    revision: WorkflowDraftRevision,
    /// The id each handle's node had in the document the draft opened.
    opened: BTreeMap<WorkflowDraftHandle, WorkflowNodeId>,
    history: Journal,
    ambient: Ambient,
}

impl WorkflowDraft {
    /// Opens `document` for editing. The draft holds the document's canonical
    /// form: its program reconstructed and projected again, with no source
    /// identity, since a draft is admitted only when it is published.
    pub fn open(document: &WorkflowGraph) -> Result<Self, WorkflowDraftOpenError> {
        let Reconstruction {
            program,
            carried,
            orphaned,
        } = reconstruct(document)?;
        if let Some(name) = orphaned.into_iter().next() {
            return Err(WorkflowGraphError::ProcessOriginMismatch {
                name,
                message: "no process literal or reference in the program carries it".to_string(),
            }
            .into());
        }
        let mut state = State {
            working: document.clone(),
            document: document.clone(),
            program: program.clone(),
            carried: Vec::new(),
            addresses: Vec::new(),
            ids: BTreeMap::new(),
            handles: BTreeMap::new(),
            next_handle: 0,
            journal: Journal::default(),
        };
        let mut opened = BTreeMap::new();
        for id in node_ids_mut(&mut state.working) {
            let handle = WorkflowDraftHandle(state.next_handle);
            state.next_handle += 1;
            opened.insert(handle, std::mem::replace(id, handle.id()));
        }
        state.settle_program(program, carried)?;
        let history = std::mem::take(&mut state.journal);
        let lexical = Lexical::of(&state.program);
        let mut ambient = Ambient::new();
        for occurrence in lexical.unresolved() {
            if let Some(owner) = state.owner(lexical.top(occurrence.frame)) {
                ambient
                    .entry(owner)
                    .or_default()
                    .insert(occurrence.name.clone());
            }
        }
        Ok(Self {
            state,
            revision: WorkflowDraftRevision(0),
            opened,
            history,
            ambient,
        })
    }

    /// Declares session globals `main` may read: names bound outside the
    /// document, beside the ones the opened document already read.
    pub fn declare_globals(&mut self, names: impl IntoIterator<Item = AstString>) {
        self.ambient.entry(Owner::Main).or_default().extend(names);
    }

    pub fn revision(&self) -> WorkflowDraftRevision {
        self.revision
    }

    /// The draft as the one graph document, in canonical form.
    pub fn document(&self) -> &WorkflowGraph {
        &self.state.document
    }

    /// The handle of the node or process container with `id` in
    /// [`Self::document`].
    pub fn handle(&self, id: &WorkflowNodeId) -> Option<WorkflowDraftHandle> {
        self.state.handles.get(id).copied()
    }

    /// The handle of the node or process container that had `id` in the
    /// document the draft opened, while edits keep it.
    pub fn opened_handle(&self, id: &WorkflowNodeId) -> Option<WorkflowDraftHandle> {
        self.opened
            .iter()
            .find_map(|(handle, opened)| (opened == id).then_some(*handle))
            .filter(|handle| self.state.ids.contains_key(handle))
    }

    /// Every node and process container of the document the draft opened,
    /// with the id it had there.
    pub fn opened(&self) -> impl Iterator<Item = (WorkflowDraftHandle, &WorkflowNodeId)> {
        self.opened.iter().map(|(handle, id)| (*handle, id))
    }

    /// The id `handle` has in [`Self::document`].
    pub fn node_id(&self, handle: WorkflowDraftHandle) -> Option<&WorkflowNodeId> {
        self.state.ids.get(&handle)
    }

    pub fn node(&self, handle: WorkflowDraftHandle) -> Option<&WorkflowNode> {
        let id = self.state.ids.get(&handle)?;
        self.state.document.nodes().find(|node| node.id == *id)
    }

    pub fn process(&self, handle: WorkflowDraftHandle) -> Option<&WorkflowProcess> {
        let id = self.state.ids.get(&handle)?;
        processes(&self.state.document).find(|process| process.id == *id)
    }

    /// The statements of `body`, in order.
    pub fn body(&self, body: &WorkflowBodyRef) -> Option<Vec<WorkflowDraftHandle>> {
        let body = edit::body(&self.state.working, body)?;
        Some(
            body.nodes
                .iter()
                .filter_map(|node| WorkflowDraftHandle::of(&node.id))
                .collect(),
        )
    }

    /// The body that holds the node `handle`.
    pub fn parent(&self, handle: WorkflowDraftHandle) -> Option<WorkflowBodyRef> {
        edit::parent_of(&self.state.working, handle).map(|(body, _)| body)
    }

    /// Applies `transaction` whole, or refuses it and changes nothing.
    pub fn apply(
        &mut self,
        transaction: WorkflowEditTransaction,
    ) -> Result<WorkflowCorrespondence, WorkflowEditRefusal> {
        if transaction.base != self.revision {
            return Err(WorkflowEditRefusal {
                diagnostics: vec![WorkflowEditDiagnostic {
                    edit: None,
                    location: WorkflowEditLocation::Document,
                    kind: WorkflowEditDiagnosticKind::StaleRevision {
                        base: transaction.base,
                        current: self.revision,
                    },
                }],
            });
        }
        let mut state = self.state.clone();
        for (index, edit) in transaction.edits.into_iter().enumerate() {
            if let Err((location, kind)) = state.edit(edit, &self.ambient) {
                return Err(WorkflowEditRefusal {
                    diagnostics: vec![WorkflowEditDiagnostic {
                        edit: Some(index),
                        location,
                        kind,
                    }],
                });
            }
        }
        let diagnostics = state.lexical_diagnostics(&self.ambient);
        if !diagnostics.is_empty() {
            return Err(WorkflowEditRefusal { diagnostics });
        }
        let revision = WorkflowDraftRevision(self.revision.0 + 1);
        let journal = std::mem::take(&mut state.journal);
        let entries = journal.correspondence(&self.state.ids, &state.ids);
        self.history.append(journal);
        self.state = state;
        self.revision = revision;
        Ok(WorkflowCorrespondence {
            base: transaction.base,
            revision,
            entries,
        })
    }

    /// The correspondence from the document the draft opened to
    /// [`Self::document`], through every transaction since.
    pub fn correspondence_since_open(&self) -> WorkflowCorrespondence {
        WorkflowCorrespondence {
            base: WorkflowDraftRevision(0),
            revision: self.revision,
            entries: self.history.correspondence(&self.opened, &self.state.ids),
        }
    }

    /// The typed edit an edge drag means. Edges are derived, so a drag never
    /// writes one: a sequence drag is a move, and a data drag puts a use of
    /// the producer's binding in a slot of the consumer.
    pub fn edit_for_edge_drag(
        &self,
        drag: &WorkflowEdgeDrag,
    ) -> Result<WorkflowEdit, WorkflowEditDiagnostic> {
        self.state
            .edge_drag(drag)
            .map_err(|(location, kind)| WorkflowEditDiagnostic {
                edit: None,
                location,
                kind,
            })
    }
}

/// A draft's document with everything derived from it for editing.
#[derive(Clone, Debug)]
struct State {
    /// The document with each node and process container named by its
    /// handle, which is what edits change.
    working: WorkflowGraph,
    /// The canonical document.
    document: WorkflowGraph,
    program: Program,
    /// The lifted containers of `working` that literals of `program` carry,
    /// each with its literal's path from `main`.
    carried: Vec<(String, Vec<u32>)>,
    /// Where each node of `document` sits in `program`.
    addresses: Vec<(WorkflowNodeId, AstPath)>,
    ids: BTreeMap<WorkflowDraftHandle, WorkflowNodeId>,
    handles: BTreeMap<WorkflowNodeId, WorkflowDraftHandle>,
    next_handle: u64,
    journal: Journal,
}

impl State {
    fn mint(&mut self, source: WorkflowNodeSource) -> WorkflowDraftHandle {
        let handle = WorkflowDraftHandle(self.next_handle);
        self.next_handle += 1;
        self.journal.minted.insert(handle, source);
        handle
    }

    /// Normalizes after an edit of `working`.
    fn settle(&mut self) -> Result<(), WorkflowEditDiagnosticKind> {
        let Reconstruction {
            program, carried, ..
        } = reconstruct(&self.working).map_err(WorkflowEditDiagnosticKind::InvalidDocument)?;
        self.settle_program(program, carried)
            .map_err(WorkflowEditDiagnosticKind::InvalidProgram)
    }

    /// Normalizes onto `program`, the IR `working` spells: validates it,
    /// projects the canonical document and carries every handle onto the
    /// node its statement became.
    fn settle_program(
        &mut self,
        program: Program,
        carried: Vec<(String, Vec<u32>)>,
    ) -> Result<(), InvalidAst> {
        validate_ast(&program)?;
        let document = workflow_graph_from_program(&program);
        let addresses = statement_addresses(&program);
        let working = self.working.clone();
        let mut assigned = BTreeMap::new();
        self.carry_body(&working.main, &document.main, &mut assigned);
        let declared = program
            .declarations
            .iter()
            .filter_map(|declaration| match declaration {
                Declaration::Process(process) => Some(process.name.as_str()),
                Declaration::Function(_) => None,
            })
            .collect::<BTreeSet<_>>();
        let mut kept = BTreeSet::new();
        for process in processes(&document) {
            let literal = match &process.origin {
                crate::ProcessOrigin::Lifted { site, .. }
                    if !declared.contains(process.name.as_str()) =>
                {
                    Some(site)
                }
                _ => None,
            };
            let twin = match literal {
                None => working.process(&process.name),
                Some(site) => carried
                    .iter()
                    .find(|(_, path)| *path == site.steps)
                    .and_then(|(name, _)| working.process(name)),
            };
            match twin.and_then(|twin| Some((twin, WorkflowDraftHandle::of(&twin.id)?))) {
                Some((twin, handle)) => {
                    kept.insert(twin.name.as_str());
                    assigned.insert(process.id.clone(), handle);
                    self.carry_body(&twin.body, &process.body, &mut assigned);
                }
                None => {
                    // A literal with no container yet: the process it lifts
                    // to derives from the statement that holds it.
                    let source = literal
                        .and_then(|site| holder(&addresses, site))
                        .and_then(|holder| assigned.get(holder).copied())
                        .map_or(WorkflowNodeSource::Authored, |holder| {
                            self.offspring_source(holder)
                        });
                    let handle = self.mint(source);
                    assigned.insert(process.id.clone(), handle);
                    self.adopt(&process.body, source, &mut assigned);
                }
            }
        }
        for process in processes(&working) {
            if !kept.contains(process.name.as_str()) {
                if let Some(handle) = WorkflowDraftHandle::of(&process.id) {
                    self.journal.deleted.insert(handle);
                }
                for node in &process.body.nodes {
                    self.forget(node, false);
                }
            }
        }
        let mut relabelled = document.clone();
        for id in node_ids_mut(&mut relabelled) {
            let handle = match assigned.get(id) {
                Some(handle) => *handle,
                None => {
                    let handle = self.mint(WorkflowNodeSource::Authored);
                    assigned.insert(id.clone(), handle);
                    handle
                }
            };
            *id = handle.id();
        }
        self.ids = assigned
            .iter()
            .map(|(id, handle)| (*handle, id.clone()))
            .collect();
        self.handles = assigned;
        // The canonical document names each carried container by the digest
        // of the body it now has, which is the name `working` has for it too.
        self.carried = processes(&document)
            .filter_map(|process| match &process.origin {
                crate::ProcessOrigin::Lifted { site, .. }
                    if !declared.contains(process.name.as_str()) =>
                {
                    Some((process.name.clone(), site.steps.clone()))
                }
                _ => None,
            })
            .collect();
        self.working = relabelled;
        self.document = document;
        self.program = program;
        self.addresses = addresses;
        Ok(())
    }

    /// Carries the handles of an edited body onto its canonical form.
    ///
    /// Each edited node spells one statement, and the canonical body holds
    /// the statements that statement lists as: one node, or several when it
    /// is itself a statement list. That count, never a position, is what
    /// pairs the two; a body whose counts do not add up has lost its
    /// provenance and none is guessed.
    fn carry_body(
        &mut self,
        edited: &WorkflowSubgraph,
        canonical: &WorkflowSubgraph,
        assigned: &mut BTreeMap<WorkflowNodeId, WorkflowDraftHandle>,
    ) {
        let single = !edited.is_statement_list();
        let counts = edited
            .nodes
            .iter()
            .map(|node| listed_statements(node, single))
            .collect::<Option<Vec<_>>>()
            .filter(|counts| counts.iter().sum::<usize>() == canonical.nodes.len());
        let Some(counts) = counts else {
            for node in &edited.nodes {
                self.forget(node, true);
            }
            self.adopt(canonical, WorkflowNodeSource::Authored, assigned);
            return;
        };
        let mut produced = canonical.nodes.iter();
        for (node, count) in edited.nodes.iter().zip(counts) {
            let pieces = produced.by_ref().take(count).collect::<Vec<_>>();
            let Some(handle) = WorkflowDraftHandle::of(&node.id) else {
                for piece in pieces {
                    self.adopt_node(piece, WorkflowNodeSource::Authored, assigned);
                }
                continue;
            };
            match pieces.as_slice() {
                [] => self.forget(node, false),
                [piece] => {
                    assigned.insert(piece.id.clone(), handle);
                    self.carry_children(node, piece, handle, assigned);
                }
                pieces => {
                    for (_, child) in children(node) {
                        for node in &child.nodes {
                            self.forget(node, false);
                        }
                    }
                    let source = self.offspring_source(handle);
                    let into = pieces
                        .iter()
                        .map(|piece| self.adopt_node(piece, source, assigned))
                        .collect();
                    self.journal.split.insert(handle, into);
                }
            }
        }
    }

    fn carry_children(
        &mut self,
        edited: &WorkflowNode,
        canonical: &WorkflowNode,
        handle: WorkflowDraftHandle,
        assigned: &mut BTreeMap<WorkflowNodeId, WorkflowDraftHandle>,
    ) {
        let edited = children(edited);
        let canonical = children(canonical);
        for (slot, body) in &canonical {
            match edited.iter().find(|(edited, _)| edited == slot) {
                Some((_, twin)) => self.carry_body(twin, body, assigned),
                None => {
                    let source = self.offspring_source(handle);
                    self.adopt(body, source, assigned);
                }
            }
        }
        for (slot, body) in &edited {
            if !canonical.iter().any(|(canonical, _)| canonical == slot) {
                for node in &body.nodes {
                    self.forget(node, false);
                }
            }
        }
    }

    /// The source of a node normalization made out of `parent`'s statement.
    fn offspring_source(&self, parent: WorkflowDraftHandle) -> WorkflowNodeSource {
        match self.journal.minted.get(&parent) {
            Some(WorkflowNodeSource::Authored) => WorkflowNodeSource::Authored,
            _ => WorkflowNodeSource::Derived { from: parent },
        }
    }

    /// Mints handles for a canonical body no edited node accounts for.
    fn adopt(
        &mut self,
        body: &WorkflowSubgraph,
        source: WorkflowNodeSource,
        assigned: &mut BTreeMap<WorkflowNodeId, WorkflowDraftHandle>,
    ) {
        for node in &body.nodes {
            self.adopt_node(node, source, assigned);
        }
    }

    fn adopt_node(
        &mut self,
        node: &WorkflowNode,
        source: WorkflowNodeSource,
        assigned: &mut BTreeMap<WorkflowNodeId, WorkflowDraftHandle>,
    ) -> WorkflowDraftHandle {
        let handle = self.mint(source);
        assigned.insert(node.id.clone(), handle);
        for (_, child) in children(node) {
            self.adopt(child, source, assigned);
        }
        handle
    }

    /// Retires an edited node and everything under it: deleted, or unmatched
    /// when its provenance was lost.
    fn forget(&mut self, node: &WorkflowNode, unmatched: bool) {
        if let Some(handle) = WorkflowDraftHandle::of(&node.id) {
            if unmatched {
                self.journal.unmatched.insert(handle);
            } else {
                self.journal.deleted.insert(handle);
            }
        }
        for (_, child) in children(node) {
            for node in &child.nodes {
                self.forget(node, unmatched);
            }
        }
    }

    fn owner(&self, root: &FrameRoot) -> Option<Owner> {
        match root {
            FrameRoot::Main => Some(Owner::Main),
            FrameRoot::Declaration(index) => {
                match self.program.declarations.get(*index as usize)? {
                    Declaration::Process(process) => {
                        let id = &self.document.process(&process.name)?.id;
                        self.handles.get(id).copied().map(Owner::Process)
                    }
                    Declaration::Function(function) => Some(Owner::Function(function.name.clone())),
                }
            }
            FrameRoot::Expr(_) => None,
        }
    }

    fn address(&self, handle: WorkflowDraftHandle) -> Option<&AstPath> {
        let id = self.ids.get(&handle)?;
        self.addresses
            .iter()
            .find_map(|(node, path)| (node == id).then_some(path))
    }

    /// The node and expression path that hold the expression at `path`.
    fn locate(&self, path: &AstPath) -> WorkflowEditLocation {
        if let Some(id) = holder(&self.addresses, path)
            && let Some(handle) = self.handles.get(id)
            && let Some(address) = self.address(*handle)
        {
            let slots = expr_at(&self.program, address)
                .and_then(|statement| statement.slot_path(&path.steps[address.steps.len()..]))
                .unwrap_or_default();
            return WorkflowEditLocation::Node {
                node: *handle,
                slot: WorkflowSlotPath::structural(slots),
            };
        }
        let AstRoot::Declaration(index) = path.root else {
            return WorkflowEditLocation::Document;
        };
        match self.owner(&FrameRoot::Declaration(index)) {
            Some(Owner::Process(process)) => WorkflowEditLocation::Process { process },
            Some(Owner::Function(name)) => WorkflowEditLocation::Function { name },
            Some(Owner::Main) | None => WorkflowEditLocation::Document,
        }
    }

    /// The binding checks of a transaction's whole result.
    fn lexical_diagnostics(&self, ambient: &Ambient) -> Vec<WorkflowEditDiagnostic> {
        let lexical = Lexical::of(&self.program);
        let mut diagnostics = Vec::new();
        for occurrence in lexical.unresolved() {
            let readable = self
                .owner(lexical.top(occurrence.frame))
                .and_then(|owner| ambient.get(&owner))
                .is_some_and(|names| names.contains(&occurrence.name));
            if readable {
                continue;
            }
            let name = occurrence.name.clone();
            diagnostics.push(WorkflowEditDiagnostic {
                edit: None,
                location: self.locate(&occurrence.path),
                kind: match occurrence.role {
                    Role::Process => WorkflowEditDiagnosticKind::UnknownProcess { name },
                    Role::Function => WorkflowEditDiagnosticKind::UnknownFunction { name },
                    _ => WorkflowEditDiagnosticKind::UnresolvedBinding { name },
                },
            });
        }
        for handle in &self.journal.reframed {
            let Some(path) = self.address(*handle) else {
                continue;
            };
            let Some(frame) = lexical.frame_at(path) else {
                continue;
            };
            let inside = |occurrence: &scope::Occurrence| {
                occurrence.path.root == path.root && occurrence.path.steps.starts_with(&path.steps)
            };
            let bound_outside = lexical
                .occurrences
                .iter()
                .filter(|occurrence| {
                    occurrence.frame == frame
                        && occurrence.role == Role::Bind
                        && !inside(occurrence)
                })
                .map(|occurrence| &occurrence.name)
                .chain(&lexical.frames[frame].params)
                .collect::<BTreeSet<_>>();
            let captured = lexical
                .occurrences
                .iter()
                .filter(|occurrence| {
                    occurrence.frame == frame
                        && !matches!(occurrence.role, Role::Process | Role::Function)
                        && inside(occurrence)
                        && bound_outside.contains(&occurrence.name)
                })
                .map(|occurrence| occurrence.name.clone())
                .collect::<BTreeSet<_>>();
            for name in captured {
                diagnostics.push(WorkflowEditDiagnostic {
                    edit: None,
                    location: WorkflowEditLocation::Node {
                        node: *handle,
                        slot: WorkflowSlotPath::default(),
                    },
                    kind: WorkflowEditDiagnosticKind::BindingCaptured { name },
                });
            }
        }
        diagnostics
    }
}

/// The node whose statement holds the expression at `path`: the deepest one,
/// since a path stops at the statements of a child body.
fn holder<'a>(
    addresses: &'a [(WorkflowNodeId, AstPath)],
    path: &AstPath,
) -> Option<&'a WorkflowNodeId> {
    addresses
        .iter()
        .filter(|(_, address)| address.root == path.root && path.steps.starts_with(&address.steps))
        .max_by_key(|(_, address)| address.steps.len())
        .map(|(id, _)| id)
}

/// How many statements a body lists an edited node's statement as. `single`
/// is whether the body is that one statement with no list around it.
fn listed_statements(node: &WorkflowNode, single: bool) -> Option<usize> {
    let statement = workflow_node_statement(node).ok()?;
    let list = single
        || matches!(
            statement,
            Expr::Role {
                role: StructuralRole::Completion,
                ..
            }
        );
    Some(if list {
        statement_list(&statement).len()
    } else {
        1
    })
}

fn children(node: &WorkflowNode) -> Vec<(&'static str, &WorkflowSubgraph)> {
    match &node.kind {
        WorkflowNodeKind::Container(container) => container.child_subgraphs().collect(),
        _ => Vec::new(),
    }
}

fn processes(graph: &WorkflowGraph) -> impl Iterator<Item = &WorkflowProcess> {
    graph
        .declarations
        .iter()
        .filter_map(|declaration| match declaration {
            WorkflowDeclaration::Process(process) => Some(process),
            WorkflowDeclaration::Function(_) => None,
        })
}

/// Every node id and process container id of `graph`, in document order.
fn node_ids_mut(graph: &mut WorkflowGraph) -> Vec<&mut WorkflowNodeId> {
    fn body<'g>(graph: &'g mut WorkflowSubgraph, ids: &mut Vec<&'g mut WorkflowNodeId>) {
        for node in &mut graph.nodes {
            ids.push(&mut node.id);
            if let WorkflowNodeKind::Container(container) = &mut node.kind {
                for (_, child) in container.child_subgraphs_mut() {
                    body(child, ids);
                }
            }
        }
    }
    let mut ids = Vec::new();
    body(&mut graph.main, &mut ids);
    for declaration in &mut graph.declarations {
        if let WorkflowDeclaration::Process(process) = declaration {
            ids.push(&mut process.id);
            body(&mut process.body, &mut ids);
        }
    }
    ids
}

/// The child body of `container` in `slot`.
fn child_body(container: &WorkflowContainer, slot: WorkflowBodySlot) -> Option<&WorkflowSubgraph> {
    match (container, slot) {
        (WorkflowContainer::If { then_graph, .. }, WorkflowBodySlot::Then) => Some(then_graph),
        (WorkflowContainer::If { else_graph, .. }, WorkflowBodySlot::Else) => Some(else_graph),
        (
            WorkflowContainer::For { body, .. } | WorkflowContainer::While { body, .. },
            WorkflowBodySlot::LoopBody,
        )
        | (WorkflowContainer::Try { body, .. }, WorkflowBodySlot::TryBody)
        | (WorkflowContainer::Scope { body, .. }, WorkflowBodySlot::Scope) => Some(body),
        (WorkflowContainer::Try { catch, .. }, WorkflowBodySlot::Catch) => {
            catch.as_ref().map(|catch| catch.body.as_ref())
        }
        (WorkflowContainer::Try { finally, .. }, WorkflowBodySlot::Finally) => finally.as_deref(),
        _ => None,
    }
}

fn child_body_mut(
    container: &mut WorkflowContainer,
    slot: WorkflowBodySlot,
) -> Option<&mut WorkflowSubgraph> {
    match (container, slot) {
        (WorkflowContainer::If { then_graph, .. }, WorkflowBodySlot::Then) => Some(then_graph),
        (WorkflowContainer::If { else_graph, .. }, WorkflowBodySlot::Else) => Some(else_graph),
        (
            WorkflowContainer::For { body, .. } | WorkflowContainer::While { body, .. },
            WorkflowBodySlot::LoopBody,
        )
        | (WorkflowContainer::Try { body, .. }, WorkflowBodySlot::TryBody)
        | (WorkflowContainer::Scope { body, .. }, WorkflowBodySlot::Scope) => Some(body),
        (WorkflowContainer::Try { catch, .. }, WorkflowBodySlot::Catch) => {
            catch.as_mut().map(|catch| catch.body.as_mut())
        }
        (WorkflowContainer::Try { finally, .. }, WorkflowBodySlot::Finally) => {
            finally.as_deref_mut()
        }
        _ => None,
    }
}

/// The slots of `container` that hold a body, in document order.
fn body_slots(container: &WorkflowContainer) -> Vec<WorkflowBodySlot> {
    [
        WorkflowBodySlot::Then,
        WorkflowBodySlot::Else,
        WorkflowBodySlot::LoopBody,
        WorkflowBodySlot::TryBody,
        WorkflowBodySlot::Catch,
        WorkflowBodySlot::Finally,
        WorkflowBodySlot::Scope,
    ]
    .into_iter()
    .filter(|slot| child_body(container, *slot).is_some())
    .collect()
}
