//! The Lash VM engine's document provider (FIG-5563): a stored definition
//! read back as its workflow graph and canonical TypeScript.
//!
//! The read goes through the engine's own worker service and module store,
//! under the same verification as execution, and names the entry process
//! through the persisted [`lash_vm::ProcessRef`], never a convenience name.

use lash_vm_client::service::runtime_ops::ServiceRuntimeOps as _;

use crate::{LASH_VM_ENGINE_KIND, LashVmProcessInput, lash_vm_type_expr_schema};

/// A definition as the language states it: the graph of the program its
/// artifact executes, the canonical TypeScript the graph's spans address, and
/// the process the definition starts.
///
/// The text is canonical, not authored: comments and formatting are gone
/// (ADR 0037). `graph.source_identity` names the artifact.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkflowDocument {
    pub graph: lash_vm::WorkflowGraph,
    pub source: String,
    /// The exported name of the process the definition starts.
    pub entry: String,
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
    pub(crate) artifact_store: lash_vm::LashVmArtifacts,
    pub(crate) workers: lash_vm_client::service::Service,
}

#[async_trait::async_trait]
impl lash_core::ProcessDocumentProvider for LashVmDocumentProvider {
    async fn document(
        &self,
        payload: &serde_json::Value,
    ) -> Result<lash_core::ProcessDocumentRead, lash_core::PluginError> {
        let identity = payload_definition_identity(payload)?;
        let Some(inspected) = self
            .workers
            .inspect_document(&self.artifact_store, &identity.module_ref)
            .await?
        else {
            return Ok(lash_core::ProcessDocumentRead::ArtifactMissing {
                artifact: lash_core::ArtifactName {
                    store: lash_core::ArtifactStoreId::VmModule,
                    artifact_ref: identity.module_ref.as_str().to_owned(),
                },
            });
        };
        let refused = |message: String| {
            lash_core::PluginError::from(
                lash_core::ProcessDefinitionRefusal::UnresolvableDefinition {
                    engine_kind: LASH_VM_ENGINE_KIND.into(),
                    message,
                },
            )
        };
        let process_type = inspected
            .artifact
            .process_type(&identity)
            .map_err(refused)?;
        let entry = inspected
            .artifact
            .process_name_for_ref(&identity.process_ref)
            .ok_or_else(|| refused("process definition has no worker-verified export".into()))?
            .to_owned();
        let draft = identity
            .draft()
            .map_err(|error| refused(error.to_string()))?;
        Ok(lash_core::ProcessDocumentRead::Inspected(Box::new(
            lash_core::InspectedProcessDefinition {
                definition: lash_core::ProcessDefinition::new(
                    draft.id(),
                    lash_core::ProcessSignature::known(lash_vm_type_expr_schema(&process_type)),
                ),
                document: lash_core::ProcessDocument::new(WorkflowDocument {
                    graph: inspected.graph,
                    source: inspected.source,
                    entry,
                }),
            },
        )))
    }
}
