//! Prepare the engine-store share before a successor frame activates in SQL.

use crate::{
    ArtifactStoreId, ProcessDefinitionId, ProcessEngineRegistry, ReferrerAcquisition,
    ReferrerClaim, ReferrerGuard, StoreError,
};

pub(super) async fn prepare(
    engines: &ProcessEngineRegistry,
    transition: Option<&crate::store::FrameTransition>,
) -> Result<(), StoreError> {
    let Some(transition) = transition else {
        return Ok(());
    };
    if !transition
        .carries
        .iter()
        .any(|name| name.store == ArtifactStoreId::ProcessDefinition)
    {
        return Ok(());
    }
    let ports = engines.artifact_ports().ok_or_else(|| {
        StoreError::Backend("definition frame carry has no artifact ports".into())
    })?;
    let claim = ReferrerClaim::guarded(ReferrerGuard::Frame {
        frame: transition.successor.clone(),
        creator: transition.gate.clone(),
    });
    let mut names = std::collections::BTreeSet::new();
    for name in &transition.carries {
        if name.store != ArtifactStoreId::ProcessDefinition {
            continue;
        }
        let id = ProcessDefinitionId::parse(&name.artifact_ref)
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        let resolved = ports
            .read_definition(engines, &id)
            .await
            .map_err(|error| StoreError::Backend(error.to_string()))?
            .ok_or_else(|| StoreError::ArtifactCarryMissing {
                artifact_ref: name.artifact_ref.clone(),
                to: claim.referrer(),
            })?;
        names.extend(
            resolved
                .draft
                .artifacts()
                .iter()
                .filter(|artifact| matches!(artifact.store, ArtifactStoreId::Engine(_)))
                .cloned(),
        );
    }
    match ports
        .acquire(engines, &claim, &names.into_iter().collect::<Vec<_>>())
        .await
        .map_err(|error| StoreError::Backend(error.to_string()))?
    {
        ReferrerAcquisition::Held => Ok(()),
        ReferrerAcquisition::Ended => Err(StoreError::ArtifactReferrerEnded {
            referrer: claim.referrer(),
        }),
    }
}
