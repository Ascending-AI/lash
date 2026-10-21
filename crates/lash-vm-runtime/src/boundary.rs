//! Code authoring reads one accepted Package Advertisement projection.
use crate::process::{KernelDocuments, WorkflowEnvironmentRequest};
use lash_execution_contracts::process::linking::{EnginePortError, NamespaceSnapshot};
use lash_vocabulary::package::{Attachment, PublicationContext};
#[derive(Clone, Debug)]
pub struct HostBoundary {
    pub snapshot: NamespaceSnapshot,
    pub context: PublicationContext,
    pub attachment: Attachment,
}
impl HostBoundary {
    pub async fn namespaces(
        &self,
        documents: &KernelDocuments,
    ) -> Result<lash_kernel_dialect::Namespaces, EnginePortError> {
        documents
            .namespace(&WorkflowEnvironmentRequest {
                snapshot: self.snapshot.clone(),
                context: self.context.clone(),
                attachment: self.attachment.clone(),
            })
            .await
    }
}
pub fn effect_controls(
    controls: &lash_vocabulary::TurnControls,
) -> std::collections::BTreeSet<lash_kernel_dialect::EffectControl> {
    controls
        .iter()
        .map(|kind| match kind {
            lash_vocabulary::TurnControlKind::Finish => lash_kernel_dialect::EffectControl::Finish,
            lash_vocabulary::TurnControlKind::SwitchAgentFrame => {
                lash_kernel_dialect::EffectControl::SwitchAgentFrame
            }
        })
        .collect()
}
pub use lash_vocabulary::schema::{
    MAX_PROJECTED_TYPE_NODES, MAX_RESOLVED_REFERENCES, SchemaConflict, SchemaProjectionRefusal,
    type_of_schema,
};
