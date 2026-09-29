//! The artifact-cleanup executor (ADR 0113 §2.5): the one relay that severs
//! artifact edges outside an end hook's own transaction.
//!
//! A cleanup row is armed by an end hook (`Ended`) or by the first
//! acquisition of a guarded referrer (a guard). Its delivery resolves the
//! plan to the carries each artifact store owes — or to "not yet", while the
//! referrer's authority has not ended it — then asks every store to apply its
//! share in one transaction of its own: fence, carry, sever, reclaim. The row
//! settles `Delivered` only after every store answered `Ok`; a store fault is
//! retried whole and idempotently, and a carry whose bytes are gone stalls
//! the row, because it means an invariant was broken and nobody may paper
//! over it.

use std::sync::Arc;

use super::drive::relay::{DeliveryFailure, ObligationRelay, RelayPolicy};
use crate::store::{ArtifactCleanupLedger, ObligationId, ObligationKey, ObligationLedger};
use crate::{
    ArtifactCarry, ArtifactCleanup, ArtifactCleanupPlan, ArtifactName, ArtifactReferrer,
    ArtifactStoreError, ArtifactStoreId, DefinitionRevisionId, EffectHost, JournalReplay,
    ModuleArtifactStore, PluginError, ProcessDefinitionRegistry, ProcessEngineRegistry,
    ProcessExecutionEnvRef, ProcessExecutionEnvStore, ProcessId, ProcessInput, ProcessRegistry,
    ReferrerClaim, ResolvedArtifactCleanup, RuntimeErrorCode, StartKey, SubscriptionRevisionId,
    TriggerStore, TriggerSubscriptionFilter, TriggerSubscriptionLifecycle, artifact_referrer_ended,
};

/// The record a start key registered, as a start's guard carries onto it.
#[derive(Clone, Debug, PartialEq)]
pub struct RetainedStart {
    pub process_id: ProcessId,
    pub env_ref: Option<ProcessExecutionEnvRef>,
    pub input: Arc<ProcessInput>,
}

/// Where a subscription revision stands (ADR 0113 §3.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SubscriptionRevisionStanding {
    /// It is the subscription's current live revision.
    pub current: bool,
    /// A delivery reserved under it has not bound its start yet.
    pub unbound_deliveries: bool,
}

/// The authorities a guard asks whether its referrer has ended. Each answer
/// is read fresh on every delivery; none of them decides alone except the
/// journal verdict.
#[async_trait::async_trait]
pub trait ArtifactCleanupAuthorities: Send + Sync {
    /// The engine's verdict on `journal` (ADR 0113 §2.5).
    async fn journal_replay(
        &self,
        journal: &lash_sansio::EffectJournalIdentity,
    ) -> Result<JournalReplay, String>;

    /// The record `key` registered, if any.
    async fn retained_start(&self, key: &StartKey) -> Result<Option<RetainedStart>, String>;

    async fn subscription_revision(
        &self,
        revision: &SubscriptionRevisionId,
    ) -> Result<SubscriptionRevisionStanding, String>;

    /// Whether `revision` is its slot's current resolvable revision.
    async fn definition_revision_current(
        &self,
        revision: &DefinitionRevisionId,
    ) -> Result<bool, String>;
}

/// The authorities of one store set and its engine.
pub struct StoreSetAuthorities {
    pub effect_host: Arc<dyn EffectHost>,
    pub processes: Arc<dyn ProcessRegistry>,
    pub triggers: Arc<dyn TriggerStore>,
    pub definitions: Arc<dyn ProcessDefinitionRegistry>,
}

#[async_trait::async_trait]
impl ArtifactCleanupAuthorities for StoreSetAuthorities {
    async fn journal_replay(
        &self,
        journal: &lash_sansio::EffectJournalIdentity,
    ) -> Result<JournalReplay, String> {
        self.effect_host
            .journal_replay(journal)
            .await
            .map_err(|error| error.to_string())
    }

    async fn retained_start(&self, key: &StartKey) -> Result<Option<RetainedStart>, String> {
        Ok(self
            .processes
            .get_process_by_start_key(key)
            .await
            .map_err(|error| error.to_string())?
            .map(|record| RetainedStart {
                process_id: record.id,
                env_ref: record.env_ref,
                input: record.input,
            }))
    }

