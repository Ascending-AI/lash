//! Execution-scoped immutable resolution winners and their owner fence.
use std::collections::BTreeMap;

use lash_core_store::artifact_referrer::ArtifactReferrer;
pub use lash_core_store::resolutions::{
    ExecutionId, ResolutionEntry, ResolutionKey, ResolutionRecord,
};
pub use lash_vocabulary::claims::ClaimRequirements;
use lash_vocabulary::names::{OperationUse, QualifiedName};
use serde::{Deserialize, Serialize};

use crate::{ActorKey, DurableError, Epoch, StateRevision};

/// Facts recorded by the admitting owner, before any deferred use can resolve.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionRegistration {
    pub execution: ExecutionId,
    #[serde(with = "lash_vocabulary::ordered_pairs")]
    pub uses: BTreeMap<OperationUse, QualifiedName>,
    pub referrer: ArtifactReferrer,
    pub requirements: ClaimRequirements,
}

/// The same immutable outcome is returned to every competing first caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommitResolution {
    Inserted(ResolutionRecord),
    Existing(ResolutionRecord),
}

/// A resolution commit has exactly the actor's existing ownership authority.
#[derive(Clone, Debug)]
pub struct ResolutionOwner {
    pub actor: ActorKey,
    pub epoch: Epoch,
    pub expected_revision: StateRevision,
}

#[async_trait::async_trait]
pub trait ExecutionResolutions: Send + Sync {
    async fn requirements(&self, actor: &ActorKey) -> Result<ClaimRequirements, DurableError>;
    async fn load(&self, key: &ResolutionKey) -> Result<Option<ResolutionRecord>, DurableError>;
    async fn commit(
        &self,
        owner: &ResolutionOwner,
        candidate: ResolutionRecord,
    ) -> Result<CommitResolution, DurableError>;
}

impl ExecutionRegistration {
    /// The recorded use, requested name and owning actor are checked together.
    pub fn validate(&self, candidate: &ResolutionRecord) -> Result<(), DurableError> {
        if self.execution != candidate.key.execution
            || self.uses.get(&candidate.key.use_) != Some(&candidate.requested)
        {
            return Err(crate::packages::PackageRefusal::ExecutionUse.into());
        }
        validate_resolution(candidate)
    }
}

/// Contradictory repeated route/definition facts are refused on writes and reads.
pub fn validate_resolution(candidate: &ResolutionRecord) -> Result<(), DurableError> {
    if let ResolutionEntry::Resolved {
        definition,
        contract,
        route,
        mode,
        ..
    } = &candidate.entry
    {
        use lash_core_store::tool_surface::linked::RouteSource;
        if *mode != lash_vocabulary::package::OperationMode::Waited
            || route.policy == lash_vocabulary::ExecutionPolicy::Read
            || definition.contract_id() != *contract
            || route.tool != definition.manifest.id
            || route.policy != definition.manifest.execution_policy
            || route.projection != definition.manifest.argument_projection
            || !matches!(&route.source, RouteSource::Resolved { definition: recorded } if recorded == definition)
        {
            return Err(crate::packages::PackageRefusal::ResolutionTarget.into());
        }
    }
    Ok(())
}

/// Fold source records, never a second claim registry.
pub fn requirements_of(
    registrations: Vec<String>,
    resolutions: Vec<String>,
) -> Result<ClaimRequirements, DurableError> {
    let mut requirements = ClaimRequirements::default();
    let corrupt = |error: serde_json::Error| {
        DurableError::Store(crate::StoreFailure {
            kind: crate::StoreFailureKind::Corrupt,
            message: format!("execution requirements: {error}"),
        })
    };
    for json in registrations {
        let registration: ExecutionRegistration = serde_json::from_str(&json).map_err(corrupt)?;
        requirements.extend(&registration.requirements);
    }
    for json in resolutions {
        let record: ResolutionRecord = serde_json::from_str(&json).map_err(corrupt)?;
        validate_resolution(&record)?;
        if let ResolutionEntry::Resolved {
            requirements: target,
            ..
        } = record.entry
        {
            requirements.extend(&target);
        }
    }
    Ok(requirements)
}

impl ExecutionRegistration {
    /// Adoption may extend executable reach, retaining old callable uses and debt.
    pub fn adopt(&self, successor: &Self) -> Result<Self, DurableError> {
        if self.execution != successor.execution
            || self.referrer != successor.referrer
            || self
                .uses
                .iter()
                .any(|(use_, name)| successor.uses.get(use_) != Some(name))
        {
            return Err(crate::packages::PackageRefusal::ExecutionUse.into());
        }
        let mut successor = successor.clone();
        successor.requirements.extend(&self.requirements);
        Ok(successor)
    }
}
