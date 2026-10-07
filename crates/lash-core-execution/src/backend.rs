//! The one value a runtime takes its persistence ports from: the durable
//! backend over one store set (ADR 0132 §1; S9 of I0, FIG-5194).

use std::collections::BTreeMap;
use std::sync::Arc;

use lash_durable::runner::Hints;
use lash_durable::{
    DurableConfig, DurableConfigError, DurableError, DurableSettings, DurableStore,
};

use crate::runtime::actor::projection::ProjectionProviders;
use crate::{
    AttachmentStore, Clock, DeploymentStore, ModuleArtifactStore, ProcessContinuationStore,
    ProcessExecutionEnvStore, ProcessRegistry, TriggerStore,
};

/// The identity of one store set: the storage it names, such as a SQLite
/// location or a PostgreSQL catalog. Stable for the life of that storage
/// and distinct between any two.
///
/// It names storage only.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct StoreBindingId(Arc<str>);

impl StoreBindingId {
    pub fn new(identity: impl Into<Arc<str>>) -> Self {
        Self(identity.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for StoreBindingId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Why a durable backend was not built.
#[derive(Debug, thiserror::Error)]
pub enum DurableBuildError {
    /// No completion secrets were configured: host-resolvable wait keys
    /// cannot be minted or verified, and there is no default.
    #[error("the durable backend needs completion secrets; there is no default")]
    MissingCompletionSecrets,
    /// Two process engines declare one kind.
    #[error("two process engines declare kind `{kind}`")]
    DuplicateEngine {
        /// The kind.
        kind: String,
    },
    /// Two projection providers answer one type.
    #[error("two projection providers answer type `{projection}`")]
    DuplicateProvider {
        /// The type.
        projection: String,
    },
    /// The substrate parameters break a rule.
    #[error("invalid durable configuration: {0}")]
    InvalidConfig(#[from] DurableConfigError),
}

/// What a durable backend is assembled from.
pub struct BackendParts {
    /// The store set.
    pub stores: Arc<dyn StoreSet>,
    /// The substrate parameters, validated by assembly.
    pub settings: DurableSettings,
    /// The completion secrets; required.
    pub secrets: Option<crate::runtime::actor::waits::CompletionKeySecrets>,
    /// The host process engines, one per kind.
    pub engines: Vec<Arc<dyn crate::ProcessEngine>>,
    /// The projection providers' catalog.
    pub providers: Arc<dyn ProjectionProviders>,
}

/// The one value a runtime takes every port from: the store set, its
/// durable store, the substrate's parameters, the completion secrets, the
/// host process engines and the projection providers (ADR 0132 §1).
///
/// It is the unit ADR 0102 rules on: no API assembles ports from different
/// substrates by hand. Every accessor hands out a handle on the store set's
/// one instance of that port, so two calls reach the same state. Cloning
/// shares it. There is no engine trait object: the durable engine is lash's
/// own, over this store set.
#[derive(Clone)]
pub struct Backend {
    inner: Arc<BackendInner>,
}

struct BackendInner {
    stores: Arc<dyn StoreSet>,
    /// The store set's durable store, taken on first use: a test store set
    /// that serves no durable store is never asked for one.
    durable: std::sync::OnceLock<Arc<dyn DurableStore>>,
    config: DurableConfig,
    secrets: crate::runtime::actor::waits::CompletionKeySecrets,
    engines: BTreeMap<String, Arc<dyn crate::ProcessEngine>>,
    providers: Arc<dyn ProjectionProviders>,
    /// The in-process half of a wake: the node runner this backend serves
    /// under, when it serves, takes its hints from here, so a mailbox commit
    /// made on this node reaches its actors without waiting for a poll.
    hints: Hints,
}

impl Backend {
    /// Assemble a durable backend from `parts`.
    ///
    /// # Errors
    ///
    /// [`DurableBuildError`]: invalid settings, duplicate engines or
    /// providers, then missing completion secrets.
    pub fn assemble(parts: BackendParts) -> Result<Self, DurableBuildError> {
        let config = parts.settings.validate()?;
        let mut engines = BTreeMap::new();
        for engine in parts.engines {
            let kind = engine.kind().to_owned();
            if engines.insert(kind.clone(), engine).is_some() {
                return Err(DurableBuildError::DuplicateEngine { kind });
            }
        }
        let providers = parts.providers;
        let mut seen = std::collections::BTreeSet::new();
        for projection in providers.projection_types() {
            if !seen.insert(projection.clone()) {
                return Err(DurableBuildError::DuplicateProvider { projection });
            }
        }
        let secrets = parts
            .secrets
            .ok_or(DurableBuildError::MissingCompletionSecrets)?;
        Ok(Self {
            inner: Arc::new(BackendInner {
                stores: parts.stores,
                durable: std::sync::OnceLock::new(),
                config,
                secrets,
                engines,
                providers,
                hints: Hints::default(),
            }),
        })
    }

    /// A backend over `stores` with the default settings, testing completion
    /// secrets, no engines and no projection providers.
    #[cfg(any(test, feature = "testing"))]
    #[expect(
        clippy::expect_used,
        reason = "the default settings validate and an empty registration has no duplicate"
    )]
    #[must_use]
    pub fn for_testing(stores: Arc<dyn StoreSet>) -> Self {
        Self::assemble(BackendParts {
            stores,
            settings: DurableSettings::default(),
            secrets: Some(crate::runtime::actor::waits::CompletionKeySecrets::for_testing()),
            engines: Vec::new(),
            providers: Arc::new(crate::runtime::actor::projection::NoProjectionProviders),
        })
        .expect("a testing backend assembles")
    }

    /// This backend over `stores` instead, with the same configuration,
    /// secrets, engines and providers: what a test that decorates store
    /// ports builds.
    #[cfg(any(test, feature = "testing"))]
    #[must_use]
    pub fn over_stores(&self, stores: Arc<dyn StoreSet>) -> Self {
        Self {
            inner: Arc::new(BackendInner {
                stores,
                durable: std::sync::OnceLock::new(),
                config: self.inner.config,
                secrets: self.inner.secrets.clone(),
                engines: self.inner.engines.clone(),
                providers: Arc::clone(&self.inner.providers),
                hints: self.inner.hints.clone(),
            }),
        }
    }

    /// The store set.
    pub fn stores(&self) -> Arc<dyn StoreSet> {
        Arc::clone(&self.inner.stores)
    }

    /// The durable store every owner and mailbox commit goes through: the
    /// store set's.
    pub fn durable(&self) -> &Arc<dyn DurableStore> {
        self.inner
            .durable
            .get_or_init(|| self.inner.stores.durable_store())
    }

    /// The substrate's parameters.
    pub fn config(&self) -> &DurableConfig {
        &self.inner.config
    }

    /// The completion secrets wait keys are minted and verified under.
    pub fn completion_secrets(&self) -> &crate::runtime::actor::waits::CompletionKeySecrets {
        &self.inner.secrets
    }

    /// The host process engine of `kind`.
    pub fn process_engine(&self, kind: &str) -> Option<&Arc<dyn crate::ProcessEngine>> {
        self.inner.engines.get(kind)
    }

    /// The projection providers.
    pub fn projection_providers(&self) -> &Arc<dyn ProjectionProviders> {
        &self.inner.providers
    }

    /// The hints a node runner serving this backend wakes its actors by.
    #[must_use]
    pub fn hints(&self) -> &Hints {
        &self.inner.hints
    }

    /// Commit the mailbox transaction `tx` under `label`, then hand every
    /// actor it woke to this node's runner: an actor the node runs is hinted
    /// in process, and with signals an actor another node owns, or a readied
    /// unowned one, is published after the commit. A hint is only a hint:
    /// an actor whose hint is lost sees the commit at its next poll.
    ///
    /// # Errors
    ///
    /// The store's refusal; nothing was written.
    pub async fn commit_mail(
        &self,
        tx: lash_durable::MailTx,
        label: lash_durable::CommitLabel,
    ) -> Result<lash_durable::MailCommit, DurableError> {
        let commit = self.durable().commit_mail(tx, label).await?;
        self.inner.hints.woke(&commit);
        Ok(commit)
    }

    /// Wake `session`'s actor from outside a store transaction: a mailbox
    /// transaction with one wake.
    ///
    /// # Errors
    ///
    /// The store's refusal.
    pub async fn wake_session(&self, _session: &crate::SessionId) -> Result<(), DurableError> {
        todo!("L3s (FIG-5196): wake a session actor in a mailbox transaction")
    }

    /// Wake `process`'s actor from outside a store transaction: a mailbox
    /// transaction with one wake.
    ///
    /// # Errors
    ///
    /// The store's refusal.
    pub async fn wake_process(&self, process: &crate::ProcessId) -> Result<(), DurableError> {
        let actor = lash_durable::ActorKey::process(process.as_str()).map_err(|error| {
            DurableError::Store(lash_durable::StoreFailure {
                kind: lash_durable::StoreFailureKind::Corrupt,
                message: error.to_string(),
            })
        })?;
        let mut tx = lash_durable::MailTx::new();
        tx.wake(actor);
        self.durable()
            .commit_mail(tx, lash_durable::CommitLabel::MAIL_PROCESS)
            .await?;
        Ok(())
    }

    /// Redrive `process`'s parked actor as an operator, `requester`: clear
    /// its park, reset its activation-loop count and control-wake it, in
    /// one mailbox transaction. Answers whether it was parked.
    ///
    /// # Errors
    ///
    /// The store's refusal.
    pub async fn redrive_process(
        &self,
        process: &crate::ProcessId,
        requester: &str,
    ) -> Result<bool, DurableError> {
        let actor = lash_durable::ActorKey::process(process.as_str()).map_err(|error| {
            DurableError::Store(lash_durable::StoreFailure {
                kind: lash_durable::StoreFailureKind::Corrupt,
                message: error.to_string(),
            })
        })?;
        let mut tx = lash_durable::MailTx::new();
        tx.write(lash_durable::MailDomainWrite::Redrive(
            lash_durable::domain::RedriveRequest {
                actor,
                requester: requester.to_owned(),
            },
        ));
        let commit = self
            .durable()
            .commit_mail(tx, lash_durable::CommitLabel::MAIL_PROCESS)
            .await?;
        Ok(commit.answers.iter().any(|answer| {
            matches!(
                answer,
                lash_durable::MailAnswer::Redrive(lash_durable::domain::RedriveAnswer::Redriven)
            )
        }))
    }

    /// The identity of the storage this backend's sessions, processes and
    /// artifacts live in.
    pub fn binding_identity(&self) -> StoreBindingId {
        self.inner.stores.binding_identity().clone()
    }

    /// The clock the store set stamps from.
    pub fn clock(&self) -> Arc<dyn Clock> {
        self.inner.stores.clock()
    }

    /// The factory that creates and reopens this backend's session stores.
    pub fn session_store_factory(&self) -> Arc<dyn DeploymentStore> {
        self.inner.stores.session_store_factory()
    }

    pub fn attachment_referrers(&self) -> Arc<dyn crate::store::AttachmentReferrers> {
        self.inner.stores.attachment_referrers()
    }

    /// The durable registry of this backend's background processes.
    pub fn process_registry(&self) -> Arc<dyn ProcessRegistry> {
        self.inner.stores.process_registry()
    }

    /// The durable trigger subscriptions and occurrences.
    pub fn trigger_store(&self) -> Arc<dyn TriggerStore> {
        self.inner.stores.trigger_store()
    }

    /// The store of process execution environments.
    pub fn process_env_store(&self) -> Arc<dyn ProcessExecutionEnvStore> {
        self.inner.stores.process_env_store()
    }

    /// The store of turns' recorded preparation.
    pub fn turn_prelude_store(&self) -> Arc<dyn crate::TurnPreludeStore> {
        self.inner.stores.turn_prelude_store()
    }

    /// Retained results published by process-terminal sources.
    pub fn tool_material_store(&self) -> Arc<dyn crate::store::ToolMaterialStore> {
        self.inner.stores.tool_material_store()
    }

    /// The store of immutable process-definition descriptors.
    pub fn definition_store(&self) -> Arc<dyn crate::ProcessDefinitionStore> {
        self.inner.stores.definition_store()
    }

    /// Parent-owned worker accounting on this backend.
    pub fn worker_recovery(&self) -> Arc<dyn crate::store::worker_recovery::WorkerRecoveryStore> {
        self.inner.stores.worker_recovery()
    }

    /// The attachment byte store sessions write through.
    pub fn attachment_store(&self) -> Arc<dyn AttachmentStore> {
        self.inner.stores.attachment_store()
    }

    /// The Lashlang module-artifact store, beside the sessions that write
    /// its artifacts.
    pub fn module_artifacts(&self) -> Arc<dyn ModuleArtifactStore> {
        self.inner.stores.module_artifacts()
    }

    /// The recovery leader lease over the store set's storage.
    pub fn recovery_leader(&self) -> Arc<dyn crate::store::RecoveryLeaderStore> {
        self.inner.stores.recovery_leader()
    }

    /// The store set's obligation ledger of `kind`.
    pub fn obligation_ledger(
        &self,
        kind: crate::store::ObligationKind,
    ) -> Arc<dyn crate::store::ObligationLedger> {
        self.inner.stores.obligation_ledger(kind)
    }

    /// The store set's artifact-cleanup ledger (ADR 0113 §2.5).
    pub fn artifact_cleanup(&self) -> Arc<dyn crate::store::ArtifactCleanupLedger> {
        self.inner.stores.artifact_cleanup()
    }

    /// The store set's session-delete reads (ADR 0109 §4).
    pub fn session_delete_ledger(
        &self,
    ) -> Arc<dyn crate::store::session_delete::SessionDeleteLedger> {
        self.inner.stores.session_delete_ledger()
    }
}

impl std::fmt::Debug for Backend {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Backend")
            .field("stores", &self.binding_identity())
            .finish_non_exhaustive()
    }
}

/// Every persistence port of one SQL substrate: the storage the durable
/// backend is built over.
///
/// Every accessor hands out a handle on the store set's one instance of
/// that port.
pub trait StoreSet: Send + Sync {
    /// The durable store over this store set's database: actors, nodes,
    /// mail and the domain rows of ADR 0132.
    fn durable_store(&self) -> Arc<dyn DurableStore>;

