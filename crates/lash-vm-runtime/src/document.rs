//! The Lash VM engine's document provider: a stored definition read back as
//! its workflow document (FIG-5563), and a workflow document admitted as a
//! definition (FIG-5574).
//!
//! Both go through the engine's own worker service and module store. A read
//! verifies the artifact as execution does and names the entry process
//! through the persisted [`lash_vm::ProcessRef`], never a convenience name.
//! An admission links the document in a worker against the environment a
//! process of it would run under, and publishes the module the linker
//! derived only once nothing is left to refuse. No source dialect is
//! involved in either.

use std::collections::BTreeMap;
use std::sync::Arc;

use lash_vm::{
    WorkflowAdmissionDiagnosticKind, WorkflowAdmissionLocation, WorkflowAdmissionRefusal,
    WorkflowGraph, WorkflowNodeId,
};
use lash_vm_client::service::runtime_ops::ServiceRuntimeOps as _;

use crate::{
    LASH_VM_ENGINE_KIND, LashVmProcessEngine, LashVmProcessInput, lash_vm_type_expr_schema,
};

/// A definition as the language states it: the graph of the program its
/// artifact executes, and the process the definition starts.
///
/// The graph is the whole program as typed IR and carries no source text;
/// `graph.source_identity` names the artifact. A source dialect is a lens
/// over it (`lash::typescript::workflow_graph::source_view`).
#[derive(Clone, Debug, PartialEq)]
pub struct WorkflowDocument {
    pub graph: WorkflowGraph,
    /// The exported name of the process the definition starts.
    pub entry: String,
}

/// Which process of an admitted module a definition starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkflowEntry {
    /// The one process the admitted module exports.
    Sole,
    /// The process container with this id in the submitted document.
    Process(WorkflowNodeId),
}

/// A workflow document to admit as a definition
/// ([`lash_core::ProcessDocumentProvider::admit`]).
#[derive(Clone, Debug)]
pub struct WorkflowAdmissionRequest {
    /// The document, in canonical form (what a `WorkflowDraft` exports).
    pub graph: WorkflowGraph,
    pub entry: WorkflowEntry,
    /// The environment a process of the definition would run under. Its
    /// recorded settings are the surface the document links against.
    pub env_spec: lash_core::ProcessExecutionEnvSpec,
    /// The tool catalogue that environment resolves to.
    pub tool_catalog: Arc<lash_core::ToolCatalog>,
}

/// A document admitted and its module published.
#[derive(Clone, Debug, PartialEq)]
pub struct AdmittedWorkflow {
    /// The descriptor of the definition, for the caller to publish.
    pub draft: lash_core::ProcessDefinitionDraft,
    /// The admitted document and its entry.
    pub document: WorkflowDocument,
    /// The admitted id of each node and process container of the submitted
    /// document.
    pub nodes: BTreeMap<WorkflowNodeId, WorkflowNodeId>,
}

/// The answer to a [`WorkflowAdmissionRequest`].
#[derive(Clone, Debug, PartialEq)]
pub enum WorkflowAdmissionOutcome {
    Admitted(Box<AdmittedWorkflow>),
    /// The document was refused and nothing was published.
    Refused(WorkflowAdmissionRefusal),
}

/// The definition a Lash VM payload names: a definition value, or a start
/// payload carrying one beside its arguments.
pub(crate) fn payload_definition_identity(
    payload: &serde_json::Value,
) -> Result<lash_vm::ProcessDefinitionIdentity, lash_core::PluginError> {
    let definition_value = payload.as_object().is_some_and(|fields| fields.len() == 3);
    if definition_value {
        lash_vm::ProcessDefinitionIdentity::from_process_value(payload).map_err(|error| {
            lash_core::PluginError::Session(format!("invalid lash_vm process definition: {error}"))
        })
    } else {
        LashVmProcessInput::from_payload(payload.clone())
            .map(|input| input.definition_identity())
            .map_err(|error| {
                lash_core::PluginError::Session(format!("invalid lash_vm process payload: {error}"))
            })
    }
}

pub(crate) struct LashVmDocumentProvider {
    pub(crate) engine: Arc<LashVmProcessEngine>,
}

fn unresolvable(message: String) -> lash_core::PluginError {
    lash_core::PluginError::from(
        lash_core::ProcessDefinitionRefusal::UnresolvableDefinition {
            engine_kind: LASH_VM_ENGINE_KIND.into(),
            message,
        },
    )
}

/// The exported process `entry` selects, or why it selects none.
fn entry_process(
    entry: &WorkflowEntry,
    admitted: &lash_vm_client::AdmittedDocument,
) -> Result<String, WorkflowAdmissionRefusal> {
    let exports = &admitted.artifact.exports().processes;
    let refused = |location, message: String| {
        WorkflowAdmissionRefusal::of(location, WorkflowAdmissionDiagnosticKind::Entry, message)
    };
    match entry {
        WorkflowEntry::Sole => {
            let mut names = exports.keys();
            match (names.next(), names.next()) {
                (Some(name), None) => Ok(name.clone()),
                _ => Err(refused(
                    WorkflowAdmissionLocation::Document,
                    format!(
                        "the document defines {} processes; its entry must name one",
                        exports.len()
                    ),
                )),
            }
        }
        WorkflowEntry::Process(submitted) => admitted
            .nodes
            .get(submitted)
            .and_then(|container| {
                admitted.artifact.graph.declarations.iter().find_map(
                    |declaration| match declaration {
                        lash_vm::WorkflowDeclaration::Process(process)
                            if process.id == *container =>
                        {
                            Some(process.name.clone())
                        }
                        _ => None,
                    },
                )
            })
            .filter(|name| exports.contains_key(name))
            .ok_or_else(|| {
                refused(
                    WorkflowAdmissionLocation::Node {
                        node: submitted.clone(),
                        slot: lash_vm::WorkflowSlotPath::default(),
                    },
                    "the entry is not a process the document defines".to_owned(),
                )
            }),
    }
}

