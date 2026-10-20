//! The durable substrate's backend (ADR 0132 §1). I0 (FIG-5194) pins this
//! builder's full shape, and L3 (FIG-5172) makes a built backend serve.
//!
//! `docs/operations/durable-hosting.md` is the host guide: node identity,
//! topology, completion keys, `DurableSettings` and the process-engine
//! contract.

use std::sync::Arc;

pub use lash_core::ResolveAnswer;
/// The durable store port: actors, nodes, epochs, the owner and mailbox
/// transactions, the domain rows the substrate lanes add, and the substrate's
/// parameters.
pub use lash_core::durable_port::*;
use lash_execution_contracts::backend::Backend;
pub use lash_execution_contracts::backend::BackendParts;
pub use lash_execution_contracts::backend::DurableBuildError;
pub use lash_execution_contracts::backend::NodePackages;
use lash_execution_contracts::backend::StoreSet;
use lash_execution_contracts::process::ProcessEngine;
pub use lash_execution_contracts::projection::NoProjectionProviders;
pub use lash_execution_contracts::projection::ProjectionProviders;
pub use lash_execution_contracts::waits::PinnedKey;

/// Builds the one [`Backend`] a [`LashCore`](crate::LashCore) takes: lash's
/// own durable engine over one store set.
///
/// [`build`](Self::build) refuses settings that break a rule, two process
/// engines of one kind and two projection providers of one type.
pub struct DurableBackendBuilder {
    stores: Arc<dyn StoreSet>,
    settings: DurableSettings,
    /// Whether a host configuration owns `settings`, so that
    /// [`config`](Self::config) is a second, refused source of them.
    host_settings: bool,
    overridden: bool,
    engines: Vec<Arc<dyn ProcessEngine>>,
    #[cfg(feature = "codemode")]
    providers: Vec<Arc<dyn crate::vm::ProjectionProvider>>,
    #[cfg(feature = "synthetic-next")]
    previous_build: bool,
    #[cfg(feature = "synthetic-next")]
    closing_window: bool,
}

impl DurableBackendBuilder {
    /// A builder of the durable backend over `stores`, with the default
    /// settings, no engines and no providers.
    pub fn new(stores: Arc<dyn StoreSet>) -> Self {
        Self {
            stores,
            settings: DurableSettings::standard(),
            host_settings: false,
            overridden: false,
            engines: Vec::new(),
            #[cfg(feature = "codemode")]
            providers: Vec::new(),
            #[cfg(feature = "synthetic-next")]
            previous_build: false,
            #[cfg(feature = "synthetic-next")]
            closing_window: false,
        }
    }

    /// A builder of the durable backend over `host`'s PostgreSQL store set,
    /// writing attachment bytes to `attachments`, under the durable
    /// settings of `host.effective_config().node`: the one place a PostgreSQL
    /// host's durable settings live.
    #[cfg(feature = "postgres")]
    pub fn postgres(
        host: &crate::postgres::PostgresHost,
        attachments: Arc<dyn crate::persistence::AttachmentStore>,
    ) -> Self {
        Self {
            settings: host.effective_config().node,
            host_settings: true,
            ..Self::new(Arc::new(crate::postgres::PostgresStoreSet::new(
                host.storage(),
                attachments,
            )))
        }
    }

    /// The substrate's parameters; [`build`](Self::build) validates them. A
    /// builder from [`postgres`](Self::postgres) refuses them at build:
    /// they belong to the host configuration.
    #[must_use]
    pub fn config(mut self, settings: DurableSettings) -> Self {
        self.settings = settings;
        self.overridden = true;
        self
    }

    /// This backend as the build before the synthetic successor declares
    /// it (ADR 0115 §6): its actor state holds that build's formats. The
    /// two-build laws run a node of each build from one binary with it.
    #[cfg(feature = "synthetic-next")]
    #[must_use]
    pub fn previous_build(mut self) -> Self {
        self.previous_build = true;
        self
    }

    /// This backend as the build after the synthetic successor's, which
    /// closes the window it opened (ADR 0115 §3.5): it no longer interprets
    /// the kernel version before the successor's, so it neither decodes nor
    /// carries the formats a build of that version wrote, and its node does
    /// not start while a process or session is still in one. The two-build
    /// laws run the closing build from the same binary with it.
    #[cfg(feature = "synthetic-next")]
    #[must_use]
    pub fn closing_window(mut self) -> Self {
        self.closing_window = true;
        self
    }

