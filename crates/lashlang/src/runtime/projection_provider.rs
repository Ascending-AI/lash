//! Projection providers (ADR 0132 §9; S8 of I0, FIG-5194). Owned by L7p
//! (FIG-5197).
//!
//! A [`ProjectionProvider`] is registered by type, as tools are in the
//! catalog. Reads are pure and `Repeatable`, and are never recorded; a read
//! before a snapshot is already in the heap and a read after it reads again.
//! A missing provider for a type found in a value or a snapshot is a typed
//! [`ProjectionRefusal::NoProvider`], never a placeholder. `read_range`
//! costs one IPC frame per batch.
//!
//! The trait lives here, beside the read types it answers, rather than
//! beside the tool catalog in `lash-core-execution`: the VM sits above that
//! crate. The backend carries the built [`ProjectionCatalog`] behind
//! `lash_core_execution::runtime::actor::projection::ProjectionProviders`.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::value::{ProjectedReadRequest, ProjectedReadResponse, ProjectionType, ResourceRef};

/// A provider's failure to answer a read.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("projection read failed: {message}")]
pub struct ProjectionError {
    /// The provider's account of it.
    pub message: String,
}

/// The reads of one projection type. No method has a default body.
#[async_trait::async_trait]
pub trait ProjectionProvider: Send + Sync {
    /// The type this provider answers.
    fn projection_type(&self) -> ProjectionType;

    /// One read of `resource`.
    async fn read(
        &self,
        resource: &ResourceRef,
        request: ProjectedReadRequest,
    ) -> Result<ProjectedReadResponse, ProjectionError>;

    /// A batch of reads of `resource`, answered in order: one IPC frame.
    async fn read_range(
        &self,
        resource: &ResourceRef,
        requests: Vec<ProjectedReadRequest>,
    ) -> Result<Vec<ProjectedReadResponse>, ProjectionError>;
}

/// Why a provider could not be registered or found.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ProjectionRefusal {
    /// No provider answers a type a value or snapshot holds.
    #[error("no projection provider for `{}`", projection.as_str())]
    NoProvider {
        /// The type.
        projection: ProjectionType,
    },
    /// Two providers answer one type.
    #[error("two projection providers for `{}`", projection.as_str())]
    Duplicate {
        /// The type.
        projection: ProjectionType,
    },
}

/// The providers registered on a backend, by type.
#[derive(Clone, Default)]
pub struct ProjectionCatalog {
    providers: BTreeMap<ProjectionType, Arc<dyn ProjectionProvider>>,
}

impl std::fmt::Debug for ProjectionCatalog {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_set()
            .entries(self.providers.keys())
            .finish()
    }
}

impl ProjectionCatalog {
    /// An empty catalog.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `provider` under its type.
    ///
    /// # Errors
    ///
    /// [`ProjectionRefusal::Duplicate`] when its type has a provider.
    pub fn register(
        &mut self,
        _provider: Arc<dyn ProjectionProvider>,
    ) -> Result<(), ProjectionRefusal> {
        todo!("L7p (FIG-5197): register a provider by type, refusing a duplicate type")
    }

    /// The provider for `projection`.
    ///
    /// # Errors
    ///
    /// [`ProjectionRefusal::NoProvider`].
    pub fn provider(
        &self,
        _projection: &ProjectionType,
    ) -> Result<&Arc<dyn ProjectionProvider>, ProjectionRefusal> {
        todo!("L7p (FIG-5197): find the provider of a projection type, or refuse")
    }

    /// The registered types.
    pub fn projection_types(&self) -> impl Iterator<Item = &ProjectionType> {
        self.providers.keys()
    }
}

impl lash_core_execution::runtime::actor::projection::ProjectionProviders for ProjectionCatalog {
    fn projection_types(&self) -> Vec<String> {
        self.providers
            .keys()
            .map(|projection| projection.as_str().to_owned())
            .collect()
    }

    fn as_any(&self) -> &(dyn std::any::Any + Send + Sync) {
        self
    }
}
