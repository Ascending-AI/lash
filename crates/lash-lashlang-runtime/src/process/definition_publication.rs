pub(super) async fn publish_exports(
    ctx: &lash_core::RuntimeExecutionContext<'_>,
    artifact: &lash_vm_client::InspectedArtifact,
) -> Result<(), lash_core::ProcessInfraError> {
    let publication = lash_core::DeclaredModuleArtifact {
        module_ref: artifact.module_ref().to_string(),
        bytes: String::from_utf8(artifact.bytes().to_vec()).map_err(|error| {
            lash_core::ProcessInfraError::new(lash_core::PluginError::attempt_fault(
                error.to_string(),
            ))
        })?,
    };
    for name in artifact.exports().processes.keys() {
        let definition = artifact.definition_identity(name).ok_or_else(|| {
            lash_core::ProcessInfraError::new(lash_core::PluginError::attempt_fault(
                "module export has no definition",
            ))
        })?;
        let draft = definition.draft().map_err(|error| {
            lash_core::ProcessInfraError::new(lash_core::PluginError::attempt_fault(
                error.to_string(),
            ))
        })?;
        let effect_id = format!("literal-definition:{}", draft.id());
        ctx.publish_compiled_definition(effect_id, draft, Some(publication.clone()))
            .await
            .map_err(|error| lash_core::ProcessInfraError::new(error.into()))?;
    }
    Ok(())
}
