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

use crate::JournalReplay;
use std::sync::Arc;

use super::obligations::relay::{
    DeliveryFailure, ObligationDelivery, ObligationRelay, RelayPolicy, plugin_delivery_error,
};
use crate::store::{ArtifactCleanupLedger, DeliveryError, ObligationKey, ObligationLedger};
use crate::{
    ArtifactCarry, ArtifactCleanup, ArtifactName, ArtifactReferrer, ArtifactStoreError,
    ArtifactStoreId, ModuleArtifactStore, PluginError, ProcessDefinitionDraft, ProcessDefinitionId,
    ProcessDefinitionStore, ProcessEngineRegistry, ProcessExecutionEnvRef,
    ProcessExecutionEnvStore, ProcessId, ProcessInput, ProcessRegistry, ReferrerClaim,
    ReferrerGuard, ResolvedArtifactCleanup, RuntimeErrorCode, StartKey, SubscriptionRevisionId,
    TriggerStore, TriggerSubscriptionFilter, TriggerSubscriptionLifecycle, artifact_referrer_ended,
};

/// The record a start key registered, as a start's guard carries onto it.
#[derive(Clone, Debug, PartialEq)]
pub struct RetainedStart {
    pub process_id: ProcessId,
    pub env_ref: Option<ProcessExecutionEnvRef>,
    pub input: Arc<ProcessInput>,
    /// The definition a start by id admitted the record from: its record
    /// holds the descriptor and its manifest too (ADR 0113 §3.6).
    pub definition_id: Option<ProcessDefinitionId>,
}

/// Where a subscription revision stands (ADR 0113 §3.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SubscriptionRevisionStanding {
    /// It is the subscription's current live revision. Every delivery
    /// reserved under it bound its process in the transaction that recorded
    /// it, so nothing else holds it.
    pub current: bool,
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

    async fn frame_is_retained(&self, frame: &crate::FrameEnvironmentId) -> Result<bool, String>;

    /// The record `key` registered, if any.
    async fn retained_start(&self, key: &StartKey) -> Result<Option<RetainedStart>, String>;

    async fn subscription_revision(
        &self,
        revision: &SubscriptionRevisionId,
    ) -> Result<SubscriptionRevisionStanding, String>;
}

/// The authorities of one store set.
pub struct StoreSetAuthorities {
    pub sessions: Arc<dyn crate::DeploymentStore>,
    pub processes: Arc<dyn ProcessRegistry>,
    pub triggers: Arc<dyn TriggerStore>,
    /// The durable rows: whether a turn is still unfinished.
    pub durable: Arc<dyn lash_durable::DurableStore>,
}

#[async_trait::async_trait]
impl ArtifactCleanupAuthorities for StoreSetAuthorities {
    /// No backend journals effects (ADR 0132): an execution's journal is
    /// settled once the execution ends. Until then its owner may still
    /// publish under it: a turn while its run is the session's unfinished
    /// one, a process until its record is terminal. A journal key this
    /// build cannot read names an execution it cannot prove ended.
    async fn journal_replay(
        &self,
        journal: &lash_sansio::EffectJournalIdentity,
    ) -> Result<JournalReplay, String> {
        let running = match crate::ExecutionScope::from_journal_key(journal.key()) {
            Some(crate::ExecutionScope::Turn {
                session_id,
                turn_id,
            }) => lash_durable::DurableReads::turn(self.durable.as_ref(), &session_id)
                .await
                .map_err(|error| error.to_string())?
                .is_some_and(|row| row.run == turn_id),
            Some(crate::ExecutionScope::Process { process_id }) => self
                .processes
                .get_process(&process_id)
                .await
                .map_err(|error| error.to_string())?
                .is_some_and(|record| !record.is_terminal()),
            Some(
                crate::ExecutionScope::SessionOperation { .. }
                | crate::ExecutionScope::SessionDelete { .. }
                | crate::ExecutionScope::RuntimeOperation { .. },
            ) => false,
            None => true,
        };
        Ok(if running {
            JournalReplay::MayReplay
        } else {
            JournalReplay::Settled
        })
    }

