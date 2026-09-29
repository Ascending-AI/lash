use lash_core_store::store::StoreTestSupport;

/// A [`DeploymentStore`](crate::DeploymentStore) together with its test-only
/// hooks: the deployment the conformance suites take.
///
/// Blanket-implemented for every `DeploymentStore + StoreTestSupport` type,
/// under the same gate as [`StoreTestSupport`]. An
/// `Arc<dyn ConformanceDeployment>` upcasts to `Arc<dyn DeploymentStore>`
/// and to `Arc<dyn RuntimeStore>` wherever production code is exercised.
pub trait ConformanceDeployment: crate::DeploymentStore + StoreTestSupport {}

impl<T> ConformanceDeployment for T where T: crate::DeploymentStore + StoreTestSupport + ?Sized {}
