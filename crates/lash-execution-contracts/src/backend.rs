//! The one value a runtime takes its persistence ports from: the durable
//! backend over one store set (ADR 0132 §1; S9 of I0, FIG-5194).

use std::collections::BTreeMap;
use std::sync::Arc;

use lash_durable::runner::Hints;
use lash_durable::{
    DurableConfig, DurableConfigError, DurableError, DurableSettings, DurableStore,
};

use crate::projection::ProjectionProviders;
use crate::{
    AttachmentStore, Clock, DeploymentStore, ModuleArtifactStore, ProcessEnvStore, ProcessRegistry,
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
///
/// New failure kinds may be added. Each named variant retains its complete
/// identifying payload so backend assemblers can construct these typed causes.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
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
    memory: lash_durable::owner_memory::NodeMemory,
    engines: BTreeMap<String, Arc<dyn crate::ProcessEngine>>,
    /// How each engine that carries an earlier build's state forward does
    /// it, by engine kind ([`Backend::with_state_migration`]).
    migrations: BTreeMap<String, Arc<dyn crate::EngineStateMigration>>,
    /// The status each engine that declares one projects from its state, by
    /// engine kind ([`Backend::with_status_projections`]).
    status_projections: BTreeMap<String, Arc<dyn crate::EngineStatusProjection>>,
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
    /// The package code a node of this build runs, which it installs and
    /// advertises before it starts ([`Backend::register_packages`]). Like
    /// the hints, it is the node's: every backend derived from this one
    /// shares the slot, so an activation built before the registration
    /// reads it.
    packages: Arc<std::sync::OnceLock<Arc<dyn NodePackages>>>,
}

/// The package code a node of this build runs (FIG-6918, FIG-6991,
/// FIG-6993). Before the node starts it installs its artifacts and sets in
/// the store, each set with its surface and every definition, and answers
/// what the node offers running code: the natives it binds, the kernel
/// versions it interprets and its own package set, whose stored surface is
/// the node's candidate for new cells. A node claims an actor to run it
/// only when its capabilities meet the actor's hard requirements, and a
/// surface is promoted to new cells only once every live serving node
/// meets its requirements.
#[async_trait::async_trait]
pub trait NodePackages: Send + Sync {
    /// The declared surface for a new Standard catalog.
    fn surface_view(&self) -> Arc<dyn lash_kernel_dialect::ExportView + Send + Sync>;
    /// The package code the node registers: every definition it binds,
    /// over which it reads a stored surface it was not built with.
    fn packages(&self) -> &Arc<lash_kernel_package::HeldPackages>;

