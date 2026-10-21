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
use lash_vocabulary::names::OperationUse;

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
    let provider = composition
        .deferred_package_provider(&grant.provider)
        .ok_or_else(|| {
            EnginePortError::Refused(AdmissionRefusal::ProviderNotHostable {
                provider: grant.provider.clone(),
                registry: environment.registry,
            })
        })?;
    let handle = lash_vocabulary::providers::ToolProviderHandle::from_name(
        environment.registry,
        &grant.provider,
    )
    .map_err(|error| ResolutionError::Corrupt(error.to_string()))?;
    let cx = ctx.actor_context();
    let tx = cx.begin().await?;
    let store = cx.backend().stores().execution_resolutions();
    let cancel = ctx.cancellation();
    FirstUse {
        store: store.as_ref(),
        owner: lash_durable::resolutions::ResolutionOwner {
            actor: cx.actor().clone(),
            epoch: cx.epoch(),
            expected_revision: tx.revision(),
        },
        execution: execution.clone(),
        environment,
        capabilities: cx.capabilities(),
        provider: provider.as_ref(),
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
