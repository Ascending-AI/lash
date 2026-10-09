//! Reading a process or a definition as its workflow (FIG-5563).
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
//! The IR the document carries (`Expr`, its slots, declarations and types)
//! is `lash::vm::ir`; this module is the document itself.

/// Which document a process runs, as its observation snapshot names it
/// ([`ProcessDocumentIdentity`](crate::process::ProcessDocumentIdentity)).
pub use lash_trace::{WorkflowDocumentEntry, WorkflowDocumentRef};
pub use lash_vm::{
    WORKFLOW_IR_VERSION, WorkflowBodyForm, WorkflowCatch, WorkflowCompletionGroup, WorkflowGraph,
    WorkflowGraphError, WorkflowIrVersionRefusal, WorkflowPinnedSlots, WorkflowProcessWrapper,
    WorkflowRunDriver, workflow_node_statement, workflow_program_from_graph,
};
pub use lash_vm::{
    WorkflowBindingRef, WorkflowBodyRef, WorkflowCorrespondence, WorkflowCorrespondenceEntry,
    WorkflowDraft, WorkflowDraftHandle, WorkflowDraftOpenError, WorkflowDraftRevision,
    WorkflowEdgeDrag, WorkflowEdit, WorkflowEditDiagnostic, WorkflowEditDiagnosticKind,
    WorkflowEditLocation, WorkflowEditRefusal, WorkflowEditTransaction, WorkflowNodeSource,
};
pub use lash_vm_runtime::WorkflowDocument;

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
