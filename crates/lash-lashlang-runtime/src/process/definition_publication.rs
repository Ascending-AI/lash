pub(super) async fn publish_exports(
    ctx: &lash_core::RuntimeExecutionContext<'_>,
    artifact: &lashlang::ModuleArtifact,
) -> Result<(), lash_core::ProcessInfraError> {
    let publication = lash_core::DeclaredModuleArtifact {
        module_ref: artifact.module_ref().to_string(),
        bytes: String::from_utf8(artifact.to_store_bytes().map_err(|error| {
            lash_core::ProcessInfraError::new(lash_core::PluginError::Session(error.to_string()))
        })?)
        .map_err(|error| {
            lash_core::ProcessInfraError::new(lash_core::PluginError::Session(error.to_string()))
        })?,
    };
    for name in artifact.exports().processes.keys() {
        let definition = lashlang::ProcessDefinitionIdentity::from_artifact_export(artifact, name)
            .ok_or_else(|| {
                lash_core::ProcessInfraError::new(lash_core::PluginError::Session(
                    "module export has no definition".into(),
                ))
            })?;
        let draft = definition.draft().map_err(|error| {
            lash_core::ProcessInfraError::new(lash_core::PluginError::Session(error.to_string()))
        })?;
        let effect_id = format!("literal-definition:{}", draft.id());
        ctx.publish_compiled_definition(effect_id, draft, Some(publication.clone()))
            .await
            .map_err(|error| lash_core::ProcessInfraError::new(error.into()))?;
    }
    Ok(())
}
