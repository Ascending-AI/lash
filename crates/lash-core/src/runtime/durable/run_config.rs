//! The configuration a run executes under, as its turn's checkpoint records
//! it (FIG-6021).
//!
//! A run resolves its spec once. Its configuration graph is shared with
//! revisions; each model round adds a typed immutable environment reference.
//! Phases retain every round the run used. A resume resolves those owners and
//! never substitutes the session-current environment. The surface here is only
//! a resident projection for existing diagnostics; it is never serialized.

use std::sync::Arc;

use lash_core_store::checkpoint_graph::{GraphStaging, NodeId, config};

use crate::ResolvedRun;

/// A run's recorded configuration: its resolved shape and the tool surface
/// it installed last, each with the node a committed phase stored it as.
#[derive(Clone, Debug)]
pub struct RecordedRunConfig {
    run: Arc<ResolvedRun>,
    run_stored: Option<NodeId>,
    surface: Option<Arc<crate::RecordedSurface>>,
    environments: std::collections::BTreeMap<u64, lash_core_store::environment::RoundEnvironment>,
    publication: Option<Arc<crate::runtime::session_environment::Publication>>,
}

impl RecordedRunConfig {
    /// The configuration of a run that resolved to `run`, with no tool
    /// surface installed yet and nothing stored.
    #[must_use]
    pub fn resolved(run: ResolvedRun) -> Self {
        Self {
            run: Arc::new(run),
            run_stored: None,
            surface: None,
            environments: Default::default(),
            publication: None,
        }
    }

    /// The run's resolved shape.
    #[must_use]
    pub fn run(&self) -> &ResolvedRun {
        &self.run
    }

    /// The run's resolved shape, shared with the execution contexts its
    /// tools read it through.
    #[must_use]
    pub fn shared_run(&self) -> Arc<ResolvedRun> {
        Arc::clone(&self.run)
    }

    /// The resident projection of the latest adopted round. It is not stored.
    #[must_use]
    pub fn surface(&self) -> Option<&Arc<crate::RecordedSurface>> {
        self.surface.as_ref()
    }

    pub(in crate::runtime) fn install_environment(
        &mut self,
        iteration: usize,
        adoption: &crate::runtime::session_environment::Adoption,
    ) {
        self.environments
            .insert(iteration as u64, adoption.round.clone());
        self.publication = adoption.publication.clone();
        let surface = adoption.handle.surface();
        if self.surface.as_deref() != Some(&*surface) {
            self.surface = Some(surface);
        }
    }

    pub(in crate::runtime) fn current_environment(
        &self,
    ) -> Option<&lash_core_store::environment::RoundEnvironment> {
        self.environments.last_key_value().map(|(_, round)| round)
    }

    /// Record that a committed phase stored this configuration as `stored`.
    pub(in crate::runtime) fn stored(&mut self, stored: &RunConfigStored) {
        if let Some(node) = stored.run {
            self.run_stored = Some(node);
        }
    }
}

/// The nodes a staged phase stored its run's configuration as, which the
/// drive is told once the phase's commit is acknowledged.
#[derive(Clone, Copy, Debug, Default)]
pub struct RunConfigStored {
    run: Option<NodeId>,
}

/// The configuration of a staged phase: the nodes its PhaseRoot names, the
/// staging of those it adds, and the nodes it requires stored.
pub(super) struct StagedRunConfig {
    pub(super) run: NodeId,
    pub(super) held: Vec<NodeId>,
    pub(super) stored: RunConfigStored,
    pub(super) environments: Vec<(u64, NodeId)>,
    pub(super) publication: Option<Arc<crate::runtime::session_environment::Publication>>,
}

impl RecordedRunConfig {
    /// Stage the shared run configuration and its typed model-round references.
    pub(super) fn stage(&self, staging: &mut GraphStaging) -> Result<StagedRunConfig, String> {
        if let Some(publication) = &self.publication {
            publication
                .encode_head(staging)
                .map_err(|error| error.to_string())?;
        }
        let mut held = Vec::new();
        let mut stored = RunConfigStored::default();
        let run = match self.run_stored {
            Some(node) => {
                held.push(node);
                node
            }
            None => {
                let node = config::stage(staging, &*self.run)?;
                stored.run = Some(node);
                node
            }
        };
        let environments = self
            .environments
            .iter()
            .map(|(iteration, round)| {
                lash_durable::environment::stage_round(staging, round)
                    .map(|node| (*iteration, node))
                    .map_err(|error| error.to_string())
            })
            .collect::<Result<_, _>>()?;
        Ok(StagedRunConfig {
            environments,
            publication: self.publication.clone(),
            run,
            held,
            stored,
        })
    }

    /// The configuration a PhaseRoot names, read back from its nodes'
    /// data, each with the node it is stored as.
    pub(super) fn load(
        run: (NodeId, ResolvedRun),
        environments: std::collections::BTreeMap<
            u64,
            lash_core_store::environment::RoundEnvironment,
        >,
    ) -> Self {
        let resolved = run.1;
        Self {
            environments,
            publication: None,
            run: Arc::new(resolved),
            run_stored: Some(run.0),
            surface: None,
        }
    }
}
