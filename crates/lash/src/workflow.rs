//! Code mode (`codemode`) edits and runs workflows without the RLM REPL protocol.
//!
//! A process's code as a document a host reads, edits and publishes.
//!
//! A process is an entry function of an admitted **kernel document**
//! ([`document::Document`]): a typed tree of statements and expressions
//! with a manifest of what it needs, the functions it declares, and the
//! entries a host may start. The document is the only authority. It holds
//! no source text, no node ids and nothing derived.
//!
//! # Reading
//!
//! [`Processes::graph`](crate::process::Processes::graph) and
//! [`HostArtifacts::definition_graph`](crate::process::HostArtifacts::definition_graph)
//! answer a [`WorkflowRead`]: the definition's identity and signature
//! beside its [`WorkflowDocument`], or the typed reason there is none. A
//! [`WorkflowDocument`] is the kernel document with the total view the
//! checker derives from it ([`graph::Graph`]): a node for every statement
//! and expression, control and data edges, scopes and bindings, a type
//! facet and an effect set for each node, and every execution site. The
//! view is derived again on every read. A host draws it; it never stores
//! or edits it.
//!
//! [`view::derive`] is the serialisable read a canvas draws from one
//! [`WorkflowDocument`]: its entry's statements with their sites, the
//! blocks a host can select with what opens each, the dataflow ports and
//! wires are drawn from, and every execution site. It is lash's wire shape
//! (the `kernel-workflow-view` host schema); labels, layout and styling
//! stay the host's. The view also takes the library catalog explicitly. A
//! document's `use` lines carry names and ids but no signatures. The runtime
//! can ship [`document::FunctionRegistry::export_declarations`] as serde data;
//! a separate reader decodes [`document::FunctionDeclarations`], passes it to
//! [`WorkflowDocument::derive`], then to [`view::derive`]. This needs no engine
//! object or native implementations.
//!
//! A place in a document is a [`document::Site`]: the unit (`main`, a
//! declared function or a library function) and the path of child indexes
//! to the node. What runs is named by an [`document::EffectIdentity`]: the
//! task that ran it, the site, its occurrence in that task and the loop
//! iterations around it. A fan-out spawns one task for each element, so
//! each element's run of one site has its own identity.
//!
//! # Editing
//!
//! [`HostArtifacts::workflow_environment`](crate::process::HostArtifacts::workflow_environment)
//! answers the [`WorkflowEnvironment`] a document is checked against: the
//! effects its processes would be offered and the library functions the
//! workers hold. An [`edit::Draft`] opens a document and applies
//! [`edit::Edit`]s in atomic [`edit::Transaction`]s against a base
//! identity: insert, remove, move and replace statements and expressions,
//! set bindings, conditions, loop and `try` headers, function signatures,
//! and insert, remove, rename and retype entries. Each transaction is
//! admitted whole against the environment or changes nothing, and a
//! refusal names the edit and the site ([`edit::EditRefusal`]).
//!
//! For effect inputs written as expressions, [`authoring::plan`] lowers an
//! ordered batch of [`authoring::Operation`]s to the same transaction. Mix
//! [`authoring::Operation::InsertEffect`] with ordinary
//! [`authoring::Operation::Edit`]s; computed inputs become preceding bindings
//! whose names avoid the whole batch. Planning neither mutates nor admits the
//! draft. Inspect the transaction, then use [`edit::Draft::apply`] as usual.
//!
//! Sites are structural paths, so they change under edits. Continuity is
//! the [`edit::Correspondence`] each applied transaction answers: where
//! every surviving node of the base sits in the result, and whether an
//! edit touched it. A host keys its layout by site and carries it across
//! with the correspondence.
//!
//! # Publishing
//!
//! [`HostArtifacts::publish_workflow`](crate::process::HostArtifacts::publish_workflow)
//! admits a draft's document as a definition of one of its entries. The
//! checker admits the document against the environment a process of it
//! would run under; lash stores the document under its identity and
//! publishes the descriptor of every entry under the host's pin, so a
//! process that starts another entry by function reference finds it. A
//! [`WorkflowPublication`] names the definition, the admitted document and
//! the correspondence since the draft was opened. A refused document
//! ([`WorkflowAdmissionRefusal`]) publishes nothing.
//!
//! A definition names its document by identity. A process keeps the
//! document it was admitted under: publishing an edited document creates a
//! new definition and changes no running process.
//!
//! A definition id is a content address, not a permission: lash does not check
//! that the caller ran or holds the definition; authorizing creation from an id
//! is the host's decision.
//!
//! # Following a run
//!
//! A process observation snapshot includes a [`ProcessWorkflowOverlay`]:
//! site states, occurrence counts and each task's last entered site. It uses
//! [`ProcessWorkflowOverlayAccumulator`], which also folds the typed process
//! events a host stores and the payloads on its process feed. Site entries
//! remain visible through pure computation; committed outcomes and the
//! process's terminal govern completion. Read labels and edges from the
//! process's document, never from the overlay.
//!
//! Session cells and an effect's nested language execution have separate
//! execution identities. Their richer provisional history remains a
//! [`WorkflowExecutionOverlay`], built with
//! [`WorkflowExecutionOverlayAccumulator`] or [`fold_workflow_overlay`].
//! Give that reducer the [`WorkflowDocument::overlay_document`] read through
//! [`HostArtifacts::execution_document`](crate::process::HostArtifacts::execution_document).
//!
//! # Source
//!
//! No part of reading, editing or publishing prints or parses a language.
//! With the `typescript` feature,
//! [`HostArtifacts::import_typescript_workflow`](crate::process::HostArtifacts::import_typescript_workflow)
//! lowers whole workflow source and admits it against the same environment as
//! publication. [`WorkflowImport`] distinguishes front-end diagnostics with
//! source spans, admission refusals and typed unsupported reasons. Import
//! writes no artifact and runs no effect. Exported async declarations and
//! const-bound async literals, including `process(async (...) => ...)`, become
//! entries.
//!
//! With the matching `typescript` or `python` feature, a workflow document's
//! `typescript(options)` or `python(options)` returns plain source, only the
//! code an agent could have written, and an optional disposable map. The
//! environment (effect manifest, number mode, entry signatures, private
//! bindings, helper identities and name renames) stays in the document and
//! is never printed. Printing does not compile or admit it. Hosts read the
//! source back, edited or not, against the document it was printed from:
//! `lash::typescript::read(source, &base, limits)` or `lash::python::read`,
//! and compare `Document::canonical_bytes()` to check reconstruction.
//! [`HostArtifacts::import_exported_workflow`](crate::process::HostArtifacts::import_exported_workflow)
//! reads and admits in one step. `vm::WorkerService` exposes separate `print`
//! and `read_exported` operations in a worker. Read documents still require
//! normal admission before execution; maps are never stored automatically.