    /// The cross-node signals over this store set's database: wake hints
    /// after commit and node liveness locks (L8, FIG-5178). `None` for a
    /// store with one node per database (SQLite), whose wakes stay in
    /// process; its runner relies on in-process hints and the polls.
    fn durable_signals(&self) -> Option<Arc<dyn lash_durable::Signals>>;

    /// The identity of this store set's storage.
    fn binding_identity(&self) -> &StoreBindingId;

    /// The clock this store set stamps from.
    fn clock(&self) -> Arc<dyn Clock>;

    /// The factory that creates and reopens this store set's session stores.
    fn session_store_factory(&self) -> Arc<dyn DeploymentStore>;

    /// The durable core attachment manifest and shared referrer fence.
    fn attachment_referrers(&self) -> Arc<dyn crate::store::AttachmentReferrers>;

    /// The durable registry of background processes.
    fn process_registry(&self) -> Arc<dyn ProcessRegistry>;

    /// The continuation records of [`Self::process_registry`]'s processes,
    /// which an engine that runs them resumes from.
    fn process_continuations(&self) -> Arc<dyn ProcessContinuationStore>;

    /// The durable trigger subscriptions and occurrences.
    fn trigger_store(&self) -> Arc<dyn TriggerStore>;

    /// The store of process execution environments.
    fn process_env_store(&self) -> Arc<dyn ProcessExecutionEnvStore>;