    /// A host process engine; one per kind.
    #[must_use]
    pub fn process_engine(mut self, engine: Arc<dyn ProcessEngine>) -> Self {
        self.engines.push(engine);
        self
    }

    /// A projection provider; one per projection type.
    #[cfg(feature = "codemode")]
    #[must_use]
    pub fn projection_provider(mut self, provider: Arc<dyn crate::vm::ProjectionProvider>) -> Self {
        self.providers.push(provider);
        self
    }

    /// The durable backend over this builder's store set.
    ///
    /// # Errors
    /// [`DurableBuildError`] when the backend cannot be assembled.
    pub fn build(self) -> Result<Backend, DurableBuildError> {
        if self.host_settings && self.overridden {
            return Err(DurableBuildError::SettingsOwnedByHost);
        }
        #[cfg(feature = "codemode")]
        let retired = self.retired_kernel_version();
        let surfaces = self.surfaces();
        let backend = Backend::assemble(BackendParts {
            #[cfg(feature = "codemode")]
            providers: projection_catalog(self.providers)?,
            #[cfg(not(feature = "codemode"))]
            providers: Arc::new(NoProjectionProviders),
            stores: self.stores,
            settings: self.settings,
            engines: self.engines,
            formats: surfaces.clone(),
        })?;
        // A build that retires the kernel version before its own does not
        // decode what that version's build wrote, and its node does not
        // start while a process or session is still in it.
        #[cfg(feature = "codemode")]
        let backend = match retired {
            Some(kernel) => crate::kernel_migration::retiring(&backend, kernel),
            None => backend,
        };
        Ok(backend)
    }

    /// The format surfaces this build's actors hold beside the core's own.
    fn surfaces(&self) -> Vec<FormatSurface> {
        #[cfg(feature = "synthetic-next")]
        if self.previous_build {
            return crate::formats::previous_actor_state_surfaces();
        }
        crate::formats::actor_state_surfaces()
    }

    /// The kernel version this build no longer interprets, whose window is
    /// closed.
    #[cfg(feature = "codemode")]
    fn retired_kernel_version(&self) -> Option<u32> {
        #[cfg(feature = "synthetic-next")]
        if self.closing_window {
            return lash_vm_runtime::previous_kernel_version();
        }
        crate::kernel_migration::retired_kernel_version()
    }
}

/// The catalog of `providers`, refusing two of one type. Lash provides
/// `history` itself when `rlm` is enabled, so that feature refuses a host
/// provider of it as the second of its type.
#[cfg(feature = "codemode")]
fn projection_catalog(
    providers: Vec<Arc<dyn crate::vm::ProjectionProvider>>,
) -> Result<Arc<dyn lash_execution_contracts::projection::ProjectionProviders>, DurableBuildError> {
    if providers.is_empty() {
        return Ok(Arc::new(NoProjectionProviders));
    }
    let mut catalog = crate::vm::ProjectionCatalog::new();
    for provider in providers {
        let projection = provider.kind().to_owned();
        let duplicate = || DurableBuildError::DuplicateProvider {
            projection: projection.clone(),
        };
        #[cfg(feature = "rlm")]
        if projection == lash_protocol_rlm::HISTORY_PROJECTION {
            return Err(duplicate());
        }
        catalog.register(provider).map_err(|_| duplicate())?;
    }
    Ok(Arc::new(catalog))
}

#[cfg(all(test, feature = "sqlite"))]
mod tests {
    use super::*;
    use std::time::Duration;

    /// D-DEFAULTS2: the facade's non-default SQLite coordination reaches its reader.
    #[tokio::test]
    async fn sqlite_operational_policy_reaches_catalog_readers() {
        let mut options = crate::sqlite::SqliteStoreSetOptions::memory();
        options
            .store
            .connection_policy
            .operational
            .readonly_busy_timeout = Duration::from_millis(37);
        options
            .store
            .connection_policy
            .operational
            .readonly_cache_size = -123;
        let stores = crate::sqlite::SqliteStoreSet::memory_with_options_and_clock(
            options,
            Arc::new(crate::testing::TestClock::new(1_000)),
        )
        .await
        .expect("configured SQLite store set");
        assert_eq!(
            stores
                .reader_settings_for_testing()
                .await
                .expect("reader pragmas"),
            (37, -123)
        );
    }

