//! Reading a process or a definition as its workflow (FIG-5563), editing
//! it, and publishing the result as a new definition (FIG-5574).
//!
//! [`Processes::graph`](crate::process::Processes::graph) and
//! [`HostArtifacts::definition_graph`](crate::process::HostArtifacts::definition_graph)
//! answer a [`WorkflowRead`]: the definition's engine-neutral identity and
//! signature beside its language document, or the typed reason there is
//! none. Lash reads the stored artifact through its own workers and module
//! store; a host names only ids.
//!
//! The document comes from the document provider its engine was registered
//! with
//! ([`ProcessEngineRegistration::with_document_provider`](crate::plugins::ProcessEngineRegistration::with_document_provider)).
//! The lash_vm engine's is built in and answers a [`WorkflowDocument`]; an
//! engine with none, or with a document of another type, reads
//! [`WorkflowRead::Unsupported`].
//!
//! # The document
//!
//! A [`WorkflowGraph`] is a total typed document of one program: every
//! admitted construct is a typed node or region and no node carries source
//! text. `workflow_graph_from_program` and `workflow_graph_from_artifact`
//! (in `lash::vm::ir`) project a program to its document; [`workflow_program_from_graph`]
//! reconstructs the program exactly. Neither direction involves a source
//! dialect, so a host reads, changes and validates a workflow without
//! printing or parsing TypeScript. `lash::typescript::workflow_graph` is an
//! optional source view of the same document.
//!
//! The document stamps the interpretation of the IR it was written under
//! ([`WORKFLOW_IR_VERSION`]); a document this build does not read is a typed
//! refusal ([`WorkflowIrVersionRefusal`]) when it is opened, never a partial
//! read.
//!
//! A place inside the document is a `(WorkflowNodeId, WorkflowSlotPath)`
//! pair. The slot path is a sequence of `ExprSlot`s from the statement
//! [`workflow_node_statement`] rebuilds for the node, and every `Expr`
//! variant names its slots exhaustively, in evaluation order, through
//! `Expr::slots`.
//!
//! What runs is named the same way. Each node lists its `execution_sites`,
//! and a site's `site_path` holds that slot path to the expression that
//! runs: two calls in one statement are two sites of its node. A run reports
//! one occurrence of a site as one value
//! ([`crate::vm::WorkflowOccurrence`]: the site, its per-site occurrence and
//! the loops around it), in language traces, durable effect occurrences and
//! waits alike.
//!
//! # Editing
//!
//! A [`WorkflowDraft`] opens a document for editing. It names each node by a
//! [`WorkflowDraftHandle`] that survives the edits the node survives and
//! applies [`WorkflowEdit`]s in atomic [`WorkflowEditTransaction`]s against a
//! base revision: insert, clone, remove, move and replace a statement,
//! replace any expression through its slot path, set bindings, conditions,
//! labels, loop and `try` headers, body forms, process signatures and
//! declarations, and rename a binding in its binders and uses. Bindings
//! resolve lexically. A refused transaction changes nothing and answers
//! [`WorkflowEditDiagnostic`]s at node and expression paths.
//!
//! Node ids follow structural paths, so they change under edits. Continuity
//! is the [`WorkflowCorrespondence`] each applied transaction answers: every
//! node of the base and of the result with its outcome (retained, moved,
//! inserted, deleted, split, merged or unmatched), recorded from the edits
//! and from normalization, never matched by position. A host keys layout by
//! handle and reads new ids from the correspondence. Edges are derived: a
//! host maps an edge drag to a move or to a use of a binding
//! ([`WorkflowDraft::edit_for_edge_drag`]).
//!
//! # Publishing
//!
//! [`HostArtifacts::publish_workflow`](crate::process::HostArtifacts::publish_workflow)
//! admits a draft as a definition. Lash reconstructs the IR in its VM
//! workers, links it against the environment a process of the definition
//! would run under, and publishes the module and the descriptor of the
//! selected [`WorkflowEntry`] under the host's pin. A [`WorkflowPublication`]
//! names the definition, the admitted document and the correspondence from
//! the opened document to the admitted one. A document the linker refuses
//! is a [`WorkflowAdmissionRefusal`] at node and expression paths, and
//! publishes nothing.
//!
//! Nothing a document states about itself is trusted: ids, types,
//! signatures, lifted processes and host requirements are derived again.
//! A lifted process keeps its name only when the declaration the linker
//! derives equals the document's in everything else, so an unchanged
//! admitted document publishes to the definition it was read from.
//!
//! # Source
//!
//! No part of reading, editing or publishing prints or parses a dialect.
//! TypeScript is a lens: `lash::typescript::workflow_graph` lowers source to
//! a document a draft opens, and [`WorkflowInspection::source_view`] answers
//! a document's canonical TypeScript with a span per node, or the lens's
//! typed refusal for a program it cannot spell. A refusal there leaves the
//! document readable, editable and publishable.
//!
//! The IR the document carries (`Expr`, its slots, declarations and types)
//! is `lash::vm::ir`; this module is the document itself.

