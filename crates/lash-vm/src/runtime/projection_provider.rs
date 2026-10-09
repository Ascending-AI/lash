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
//!
//! The VM reads synchronously and inline, so it reads through a
//! [`ProjectionReader`]: the worker's reader is its IPC wire to the parent,
//! which awaits the provider through [`ProjectionCatalog::answer`]. The reader
//! is the execution's, never the value's: a projection value is plain data,
//! and the reader of the execution that holds it is installed for every poll
//! of that execution. An execution with no reader has no provider for any
//! type.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::value::{ProjectedReadRequest, ProjectedReadResponse, ProjectionType, ResourceRef};

/// A provider's failure to answer a read.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[error("projection read failed: {message}")]
pub struct ProjectionError {
    /// The provider's account of it.
    pub message: String,
}

/// The reads of one projection type. No method has a default body.
///
/// A read answers `Ok(None)` when the provider does not answer that request
/// at all, which is not the same as answering that there is no value
/// (FIG-2863): a consumer that needs an answer refuses with
/// `RuntimeError::ProjectedReadUnsupported`, and the string and iteration
/// helpers fall back to materializing.
#[async_trait::async_trait]
pub trait ProjectionProvider: Send + Sync {
    /// The type this provider answers.
    fn projection_type(&self) -> ProjectionType;

    /// One read of `resource`; `None` when this provider does not answer
    /// `request`.
    async fn read(
        &self,
        resource: &ResourceRef,
        request: ProjectedReadRequest,
    ) -> Result<Option<ProjectedReadResponse>, ProjectionError>;

    /// A batch of reads of `resource`, answered in order: one IPC frame.
    async fn read_range(
        &self,
        resource: &ResourceRef,
        requests: Vec<ProjectedReadRequest>,
    ) -> Result<Vec<Option<ProjectedReadResponse>>, ProjectionError>;
}

/// Why a provider could not be registered or found.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
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

/// Why a projection read has no answer: no provider answers the type, or the
/// provider failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
pub enum ProjectionReadError {
    /// The type has no provider.
    #[error(transparent)]
    Refused(#[from] ProjectionRefusal),
    /// The provider failed.
    #[error(transparent)]
    Failed(#[from] ProjectionError),
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
        provider: Arc<dyn ProjectionProvider>,
    ) -> Result<(), ProjectionRefusal> {
        match self.providers.entry(provider.projection_type()) {
            std::collections::btree_map::Entry::Occupied(entry) => {
                Err(ProjectionRefusal::Duplicate {
                    projection: entry.key().clone(),
                })
            }
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(provider);
                Ok(())
            }
        }
    }

    /// The provider for `projection`.
    ///
    /// # Errors
    ///
    /// [`ProjectionRefusal::NoProvider`].
    pub fn provider(
        &self,
        projection: &ProjectionType,
    ) -> Result<&Arc<dyn ProjectionProvider>, ProjectionRefusal> {
        self.providers
            .get(projection)
            .ok_or_else(|| ProjectionRefusal::NoProvider {
                projection: projection.clone(),
            })
    }

    /// The catalog a backend carries, read back typed; empty for a backend
    /// built without providers.
    pub fn of_backend(
        providers: Option<
            &dyn lash_core_execution::runtime::actor::projection::ProjectionProviders,
        >,
    ) -> Self {
        providers
            .and_then(|providers| providers.as_any().downcast_ref::<Self>())
            .cloned()
            .unwrap_or_default()
    }

    /// The registered types.
    pub fn projection_types(&self) -> impl Iterator<Item = &ProjectionType> {
        self.providers.keys()
    }

    /// Answer `requests` of `resource` through its type's provider: one
    /// request is one `read`, more are one `read_range`. This is what the
    /// parent runs for each projection frame a worker sends, on whichever node
    /// runs the actor.
    ///
    /// # Errors
    ///
    /// [`ProjectionReadError::Refused`] when no provider answers the type,
    /// [`ProjectionReadError::Failed`] when the provider fails.
    pub async fn answer(
        &self,
        resource: &ResourceRef,
        mut requests: Vec<ProjectedReadRequest>,
    ) -> Result<Vec<Option<ProjectedReadResponse>>, ProjectionReadError> {
        let provider = self.provider(&resource.projection)?;
        if requests.len() == 1
            && let Some(request) = requests.pop()
        {
            return Ok(vec![provider.read(resource, request).await?]);
        }
        Ok(provider.read_range(resource, requests).await?)
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

/// The VM's synchronous read of a projection resource.
pub trait ProjectionReader: Send + Sync {
    /// One read; `None` when the provider does not answer `request`.
    ///
    /// # Errors
    ///
    /// [`ProjectionReadError`].
    fn read(
        &self,
        resource: &ResourceRef,
        request: ProjectedReadRequest,
    ) -> Result<Option<ProjectedReadResponse>, ProjectionReadError>;

    /// A batch of reads, answered in order.
    ///
    /// # Errors
    ///
    /// [`ProjectionReadError`].
    fn read_range(
        &self,
        resource: &ResourceRef,
        requests: Vec<ProjectedReadRequest>,
    ) -> Result<Vec<Option<ProjectedReadResponse>>, ProjectionReadError>;
}

thread_local! {
    static READER: RefCell<Option<Arc<dyn ProjectionReader>>> = const { RefCell::new(None) };
}

/// Restores the reader that was current when it was entered.
struct ReaderScope {
    previous: Option<Arc<dyn ProjectionReader>>,
}

impl ReaderScope {
    fn enter(reader: Option<Arc<dyn ProjectionReader>>) -> Self {
        Self {
            previous: READER.with(|current| current.replace(reader)),
        }
    }
}

impl Drop for ReaderScope {
    fn drop(&mut self) {
        let previous = self.previous.take();
        READER.with(|current| *current.borrow_mut() = previous);
    }
}

/// Run `f` with `reader` answering every projection read it makes.
pub fn with_projection_reader<R>(
    reader: Option<Arc<dyn ProjectionReader>>,
    f: impl FnOnce() -> R,
) -> R {
    let _scope = ReaderScope::enter(reader);
    f()
}

/// Drive `future` with `reader` answering every projection read any of its
/// polls makes, whichever thread polls it.
pub(crate) async fn reading_through<F: Future>(
    reader: Option<Arc<dyn ProjectionReader>>,
    future: F,
) -> F::Output {
    let mut future = std::pin::pin!(future);
    std::future::poll_fn(|cx| {
        let _scope = ReaderScope::enter(reader.clone());
        future.as_mut().poll(cx)
    })
    .await
}

fn current_reader(
    resource: &ResourceRef,
) -> Result<Arc<dyn ProjectionReader>, ProjectionReadError> {
    READER
        .with(|current| current.borrow().clone())
        .ok_or_else(|| {
            ProjectionRefusal::NoProvider {
                projection: resource.projection.clone(),
            }
            .into()
        })
}

/// One read through the current execution's reader. With no reader, nothing
/// answers the type: [`ProjectionRefusal::NoProvider`].
pub(crate) fn read(
    resource: &ResourceRef,
    request: ProjectedReadRequest,
) -> Result<Option<ProjectedReadResponse>, ProjectionReadError> {
    current_reader(resource)?.read(resource, request)
}

/// A batch of reads through the current execution's reader.
pub(crate) fn read_range(
    resource: &ResourceRef,
    requests: Vec<ProjectedReadRequest>,
) -> Result<Vec<Option<ProjectedReadResponse>>, ProjectionReadError> {
    current_reader(resource)?.read_range(resource, requests)
}