    /// Installs the node's artifacts and sets, each set with its surface
    /// and definitions, in `backend`'s store, and answers what the node
    /// offers running code.
    ///
    /// # Errors
    ///
    /// The store's refusal of the install.
    async fn install(
        &self,
        backend: &Backend,
    ) -> Result<lash_durable::NodeCapabilities, lash_durable::DurableError>;
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
        let policy = parts
            .settings
            .owner_retention
            .ok_or(DurableConfigError::MissingOwnerRetention)?;
        let memory = lash_durable::owner_memory::NodeMemory::new(policy);
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
                migrations: BTreeMap::new(),
                status_projections: BTreeMap::new(),
                stores: parts.stores,
                durable: std::sync::OnceLock::new(),
                config,
                memory,
                engines,
                providers,
                formats,
                surfaces: parts.formats,
                hints: Hints::default(),
                packages: Arc::default(),
            }),
        })
    }

    /// A backend over `stores` with explicit fixture retention limits, the
    /// other default settings, no engines and no projection providers.
    #[cfg(any(test, feature = "testing"))]
    #[expect(
        clippy::expect_used,
        reason = "the default settings validate and an empty registration has no duplicate"
    )]
    #[must_use]
    pub fn for_testing(
        stores: Arc<dyn StoreSet>,
        owner_retention: lash_durable::OwnerRetentionSettings,
    ) -> Self {
        Self::assemble(BackendParts {
            stores,
            settings: DurableSettings {
                owner_retention: Some(owner_retention),
                ..DurableSettings::default()
            },
            engines: Vec::new(),
            providers: Arc::new(crate::projection::NoProjectionProviders),
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
                migrations: self.inner.migrations.clone(),
                status_projections: self.inner.status_projections.clone(),
                stores,
                durable: std::sync::OnceLock::new(),
                config: self.inner.config,
                memory: self.inner.memory.clone(),
                engines: self.inner.engines.clone(),
                providers: Arc::clone(&self.inner.providers),
                formats: self.inner.formats.clone(),
                surfaces: self.inner.surfaces.clone(),
                hints: self.inner.hints.clone(),
                packages: self.inner.packages.clone(),
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
                migrations: self.inner.migrations.clone(),
                status_projections: self.inner.status_projections.clone(),
                stores: Arc::clone(&self.inner.stores),
                durable,
                config: self.inner.config,
                memory: self.inner.memory.clone(),
                formats: crate::formats::BuildFormats::new(&engine_formats, &self.inner.surfaces)
                    .keeping(&self.inner.formats),
                engines: held,
                providers: Arc::clone(&self.inner.providers),
                surfaces: self.inner.surfaces.clone(),
                hints: self.inner.hints.clone(),
                packages: self.inner.packages.clone(),
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

    /// Node-wide owner allocation accounting, shared by derived backends.
    pub fn owner_memory(&self) -> &lash_durable::owner_memory::NodeMemory {
        &self.inner.memory
    }

    /// The substrate's parameters.
    pub fn config(&self) -> &DurableConfig {
        &self.inner.config
    }

    /// This backend with the engine of `kind` carrying its state forward
    /// from the formats `migration` names (ADR 0106 §1): a node serving it
    /// also decodes a process the previous build left in one of them, whose
    /// set is that format with `previous`, the other formats that build's
    /// actors held.
    #[must_use]
    pub fn with_state_migration(
        &self,
        kind: &str,
        migration: Arc<dyn crate::EngineStateMigration>,
        previous: &[lash_durable::FormatSurface],
    ) -> Self {
        let mut formats = self.inner.formats.clone();
        for format in migration.carries() {
            formats = formats.carrying(&format, previous);
        }
        // A build that retires every format the migration carries from
        // carries nothing forward (ADR 0115 §3.5).
        if formats == self.inner.formats {
            return self.clone();
        }
        let mut migrations = self.inner.migrations.clone();
        migrations.insert(kind.to_owned(), migration);
        // Both serve the one durable store, whose wakes reach one runner.
        let durable = std::sync::OnceLock::from(Arc::clone(self.durable()));
        Self {
            inner: Arc::new(BackendInner {
                stores: Arc::clone(&self.inner.stores),
                durable,
                config: self.inner.config,
                memory: self.inner.memory.clone(),
                formats,
                engines: self.inner.engines.clone(),
                migrations,
                status_projections: self.inner.status_projections.clone(),
                providers: Arc::clone(&self.inner.providers),
                surfaces: self.inner.surfaces.clone(),
                hints: self.inner.hints.clone(),
                packages: self.inner.packages.clone(),
            }),
        }
    }

    /// This backend retiring the sets of the build before it, whose process
    /// engine state was in `format` and whose actors also held `previous`
    /// (ADR 0115 §3.5): its nodes neither decode nor carry them forward, and
    /// do not start while an actor is still in one. `sweep` is the operator
    /// command that has a node of that build carry one forward.
    #[must_use]
    pub fn retiring(
        &self,
        format: &crate::EngineStateFormat,
        previous: &[lash_durable::FormatSurface],
        sweep: &str,
    ) -> Self {
        let durable = std::sync::OnceLock::from(Arc::clone(self.durable()));
        Self {
            inner: Arc::new(BackendInner {
                stores: Arc::clone(&self.inner.stores),
                durable,
                config: self.inner.config,
                memory: self.inner.memory.clone(),
                formats: self.inner.formats.clone().retiring(format, previous, sweep),
                engines: self.inner.engines.clone(),
                migrations: self.inner.migrations.clone(),
                status_projections: self.inner.status_projections.clone(),
                providers: Arc::clone(&self.inner.providers),
                surfaces: self.inner.surfaces.clone(),
                hints: self.inner.hints.clone(),
                packages: self.inner.packages.clone(),
            }),
        }
    }

    /// The formats this build's actors hold beside the core's own.
    pub fn surfaces(&self) -> &[lash_durable::FormatSurface] {
        &self.inner.surfaces
    }

    /// The package code a node of this build runs, when the host
    /// registered any.
    pub fn packages(&self) -> Option<&Arc<dyn NodePackages>> {
        self.inner.packages.get()
    }

    /// Register `packages` as the code this backend's node runs, which it
    /// installs and advertises before it starts. A node has one
    /// registration: a later one is refused, answering `false`.
    pub fn register_packages(&self, packages: Arc<dyn NodePackages>) -> bool {
        self.inner.packages.set(packages).is_ok()
    }

    /// This backend recording the status each of `projections` projects
    /// from its engine's committed state, with each transition of a process
    /// of that engine kind.
    #[must_use]
    pub fn with_status_projections<'a>(
        &self,
        projections: impl IntoIterator<Item = (&'a str, &'a Arc<dyn crate::EngineStatusProjection>)>,
    ) -> Self {
        let mut status_projections = self.inner.status_projections.clone();
        for (kind, projection) in projections {
            status_projections.insert(kind.to_owned(), Arc::clone(projection));
        }
        let durable = std::sync::OnceLock::from(Arc::clone(self.durable()));
        Self {
            inner: Arc::new(BackendInner {
                stores: Arc::clone(&self.inner.stores),
                durable,
                config: self.inner.config,
                memory: self.inner.memory.clone(),
                formats: self.inner.formats.clone(),
                engines: self.inner.engines.clone(),
                migrations: self.inner.migrations.clone(),
                status_projections,
                providers: Arc::clone(&self.inner.providers),
                surfaces: self.inner.surfaces.clone(),
                hints: self.inner.hints.clone(),
                packages: self.inner.packages.clone(),
            }),
        }
    }

    /// The status the engine of `kind` projects from its state, when its
    /// registration declares one.
    pub fn status_projection(&self, kind: &str) -> Option<&Arc<dyn crate::EngineStatusProjection>> {
        self.inner.status_projections.get(kind)
    }

    /// How the engine of `kind` carries an earlier build's state forward,
    /// when it does.
    pub fn state_migration(&self, kind: &str) -> Option<&Arc<dyn crate::EngineStateMigration>> {
        self.inner.migrations.get(kind)
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
    pub fn hint_woken(&self, commit: &lash_durable::MailCommit) {
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
        let actor = lash_durable::ActorKey::process(process.as_str()).map_err(corrupt_key)?;
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
        let actor = lash_durable::ActorKey::process(process.as_str()).map_err(corrupt_key)?;
        self.redrive(actor, requester, lash_durable::CommitLabel::MAIL_PROCESS)
            .await
    }

    /// Redrive `session`'s parked actor as an operator, `requester`, as
    /// [`redrive_process`](Self::redrive_process) redrives a process's: its
    /// next claim runs its unfinished turn again from its committed state
    /// (FIG-5367). Answers whether it was parked.
    ///
    /// # Errors
    ///
    /// The store's refusal.
    pub async fn redrive_session(
        &self,
        session: &crate::SessionId,
        requester: &str,
    ) -> Result<bool, DurableError> {
        let actor = lash_durable::ActorKey::session(session.as_str()).map_err(corrupt_key)?;
        self.redrive(actor, requester, lash_durable::CommitLabel::MAIL_SESSION)
            .await
    }

    async fn redrive(
        &self,
        actor: lash_durable::ActorKey,
        requester: &str,
        label: lash_durable::CommitLabel,
    ) -> Result<bool, DurableError> {
        let mut tx = lash_durable::MailTx::new();
        tx.write(lash_durable::MailDomainWrite::Redrive(
            lash_durable::domain::RedriveRequest {
                actor,
                requester: requester.to_owned(),
            },
        ));
        let commit = self.durable().commit_mail(tx, label).await?;
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

    /// A process's recorded authority, read atomically with its environment bytes.
    pub fn process_authority_reads(&self) -> Arc<dyn ProcessAuthorityReads> {
        self.inner.stores.process_authority_reads()
    }

    /// The store of process execution environments.
    pub fn process_env_store(&self) -> Arc<dyn ProcessEnvStore> {
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

    /// Where this store set's producer commits hand the actors they woke,
    /// after the commit (ADR 0132 §12): a node serving over the store set
    /// attaches its hints here, so an accepted send reaches the session's
    /// owner on this node at once and its owner on another node through the
    /// node wakes, not at the owner's claim poll.
    fn wake_sink(&self) -> lash_durable::runner::WakeSink;

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

    /// Atomic process/environment observation, without executable reconstruction.
    fn process_authority_reads(&self) -> Arc<dyn ProcessAuthorityReads>;

    /// The store of process execution environments.
    fn process_env_store(&self) -> Arc<dyn ProcessEnvStore>;

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

/// An id no actor key can carry, as the store's refusal.
fn corrupt_key(error: lash_durable::ActorKeyError) -> DurableError {
    DurableError::Store(lash_durable::StoreFailure {
        kind: lash_durable::StoreFailureKind::Corrupt,
        message: error.to_string(),
    })
}

/// Atomic observation of a retained process and its recorded environment.
/// Implementations issue one SQL statement, including tombstone detection.
/// Unknown ids return `None`; pruned ids return `StoreError::ProcessAuthorityPruned`.
/// A retained engine row with absent environment bytes still returns its record;
/// the caller verifies and decodes the bytes and reports their absence.
#[async_trait::async_trait]
pub trait ProcessAuthorityReads: Send + Sync {
    /// Recorded environment bytes by content reference, in one store statement.
    /// Missing bytes return `None`; the caller verifies the content address.
    async fn environment_bytes(
        &self,
        env: &crate::ProcessEnvRef,
    ) -> Result<Option<Vec<u8>>, crate::StoreError>;

    async fn process_authority(
        &self,
        process: &crate::ProcessId,
    ) -> Result<Option<(crate::ProcessRecord, Option<Vec<u8>>)>, crate::StoreError>;
}