    async fn subscription_revision(
        &self,
        revision: &SubscriptionRevisionId,
    ) -> Result<SubscriptionRevisionStanding, String> {
        // The store has no read by subscription id; the guard is polled at
        // the relay's maximum backoff, so a listing is affordable here.
        let current = self
            .triggers
            .list_subscriptions(TriggerSubscriptionFilter::default())
            .await
            .map_err(|error| error.to_string())?
            .iter()
            .any(|record| {
                record.subscription_id == revision.subscription_id()
                    && record.incarnation == revision.incarnation()
                    && record.revision == revision.revision()
                    && !matches!(
                        record.lifecycle,
                        TriggerSubscriptionLifecycle::Tombstoned(_)
                    )
            });
        let unbound_deliveries = self
            .triggers
            .list_deliveries_by_subscription_id(revision.subscription_id())
            .await
            .map_err(|error| error.to_string())?
            .iter()
            .any(|delivery| {
                delivery.process_id.is_none()
                    && delivery.subscription.incarnation == revision.incarnation()
                    && delivery.subscription.revision == revision.revision()
            });
        Ok(SubscriptionRevisionStanding {
            current,
            unbound_deliveries,
        })
    }

    async fn definition_revision_current(
        &self,
        revision: &DefinitionRevisionId,
    ) -> Result<bool, String> {
        Ok(self
            .definitions
            .definition_state(revision.definition_id())
            .await
            .map_err(|error| error.to_string())?
            .is_some_and(|record| {
                record.revision == revision.revision() && record.lifecycle.resolvable()
            }))
    }
}

/// The ledger, the authorities a guard asks, and the stores a resolved
/// cleanup is applied to.
#[derive(Clone)]
pub struct ArtifactCleanupPorts {
    pub ledger: Arc<dyn ArtifactCleanupLedger>,
    pub authorities: Arc<dyn ArtifactCleanupAuthorities>,
    pub process_env: Arc<dyn ProcessExecutionEnvStore>,
    pub modules: Arc<dyn ModuleArtifactStore>,
    /// Every installed engine: a start's engine names, and each engine's own
    /// store.
    pub engines: ProcessEngineRegistry,
}

/// The `ArtifactCleanup` relay.
pub struct ArtifactCleanupRelay {
    ports: ArtifactCleanupPorts,
    policy: RelayPolicy,
}

/// What a plan resolved to.
#[derive(Debug, PartialEq, Eq)]
enum Resolution {
    /// The referrer has ended: carry these, then fence and sever.
    Carry(Vec<ArtifactCarry>),
    /// The referrer's authority has not ended it yet.
    NotYet,
}

impl ArtifactCleanupRelay {
    #[must_use]
    pub fn new(ports: ArtifactCleanupPorts) -> Self {
        Self {
            ports,
            policy: RelayPolicy::default(),
        }
    }

    /// The same relay under a non-default policy (a host lever, ADR 0014).
    #[must_use]
    pub fn with_policy(mut self, policy: RelayPolicy) -> Self {
        self.policy = policy;
        self
    }

    async fn journal_settled(
        &self,
        journal: &lash_sansio::EffectJournalIdentity,
    ) -> Result<bool, DeliveryFailure> {
        match self.ports.authorities.journal_replay(journal).await {
            Ok(JournalReplay::Settled) => Ok(true),
            Ok(JournalReplay::MayReplay) => Ok(false),
            Err(error) => Err(DeliveryFailure::Retryable(format!(
                "journal verdict for `{}`: {error}",
                journal.key()
            ))),
        }
    }

    /// Resolve `cleanup`'s plan to its carries, or to not yet (ADR 0113
    /// §2.5 step 3).
    async fn resolve(&self, cleanup: &ArtifactCleanup) -> Result<Resolution, DeliveryFailure> {
        let settled_or_not_yet = |settled: bool| {
            if settled {
                Resolution::Carry(Vec::new())
            } else {
                Resolution::NotYet
            }
        };
        let authorities = &self.ports.authorities;
        match (&cleanup.plan, &cleanup.referrer) {
            (ArtifactCleanupPlan::Ended { carries }, ArtifactReferrer::Start(key)) => {
                self.hold_retained_start(key).await?;
                Ok(Resolution::Carry(carries.clone()))
            }
            (ArtifactCleanupPlan::Ended { carries }, _) => Ok(Resolution::Carry(carries.clone())),
            (ArtifactCleanupPlan::AwaitJournal, ArtifactReferrer::Execution(journal)) => {
                Ok(settled_or_not_yet(self.journal_settled(journal).await?))
            }
            (ArtifactCleanupPlan::AwaitStart { starter }, ArtifactReferrer::Start(key)) => {
                match authorities
                    .retained_start(key)
                    .await
                    .map_err(retryable_text("start-key read"))?
                {
                    Some(retained) => Ok(Resolution::Carry(self.start_carries(&retained)?)),
                    None => Ok(settled_or_not_yet(self.journal_settled(starter).await?)),
                }
            }
            (
                ArtifactCleanupPlan::AwaitSubscriptionRevision { creator },
                ArtifactReferrer::SubscriptionRevision(revision),
            ) => {
                let standing = authorities
                    .subscription_revision(revision)
                    .await
                    .map_err(retryable_text("subscription read"))?;
                if standing.current || standing.unbound_deliveries {
                    return Ok(Resolution::NotYet);
                }
                Ok(settled_or_not_yet(self.journal_settled(creator).await?))
            }
            (
                ArtifactCleanupPlan::AwaitDefinitionRevision { creator },
                ArtifactReferrer::DefinitionRevision(revision),
            ) => {
                if authorities
                    .definition_revision_current(revision)
                    .await
                    .map_err(retryable_text("definition read"))?
                {
                    return Ok(Resolution::NotYet);
                }
                Ok(settled_or_not_yet(self.journal_settled(creator).await?))
            }
            (plan, referrer) => Err(DeliveryFailure::Undecodable(format!(
                "`{}` is not a guard of referrer `{referrer}`",
                plan.label()
            ))),
        }
    }

