pub(super) async fn hold_segment_definitions(
    ctx: &lash_core::RuntimeExecutionContext<'_>,
    ids: &[lash_core::ProcessDefinitionId],
) -> Result<(), lash_core::ProcessInfraError> {
    if ids.is_empty() {
        return Ok(());
    }
    let infra = |message: String| {
        lash_core::ProcessInfraError::new(lash_core::PluginError::attempt_fault(message))
    };
    let engines = ctx.definition_engines();
    let ports = engines
        .artifact_ports()
        .ok_or_else(|| infra("definition artifact ports are unavailable".into()))?;
    let claim = ctx
        .execution_claim()
        .map_err(|error| infra(error.to_string()))?;
    for id in ids {
        match ports
            .acquire_definition(engines, &claim, id)
            .await
            .map_err(|error| infra(error.to_string()))?
        {
            lash_core::DefinitionAcquisition::Held(_) => {}
            lash_core::DefinitionAcquisition::Ended => break,
        }
    }
    Ok(())
}
