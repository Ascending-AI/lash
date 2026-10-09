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
    AttachmentStore, Clock, DeploymentStore, ModuleArtifactStore, ProcessExecutionEnvStore,
    ProcessRegistry,
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
    /// Two process engines declare one kind.
    #[error("two process engines declare kind `{kind}`")]
    DuplicateEngine {
        /// The kind.
        kind: String,
    },
    /// A process engine declares a state format for another kind.
    #[error("process engine `{kind}` declares state format for `{format_kind}`")]
    EngineFormatMismatch {
        /// The registered engine kind.
        kind: String,
        /// The kind its state format declares.
        format_kind: String,
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
    /// Settings were given to a builder whose host configuration already
    /// owns them: a host's durable settings live in one place.
    #[error("the durable settings belong to the host configuration; set them there")]
    SettingsOwnedByHost,
}

/// What a durable backend is assembled from.
pub struct BackendParts {
    /// The store set.
    pub stores: Arc<dyn StoreSet>,
    /// The substrate parameters, validated by assembly.
    pub settings: DurableSettings,
    /// The host process engines, one per kind.
    pub engines: Vec<Arc<dyn crate::ProcessEngine>>,
    /// The projection providers' catalog.
    pub providers: Arc<dyn ProjectionProviders>,
    /// The durable formats actor state holds beyond this crate's own: the
    /// VM's continuation and snapshot formats when the assembler links the
    /// VM. Part of every actor kind's format set (ADR 0106 §1).
    pub formats: Vec<lash_durable::FormatSurface>,
}

