//! Explicit publication revisions use the current ledger without changing an alias.
use super::*;
use lash_kernel_doc::{
    HelperPublicationUpgrade, HelperPublicationUpgradeRequest, HelperUpgradeRefusal,
};

impl HostArtifacts {
    pub async fn upgrade_publication(
        &self,
        pin: &HostArtifactPin,
        request: &HelperPublicationUpgradeRequest,
    ) -> Result<std::result::Result<HelperPublicationUpgrade, HelperUpgradeRefusal>> {
        Ok(self
            .documents()?
            .upgrade_publication(&claim(pin)?, request)
            .await
            .map_err(|error| publication_error(pin, error))?)
    }

    /// Upgrade a retained linked process definition and publish a new definition revision.
    pub async fn upgrade_definition(
        &self,
        pin: &HostArtifactPin,
        definition: &ProcessDefinitionId,
    ) -> Result<
        std::result::Result<(ProcessDefinition, HelperPublicationUpgrade), HelperUpgradeRefusal>,
    > {
        let draft = self
            .definition_ports
            .read_definition_draft(definition)
            .await?;
        let Some(draft) = draft else {
            return Err(lash_execution_contracts::PluginError::Session(
                "definition is not retained".into(),
            )
            .into());
        };
        let definition =
            lash_vm_runtime::KernelProcessDefinition::from_payload(draft.value().as_json())
                .map_err(|e| lash_execution_contracts::PluginError::Session(e.to_string()))?;
        let (code, publication) = match self
            .documents()?
            .upgrade_code(&claim(pin)?, &definition.code)
            .await
            .map_err(|error| publication_error(pin, error))?
        {
            Ok(upgraded) => upgraded,
            Err(refusal) => return Ok(Err(refusal)),
        };
        let draft =
            lash_vm_runtime::definition_draft(&lash_vm_runtime::KernelProcessDefinition { code })
                .map_err(|e| lash_execution_contracts::PluginError::Session(e.to_string()))?;
        let definition = self.publish_definition(pin, &draft).await?;
        Ok(Ok((definition, publication)))
    }
}
