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

pub use lash_vm::WorkflowGraph;
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