    /// D-DEFAULTS2: operational settings cross the facade builder unchanged.
    #[tokio::test]
    async fn durable_preset_overrides_reach_the_backend() {
        let stores = Arc::new(
            crate::sqlite::SqliteStoreSet::memory()
                .await
                .expect("SQLite"),
        );
        let mut settings = DurableSettings::development();
        settings.claim_batch = 3;
        settings.max_active = 11;
        settings.group_commit.max_members = 7;
        settings.group_commit.window = Duration::from_millis(9);
        settings.cascade_batch = 19;
        settings.lease.claim_poll = Duration::from_millis(79);
        settings.notifier = Notifier::PollOnly;
        let backend = DurableBackendBuilder::new(stores)
            .config(settings)
            .build()
            .expect("backend");
        assert_eq!(backend.config().settings(), settings);
        assert_eq!(backend.config().lease().settings(), settings.lease);
    }

    /// D-DEFAULTS2: the facade's PostgreSQL policy sizes the actual lazy pool
    /// and carries deadlines to the SQL prelude without connecting a server.
    #[cfg(feature = "postgres")]
    #[tokio::test]
    async fn postgres_operational_policy_reaches_pool_and_transaction() {
        use crate::postgres::*;
        let mut config = PostgresHostConfig::development();
        config.roles.work.max_connections = 6;
        config.roles.work.acquire_timeout = Duration::from_millis(73);
        config.guards.durable.lock = ServerTimeout::Limit(Duration::from_millis(67));
        config.validate().expect("host policy");
        let factory = PostgresConnectionFactory::new(
            PostgresEndpoints::from_url("postgres://localhost/lash").expect("endpoint"),
            config.connection.clone(),
        );
        let pool = factory.pool(
            ConnectionRole::Work,
            &config.roles.work,
            Some(&config.guards.ordinary),
        );
        assert_eq!(pool.options().get_max_connections(), 6);
        assert_eq!(
            pool.options().get_acquire_timeout(),
            Duration::from_millis(73)
        );
        let prelude = TransactionPrelude::new(&config.guards.durable);
        assert!(
            prelude.statement().contains("lock_timeout = 67"),
            "{}",
            prelude.statement()
        );
        assert_eq!(prelude.deadline(), config.guards.durable.operation_deadline);
        let omitted: PostgresHostConfig =
            serde_json::from_str("{}").expect("omitted optional settings");
        assert_eq!(omitted, PostgresHostConfig::standard());
        PostgresHostConfig::standard().validate().expect("standard");
    }
}

/// One unfinished actor no live node can run (FIG-6993): what the live
/// node closest to running it lacks, or its whole hard record when no
/// node is live.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct StrandedWork {
    /// The actor's stored key.
    pub actor: String,
    /// What it requires and no live node offers it all of.
    pub lacks: ExecutionRequirements,
}

/// One page of the strand report: the stranded work among the unfinished
/// actors after `after`, scanning at most `limit`, and the key to resume
/// from when more may follow.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct StrandPage {
    pub stranded: Vec<StrandedWork>,
    pub next: Option<String>,
}

/// The strand report (FIG-6993): of the unfinished actors with a claim
/// record after `after`, at most `limit` scanned in key order, each no live
/// node can run, with what it lacks. Work stays parked rather than failing
/// while no node registers what it needs, so this is where an operator
/// finds it: a deploy that offers what it lacks lets it run. It writes
/// nothing.
///
/// # Errors
///
/// The store's failure to answer.
pub async fn strand_report(
    backend: &Backend,
    after: Option<&ActorKey>,
    limit: usize,
) -> Result<StrandPage, DurableError> {
    let durable = backend.durable();
    let live = durable.live_nodes().await?;
    let rows = durable.actor_requirements(after, limit).await?;
    let next = (rows.len() == limit && limit > 0)
        .then(|| rows.last().map(|(key, _)| key.as_str().to_owned()))
        .flatten();
    let stranded = rows
        .into_iter()
        .filter_map(|(actor, requires)| {
            lash_core::durable_port::packages::strand(&requires, &live).map(|lacks| StrandedWork {
                actor: actor.as_str().to_owned(),
                lacks,
            })
        })
        .collect();
    Ok(StrandPage { stranded, next })
}
