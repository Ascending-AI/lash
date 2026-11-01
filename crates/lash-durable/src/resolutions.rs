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

fn corrupt(detail: impl std::fmt::Display) -> DurableError {
    DurableError::Store(crate::StoreFailure {
        code: None,
        kind: crate::StoreFailureKind::Corrupt,
        message: format!("execution membership: {detail}"),
    })
}

fn decode<T: serde::de::DeserializeOwned>(json: &str) -> Result<T, DurableError> {
    serde_json::from_str(json).map_err(corrupt)
}

fn json<T: Serialize>(value: &T) -> Result<String, DurableError> {
    serde_json::to_string(value).map_err(corrupt)
}

fn use_key(execution: &ExecutionId, link: &str, slot: &str) -> Result<ResolutionKey, DurableError> {
    use lash_kernel_value::names::{ImportSlot, OperationSlot};
    let link = link.parse().map_err(corrupt)?;
    let parsed = if let Some(slot) = slot.strip_prefix("import:") {
        OperationSlot::Import(ImportSlot(slot.parse().map_err(corrupt)?))
    } else if let Some(slot) = slot.strip_prefix("helper:") {
        OperationSlot::Helper(slot.parse().map_err(corrupt)?)
    } else {
        return Err(corrupt("unknown operation slot"));
    };
    if parsed.to_string() != slot {
        return Err(corrupt("noncanonical operation slot"));
    }
    Ok(ResolutionKey {
        execution: execution.clone(),
        use_: OperationUse { link, slot: parsed },
    })
}

fn validate_use(
    key: &ResolutionKey,
    declaration: &DeferredUse,
    candidate: &ResolutionRecord,
) -> Result<(), DurableError> {
    if candidate.key != *key || candidate.requested != declaration.requested {
        return Err(crate::packages::PackageRefusal::ExecutionUse.into());
    }
    validate_resolution(candidate)
}

/// A registration as the stores write it: its JSON columns, its referrer
/// decoded, and its debt decoded into atoms, so a store locks and fences the
/// referrer and merges the debt without decoding JSON (FIG-7576).
pub struct RegistrationRows {
    pub referrer: String,
    pub referrer_kind: &'static str,
    pub referrer_id: String,
    pub requirements: String,
    /// A JSON array of `[link, slot, use_json, requested_json]`.
    pub uses: String,
    /// A JSON array of `[link, slot, source, family, token, detail]`: the
    /// stable debt (empty link and slot) and each use's discovery debt.
    pub debts: String,
}

/// Each atom of `requirements` ([`ClaimRequirements::atoms`]) as a debt row
/// of `source` keyed by `link` and `slot`.
fn debt_rows(
    rows: &mut Vec<[String; 6]>,
    link: &str,
    slot: &str,
    source: &str,
    requirements: &ClaimRequirements,
) {
    for (family, token, detail) in requirements.atoms() {
        rows.push([
            link.to_owned(),
            slot.to_owned(),
            source.to_owned(),
            family.to_owned(),
            token,
            detail,
        ]);
    }
}

/// The target debt a resolution owes once it wins its use, as debt rows
/// ([`RegistrationRows::debts`]): none unless it resolved.
pub fn resolution_debts(record: &ResolutionRecord) -> Result<String, DurableError> {
    let mut rows = Vec::new();
    if let ResolutionEntry::Resolved { requirements, .. } = &record.entry {
        debt_rows(
            &mut rows,
            &record.key.use_.link.to_string(),
            &record.key.use_.slot.to_string(),
            "target",
            requirements,
        );
    }
    json(&rows)
}

impl ExecutionRegistration {
    /// Referrer, stable debt and a bulk array of keyed current declarations.
    /// The arrays are insertion parameters, never a second stored membership.
    pub fn storage_rows(&self) -> Result<RegistrationRows, DurableError> {
        let mut debts = Vec::new();
        debt_rows(&mut debts, "", "", "stable", &self.requirements);
        let uses = self
            .uses
            .iter()
            .map(|(key, declaration)| {
                let (link, slot) = (key.link.to_string(), key.slot.to_string());
                debt_rows(
                    &mut debts,
                    &link,
                    &slot,
                    "discovery",
                    &declaration.discovery,
                );
                Ok((
                    link,
                    slot,
                    json(declaration)?,
                    json(&declaration.requested)?,
                ))
            })
            .collect::<Result<Vec<_>, DurableError>>()?;
        Ok(RegistrationRows {
            referrer: json(&self.referrer)?,
            referrer_kind: self.referrer.kind().as_str(),
            referrer_id: self.referrer.canonical_id(),
            requirements: json(&self.requirements)?,
            uses: json(&uses)?,
            debts: json(&debts)?,
        })
    }

    /// Reconstruct current membership under the store's protected transaction.
    /// Ended registrations retain these rows for exact registration replay.
    pub fn from_storage(
        execution: ExecutionId,
        referrer: &str,
        requirements: &str,
        rows: Vec<(String, String, String)>,
    ) -> Result<Self, DurableError> {
        let mut uses = BTreeMap::new();
        for (link, slot, declaration) in rows {
            let key = use_key(&execution, &link, &slot)?;
            if uses
                .insert(key.use_, decode::<DeferredUse>(&declaration)?)
                .is_some()
            {
                return Err(corrupt("duplicate current use"));
            }
        }
        Ok(Self {
            execution,
            uses,
            referrer: decode(referrer)?,
            requirements: decode(requirements)?,
        })
    }
}

/// Validate one keyed current declaration before a candidate can win or seed.
/// Its actor, referrer and declaration come from one joined store observation.
pub fn validate_current_use(
    actor: &ActorKey,
    candidate: &ResolutionRecord,
    row: Option<(String, String, String)>,
) -> Result<ArtifactReferrer, DurableError> {
    let Some((owner, referrer, declaration)) = row else {
        return Err(crate::packages::PackageRefusal::ExecutionUse.into());
    };
    if owner != actor.as_str() {
        return Err(crate::packages::PackageRefusal::ExecutionUse.into());
    }
    let declaration = decode(&declaration)?;
    validate_use(&candidate.key, &declaration, candidate)?;
    decode(&referrer)
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
