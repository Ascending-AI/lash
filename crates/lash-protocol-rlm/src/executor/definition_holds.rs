use super::{RlmExecutionState, RuntimeExecutionContext, frame_environment};

/// Acquire every worker-discovered definition and its manifest under the frame.
pub(super) async fn hold_global_definitions(
    state: &mut RlmExecutionState,
    ctx: &RuntimeExecutionContext<'_>,
) -> Result<(), String> {
    hold_definitions(ctx, state.vm.state().referenced_definition_ids()).await
}

/// Acquire, under the frame, every definition a cell's continuation
/// references (FIG-4739): the cell stopped at a segment boundary before it
/// bound them to a global, and the segment that resumes it reads them.
pub(super) async fn hold_continuation_definitions(
    ctx: &RuntimeExecutionContext<'_>,
    vm: &lash_vm_protocol::OpaqueVmState,
) -> Result<(), String> {
    hold_definitions(ctx, vm.definition_ids().iter().cloned()).await
}

async fn hold_definitions(
    ctx: &RuntimeExecutionContext<'_>,
    ids: impl IntoIterator<Item = lash_core::ProcessDefinitionId>,
) -> Result<(), String> {
    if frame_environment(ctx).is_none() {
        return Ok(());
    }
    let engines = ctx.definition_engines();
    let mut ids = ids.into_iter().peekable();
    if ids.peek().is_none() {
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