#[cfg(feature = "typescript")]
pub(crate) mod typescript;
#[cfg(feature = "typescript")]
pub use typescript::{
    WorkflowImport, WorkflowImportUnsupported, WorkflowSourceDiagnostic,
    WorkflowSourceDiagnosticKind, WorkflowSourceSpan,
};

/// The kernel document: its statements, expressions, types, data, sites
/// and identities, and its canonical encoding.
///
/// # Kernel text
///
/// A document is one whole workflow: the `kernel` and `numbers` header
/// lines, an `entry` signature for each declared function a host may start,
/// and `main`. [`parse_document`] reads one and [`print_document`] writes
/// the canonical text that parses back to it. Statements end at the line;
/// kernel text has no `;`:
///
/// ```rust
#[doc = include_str!("workflow/document_example.rs")]
/// ```
///
/// The example is `src/workflow/document_example.rs`, run by the
/// `the_document_module_docs_example_runs` law in `tests/workflow_laws.rs`;
/// the workspace builds no doctests, so that test is its proof.
pub mod document {
    pub use lash_kernel_doc::*;
}

/// The total view of a document and its admission: nodes, edges, scopes,
/// type facets, effect sets, execution sites, and the refusals of a
/// document that is not admitted.
pub mod graph {
    pub use lash_kernel_check::*;
}

