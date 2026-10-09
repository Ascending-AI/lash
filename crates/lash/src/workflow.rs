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
//! # Following a run
//!
//! A [`WorkflowExecutionOverlay`] is what one execution was observed to
//! do, keyed by [`WorkflowTaskSite`]: an execution site in the task that
//! ran it. A host folds it from a session or process feed with
//! [`WorkflowExecutionOverlayAccumulator`] (or the pure
//! [`fold_workflow_overlay`]), reads the document the execution names with
//! [`HostArtifacts::execution_document`](crate::persistence::HostArtifacts::execution_document),
//! gives the reducer its [`WorkflowDocument::overlay_document`], and
//! settles the overlay with the process's committed end. Labels, kinds
//! and edges are read from the document, never from the overlay.
//!
//! # Source
//!
//! No part of reading, editing or publishing prints or parses a language.
//! With the `typescript` feature, [`WorkflowDocument::typescript`] prints a
//! document as TypeScript that lowers back to the same behaviour, or
//! answers the printer's typed refusal. A refusal leaves the document
//! readable, editable and publishable.

/// The kernel document: its statements, expressions, types, data, sites
/// and identities, and its canonical encoding.
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
    pub use lash_kernel_edit::*;
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
