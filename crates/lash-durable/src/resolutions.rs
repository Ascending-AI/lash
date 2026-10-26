//! Execution-scoped immutable resolution winners and their owner fence.
use std::collections::BTreeMap;

use lash_core_store::artifact_referrer::ArtifactReferrer;
pub use lash_core_store::resolutions::{
    ExecutionId, ResolutionEntry, ResolutionKey, ResolutionRecord,
};
pub use lash_vocabulary::claims::ClaimRequirements;
use serde::{Deserialize, Serialize};
use {lash_kernel_value::names::OperationUse, lash_kernel_value::names::QualifiedName};

use crate::{ActorCommit, ActorKey, DurableError, Epoch, StateRevision};

/// Facts recorded by the admitting owner, before any deferred use can resolve.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionRegistration {
    pub execution: ExecutionId,
    #[serde(with = "lash_vocabulary::ordered_pairs")]
    pub uses: BTreeMap<OperationUse, DeferredUse>,
    pub referrer: ArtifactReferrer,
    pub requirements: ClaimRequirements,
}

/// One live deferred use and its discovery obligation, before a winner exists.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeferredUse {
    pub requested: QualifiedName,
    pub discovery: ClaimRequirements,
}

/// The same immutable outcome is returned to every competing first caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommitResolution {
    Inserted(ResolutionRecord),
    Existing(ResolutionRecord),
}

/// The winner and the actor authority acknowledged by the same transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcknowledgedResolution {
    pub result: CommitResolution,
    pub actor: ActorCommit,
}

/// A resolution commit has exactly the actor's existing ownership authority.
#[derive(Clone, Debug)]
pub struct ResolutionOwner {
    /// A retained process preimage at exactly expected_revision.
    pub process_view: Option<crate::domain::ProcessView>,
    pub actor: ActorKey,
    pub epoch: Epoch,
    pub expected_revision: StateRevision,
}

#[async_trait::async_trait]
pub trait ExecutionResolutions: Send + Sync {
    async fn requirements(&self, actor: &ActorKey) -> Result<ClaimRequirements, DurableError>;
    async fn load(&self, key: &ResolutionKey) -> Result<Option<ResolutionRecord>, DurableError>;
    /// Compatibility result over the same fenced acknowledgement path.
    async fn commit(
        &self,
        owner: &ResolutionOwner,
        candidate: ResolutionRecord,
    ) -> Result<CommitResolution, DurableError> {
        Ok(self.commit_acknowledged(owner, candidate).await?.result)
    }

    /// Fence and advance authority once, even when returning an existing winner.
    /// A retained winner needs neither this call nor a new revision.
    async fn commit_acknowledged(
        &self,
        owner: &ResolutionOwner,
        candidate: ResolutionRecord,
    ) -> Result<AcknowledgedResolution, DurableError>;
}

impl ExecutionRegistration {
    /// The recorded use, requested name and owning actor are checked together.
    pub fn validate(&self, candidate: &ResolutionRecord) -> Result<(), DurableError> {
        if self.execution != candidate.key.execution
            || self
                .uses
                .get(&candidate.key.use_)
                .map(|use_| &use_.requested)
                != Some(&candidate.requested)
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
        use lash_core_store::tool_surface::RouteSource;
        if *mode != lash_kernel_value::package::OperationMode::Waited
            || route.policy == lash_kernel_value::ExecutionPolicy::Read
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
    let mut winners = BTreeMap::new();
    for json in resolutions {
        let record: ResolutionRecord = serde_json::from_str(&json).map_err(corrupt)?;
        validate_resolution(&record)?;
        winners.insert(record.key.clone(), record);
    }
    for json in registrations {
        let registration: ExecutionRegistration = serde_json::from_str(&json).map_err(corrupt)?;
        requirements.extend(&registration.requirements);
        for (use_, discovery) in &registration.uses {
            match winners.get(&ResolutionKey {
                execution: registration.execution.clone(),
                use_: *use_,
            }) {
                Some(record) => {
                    registration.validate(record)?;
                    if let ResolutionEntry::Resolved {
                        requirements: target,
                        ..
                    } = &record.entry
                    {
                        requirements.extend(target);
                    }
                }
                None => requirements.extend(&discovery.discovery),
            }
        }
    }
    Ok(requirements)
}

impl ExecutionRegistration {
    /// Adoption replaces live membership and stable debt. Historical winners stay
    /// immutable in their own rows; dead uses owe no discovery or target debt.
    pub fn adopt(&self, successor: &Self) -> Result<Self, DurableError> {
        if self.execution != successor.execution
            || self.referrer != successor.referrer
            || self.uses.iter().any(|(use_, name)| {
                successor
                    .uses
                    .get(use_)
                    .is_some_and(|next| next.requested != name.requested)
            })
        {
            return Err(crate::packages::PackageRefusal::ExecutionUse.into());
        }
        Ok(successor.clone())
    }
}