/// Editing a document: drafts, edits, transactions, refusals and the
/// correspondence an applied transaction answers.
pub mod edit {
    pub use lash_kernel_edit::{
        Applied, Correspondence, Draft, Edit, EditDiagnostic, EditDiagnosticKind, EditRefusal,
        Extraction, Location, NodeClass, Position, Survivor, Transaction, extract_entry,
    };
}

/// Compose expression-valued effect insertions and direct edits into a
/// canonical transaction, without admission, publication or execution.
pub use lash_kernel_edit::authoring;

/// The canvas read of a document's entry: statements, scopes, dataflow and
/// execution sites, as serialisable data with no presentation.
pub mod view {
    pub use lash_kernel_edit::view::{
        ActionView, ArgumentView, AssignmentView, BindingKind, BindingView, CalleeView, Dataflow,
        ExecutionSiteView, ExpressionView, ReturnView, ScopeKind, ScopeView, SiteKind,
        StatementView, Use, ViewError, WorkflowView,
    };

    /// The view of `document` at the entry it is entered at.
    ///
    /// `functions` is the catalog used to derive the document. Its `use`
    /// lines hold names and identities, not parameter signatures. A runtime
    /// ships [`document::FunctionRegistry::export_declarations`](super::document::FunctionRegistry::export_declarations)
    /// as serde data; a reader decodes [`document::FunctionDeclarations`](super::document::FunctionDeclarations),
    /// passes it to [`WorkflowDocument::derive`](super::WorkflowDocument::derive),
    /// then passes the same catalog here. No engine or native code is needed.
    ///
    /// # Errors
    ///
    /// The projection refuses a graph that does not describe the document,
    /// or a catalog missing a library call's definition.
    pub fn derive(
        document: &super::WorkflowDocument,
        functions: &dyn super::document::FunctionCatalog,
    ) -> Result<WorkflowView, ViewError> {
        lash_kernel_edit::view::derive(
            document.document(),
            document.graph(),
            &document.entry_unit(),
            functions,
        )
    }
}

pub use lash_trace::{
    DEFAULT_WORKFLOW_OVERLAY_HISTORY_LIMIT, WorkflowExecutionOverlay,
    WorkflowExecutionOverlayAccumulator, WorkflowOverlayCall, WorkflowOverlayChildLink,
    WorkflowOverlayConflict, WorkflowOverlayConflictKind, WorkflowOverlayCoverage,
    WorkflowOverlayDocument, WorkflowOverlayDocumentBinding, WorkflowOverlayEventIdentity,
    WorkflowOverlayExecutionTransition, WorkflowOverlayFact, WorkflowOverlayFoldError,
    WorkflowOverlayHistoryEvent, WorkflowOverlayMismatch, WorkflowOverlayNodeTransition,
    WorkflowOverlayOccurrence, WorkflowOverlaySettlement, WorkflowOverlaySite,
    WorkflowOverlaySiteReport, WorkflowOverlaySiteRetention, WorkflowOverlaySiteState,
    WorkflowOverlayTerminal, WorkflowOverlayTerminalRecord, WorkflowOverlayTerminalStatus,
    WorkflowTaskSite, fold_workflow_overlay,
};
/// Which document an execution runs, as a process's observation snapshot
/// ([`ProcessDocumentIdentity`](crate::process::ProcessDocumentIdentity))
/// and an execution's start name it.
pub use lash_trace::{WorkflowDocumentEntry, WorkflowDocumentRef};
pub use lash_vm_runtime::{
    WorkflowAdmissionRefusal, WorkflowDocument, WorkflowDocumentError, WorkflowEnvironment,
};

use crate::persistence::ArtifactName;
use crate::process::{ProcessDefinition, ProcessDefinitionId, ProcessEngineKind};
use crate::{ProcessId, Result};

