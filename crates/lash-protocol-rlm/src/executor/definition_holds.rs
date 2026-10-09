use super::{
    FrameHoldError, RlmExecutionState, RuntimeExecutionContext, acquire_frame_edge,
    frame_environment,
};

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

/// Publish a cell's module under the cell's execution, then hold it in the
/// frame (ADR 0113 §3.1). The execution edge protects the bytes while the
/// cell's journal may replay; the frame edge keeps them for the globals that
/// name them. A module the frame already holds is not published again.
pub(super) async fn publish_cell_module(
    state: &mut RlmExecutionState,
    ctx: &RuntimeExecutionContext<'_>,
    artifact_store: &lash_vm::LashVmArtifacts,
    artifact: &lash_vm_client::InspectedArtifact,
) -> Result<(), String> {
    let frame = frame_environment(ctx);
    let module_ref = artifact.module_ref();
    if frame
        .as_ref()
        .is_some_and(|frame| state.frame_holds(frame, module_ref))
    {
        return Ok(());
    }
    let claim = ctx.execution_claim().map_err(|error| error.to_string())?;
    artifact_store
        .publish_module_artifact(&claim, artifact)
        .await
        .map_err(|error| error.to_string())?;
    let publication = lash_core::DeclaredModuleArtifact {
        module_ref: artifact.module_ref().to_string(),
        bytes: String::from_utf8(artifact.bytes().to_vec()).map_err(|error| error.to_string())?,
    };
    for name in artifact.exports().processes.keys() {
        let identity = artifact
            .definition_identity(name)
            .ok_or_else(|| format!("module has no process `{name}`"))?;
        let draft = identity.draft().map_err(|error| error.to_string())?;
        let definition = ctx
            .publish_compiled_definition(
                format!("literal-definition:{}", draft.id()),
                draft,
                Some(publication.clone()),
            )
            .await
            .map_err(|error| error.to_string())?;
        if frame.is_some() {
            let engines = ctx.definition_engines();
            let ports = engines
                .artifact_ports()
                .ok_or_else(|| "definition artifact ports are unavailable".to_string())?;
            let claim = ctx.frame_claim().map_err(|error| error.to_string())?;
            match ports
                .acquire_definition(engines, &claim, &definition.id)
                .await
                .map_err(|error| error.to_string())?
            {
                lash_core::DefinitionAcquisition::Held(_) => {}
                lash_core::DefinitionAcquisition::Ended => return Ok(()),
            }
        }
    }
    let Some(frame) = frame else {
        return Ok(());
    };
    match acquire_frame_edge(&frame, artifact_store, module_ref).await {
        Ok(()) => {
            state.record_frame_hold(&frame, module_ref.clone());
            Ok(())
        }
        Err(FrameHoldError::Ended) => Ok(()),
        Err(FrameHoldError::Store(error)) => Err(error.to_string()),
    }
}