    /// A registered start's carries: the retained record's environment and
    /// engine artifacts, onto its `ProcessRecord` (ADR 0113 §4.3). Never this
    /// attempt's content: the record is what the registrar kept.
    fn start_carries(
        &self,
        retained: &RetainedStart,
    ) -> Result<Vec<ArtifactCarry>, DeliveryFailure> {
        let to = ArtifactReferrer::ProcessRecord(retained.process_id.clone());
        Ok(self
            .retained_names(retained)?
            .into_iter()
            .map(|artifact| ArtifactCarry {
                artifact,
                to: to.clone(),
            })
            .collect())
    }

    /// Hold the key's registered record's content under its `ProcessRecord`
    /// before `Start(key)`'s end severs anything (ADR 0113 §3.3, FIG-4130).
    ///
    /// A terminal refusal ends `Start(key)` carrying nothing, because it read
    /// no record for the key; a concurrent start's row can commit after that
    /// read and before the end, having checked for a fence before there was
    /// one, so it relies on `Start(key)`'s cleanup to carry its content. This
    /// read runs after the fence: a row it misses commits later, and its
    /// start then meets the fence and holds its own content. So every row
    /// registered under the key has its content held by its record before
    /// `Start(key)`'s edges go.
    ///
    /// An acquisition, not a carry: a name with no stored bytes was never
    /// held by `Start(key)` (its start met the fence while staging and holds
    /// it itself), so it is skipped rather than stalled; a pruned record's own
    /// cleanup owns what it held.
    async fn hold_retained_start(&self, key: &StartKey) -> Result<(), DeliveryFailure> {
        let Some(retained) = self
            .ports
            .authorities
            .retained_start(key)
            .await
            .map_err(retryable_text("start-key read"))?
        else {
            return Ok(());
        };
        let record = ArtifactReferrer::ProcessRecord(retained.process_id.clone());
        let claim = ReferrerClaim::unguarded(record.clone())
            .map_err(|error| DeliveryFailure::Undecodable(error.to_string()))?;
        for name in self.retained_names(&retained)? {
            let acquired = match &name.store {
                ArtifactStoreId::ProcessEnv => self
                    .ports
                    .process_env
                    .acquire_process_execution_env(
                        &claim,
                        &ProcessExecutionEnvRef::new(name.artifact_ref.clone()),
                    )
                    .await
                    .map_err(PluginError::from),
                ArtifactStoreId::LashlangModule => self
                    .ports
                    .modules
                    .acquire_module_artifact(&claim, &name.artifact_ref)
                    .await
                    .map_err(PluginError::from),
                ArtifactStoreId::Engine(kind) => {
                    self.ports
                        .engines
                        .require(kind)
                        .map_err(retryable("engine store"))?
                        .acquire_engine_artifact(&claim, &name.artifact_ref)
                        .await
                }
            };
            match acquired {
                Ok(()) => {}
                Err(error) if artifact_referrer_ended(&error) == Some(&record) => return Ok(()),
                Err(PluginError::Runtime(error))
                    if error.code == RuntimeErrorCode::ArtifactMissing => {}
                Err(error) => {
                    return Err(DeliveryFailure::Retryable(format!(
                        "holding `{}` under `{record}`: {error}",
                        name.artifact_ref
                    )));
                }
            }
        }
        Ok(())
    }

