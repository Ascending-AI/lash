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
    pub projection: EnvironmentProjection,
    /// Collected metadata only. Each override prepares its own durable owner.
    pub run_collection: Option<CollectedRunEnvironment>,
}

/// Durable hydration has content but no native catalog. Only an acknowledged
/// publication installs a ready projection, including its native source progress.
#[derive(Clone, Default)]
pub enum EnvironmentProjection {
    #[default]
    Empty,
    Hydrated {
        head: EnvironmentHead,
        body: Arc<EnvironmentBody>,
    },
    Ready(ReadyEnvironment),
}

impl EnvironmentProjection {
    pub fn head(&self) -> Option<&EnvironmentHead> {
        match self {
            Self::Empty => None,
            Self::Hydrated { head, .. } => Some(head),
            Self::Ready(ready) => Some(&ready.head),
        }
    }
}

/// One complete candidate, transferred intact into the resident on acknowledgement.
#[derive(Clone)]
pub struct ReadyEnvironment {
    pub head: EnvironmentHead,
    pub body: Arc<EnvironmentBody>,
    pub handle: crate::session::ToolCatalogHandle,
    /// Progress of these native producers, independent of the last content publication.
    pub frontier: InputFrontier,
}

/// A claim-local collection, scoped to its base environment and owner config.
/// No run lifetime owner is cached or transferred between runs.
#[derive(Clone)]
pub struct CollectedRunEnvironment {
    pub base: Option<EnvironmentHead>,
    pub config: crate::PluginConfig,
    pub body: Arc<EnvironmentBody>,
    pub handle: crate::session::ToolCatalogHandle,
    pub frontier: InputFrontier,
    pub observed: BTreeSet<uuid::Uuid>,
}
