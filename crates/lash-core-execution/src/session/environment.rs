//! Native environment ownership for one acknowledged session claim.
use crate::plugin::environment_sources::EnvironmentSources;
use lash_core_store::environment::*;
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

/// The claim drops providers and subscriptions with its acknowledged projection.
/// Temporary turn runtimes borrow this owner; durable recovery constructs another.
pub struct ClaimedEnvironment {
    pub plugins: Arc<crate::PluginSession>,
    pub sources: EnvironmentSources,
    #[cfg(any(test, feature = "testing"))]
    pub assemblies: std::sync::atomic::AtomicUsize,
    pub mailbox: Arc<Mutex<Vec<(uuid::Uuid, EnvironmentChange)>>>,
    pub state: Mutex<ResidentEnvironment>,
    pub registered: BTreeSet<SourceId>,
}

#[derive(Default)]
pub struct ResidentEnvironment {
    pub epoch: Option<lash_durable::Epoch>,
    pub reconcile: bool,
    pub head: Option<EnvironmentHead>,
    /// Progress of these native producers, independent of the last content publication.
    pub frontier: Option<InputFrontier>,
    pub body: Option<Arc<EnvironmentBody>>,
    pub handle: Option<crate::session::ToolCatalogHandle>,
}
