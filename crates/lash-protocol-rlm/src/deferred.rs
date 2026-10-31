//! First-use package discovery. Lowering never invokes a resolver.
use lash_core::{
    RuntimeExecutionContext,
    facade_support::{CommittedUse, FirstUse, ResolutionError},
};
use lash_core_store::{
    resolutions::ExecutionId,
    tool_surface::{ExecutionEnvironment, UseBinding},
};
use lash_execution_contracts::process::linking::{
    AdmissionRefusal, EnginePortError, LinkInspection, OperationNeed,
};
use lash_kernel_value::names::OperationUse;

pub type DeferredResolutionError = ResolutionError;

/// The immutable winner is durable before the caller projects or fits arguments.
pub(crate) async fn resolve_use(
    ctx: &RuntimeExecutionContext<'_>,
    execution: &ExecutionId,
    environment: &ExecutionEnvironment,
    inspection: &LinkInspection,
    use_: OperationUse,
) -> Result<CommittedUse, ResolutionError> {
    let Some(UseBinding::Deferred(grant)) = environment.uses.get(&use_) else {
        return Err(EnginePortError::Refused(AdmissionRefusal::MissingRoute { use_ }).into());
    };
    let Some(OperationNeed::Deferred { name, .. }) = inspection.operations.get(&use_) else {
        return Err(EnginePortError::Refused(AdmissionRefusal::MissingRoute { use_ }).into());
    };
    let composition = ctx.package_composition();
    let provider = composition.deferred_package_provider(&grant.provider);
    let handle = lash_vocabulary::providers::ToolProviderHandle::from_name(
        environment.registry,
        &grant.provider,
    )
    .map_err(|error| ResolutionError::Corrupt(error.to_string()))?;
    let cx = lash_core::facade_support::execution_actor(ctx);
    let cancel = ctx.cancellation();
    FirstUse {
        held: ctx.held_response(),
        cx,
        execution: execution.clone(),
        environment,
        capabilities: cx.capabilities(),
        provider: provider.map(|provider| provider.as_ref()),
        context: lash_core::DeferredResolveContext::new(
            &handle,
            ctx.owner(),
            ctx.logical_run(),
            ctx.run_capabilities(),
            ctx.owner_config(),
        ),
        validator: composition.as_ref(),
        clock: cx.clock().as_ref(),
        cancel: &cancel,
    }
    .resolve(use_, name.clone())
    .await
}