    async fn frame_is_retained(&self, frame: &crate::FrameEnvironmentId) -> Result<bool, String> {
        self.sessions
            .artifact_frame_is_retained(frame)
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
                definition_id: record.identity.definition_id,
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
        Ok(SubscriptionRevisionStanding { current })
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
    pub definitions: Arc<dyn ProcessDefinitionStore>,
    /// Turns' recorded preludes, held by their turn journals.
    pub turn_preludes: Arc<dyn crate::TurnPreludeStore>,
    /// Every installed engine: a start's engine names, and each engine's own
    /// store.
    pub engines: ProcessEngineRegistry,
    /// Attachment edges: the fourth store `apply` ends a referrer in, and
    /// the session state an upload or session guard resolves against.
    pub attachments: Arc<dyn crate::AttachmentReferrers>,
    /// Resolves `AwaitUploadExpiry`.
    pub clock: Arc<dyn crate::runtime::Clock>,
}

/// The `ArtifactCleanup` relay.
pub struct ArtifactCleanupRelay {
    ports: ArtifactCleanupPorts,
    policy: RelayPolicy,
    metrics: lash_trace::telemetry::metrics::TelemetryMetrics,
}

/// What a plan resolved to.
#[derive(Debug, PartialEq, Eq)]
enum Resolution {
    /// The referrer has ended: carry these, then fence and sever.
    Carry(Vec<ArtifactCarry>),
    /// The referrer's authority has not ended it yet.
    NotYet,
    /// The referrer's authority ends it at this instant, unless it ends
    /// sooner.
    NotBefore(u64),
}

impl ArtifactCleanupRelay {
    #[must_use]
    pub fn with_metrics(
        mut self,
        metrics: lash_trace::telemetry::metrics::TelemetryMetrics,
    ) -> Self {
        self.metrics = metrics;
        self
    }

    #[must_use]
    pub fn new(ports: ArtifactCleanupPorts) -> Self {
        Self {
            ports,
            policy: RelayPolicy::default(),
            metrics: Default::default(),
        }
    }