    /// The store of turns' recorded preparation, journaled by digest
    /// (FIG-5133), over the artifact referrer edges the cleanup relay ends.
    fn turn_prelude_store(&self) -> Arc<dyn crate::TurnPreludeStore>;

    /// Retained results published by Deferred sources.
    fn tool_material_store(&self) -> Arc<dyn crate::store::ToolMaterialStore>;

    /// The store of immutable process-definition descriptors (ADR 0113
    /// §3.6), in the same database as the modules and environments their
    /// manifests name.
    fn definition_store(&self) -> Arc<dyn crate::ProcessDefinitionStore>;

    /// Parent-owned recovery counters for model-code executions.
    fn worker_recovery(&self) -> Arc<dyn crate::store::worker_recovery::WorkerRecoveryStore>;

    /// The attachment byte store sessions write through.
    fn attachment_store(&self) -> Arc<dyn AttachmentStore>;

    /// The Lashlang module-artifact store.
    fn module_artifacts(&self) -> Arc<dyn ModuleArtifactStore>;

    /// The recovery leader lease over this storage (ADR 0109 §1.6).
    fn recovery_leader(&self) -> Arc<dyn crate::store::RecoveryLeaderStore>;

    /// The obligation ledger of `kind` (ADR 0109 §1.3).
    fn obligation_ledger(
        &self,
        kind: crate::store::ObligationKind,
    ) -> Arc<dyn crate::store::ObligationLedger>;

    /// The reads of a session's two-phase delete (ADR 0109 §4).
    fn session_delete_ledger(&self) -> Arc<dyn crate::store::session_delete::SessionDeleteLedger>;

    /// The artifact-cleanup ledger (ADR 0113 §2.5): the ledger
    /// [`Self::obligation_ledger`] answers for
    /// [`ObligationKind::ArtifactCleanup`](crate::store::ObligationKind::ArtifactCleanup),
    /// with the verbs beyond ADR 0109's.
    fn artifact_cleanup(&self) -> Arc<dyn crate::store::ArtifactCleanupLedger>;
}
