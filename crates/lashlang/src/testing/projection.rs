//! Test projections: a view a test answers reads from synchronously, behind
//! a real [`ProjectionProvider`] in a real [`ProjectionCatalog`], so a test
//! reads a projection value the way a run does (ADR 0132 §9).

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::{
    ProjectedBindings, ProjectedReadRequest, ProjectedReadResponse, ProjectedValue,
    ProjectionCatalog, ProjectionError, ProjectionProvider, ProjectionReadError, ProjectionReader,
    ProjectionType, ResourceRef,
};

/// The projection type every test view is read through.
pub const TEST_VIEW_PROJECTION: &str = "test-view";

/// What a test's projection answers; `None` for a request it does not answer.
pub trait TestView: Send + Sync {
    /// The declared type of the view's values.
    fn type_name(&self) -> &str;

    /// One read.
    fn read_one(&self, request: ProjectedReadRequest) -> Option<ProjectedReadResponse>;
}

/// The provider of every test view, by resource id.
#[derive(Default)]
pub struct TestViews {
    views: Mutex<HashMap<String, Arc<dyn TestView>>>,
    next: AtomicUsize,
}

impl TestViews {
    /// A resource projection named `name` that reads `view`.
    pub fn value(&self, name: &str, view: Arc<dyn TestView>) -> ProjectedValue {
        let id = format!("view-{}", self.next.fetch_add(1, Ordering::SeqCst));
        let type_name = view.type_name().to_owned();
        self.views
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.clone(), view);
        ProjectedValue::resource(
            name,
            type_name,
            ResourceRef {
                projection: ProjectionType::new(TEST_VIEW_PROJECTION),
                id,
                revision: None,
            },
        )
    }

    fn view(&self, resource: &ResourceRef) -> Result<Arc<dyn TestView>, ProjectionError> {
        self.views
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&resource.id)
            .cloned()
            .ok_or_else(|| ProjectionError {
                message: format!("no test view `{}`", resource.id),
            })
    }
}

#[async_trait::async_trait]
impl ProjectionProvider for TestViews {
    fn projection_type(&self) -> ProjectionType {
        ProjectionType::new(TEST_VIEW_PROJECTION)
    }

    async fn read(
        &self,
        resource: &ResourceRef,
        request: ProjectedReadRequest,
    ) -> Result<Option<ProjectedReadResponse>, ProjectionError> {
        Ok(self.view(resource)?.read_one(request))
    }

    async fn read_range(
        &self,
        resource: &ResourceRef,
        requests: Vec<ProjectedReadRequest>,
    ) -> Result<Vec<Option<ProjectedReadResponse>>, ProjectionError> {
        let view = self.view(resource)?;
        Ok(requests
            .into_iter()
            .map(|request| view.read_one(request))
            .collect())
    }
}

fn views() -> &'static Arc<TestViews> {
    static VIEWS: OnceLock<Arc<TestViews>> = OnceLock::new();
    VIEWS.get_or_init(Arc::default)
}

/// A resource projection named `name` that reads `view`.
pub fn test_view(name: &str, view: Arc<dyn TestView>) -> ProjectedValue {
    views().value(name, view)
}

/// A catalog holding the test-view provider, as a run's parent holds its
/// providers.
pub fn test_catalog() -> ProjectionCatalog {
    static CATALOG: OnceLock<ProjectionCatalog> = OnceLock::new();
    CATALOG
        .get_or_init(|| {
            let mut catalog = ProjectionCatalog::new();
            catalog
                .register(Arc::clone(views()) as Arc<dyn ProjectionProvider>)
                .unwrap_or_else(|refusal| panic!("one test-view provider: {refusal}"));
            catalog
        })
        .clone()
}

/// A catalog as the reader of an in-process test execution, blocking on each
/// provider read. A run reads through the worker's wire instead, and its
/// parent awaits the provider.
pub struct CatalogReader(pub ProjectionCatalog);

impl ProjectionReader for CatalogReader {
    fn read(
        &self,
        resource: &ResourceRef,
        request: ProjectedReadRequest,
    ) -> Result<Option<ProjectedReadResponse>, ProjectionReadError> {
        let answers = futures_executor::block_on(self.0.answer(resource, vec![request]))?;
        Ok(answers.into_iter().next().flatten())
    }

    fn read_range(
        &self,
        resource: &ResourceRef,
        requests: Vec<ProjectedReadRequest>,
    ) -> Result<Vec<Option<ProjectedReadResponse>>, ProjectionReadError> {
        futures_executor::block_on(self.0.answer(resource, requests))
    }
}

/// [`test_catalog`] as the reader of an in-process test execution.
pub fn test_reader() -> Arc<dyn ProjectionReader> {
    Arc::new(CatalogReader(test_catalog()))
}

/// `bindings` read through [`test_reader`].
pub fn reading_test_views(bindings: ProjectedBindings) -> ProjectedBindings {
    bindings.with_reader(test_reader())
}

/// Run `f` with its projection reads answered by [`test_reader`].
pub fn with_test_views<R>(f: impl FnOnce() -> R) -> R {
    crate::with_projection_reader(Some(test_reader()), f)
}
