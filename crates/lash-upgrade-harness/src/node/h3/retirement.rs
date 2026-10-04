//! The operator floor: independent drain evidence precedes forced retirement.
use lash::GenerationDrainStatus;
use lash_core::engine::BuildGeneration;

#[derive(Debug)]
pub enum RetirementRefusal {
    OwnedWork { status: Box<GenerationDrainStatus> },
    DrainReadFailed { cause: Box<lash::EmbedError> },
    DeploymentMismatch { deployment: String },
}

/// A read failure produces a typed refusal, never an empty drain report.
pub async fn proof(
    core: &lash::LashCore,
    generation: &BuildGeneration,
) -> Result<GenerationDrainStatus, RetirementRefusal> {
    let status = core
        .generation_drain_status(generation)
        .await
        .map_err(|cause| RetirementRefusal::DrainReadFailed {
            cause: Box::new(cause),
        })?;
    if !status.drained() {
        return Err(RetirementRefusal::OwnedWork {
            status: Box::new(status),
        });
    }
    Ok(status)
}

/// The double's real deployment operation uses the same operator precondition
/// as the live adapter, and never sends a forced removal after a refused read.
pub async fn retire_double(
    core: &lash::LashCore,
    double: &lash_restate_test::RestateTestBackend<dyn lash::StoreSet>,
    generation: &BuildGeneration,
    deployment: &lash_restate_test::DeploymentId,
) -> anyhow::Result<Result<GenerationDrainStatus, RetirementRefusal>> {
    let status = match proof(core, generation).await {
        Ok(status) => status,
        Err(refusal) => return Ok(Err(refusal)),
    };
    let registry = double.lash_backend().deployment_registry();
    let actual = registry.deployments_serving(generation).await?;
    if !actual
        .iter()
        .any(|candidate| candidate.id == deployment.as_str())
    {
        return Ok(Err(RetirementRefusal::DeploymentMismatch {
            deployment: deployment.as_str().into(),
        }));
    }
    double
        .server()
        .remove_deployment(deployment, true)
        .map_err(|error| anyhow::anyhow!("drained deployment removal refused: {error:?}"))?;
    Ok(Ok(status))
}

/// Live retirement preserves the authoritative registry/read provenance.
pub async fn retire_live(
    core: &lash::LashCore,
    view: &crate::restate_view::RestateView,
    generation: &BuildGeneration,
    deployment: &str,
) -> anyhow::Result<Result<GenerationDrainStatus, RetirementRefusal>> {
    let status = match proof(core, generation).await {
        Ok(status) => status,
        Err(refusal) => return Ok(Err(refusal)),
    };
    let actual = view.deployments_of_generation(generation.as_str()).await?;
    if !actual.iter().any(|candidate| candidate.id == deployment) {
        return Ok(Err(RetirementRefusal::DeploymentMismatch {
            deployment: deployment.into(),
        }));
    }
    view.remove_deployment(deployment).await?;
    Ok(Ok(status))
}