/// The one value a runtime takes every port from: the store set, its
/// durable store, the substrate's parameters, the host process engines and the projection providers (ADR 0132 §1).
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
    engines: BTreeMap<String, Arc<dyn crate::ProcessEngine>>,
    providers: Arc<dyn ProjectionProviders>,
    /// The format sets this build writes and decodes.
    formats: crate::formats::BuildFormats,
    /// The formats actor state holds beyond this crate's own, which every
    /// format set adds ([`BackendParts::formats`]).
    surfaces: Vec<lash_durable::FormatSurface>,
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
    /// [`DurableBuildError`]: invalid settings, mismatched engine formats,
    /// duplicate engines or providers.
    pub fn assemble(parts: BackendParts) -> Result<Self, DurableBuildError> {
        let config = parts.settings.validate()?;
        let mut engines = BTreeMap::new();
        for engine in parts.engines {
            let kind = engine.kind().to_owned();
            let format = engine.state_format();
            if format.kind != kind {
                return Err(DurableBuildError::EngineFormatMismatch {
                    kind,
                    format_kind: format.kind,
                });
            }
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
        let engine_formats: Vec<_> = engines
            .values()
            .map(|engine| engine.state_format())
            .collect();
        let formats = crate::formats::BuildFormats::new(&engine_formats, &parts.formats);
        Ok(Self {
            inner: Arc::new(BackendInner {
                stores: parts.stores,
                durable: std::sync::OnceLock::new(),
                config,
                engines,
                providers,
                formats,
                surfaces: parts.formats,
                hints: Hints::default(),
            }),
        })
    }

    /// A backend over `stores` with the default settings, no engines and no
    /// projection providers.
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
            engines: Vec::new(),
            providers: Arc::new(crate::runtime::actor::projection::NoProjectionProviders),
            formats: Vec::new(),
        })
        .expect("a testing backend assembles")
    }

    /// This backend over `stores` instead, with the same configuration,
    /// engines and providers: what a test that decorates store
    /// ports builds.
    #[cfg(any(test, feature = "testing"))]
    #[must_use]
    pub fn over_stores(&self, stores: Arc<dyn StoreSet>) -> Self {
        Self {
            inner: Arc::new(BackendInner {
                stores,
                durable: std::sync::OnceLock::new(),
                config: self.inner.config,
                engines: self.inner.engines.clone(),
                providers: Arc::clone(&self.inner.providers),
                formats: self.inner.formats.clone(),
                surfaces: self.inner.surfaces.clone(),
                hints: self.inner.hints.clone(),
            }),
        }
    }

    /// This backend also advancing `engines`: the same store set, durable
    /// store, configuration, providers and hints, with each engine
    /// of a kind it does not hold added, and its format sets with theirs. A
    /// kind it holds keeps its own engine. What a core's node serves, so an
    /// engine a plugin contributes advances there as a host's does.
    #[must_use]
    pub fn with_process_engines(
        &self,
        engines: impl IntoIterator<Item = Arc<dyn crate::ProcessEngine>>,
    ) -> Self {
        let mut held = self.inner.engines.clone();
        for engine in engines {
            held.entry(engine.kind().to_owned()).or_insert(engine);
        }
        let engine_formats: Vec<_> = held.values().map(|engine| engine.state_format()).collect();
        // Both serve the one durable store, whose wakes reach one runner.
        let durable = std::sync::OnceLock::from(Arc::clone(self.durable()));
        Self {
            inner: Arc::new(BackendInner {
                stores: Arc::clone(&self.inner.stores),
                durable,
                config: self.inner.config,
                formats: crate::formats::BuildFormats::new(&engine_formats, &self.inner.surfaces),
                engines: held,
                providers: Arc::clone(&self.inner.providers),
                surfaces: self.inner.surfaces.clone(),
                hints: self.inner.hints.clone(),
            }),
        }
    }

    /// The format sets this build writes and decodes (ADR 0106 §1): a node
    /// serving this backend registers [`BuildFormats::decodes`](crate::formats::BuildFormats::decodes).
    #[must_use]
    pub fn formats(&self) -> &crate::formats::BuildFormats {
        &self.inner.formats
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

    /// The host process engine of `kind`.
    pub fn process_engine(&self, kind: &str) -> Option<&Arc<dyn crate::ProcessEngine>> {
        self.inner.engines.get(kind)
    }

    /// Every host process engine, by kind.
    pub fn process_engines(&self) -> impl Iterator<Item = &Arc<dyn crate::ProcessEngine>> {
        self.inner.engines.values()
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
    /// in process, and with node wakes an actor another node owns, or a readied
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
        self.hint_woken(&commit);
        Ok(commit)
    }

    /// Hand every actor `commit` woke to this node's runner, as
    /// [`Self::commit_mail`] does after its commit.
    pub(crate) fn hint_woken(&self, commit: &lash_durable::MailCommit) {
        self.inner.hints.woke(commit);
    }

    /// Wake `session`'s actor from outside a store transaction: a mailbox
    /// transaction with one wake.
    ///
    /// # Errors
    ///
    /// The store's refusal.
    pub async fn wake_session(&self, session: &crate::SessionId) -> Result<(), DurableError> {
        let actor = lash_durable::ActorKey::session(session.as_str()).map_err(|error| {
            DurableError::Store(lash_durable::StoreFailure {
                kind: lash_durable::StoreFailureKind::Corrupt,
                message: format!("session {session} names no actor: {error}"),
            })
        })?;
        let mut tx = lash_durable::MailTx::new();
        tx.wake(actor);
        self.durable()
            .commit_mail(tx, lash_durable::CommitLabel::MAIL_SESSION)
            .await
            .map(|_| ())
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

    /// The attachment byte store sessions write through.
    pub fn attachment_store(&self) -> Arc<dyn AttachmentStore> {
        self.inner.stores.attachment_store()
    }

    /// The Lash VM module-artifact store, beside the sessions that write
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

    /// The node wakes over this store set's database: wake hints after
    /// commit and node liveness locks (L8, FIG-5178), shared by every node
    /// over it, in this process or another. `None` for a store that lives
    /// in one process (a SQLite memory database); its runner relies on
    /// in-process hints and the polls.
    fn node_wakes(&self) -> Option<Arc<dyn lash_durable::NodeWakes>>;

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

    /// The attachment byte store sessions write through.
    fn attachment_store(&self) -> Arc<dyn AttachmentStore>;

    /// The Lash VM module-artifact store.
    fn module_artifacts(&self) -> Arc<dyn ModuleArtifactStore>;

    /// The recovery leader lease over this storage (ADR 0109 §1.6).
    fn recovery_leader(&self) -> Arc<dyn crate::store::RecoveryLeaderStore>;

    /// The obligation ledger of `kind` (ADR 0109 §1.3).
    fn obligation_ledger(
        &self,
        kind: crate::store::ObligationKind,
    ) -> Arc<dyn crate::store::ObligationLedger>;

    /// The artifact-cleanup ledger (ADR 0113 §2.5): the ledger
    /// [`Self::obligation_ledger`] answers for
    /// [`ObligationKind::ArtifactCleanup`](crate::store::ObligationKind::ArtifactCleanup),
    /// with the verbs beyond ADR 0109's.
    fn artifact_cleanup(&self) -> Arc<dyn crate::store::ArtifactCleanupLedger>;
}