    /// The relay over `backend`'s store set, its referrers' authorities read
    /// from that same store set, with `engines` for each engine's own store.
    #[must_use]
    pub fn over_backend(backend: &crate::Backend, engines: ProcessEngineRegistry) -> Self {
        Self::new(ArtifactCleanupPorts {
            ledger: backend.artifact_cleanup(),
            authorities: Arc::new(StoreSetAuthorities {
                sessions: backend.session_store_factory(),
                processes: backend.process_registry(),
                triggers: backend.trigger_store(),
                durable: Arc::clone(backend.durable()),
            }),
            process_env: backend.process_env_store(),
            modules: backend.module_artifacts(),
            definitions: backend.definition_store(),
            turn_preludes: backend.turn_prelude_store(),
            engines,
            attachments: backend.attachment_referrers(),
            clock: backend.clock(),
        })
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
            Err(error) => Err(retryable_text("journal verdict")(format!(
                "`{}`: {error}",
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
        match cleanup {
            ArtifactCleanup::Ended {
                referrer, carries, ..
            } => {
                match referrer {
                    ArtifactReferrer::Start(key) => self.hold_retained_start(key).await?,
                    ArtifactReferrer::StartInput { start_key, .. } => {
                        self.hold_start_input(start_key).await?;
                    }
                    ArtifactReferrer::FrameEnvironment(_)
                    | ArtifactReferrer::ProcessRecord(_)
                    | ArtifactReferrer::SubscriptionRevision(_)
                    | ArtifactReferrer::Execution(_)
                    | ArtifactReferrer::HostPin(_)
                    | ArtifactReferrer::Session(_)
                    | ArtifactReferrer::Upload(_)
                    | ArtifactReferrer::Source(_) => {}
                }
                Ok(Resolution::Carry(carries.clone()))
            }
            ArtifactCleanup::Await(guard) => match guard {
                ReferrerGuard::Frame { frame, creator } => {
                    if authorities
                        .frame_is_retained(frame)
                        .await
                        .map_err(retryable_text("frame root read"))?
                    {
                        return Ok(Resolution::NotYet);
                    }
                    Ok(settled_or_not_yet(self.journal_settled(creator).await?))
                }
                ReferrerGuard::Journal(journal) => {
                    Ok(settled_or_not_yet(self.journal_settled(journal).await?))
                }
                ReferrerGuard::Start { start_key, starter } => {
                    match authorities
                        .retained_start(start_key)
                        .await
                        .map_err(retryable_text("start-key read"))?
                    {
                        Some(retained) => {
                            Ok(Resolution::Carry(self.start_carries(&retained).await?))
                        }
                        None => self.unregistered_start(start_key, starter).await,
                    }
                }
                ReferrerGuard::StartInput { start_key, starter } => {
                    if self.hold_start_input(start_key).await? {
                        Ok(Resolution::Carry(Vec::new()))
                    } else {
                        self.unregistered_start(start_key, starter).await
                    }
                }
                ReferrerGuard::SubscriptionRevision { revision, creator } => {
                    let standing = authorities
                        .subscription_revision(revision)
                        .await
                        .map_err(retryable_text("subscription read"))?;
                    if standing.current {
                        return Ok(Resolution::NotYet);
                    }
                    Ok(settled_or_not_yet(self.journal_settled(creator).await?))
                }
                ReferrerGuard::Upload {
                    upload,
                    expires_at_ms,
                } => {
                    if self.ports.clock.timestamp_ms() >= *expires_at_ms {
                        return Ok(Resolution::Carry(Vec::new()));
                    }
                    let state = self.session_state(upload.session_id()).await?;
                    Ok(if state == crate::SessionReferrerState::Live {
                        Resolution::NotBefore(*expires_at_ms)
                    } else {
                        Resolution::Carry(Vec::new())
                    })
                }
                ReferrerGuard::SessionGraphRetired(session) => {
                    let state = self.session_state(session).await?;
                    Ok(settled_or_not_yet(
                        state == crate::SessionReferrerState::DeletedRetired,
                    ))
                }
            },
        }
    }

    /// A start staging under `start_key` with no record yet ends when its
    /// starter settles. A start that is its own operation has no execution
    /// to settle: only its registration or its abandonment's `Ended`, which
    /// replaces this guard, decides its staging (ADR 0113 §3.3).
    async fn unregistered_start(
        &self,
        start_key: &StartKey,
        starter: &lash_sansio::EffectJournalIdentity,
    ) -> Result<Resolution, DeliveryFailure> {
        if super::is_start_operation(starter, start_key) || !self.journal_settled(starter).await? {
            return Ok(Resolution::NotYet);
        }
        Ok(Resolution::Carry(Vec::new()))
    }

    /// Where `session` stands for its upload and session guards.
    async fn session_state(
        &self,
        session: &crate::SessionId,
    ) -> Result<crate::SessionReferrerState, DeliveryFailure> {
        self.ports
            .attachments
            .session_referrer_state(session)
            .await
            .map_err(durable_store_failure("attachment store"))
    }

    /// A registered start's carries: the retained record's environment and
    /// engine artifacts, onto its `ProcessRecord` (ADR 0113 §4.3). Never this
    /// attempt's content: the record is what the registrar kept.
    async fn start_carries(
        &self,
        retained: &RetainedStart,
    ) -> Result<Vec<ArtifactCarry>, DeliveryFailure> {
        let to = ArtifactReferrer::ProcessRecord(retained.process_id.clone());
        Ok(self
            .retained_names(retained)
            .await?
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
            .map_err(|error| undecodable(error.to_string()))?;
        for name in self.retained_names(&retained).await? {
            let acquired = match &name.store {
                ArtifactStoreId::ProcessDefinition => {
                    let id = ProcessDefinitionId::parse(&name.artifact_ref).map_err(|error| {
                        undecodable(format!(
                            "the retained record names definition `{}`: {error}",
                            name.artifact_ref
                        ))
                    })?;
                    self.ports
                        .definitions
                        .acquire_process_definition(&claim, &id, &[])
                        .await
                        .map_err(PluginError::from)
                }
                ArtifactStoreId::ProcessEnv => self
                    .ports
                    .process_env
                    .acquire_process_execution_env(
                        &claim,
                        &ProcessExecutionEnvRef::new(name.artifact_ref.clone()),
                    )
                    .await
                    .map_err(PluginError::from),
                store if *store == ArtifactStoreId::module() => self
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
                store => {
                    return Err(undecodable(format!(
                        "unknown artifact store for retained start: {store:?}"
                    )));
                }
            };
            match acquired {
                Ok(()) => {}
                Err(error) if artifact_referrer_ended(&error) == Some(&record) => return Ok(()),
                Err(PluginError::Runtime(error))
                    if error.code == RuntimeErrorCode::ArtifactMissing => {}
                Err(error) => {
                    return Err(DeliveryFailure::Retryable(
                        plugin_delivery_error(error).in_context(format_args!(
                            "holding `{}` under `{record}`",
                            name.artifact_ref
                        )),
                    ));
                }
            }
        }
        Ok(())
    }

    /// Attachments carry no artifact names. Acquire the retained input
    /// before either AwaitStart or Ended can sever its staging edges.
    async fn hold_start_input(&self, key: &StartKey) -> Result<bool, DeliveryFailure> {
        let Some(retained) = self
            .ports
            .authorities
            .retained_start(key)
            .await
            .map_err(retryable_text("start-input key read"))?
        else {
            return Ok(false);
        };
        let ids = retained.input.stored_attachment_ids();
        if ids.is_empty() {
            return Ok(true);
        }
        let referrer = ArtifactReferrer::ProcessRecord(retained.process_id.clone());
        let claim = ReferrerClaim::unguarded(referrer.clone())
            .map_err(|error| undecodable(error.to_string()))?;
        match self
            .ports
            .attachments
            .acquire_attachment_refs(&claim, &ids)
            .await
        {
            Ok(()) => Ok(true),
            Err(crate::StoreError::ArtifactReferrerEnded { referrer: ended })
                if ended == referrer =>
            {
                Ok(true)
            }
            Err(error) => Err(durable_store_failure("attachment store")(error)),
        }
    }

    /// Every artifact the retained record names: its environment, its
    /// engine's start artifacts and, for a start by id, its definition's
    /// descriptor and manifest.
    async fn retained_names(
        &self,
        retained: &RetainedStart,
    ) -> Result<Vec<ArtifactName>, DeliveryFailure> {
        let mut names = Vec::new();
        if let Some(definition_id) = &retained.definition_id {
            names.push(ArtifactName {
                store: ArtifactStoreId::ProcessDefinition,
                artifact_ref: definition_id.as_str().to_owned(),
            });
            // The descriptor is still held by `Start(key)` here; with it gone
            // the carry of its name alone stalls, which is the invariant's
            // own signal.
            if let Some(bytes) = self
                .ports
                .definitions
                .get_process_definition(definition_id)
                .await
                .map_err(|error| retryable("definition read")(PluginError::from(error)))?
            {
                let draft = ProcessDefinitionDraft::from_store_bytes(definition_id, &bytes)
                    .map_err(|error| {
                        undecodable(format!("stored definition `{definition_id}`: {error}"))
                    })?;
                names.extend(draft.artifacts().iter().cloned());
            }
        }
        if let Some(env_ref) = &retained.env_ref {
            names.push(ArtifactName {
                store: ArtifactStoreId::ProcessEnv,
                artifact_ref: env_ref.as_str().to_owned(),
            });
        }
        if let ProcessInput::Engine { kind, payload } = retained.input.as_ref() {
            let engine = self.ports.engines.require(kind).map_err(|error| {
                DeliveryFailure::Refused(
                    plugin_delivery_error(error)
                        .in_context(format_args!("the retained record names engine `{kind}`")),
                )
            })?;
            names.extend(engine.start_artifacts(payload).map_err(|error| {
                DeliveryFailure::Refused(
                    plugin_delivery_error(error)
                        .in_context("the retained record's engine artifacts"),
                )
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
        self.ports
            .definitions
            .end_process_definition_referrer(&ResolvedArtifactCleanup::for_store(
                referrer,
                carries,
                &ArtifactStoreId::ProcessDefinition,
            ))
            .await
            .map_err(store_failure("process-definition store"))?;
        self.ports
            .turn_preludes
            .end_turn_prelude_referrer(&ResolvedArtifactCleanup::for_store(
                referrer,
                carries,
                &ArtifactStoreId::TurnPrelude,
            ))
            .await
            .map_err(store_failure("turn-prelude store"))?;
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
            .map_err(store_failure("engine store"))?;
        if referrer.kind().holds_attachments() {
            self.ports
                .attachments
                .end_attachment_referrer(referrer)
                .await
                .map_err(durable_store_failure("attachment store"))?;
        }
        Ok(())
    }
}

/// An authority read that answered only text: a store fault, retried.
fn retryable_text(context: &'static str) -> impl Fn(String) -> DeliveryFailure {
    move |error| {
        DeliveryFailure::Retryable(
            DeliveryError::new(RuntimeErrorCode::RuntimeStore, error).in_context(context),
        )
    }
}

fn retryable(context: &'static str) -> impl Fn(PluginError) -> DeliveryFailure {
    move |error| DeliveryFailure::Retryable(plugin_delivery_error(error).in_context(context))
}

/// A cleanup row or the record it names that this build cannot read.
fn undecodable(message: String) -> DeliveryFailure {
    DeliveryFailure::Undecodable(DeliveryError::new(
        RuntimeErrorCode::RuntimeStoreCorrupt,
        message,
    ))
}

/// A carry whose bytes are gone is refused and stalls the row; every other
/// store failure is retried.
fn store_failure(context: &'static str) -> impl Fn(ArtifactStoreError) -> DeliveryFailure {
    move |error| {
        let class: fn(DeliveryError) -> DeliveryFailure = match &error {
            ArtifactStoreError::CarryArtifactMissing { .. }
            | ArtifactStoreError::ReferrerKindRefused { .. } => DeliveryFailure::Refused,
            ArtifactStoreError::Incompatible { .. }
            | ArtifactStoreError::UnsupportedGeneration { .. }
            | ArtifactStoreError::StoredDataCorrupt { .. } => DeliveryFailure::Undecodable,
            _ => DeliveryFailure::Retryable,
        };
        class(plugin_delivery_error(PluginError::from(error)).in_context(context))
    }
}

/// An attachment-store fault is retried, except a row this build cannot read,
/// which no retry repairs.
fn durable_store_failure(context: &'static str) -> impl Fn(crate::StoreError) -> DeliveryFailure {
    move |error| {
        let class: fn(DeliveryError) -> DeliveryFailure = match &error {
            crate::StoreError::Incompatible { .. }
            | crate::StoreError::StoredDataCorrupt { .. } => DeliveryFailure::Undecodable,
            _ => DeliveryFailure::Retryable,
        };
        class(DeliveryError::from(error).in_context(context))
    }
}

#[async_trait::async_trait]
impl ObligationRelay for ArtifactCleanupRelay {
    fn metrics(&self) -> lash_trace::telemetry::metrics::TelemetryMetrics {
        self.metrics.clone()
    }

    fn ledger(&self) -> &dyn ObligationLedger {
        self.ports.ledger.as_ref()
    }

    fn policy(&self) -> RelayPolicy {
        self.policy
    }

    async fn deliver(&self, delivery: ObligationDelivery<'_>) -> Result<(), DeliveryFailure> {
        let ObligationDelivery { id, key, .. } = delivery;
        let ObligationKey::ArtifactCleanup { referrer } = key;
        // 1. A missing row was settled by another relay.
        let Some(cleanup) = self
            .ports
            .ledger
            .load_cleanup(id)
            .await
            .map_err(durable_store_failure("cleanup ledger"))?
        else {
            return Ok(());
        };
        if cleanup.referrer() != *referrer {
            return Err(undecodable(format!(
                "cleanup `{id}` names referrer `{}`, not its row's `{referrer}`",
                cleanup.referrer()
            )));
        }
        // 2. Sever nothing while the gate's journal may still replay.
        if let Some(gate) = cleanup.gate()
            && !self.journal_settled(gate).await?
        {
            return Err(DeliveryFailure::NotYet);
        }
        // 3. Resolve the plan.
        let carries = match self.resolve(&cleanup).await? {
            Resolution::Carry(carries) => carries,
            Resolution::NotYet => return Err(DeliveryFailure::NotYet),
            Resolution::NotBefore(due_at_ms) => {
                return Err(DeliveryFailure::NotBefore { due_at_ms });
            }
        };
        // 4 and 5. Every store applies its share; only then is it delivered.
        self.apply(&cleanup.referrer(), &carries).await
    }
}

#[cfg(test)]
#[path = "artifact_cleanup_tests.rs"]
mod tests;