/// One definition as a host inspects it.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkflowInspection {
    /// The content-derived id and the signature of the entry the
    /// definition starts.
    pub definition: ProcessDefinition,
    /// The engine that owns the definition.
    pub engine_kind: ProcessEngineKind,
    /// The document the definition runs, entered at its entry.
    pub document: WorkflowDocument,
}

/// A workflow published as a definition.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkflowPublication {
    /// The content-derived id and the signature of the entry.
    pub definition: ProcessDefinition,
    /// The admitted document, entered at the entry the definition starts.
    pub document: WorkflowDocument,
    /// Where every surviving node of the document the draft was opened on
    /// sits in the admitted one.
    pub correspondence: edit::Correspondence,
}

/// The answer to publishing a workflow.
#[derive(Clone, Debug, PartialEq)]
pub enum WorkflowPublish {
    Published(Box<WorkflowPublication>),
    /// The document was refused admission. Nothing was published.
    Refused(WorkflowAdmissionRefusal),
    /// No engine of this core admits workflow documents.
    Unsupported {
        engine_kind: ProcessEngineKind,
    },
}

/// A workflow inspection, or why there is none.
#[derive(Clone, Debug, PartialEq)]
pub enum WorkflowRead {
    Inspected(Box<WorkflowInspection>),
    /// Nothing retains what the read needs any more.
    Unavailable(WorkflowUnavailable),
    /// The engine has no workflow document: it was registered with no
    /// document provider, or it is a core built-in with no definition.
    Unsupported {
        engine_kind: ProcessEngineKind,
    },
}

/// The document an execution names, or why there is none.
#[derive(Clone, Debug, PartialEq)]
pub enum WorkflowDocumentRead {
    Read(Box<WorkflowDocument>),
    /// Nothing retains the document the reference names any more.
    Unavailable(WorkflowUnavailable),
    /// This core has no engine that reads workflow documents.
    Unsupported,
}

/// What a workflow read found no longer retained.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum WorkflowUnavailable {
    /// No row is retained under the process id.
    Process { process_id: ProcessId },
    /// No referrer holds the definition's descriptor.
    Definition { definition_id: ProcessDefinitionId },
    /// No referrer holds an artifact the definition reads.
    Artifact { artifact: ArtifactName },
}

/// Read the definition `payload` names through the `engine_kind` engine's
/// document provider.
pub(crate) async fn read(
    engines: &lash_core::ProcessEngineRegistry,
    engine_kind: &ProcessEngineKind,
    payload: &serde_json::Value,
) -> Result<WorkflowRead> {
    let unsupported = || WorkflowRead::Unsupported {
        engine_kind: engine_kind.clone(),
    };
    let Some(provider) = engines.document_provider(engine_kind.as_str()) else {
        return Ok(unsupported());
    };
    let inspected = match provider.document(payload).await? {
        lash_core::ProcessDocumentRead::Inspected(inspected) => *inspected,
        lash_core::ProcessDocumentRead::ArtifactMissing { artifact } => {
            return Ok(WorkflowRead::Unavailable(WorkflowUnavailable::Artifact {
                artifact,
            }));
        }
    };
    let Ok(document) = inspected.document.downcast::<WorkflowDocument>() else {
        return Ok(unsupported());
    };
    Ok(WorkflowRead::Inspected(Box::new(WorkflowInspection {
        definition: inspected.definition,
        engine_kind: engine_kind.clone(),
        document,
    })))
}

/// Process overlays consume the lifecycle events hosts store and the same
/// process feed's live entries. The trace overlay above serves session cells,
/// which have no process log. Process observation snapshots include this fold.
pub use lash_core::{
    ProcessWorkflowFoldError, ProcessWorkflowLiveCoverage, ProcessWorkflowOverlay,
    ProcessWorkflowOverlayAccumulator, ProcessWorkflowSite, ProcessWorkflowSiteState,
    ProcessWorkflowWaitKind,
};
