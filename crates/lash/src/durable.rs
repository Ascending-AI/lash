//! The durable substrate's backend (ADR 0132 §1). I0 (FIG-5194) pins this
//! builder's full shape, and L3 (FIG-5172) makes a built backend serve.
//!
//! `docs/operations/durable-hosting.md` is the host guide: node identity,
//! topology, completion keys, `DurableSettings` and the process-engine
//! contract.

use std::sync::Arc;

/// The durable store port: actors, nodes, epochs, the owner and mailbox
/// transactions, the domain rows the substrate lanes add, and the substrate's
/// parameters.
pub use lash_core::durable_port::*;
use lash_core::{Backend, ProcessEngine, StoreSet};
pub use lash_core::{
    BackendParts, DurableBuildError, NoProjectionProviders, PinnedKey, ProjectionProviders,
    ResolveAnswer,
};

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
    #[cfg(feature = "rlm")]
    providers: Vec<Arc<dyn lashlang::ProjectionProvider>>,
}

impl DurableBackendBuilder {
    /// A builder of the durable backend over `stores`, with the default
    /// settings, no engines and no providers.
    pub fn new(stores: Arc<dyn StoreSet>) -> Self {
        Self {
            stores,
            settings: DurableSettings::default(),
            host_settings: false,
            overridden: false,
            engines: Vec::new(),
            #[cfg(feature = "rlm")]
            providers: Vec::new(),
        }
    }

    /// A builder of the durable backend over `host`'s PostgreSQL store set,
    /// writing attachment bytes to `attachments`, under the durable
    /// settings of `host.effective_config.node`: the one place a PostgreSQL
    /// host's durable settings live.
    #[cfg(feature = "postgres")]
    pub fn postgres(
        host: &crate::postgres::PostgresHost,
        attachments: Arc<dyn crate::persistence::AttachmentStore>,
    ) -> Self {
        Self {
            settings: host.effective_config.node,
            host_settings: true,
            ..Self::new(Arc::new(crate::postgres::PostgresStoreSet::new(
                &host.storage,
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

    /// A host process engine; one per kind.
    #[must_use]
    pub fn process_engine(mut self, engine: Arc<dyn ProcessEngine>) -> Self {
        self.engines.push(engine);
        self
    }

    /// A projection provider; one per projection type.
    #[cfg(feature = "rlm")]
    #[must_use]
    pub fn projection_provider(mut self, provider: Arc<dyn lashlang::ProjectionProvider>) -> Self {
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
        Backend::assemble(BackendParts {
            #[cfg(feature = "rlm")]
            providers: projection_catalog(self.providers)?,
            #[cfg(not(feature = "rlm"))]
            providers: Arc::new(NoProjectionProviders),
            stores: self.stores,
            settings: self.settings,
            engines: self.engines,
            formats: crate::formats::actor_state_surfaces(),
        })
    }
}

/// The catalog of `providers`, refusing two of one type. Lash provides
/// `history` itself, so a host provider of it is the second of its type.
#[cfg(feature = "rlm")]
fn projection_catalog(
    providers: Vec<Arc<dyn lashlang::ProjectionProvider>>,
) -> Result<Arc<dyn lash_core::ProjectionProviders>, DurableBuildError> {
    if providers.is_empty() {
        return Ok(Arc::new(NoProjectionProviders));
    }
    let mut catalog = lashlang::ProjectionCatalog::new();
    for provider in providers {
        let projection = provider.projection_type();
        let duplicate = || DurableBuildError::DuplicateProvider {
            projection: projection.as_str().to_owned(),
        };
        if projection.as_str() == lash_protocol_rlm::HISTORY_PROJECTION {
            return Err(duplicate());
        }
        catalog.register(provider).map_err(|_| duplicate())?;
    }
    Ok(Arc::new(catalog))
}
