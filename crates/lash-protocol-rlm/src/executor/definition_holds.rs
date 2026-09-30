use super::{RlmExecutionState, RuntimeExecutionContext, frame_environment};

/// Acquire every worker-discovered definition and its manifest under the frame.
pub(super) async fn hold_global_definitions(
    state: &mut RlmExecutionState,
    ctx: &RuntimeExecutionContext<'_>,
) -> Result<(), String> {
    if frame_environment(ctx).is_none() {
        return Ok(());
    }
    let engines = ctx.definition_engines();
    let ids = state.vm.state().referenced_definition_ids();
    if ids.is_empty() {
        return Ok(());
    }
    let ports = engines
        .artifact_ports()
        .ok_or_else(|| "definition artifact ports are unavailable".to_string())?;
    let claim = ctx.frame_claim().map_err(|error| error.to_string())?;
    for id in ids {
        match ports
            .acquire_definition(engines, &claim, &id)
            .await
            .map_err(|e| e.to_string())?
        {
            lash_core::DefinitionAcquisition::Held(_) => {}
            lash_core::DefinitionAcquisition::Ended => return Ok(()),
        }
    }
    Ok(())
}