    /// Every artifact the retained record names: its environment and its
    /// engine's start artifacts.
    fn retained_names(
        &self,
        retained: &RetainedStart,
    ) -> Result<Vec<ArtifactName>, DeliveryFailure> {
        let mut names = Vec::new();
        if let Some(env_ref) = &retained.env_ref {
            names.push(ArtifactName {
                store: ArtifactStoreId::ProcessEnv,
                artifact_ref: env_ref.as_str().to_owned(),
            });
        }
        if let ProcessInput::Engine { kind, payload } = retained.input.as_ref() {
            let engine = self.ports.engines.require(kind).map_err(|error| {
                DeliveryFailure::Refused(format!(
                    "the retained record names engine `{kind}`: {error}"
                ))
            })?;
            names.extend(engine.start_artifacts(payload).map_err(|error| {
                DeliveryFailure::Refused(format!("the retained record's engine artifacts: {error}"))
            })?);
        }
        Ok(names)
    }

    /// Ask every store to apply its share of the resolved cleanup (ADR 0113
    /// §2.5 step 4). Any failure fails the whole delivery.
    async fn apply(
        &self,
        referrer: &ArtifactReferrer,
        carries: &[ArtifactCarry],
    ) -> Result<(), DeliveryFailure> {
        self.ports
            .process_env
            .end_process_env_referrer(&ResolvedArtifactCleanup::for_store(
                referrer,
                carries,
                &ArtifactStoreId::ProcessEnv,
            ))
            .await
            .map_err(store_failure("process-environment store"))?;
        self.ports
            .modules
            .end_module_referrer(&ResolvedArtifactCleanup::for_store(
                referrer,
                carries,
                &ArtifactStoreId::module(),
            ))
            .await
            .map_err(store_failure("module store"))?;
        let engine_carries: Vec<ArtifactCarry> = carries
            .iter()
            .filter(|carry| matches!(carry.artifact.store, ArtifactStoreId::Engine(_)))
            .cloned()
            .collect();
        self.ports
            .engines
            .end_artifact_referrer(&ResolvedArtifactCleanup {
                referrer: referrer.clone(),
                carries: engine_carries,
            })
            .await
            .map_err(retryable("engine store"))
    }
}

fn retryable_text(context: &'static str) -> impl Fn(String) -> DeliveryFailure {
    move |error| DeliveryFailure::Retryable(format!("{context}: {error}"))
}

fn retryable(context: &'static str) -> impl Fn(PluginError) -> DeliveryFailure {
    move |error| DeliveryFailure::Retryable(format!("{context}: {error}"))
}

/// A carry whose bytes are gone is refused and stalls the row; every other
/// store failure is retried.
fn store_failure(context: &'static str) -> impl Fn(ArtifactStoreError) -> DeliveryFailure {
    move |error| match error {
        ArtifactStoreError::CarryArtifactMissing { .. } => {
            DeliveryFailure::Refused(format!("{context}: {error}"))
        }
        ArtifactStoreError::Incompatible { .. } | ArtifactStoreError::StoredDataCorrupt { .. } => {
            DeliveryFailure::Undecodable(format!("{context}: {error}"))
        }
        other => DeliveryFailure::Retryable(format!("{context}: {other}")),
    }
}

#[async_trait::async_trait]
impl ObligationRelay for ArtifactCleanupRelay {
    fn ledger(&self) -> &dyn ObligationLedger {
        self.ports.ledger.as_ref()
    }

    fn policy(&self) -> RelayPolicy {
        self.policy
    }

    async fn deliver(
        &self,
        id: &ObligationId,
        key: &ObligationKey,
        _attempt: u32,
    ) -> Result<(), DeliveryFailure> {
        let ObligationKey::ArtifactCleanup { referrer } = key else {
            return Err(DeliveryFailure::Undecodable(format!(
                "the artifact-cleanup relay was handed a {} key",
                key.kind()
            )));
        };
        // 1. A missing row was settled by another relay.
        let Some(cleanup) = self
            .ports
            .ledger
            .load_cleanup(id)
            .await
            .map_err(|error| DeliveryFailure::Retryable(format!("cleanup read: {error}")))?
        else {
            return Ok(());
        };
        if cleanup.referrer != *referrer {
            return Err(DeliveryFailure::Undecodable(format!(
                "cleanup `{id}` names referrer `{}`, not its row's `{referrer}`",
                cleanup.referrer
            )));
        }
        // 2. Sever nothing while the gate's journal may still replay.
        if let Some(gate) = &cleanup.gate
            && !self.journal_settled(gate).await?
        {
            return Err(DeliveryFailure::NotYet);
        }
        // 3. Resolve the plan.
        let carries = match self.resolve(&cleanup).await? {
            Resolution::Carry(carries) => carries,
            Resolution::NotYet => return Err(DeliveryFailure::NotYet),
        };
        // 4 and 5. Every store applies its share; only then is it delivered.
        self.apply(&cleanup.referrer, &carries).await
    }
}

#[cfg(test)]
#[path = "artifact_cleanup_tests.rs"]
mod tests;