#[async_trait::async_trait]
impl lash_core::ProcessDocumentProvider for LashVmDocumentProvider {
    async fn document(
        &self,
        payload: &serde_json::Value,
    ) -> Result<lash_core::ProcessDocumentRead, lash_core::PluginError> {
        let identity = payload_definition_identity(payload)?;
        let Some(inspected) = self
            .engine
            .workers
            .inspect_artifact(&self.engine.artifact_store, &identity.module_ref)
            .await?
        else {
            return Ok(lash_core::ProcessDocumentRead::ArtifactMissing {
                artifact: lash_core::ArtifactName {
                    store: lash_core::ArtifactStoreId::VmModule,
                    artifact_ref: identity.module_ref.as_str().to_owned(),
                },
            });
        };
        let process_type = inspected.process_type(&identity).map_err(unresolvable)?;
        let entry = inspected
            .process_name_for_ref(&identity.process_ref)
            .ok_or_else(|| unresolvable("process definition has no worker-verified export".into()))?
            .to_owned();
        let draft = identity
            .draft()
            .map_err(|error| unresolvable(error.to_string()))?;
        Ok(lash_core::ProcessDocumentRead::Inspected(Box::new(
            lash_core::InspectedProcessDefinition {
                definition: lash_core::ProcessDefinition::new(
                    draft.id(),
                    lash_core::ProcessSignature::known(lash_vm_type_expr_schema(&process_type)),
                ),
                document: lash_core::ProcessDocument::new(WorkflowDocument {
                    graph: inspected.graph,
                    entry,
                }),
            },
        )))
    }

    async fn document_ref(
        &self,
        payload: &serde_json::Value,
    ) -> Result<lash_core::ProcessDocumentRefRead, lash_core::PluginError> {
        let identity = payload_definition_identity(payload)?;
        let Some(inspected) = self
            .engine
            .workers
            .inspect_artifact(&self.engine.artifact_store, &identity.module_ref)
            .await?
        else {
            return Ok(lash_core::ProcessDocumentRefRead::ArtifactMissing {
                artifact: lash_core::ArtifactName {
                    store: lash_core::ArtifactStoreId::VmModule,
                    artifact_ref: identity.module_ref.as_str().to_owned(),
                },
            });
        };
        Ok(lash_core::ProcessDocumentRefRead::Named(
            lash_trace::WorkflowDocumentRef {
                source_identity: inspected.source_identity(),
                module_ref: identity.module_ref.to_string(),
                entry: lash_trace::WorkflowDocumentEntry::Process {
                    process_ref: lash_vm::process_ref_key(&identity.process_ref),
                },
                ir_version: inspected.graph.ir_version,
            },
        ))
    }

    async fn admit(
        &self,
        claim: &lash_core::ReferrerClaim,
        request: lash_core::ProcessDocument,
    ) -> Result<lash_core::ProcessDocument, lash_core::PluginError> {
        let Ok(request) = request.downcast::<WorkflowAdmissionRequest>() else {
            return Err(lash_core::PluginError::Invoke(
                "the lash_vm engine admits a WorkflowAdmissionRequest".to_owned(),
            ));
        };
        // The surface a process created under this environment records, over
        // the catalogue it resolves to: what a run of the definition links
        // and checks its requirements against.
        let environment = self
            .engine
            .recorded_settings(&request.env_spec)?
            .into_surface()
            .host_environment(&request.tool_catalog)
            .map_err(|error| {
                lash_core::PluginError::Session(format!(
                    "invalid lash_vm host tool surface: {error}"
                ))
            })?;
        let refused = |refusal| {
            Ok(lash_core::ProcessDocument::new(
                WorkflowAdmissionOutcome::Refused(refusal),
            ))
        };
        let admitted = match self
            .engine
            .workers
            .admit_document(request.graph, environment)
            .await
            .map_err(|error| lash_core::PluginError::Runtime(error.into_runtime_error()))?
        {
            Ok(admitted) => admitted,
            Err(refusal) => return refused(refusal),
        };
        let entry = match entry_process(&request.entry, &admitted) {
            Ok(entry) => entry,
            Err(refusal) => return refused(refusal),
        };
        let draft = admitted
            .artifact
            .definition_identity(&entry)
            .ok_or_else(|| unresolvable("the entry has no worker-verified export".into()))?
            .draft()
            .map_err(|error| unresolvable(error.to_string()))?;
        // Nothing is left to refuse: only now does anything reach a store.
        self.engine
            .artifact_store
            .publish_module_artifact(claim, &admitted.artifact)
            .await?;
        Ok(lash_core::ProcessDocument::new(
            WorkflowAdmissionOutcome::Admitted(Box::new(AdmittedWorkflow {
                draft,
                document: WorkflowDocument {
                    graph: admitted.artifact.graph,
                    entry,
                },
                nodes: admitted.nodes,
            })),
        ))
    }
}