/// The execution overlay: what one execution was observed to do, keyed by
/// the execution sites of the document it runs. A host folds it from the
/// observations of a session or process feed with
/// [`WorkflowExecutionOverlayAccumulator`] (or the pure
/// [`fold_workflow_overlay`]), reads the document the execution names with
/// [`HostArtifacts::execution_document`](crate::persistence::HostArtifacts::execution_document),
/// gives the reducer its [`WorkflowExecutionDocument::overlay_document`],
/// and settles the overlay with the process's committed end. Labels, kinds, edges and the arms of a branch are read
/// from the document, never from the overlay.
pub use lash_trace::{
    DEFAULT_WORKFLOW_OVERLAY_HISTORY_LIMIT, WorkflowExecutionOverlay,
    WorkflowExecutionOverlayAccumulator, WorkflowOverlayCall, WorkflowOverlayChildLink,
    WorkflowOverlayConflict, WorkflowOverlayConflictKind, WorkflowOverlayCoverage,
    WorkflowOverlayDocument, WorkflowOverlayEventIdentity, WorkflowOverlayEventTransition,
    WorkflowOverlayFact, WorkflowOverlayFoldError, WorkflowOverlayHistoryEvent,
    WorkflowOverlayMismatch, WorkflowOverlayOccurrence, WorkflowOverlaySettlement,
    WorkflowOverlaySite, WorkflowOverlaySiteReport, WorkflowOverlaySiteRetention,
    WorkflowOverlayTerminal, WorkflowOverlayTerminalRecord, WorkflowOverlayTerminalStatus,
    fold_workflow_overlay,
};
/// Which document an execution runs, as a process's observation snapshot
/// ([`ProcessDocumentIdentity`](crate::process::ProcessDocumentIdentity))
/// and an execution's start name it.
pub use lash_trace::{WorkflowDocumentEntry, WorkflowDocumentRef};

pub use lash_vm::{
    WORKFLOW_IR_VERSION, WorkflowBodyItem, WorkflowBodyShape, WorkflowCatch, WorkflowGraph,
    WorkflowGraphError, WorkflowIrVersionRefusal, WorkflowMemberStep, WorkflowProcessWrapper,
    WorkflowRunDriver, WorkflowStateWrite, workflow_node_statement, workflow_program_from_graph,
};
pub use lash_vm::{
    WorkflowAdmission, WorkflowAdmissionDiagnostic, WorkflowAdmissionDiagnosticKind,
    WorkflowAdmissionLocation, WorkflowAdmissionRefusal,
};
pub use lash_vm::{
    WorkflowBindingRef, WorkflowBodyLayout, WorkflowBodyLayoutItem, WorkflowBodyRef,
    WorkflowCorrespondence, WorkflowCorrespondenceEntry, WorkflowDraft, WorkflowDraftHandle,
    WorkflowDraftOpenError, WorkflowDraftRevision, WorkflowEdgeDrag, WorkflowEdit,
    WorkflowEditDiagnostic, WorkflowEditDiagnosticKind, WorkflowEditLocation, WorkflowEditRefusal,
    WorkflowEditTransaction, WorkflowExpressionRef, WorkflowNodeSource,
};
pub use lash_vm_runtime::{WorkflowDocument, WorkflowEntry, WorkflowExecutionDocument};

use crate::persistence::ArtifactName;
use crate::process::{ProcessDefinition, ProcessDefinitionId, ProcessEngineKind};
use crate::{ProcessId, Result};

/// One definition as a host inspects it.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkflowInspection {
    /// The content-derived id and the signature the stored artifact states.
    pub definition: ProcessDefinition,
    /// The engine that owns the definition.
    pub engine_kind: ProcessEngineKind,
    /// The definition in its language. `document.graph.source_identity`
    /// names the artifact the definition executes.
    pub document: WorkflowDocument,
}

#[cfg(feature = "typescript")]
impl WorkflowInspection {
    /// The document's canonical TypeScript and where each node sits in it,
    /// or the typed reason the TypeScript lens has none. The lens runs here,
    /// when asked: the read that answered this inspection printed nothing.
    pub fn source_view(
        &self,
    ) -> std::result::Result<
        lash_typescript::workflow_graph::SourceView,
        lash_typescript::workflow_graph::GraphRenderError,
    > {
        lash_typescript::workflow_graph::source_view(&self.document.graph)
    }
}

/// A workflow published as a definition.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkflowPublication {
    /// The content-derived id and the signature the stored artifact states.
    pub definition: ProcessDefinition,
    /// The admitted document and the process the definition starts.
    pub document: WorkflowDocument,
    /// What became of every node since the draft was opened, ending at the
    /// ids of the admitted document.
    pub correspondence: WorkflowCorrespondence,
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
    Read(Box<WorkflowExecutionDocument>),
    /// Nothing retains the module the reference names any more.
    Unavailable(WorkflowUnavailable),
    /// This core has no engine that reads workflow documents.
    Unsupported,
}

/// What a workflow read found no longer retained.
#[derive(Clone, Debug, PartialEq, Eq)]
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
