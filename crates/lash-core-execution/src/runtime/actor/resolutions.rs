//! First-use discovery is repeatable until a fenced immutable winner commits.
//! This module never accepts arguments and never executes the selected target.
//! Each invocation fits and admits separately after obtaining the winner.
use std::collections::BTreeMap;

use lash_core_store::resolutions::{ExecutionId, ResolutionEntry, ResolutionKey, ResolutionRecord};
use lash_core_store::tool_surface::{ExecutionEnvironment, RouteSource, UseBinding};
use lash_durable::DurableError;
use lash_durable::resolutions::ExecutionRegistration;
use lash_execution_contracts::process::linking::{
    AdmissionRefusal, EnginePortError, LinkInspection, OperationNeed,
};
use tokio_util::sync::CancellationToken;
use {
    lash_kernel_value::ExecutionPolicy, lash_kernel_value::names::*, lash_kernel_value::package::*,
    lash_vocabulary::claims::*,
};

use crate::deferred_provider::DeferredResolveContext;
use crate::plugin::packages::{DeferredAnswer, DeferredPackageProvider, ResolverFault};

/// A committed outcome can be consumed here, or must be handed to a capable node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommittedUse {
    Ready(ResolutionRecord),
    Parked {
        record: ResolutionRecord,
        unmet: Box<ClaimRequirements>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ResolutionError {
    #[error(transparent)]
    Durable(#[from] DurableError),
    #[error(transparent)]
    Admission(#[from] EnginePortError),
    #[error("deferred discovery fault: {0}")]
    Provider(ResolverFault),
    #[error("deferred discovery cancelled before target admission")]
    Cancelled,
    #[error("invalid committed resolution: {0}")]
    Corrupt(String),
}

/// A trusted registration validates scope, ownership, visibility, provider,
/// declared codecs and projection without attempting a locally unsupported decoder.
/// Deterministic refusals become immutable negative outcomes; faults do not.
pub trait ResolutionValidator: Send + Sync {
    fn validate(
        &self,
        grant: &DeferredGrant,
        requested: &QualifiedName,
        definition: &lash_vocabulary::ToolDefinition,
        route: &lash_core_store::tool_surface::OperationRoute,
        registry: RegistryKind,
    ) -> Result<ClaimRequirements, EnginePortError>;
}

pub struct FirstUse<'a> {
    /// A model response whose registration accompanies the first resolution.
    pub held: Option<&'a crate::HeldResponse>,
    pub cx: &'a crate::ActorContext,
    pub execution: ExecutionId,
    pub environment: &'a ExecutionEnvironment,
    pub capabilities: &'a NodeCapabilities,
    pub provider: Option<&'a dyn DeferredPackageProvider>,
    pub context: DeferredResolveContext<'a>,
    pub validator: &'a dyn ResolutionValidator,
    pub clock: &'a dyn lash_core_ids::clock::Clock,
    pub cancel: &'a CancellationToken,
}

impl FirstUse<'_> {
    /// Load before discovery; commit before fitting or executing any call.
    pub async fn resolve(
        self,
        use_: OperationUse,
        requested: QualifiedName,
    ) -> Result<CommittedUse, ResolutionError> {
        let key = ResolutionKey {
            execution: self.execution.clone(),
            use_,
        };
        let Some(UseBinding::Deferred(grant)) = self.environment.uses.get(&use_) else {
            return Err(EnginePortError::Refused(AdmissionRefusal::MissingRoute { use_ }).into());
        };
        if let Some(record) = self.cx.retained_resolution(&key).await? {
            return self.consume(record, &key, &requested);
        }
        validate_requested(grant, &requested).map_err(|message| {
            EnginePortError::Refused(AdmissionRefusal::InvalidDocument { message })
        })?;
        let entry = self.discover(grant, &requested).await?;
        let candidate = ResolutionRecord {
            key: key.clone(),
            requested: requested.clone(),
            entry,
        };
        if self.cancel.is_cancelled() {
            return Err(ResolutionError::Cancelled);
        }
        let winner = if let Some(held) = self.held {
            let mut tx = self.cx.begin().await?;
            if let Some(writes) = held.writes() {
                if held.stops_at(&tx) {
                    held.stop();
                    return Err(ResolutionError::Cancelled);
                }
                tx.append(writes)?;
                tx.seed_resolution(candidate.clone());
                let label = lash_durable::CommitLabel::RESOLUTION_COMMIT;
                let committed = self.cx.commit(tx, label).await?;
                held.committed(label);
                if held.stops_in(&committed) {
                    held.stop();
                    return Err(ResolutionError::Cancelled);
                }
                candidate
            } else {
                self.cx.commit_resolution(candidate).await?
            }
        } else {
            self.cx.commit_resolution(candidate).await?
        };
        self.consume(winner, &key, &requested)
    }

    async fn discover(
        &self,
        grant: &DeferredGrant,
        requested: &QualifiedName,
    ) -> Result<ResolutionEntry, ResolutionError> {
        let provider = self.provider.ok_or_else(|| {
            EnginePortError::Refused(AdmissionRefusal::ProviderNotHostable {
                provider: grant.provider.clone(),
                registry: self.environment.registry,
            })
        })?;
        let policy = grant.resolution_policy;
        let retry = policy.execution_policy.retry();
        let max_attempts = retry.map_or(1, |retry| retry.max_attempts.get());
        for ordinal in 1..=max_attempts {
            let attempt = tokio::select! {
                biased;
                () = self.cancel.cancelled() => return Err(ResolutionError::Cancelled),
                () = self.clock.sleep(policy.execution) => Err(ResolverFault { message: "resolver execution bound elapsed".into() }),
                result = provider.resolve(&self.context, grant, requested) => result,
            };
            let attempt = match attempt {
                Ok(answer) => self.validate_answer(grant, requested, answer),
                Err(error) => Err(ResolutionError::Provider(error)),
            };
            match attempt {
                Ok(entry) => return Ok(entry),
                Err(fault) if ordinal == max_attempts => {
                    return Err(fault);
                }
                Err(_) => {
                    if let Some(retry) = retry {
                        let delay = retry
                            .backoff
                            .base_delay_ms
                            .saturating_mul(1_u64.checked_shl(ordinal - 1).unwrap_or(u64::MAX))
                            .min(retry.backoff.max_delay_ms);
                        tokio::select! {
                            biased;
                            () = self.cancel.cancelled() => return Err(ResolutionError::Cancelled),
                            () = self.clock.sleep(std::time::Duration::from_millis(delay)) => {}
                        }
                    }
                }
            }
        }
        Err(ResolutionError::Provider(ResolverFault {
            message: "resolver has no admitted attempt".into(),
        }))
    }

    fn validate_answer(
        &self,
        grant: &DeferredGrant,
        requested: &QualifiedName,
        answer: DeferredAnswer,
    ) -> Result<ResolutionEntry, ResolutionError> {
        match answer {
            DeferredAnswer::Refused { note } => Ok(ResolutionEntry::Refused { note }),
            DeferredAnswer::Unavailable { note } => Ok(ResolutionEntry::Unavailable { note }),
            DeferredAnswer::Resolved { definition, route } => {
                if definition.manifest.execution_policy == ExecutionPolicy::Read
                    || route.policy == ExecutionPolicy::Read
                {
                    return Ok(ResolutionEntry::Refused {
                        note: "a deferred use cannot resolve to a read operation".into(),
                    });
                }
                if !matches!(&route.source, RouteSource::Resolved { definition: retained } if retained == &definition)
                {
                    return Ok(ResolutionEntry::Refused {
                        note: "the returned route does not retain its resolved definition".into(),
                    });
                }
                let requirements = match self.validator.validate(
                    grant,
                    requested,
                    &definition,
                    &route,
                    self.environment.registry,
                ) {
                    Ok(requirements) => requirements,
                    Err(EnginePortError::Refused(refusal)) => {
                        return Ok(ResolutionEntry::Refused {
                            note: format!("{refusal:?}"),
                        });
                    }
                    Err(error) => return Err(error.into()),
                };
                let entry = ResolutionEntry::Resolved {
                    contract: definition.contract_id(),
                    definition,
                    route,
                    mode: OperationMode::Waited,
                    requirements: Box::new(requirements),
                };
                match entry.validate() {
                    Ok(()) => Ok(entry),
                    Err(note) => Ok(ResolutionEntry::Refused { note }),
                }
            }
        }
    }

    fn consume(
        &self,
        record: ResolutionRecord,
        key: &ResolutionKey,
        requested: &QualifiedName,
    ) -> Result<CommittedUse, ResolutionError> {
        if record.key != *key || record.requested != *requested {
            return Err(ResolutionError::Corrupt(
                "resolution identity or requested name differs from the registered use".into(),
            ));
        }
        record.entry.validate().map_err(ResolutionError::Corrupt)?;
        let unmet = record
            .entry
            .requirements()
            .map(|required| required.unmet_by(self.capabilities))
            .unwrap_or_default();
        if unmet.is_empty() {
            Ok(CommittedUse::Ready(record))
        } else {
            Ok(CommittedUse::Parked {
                record,
                unmet: Box::new(unmet),
            })
        }
    }
}

fn validate_requested(grant: &DeferredGrant, requested: &QualifiedName) -> Result<(), String> {
    let path: Vec<_> = grant.path.iter().collect();
    let package: Vec<_> = requested.package.iter().collect();
    if package.len() < path.len() || !package.starts_with(&path) || requested.type_member.is_some()
    {
        return Err("requested member is outside the pinned deferred package".into());
    }
    if Some(&grant.context.root) != grant.path.first() {
        return Err("deferred grant is outside its publisher's root".into());
    }
    Ok(())
}

/// Initial immutable environment vs runtime inspection, independent of spawn policy.
/// Known debts are assembled before the actor is made claimable.
pub fn start_requirements(
    inspection: &LinkInspection,
    environment: &ExecutionEnvironment,
    seeded: &std::collections::BTreeSet<OperationUse>,
    mut deferred: impl FnMut(&DeferredGrant, RegistryKind) -> Result<ClaimRequirements, EnginePortError>,
) -> Result<ClaimRequirements, EnginePortError> {
    let mut requirements = ClaimRequirements {
        machine: inspection.machine.clone(),
        ..ClaimRequirements::default()
    };
    for (use_, need) in &inspection.operations {
        match (need, environment.uses.get(use_)) {
            (OperationNeed::Fixed { requirement, .. }, Some(UseBinding::Fixed(fixed))) => {
                if fixed.requirement != *requirement {
                    return Err(EnginePortError::Refused(
                        AdmissionRefusal::ContractMismatch {
                            use_: Box::new(*use_),
                            pinned: requirement.contract.clone(),
                            offered: fixed.requirement.contract.clone(),
                        },
                    ));
                }
                fixed.validate().map_err(|message| {
                    EnginePortError::Refused(AdmissionRefusal::InvalidDocument { message })
                })?;
                fixed.route.require(&mut requirements);
            }
            (OperationNeed::Deferred { grant, .. }, Some(UseBinding::Deferred(recorded)))
                if grant.as_ref() == recorded =>
            {
                if !seeded.contains(use_) {
                    deferred(grant, environment.registry)?;
                }
            }
            _ => {
                return Err(EnginePortError::Refused(AdmissionRefusal::MissingRoute {
                    use_: *use_,
                }));
            }
        }
    }
    // Do not let a host omit already-recorded config or callback requirements.
    requirements.extend(&environment.requirements);
    Ok(requirements)
}

/// Membership is explicitly persisted at execution admission; ExecutionId text is not authority.
pub fn registration(
    execution: ExecutionId,
    inspection: &LinkInspection,
    referrer: lash_core_store::artifact_referrer::ArtifactReferrer,
    requirements: ClaimRequirements,
    registry: RegistryKind,
) -> ExecutionRegistration {
    let uses = inspection
        .operations
        .iter()
        .filter_map(|(use_, need)| match need {
            OperationNeed::Deferred { name, grant } => Some((
                *use_,
                lash_durable::resolutions::DeferredUse {
                    requested: name.clone(),
                    discovery: crate::plugin::packages::deferred_requirements(grant, registry),
                },
            )),
            OperationNeed::Fixed { .. } => None,
        })
        .collect::<BTreeMap<_, _>>();
    ExecutionRegistration {
        execution,
        uses,
        referrer,
        requirements,
    }
}

/// Trusted explicit seeds are checked under the successor use's grant without discovery.
/// The returned debt is unioned with known requirements in the start transaction.
pub fn validate_seeds(
    inspection: &LinkInspection,
    environment: &ExecutionEnvironment,
    seeds: &[lash_core_store::resolutions::ResolutionSeed],
    mut validate: impl FnMut(
        &DeferredGrant,
        &QualifiedName,
        &lash_vocabulary::ToolDefinition,
        &lash_core_store::tool_surface::OperationRoute,
        RegistryKind,
    ) -> Result<ClaimRequirements, EnginePortError>,
) -> Result<ClaimRequirements, EnginePortError> {
    let mut seen = std::collections::BTreeSet::new();
    let mut requirements = environment.requirements.clone();
    let refused = |message| EnginePortError::Refused(AdmissionRefusal::InvalidDocument { message });
    for seed in seeds {
        if !seen.insert(seed.use_) {
            return Err(refused("duplicate initial resolution use".into()));
        }
        let Some(OperationNeed::Deferred { name, grant }) = inspection.operations.get(&seed.use_)
        else {
            return Err(EnginePortError::Refused(AdmissionRefusal::MissingRoute {
                use_: seed.use_,
            }));
        };
        if !matches!(environment.uses.get(&seed.use_), Some(UseBinding::Deferred(recorded)) if recorded == grant.as_ref())
        {
            return Err(refused(
                "initial resolution grant differs from selected reach".into(),
            ));
        }
        seed.entry.validate().map_err(refused)?;
        if let ResolutionEntry::Resolved {
            definition,
            route,
            requirements: recorded,
            ..
        } = &seed.entry
        {
            let mut expected = validate(grant, name, definition, route, environment.registry)?;
            expected.extend(recorded);
            if &expected != recorded.as_ref() {
                return Err(refused(
                    "initial resolution omits validated target debt".into(),
                ));
            }
            requirements.extend(recorded);
        }
    }
    Ok(requirements)
}

impl ResolutionValidator for crate::plugin::ExecutionComposition {
    fn validate(
        &self,
        grant: &DeferredGrant,
        requested: &QualifiedName,
        definition: &lash_vocabulary::ToolDefinition,
        route: &lash_core_store::tool_surface::OperationRoute,
        registry: RegistryKind,
    ) -> Result<ClaimRequirements, EnginePortError> {
        self.validate_resolved_route(grant, requested, definition, route, registry)
    }
}
