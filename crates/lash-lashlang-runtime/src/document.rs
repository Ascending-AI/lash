//! The lashlang engine's document provider (FIG-5563): a stored definition
//! read back as its workflow graph and canonical TypeScript.
//!
//! The read goes through the engine's own worker service and module store,
//! under the same verification as execution, and names the entry process
//! through the persisted [`lashlang::ProcessRef`], never a convenience name.

use lash_vm_client::service::runtime_ops::ServiceRuntimeOps as _;

use crate::{LASHLANG_ENGINE_KIND, LashlangProcessInput, lashlang_type_expr_schema};

/// A definition as the language states it: the graph of the program its
/// artifact executes, the canonical TypeScript the graph's spans address, and
/// the process the definition starts.
///
/// The text is canonical, not authored: comments and formatting are gone
/// (ADR 0037). `graph.source_identity` names the artifact.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkflowDocument {
    pub graph: lashlang::WorkflowGraph,
    pub source: String,
    /// The exported name of the process the definition starts.
    pub entry: String,
}

/// The definition a lashlang payload names: a definition value, or a start
/// payload carrying one beside its arguments.
pub(crate) fn payload_definition_identity(
    payload: &serde_json::Value,
) -> Result<lashlang::ProcessDefinitionIdentity, lash_core::PluginError> {
    let definition_value = payload.as_object().is_some_and(|fields| fields.len() == 3);
    if definition_value {
        lashlang::ProcessDefinitionIdentity::from_process_value(payload).map_err(|error| {
            lash_core::PluginError::Session(format!("invalid lashlang process definition: {error}"))
        })
    } else {
        LashlangProcessInput::from_payload(payload.clone())
            .map(|input| input.definition_identity())
            .map_err(|error| {
                lash_core::PluginError::Session(format!(
                    "invalid lashlang process payload: {error}"
                ))
            })
    }
}

pub(crate) struct LashlangDocumentProvider {
    pub(crate) artifact_store: lashlang::LashlangArtifacts,
    pub(crate) workers: lash_vm_client::service::Service,
}

#[async_trait::async_trait]
impl lash_core::ProcessDocumentProvider for LashlangDocumentProvider {
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
                    store: lash_core::ArtifactStoreId::LashlangModule,
                    artifact_ref: identity.module_ref.as_str().to_owned(),
                },
            });
        };
        let refused = |message: String| {
            lash_core::PluginError::from(
                lash_core::ProcessDefinitionRefusal::UnresolvableDefinition {
                    engine_kind: LASHLANG_ENGINE_KIND.into(),
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
                    lash_core::ProcessSignature::known(lashlang_type_expr_schema(&process_type)),
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
