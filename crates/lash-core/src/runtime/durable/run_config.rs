//! The configuration a run executes under, as its turn's checkpoint records
//! it (FIG-6021).
//!
//! A run resolves its spec once, before its first durable effect, and its
//! first phase commit records the result: the run's [`ResolvedRun`], spec
//! included, and the tool surface its latest environment sync installed.
//! The run is a field-addressed configuration graph shared with revisions;
//! the surface is one TurnContent node. The PhaseRoot names both. A phase stages a node
//! only when it holds something no earlier phase of the turn stored; every
//! other phase requires the stored node, so a large spec or surface is
//! written once per turn, and counts against the commit that writes it.
//!
//! A resume and a cancel restore read the record back and never resolve
//! again: no definition, model snapshot, owner option or render producer
//! runs for a committed run, and its prompt offers the surface it recorded
//! however the live tool sources changed since.

use std::sync::Arc;

use super::phase_root::ContentText;
use lash_core_store::checkpoint_graph::{GraphStaging, NodeId, NodeKind, config};

use crate::ResolvedRun;

/// A run's recorded configuration: its resolved shape and the tool surface
/// it installed last, each with the node a committed phase stored it as.
#[derive(Clone, Debug)]
pub struct RecordedRunConfig {
    run: Arc<ResolvedRun>,
    run_stored: Option<NodeId>,
    surface: Option<Arc<crate::RecordedSurface>>,
    surface_stored: Option<NodeId>,
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
            surface_stored: None,
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

    /// The tool surface the run's latest environment sync installed, with
    /// its deferred provider, as a resume reinstalls it: `None` before the
    /// run's first sync.
    #[must_use]
    pub fn surface(&self) -> Option<&Arc<crate::RecordedSurface>> {
        self.surface.as_ref()
    }

    /// Record `surface` as the one the run's latest sync installed. An
    /// unchanged surface keeps the node an earlier phase stored it as.
    pub(in crate::runtime) fn install_surface(&mut self, surface: Arc<crate::RecordedSurface>) {
        if self.surface.as_deref() == Some(&*surface) {
            return;
        }
        self.surface = Some(surface);
        self.surface_stored = None;
    }

    /// Record that a committed phase stored this configuration as `stored`.
    pub(in crate::runtime) fn stored(&mut self, stored: &RunConfigStored) {
        if let Some(node) = stored.run {
            self.run_stored = Some(node);
        }
        if let Some(node) = stored.surface {
            self.surface_stored = Some(node);
        }
    }
}

/// The nodes a staged phase stored its run's configuration as, which the
/// drive is told once the phase's commit is acknowledged.
#[derive(Clone, Copy, Debug, Default)]
pub struct RunConfigStored {
    run: Option<NodeId>,
    surface: Option<NodeId>,
}

/// The configuration of a staged phase: the nodes its PhaseRoot names, the
/// staging of those it adds, and the nodes it requires stored.
pub(super) struct StagedRunConfig {
    pub(super) run: NodeId,
    pub(super) surface: Option<NodeId>,
    pub(super) held: Vec<NodeId>,
    pub(super) stored: RunConfigStored,
}

/// One content node's text: the canonical JSON its staging encodes.
fn text<T: serde::Serialize + ?Sized>(value: &T, what: &str) -> Result<String, String> {
    serde_json::to_string(value)
        .map_err(|error| format!("the run's {what} does not encode: {error}"))
}

impl RecordedRunConfig {
    /// Stage this run as shared configuration objects and its surface as a
    /// TurnContent node, naming the nodes an earlier phase stored.
    pub(super) fn stage(&self, staging: &mut GraphStaging) -> Result<StagedRunConfig, String> {
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
        let surface = match (&self.surface, self.surface_stored) {
            (None, _) => None,
            (Some(_), Some(node)) => {
                held.push(node);
                Some(node)
            }
            (Some(surface), None) => {
                let node = staging
                    .encode_value(
                        NodeKind::TURN_CONTENT,
                        &ContentText(text(&**surface, "tool surface")?),
                    )
                    .map_err(|error| error.to_string())?;
                stored.surface = Some(node);
                Some(node)
            }
        };
        Ok(StagedRunConfig {
            run,
            surface,
            held,
            stored,
        })
    }

    /// The configuration a PhaseRoot names, read back from its nodes'
    /// data, each with the node it is stored as.
    pub(super) fn load(
        run: (NodeId, ResolvedRun),
        surface: Option<(NodeId, &str)>,
    ) -> Result<Self, String> {
        let resolved = run.1;
        let surface = surface
            .map(|(node, text)| {
                serde_json::from_str::<crate::RecordedSurface>(text)
                    .map(|surface| (node, Arc::new(surface)))
                    .map_err(|error| format!("the run's tool surface does not decode: {error}"))
            })
            .transpose()?;
        Ok(Self {
            run: Arc::new(resolved),
            run_stored: Some(run.0),
            surface_stored: surface.as_ref().map(|(node, _)| *node),
            surface: surface.map(|(_, surface)| surface),
        })
    }
}
