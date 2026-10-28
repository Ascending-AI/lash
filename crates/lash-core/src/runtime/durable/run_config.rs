//! The configuration a run executes under, as its turn's checkpoint records
//! it (FIG-6021).
//!
//! A run resolves its spec once. Its configuration graph is shared with
//! revisions. A default run references its acknowledged head's configuration
//! without staging it again; each model round adds a typed immutable environment reference.
//! Phases retain every round the run used. A resume resolves those owners and
//! never substitutes the session-current environment. The surface here is only
//! a resident projection for existing diagnostics; it is never serialized.

use std::sync::Arc;

use lash_core_store::checkpoint_graph::bodies::RunConfigReference;
use lash_core_store::checkpoint_graph::{GraphStaging, NodeId, config};
use lash_core_store::phase_preparation::PhaseContentPreparations;

use crate::ResolvedRun;
use lash_core_store::checkpoint_graph::node::VerifiedNode;

/// A run's recorded configuration: its resolved shape and the tool surface
/// it installed last, each with the node a committed phase stored it as.
#[derive(Clone, Debug)]
pub struct RecordedRunConfig {
    run: Arc<ResolvedRun>,
    run_stored: Option<NodeId>,
    run_prepared: Option<(
        NodeId,
        Arc<[lash_core_store::checkpoint_graph::EncodedNode]>,
    )>,
    configuration: Option<lash_core_execution::runtime::actor::owner_state::SessionConfiguration>,
    session_config: Option<Arc<VerifiedNode>>,
    surface: Option<Arc<crate::RecordedSurface>>,
    environments: std::collections::BTreeMap<u64, lash_core_store::environment::RoundEnvironment>,
    publication: Option<Arc<crate::runtime::session_environment::Publication>>,
    content_prepared: PhaseContentPreparations,
}

impl RecordedRunConfig {
    /// The configuration of a run that resolved to `run`, with no tool
    /// surface installed yet and nothing stored.
    #[must_use]
    pub fn resolved(run: ResolvedRun) -> Self {
        Self {
            run: Arc::new(run),
            run_stored: None,
            run_prepared: None,
            configuration: None,
            session_config: None,
            surface: None,
            environments: Default::default(),
            publication: None,
            content_prepared: PhaseContentPreparations::default(),
        }
    }

    /// Reuse exactly the configuration the acknowledged head retains.
    pub(in crate::runtime) fn with_head_config(
        mut self,
        config: &crate::PersistedSessionConfig,
        node: Arc<VerifiedNode>,
    ) -> Self {
        if self.run.spec().is_default() && !self.run.has_override() && self.run.base() == config {
            self.session_config = Some(node);
        }
        self
    }

    pub(in crate::runtime) fn with_claim_configuration(
        mut self,
        configuration: lash_core_execution::runtime::actor::owner_state::SessionConfiguration,
    ) -> Self {
        self.run_prepared = configuration.retained_run(&self.run);
        self.configuration = Some(configuration);
        self
    }

    pub(in crate::runtime) fn with_content_preparations(
        mut self,
        head: Option<&PhaseContentPreparations>,
    ) -> Self {
        if let Some(head) = head {
            self.content_prepared = self.content_prepared.with_head(head);
        }
        self
    }

    pub(in crate::runtime) fn content_preparations(&self) -> &PhaseContentPreparations {
        &self.content_prepared
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
        if let Some(prepared) = &stored.content_prepared {
            self.content_prepared.clone_from(prepared);
        }
        if let Some(node) = stored.run {
            self.run_stored = Some(node);
        }
        if let Some(node) = &stored.prepared {
            self.run_prepared = Some(node.clone());
            if let Some(configuration) = &self.configuration {
                configuration.remember_run(self.run.clone(), node.0, node.1.clone());
            }
        }
    }
}

/// The nodes a staged phase stored its run's configuration as, which the
/// drive is told once the phase's commit is acknowledged.
#[derive(Clone, Debug, Default)]
pub struct RunConfigStored {
    pub(super) content_prepared: Option<PhaseContentPreparations>,
    run: Option<NodeId>,
    prepared: Option<(
        NodeId,
        Arc<[lash_core_store::checkpoint_graph::EncodedNode]>,
    )>,
}

/// The configuration of a staged phase: the nodes its PhaseRoot names, the
/// staging of those it adds, and the nodes it requires stored.
pub(super) struct StagedRunConfig {
    pub(super) run: RunConfigReference,
    pub(super) held: Vec<NodeId>,
    pub(super) prepared: Vec<Arc<VerifiedNode>>,
    pub(super) supplied: Vec<lash_core_store::checkpoint_graph::EncodedNode>,
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
        let mut prepared = Vec::new();
        let mut stored = RunConfigStored::default();
        let mut supplied = Vec::new();
        let run = match (self.run_stored, self.session_config.as_ref()) {
            (Some(node), _) => {
                held.push(node);
                RunConfigReference::Resolved(node)
            }
            (None, Some(config)) => {
                prepared.push(config.clone());
                held.push(config.id);
                RunConfigReference::Snapshot {
                    config: config.id,
                    render: self.run.render.clone(),
                }
            }
            (None, None) if self.run_prepared.is_some() => {
                let (root, nodes) = self
                    .run_prepared
                    .as_ref()
                    .ok_or_else(|| "prepared run is absent".to_owned())?;
                let mut graph: std::collections::BTreeMap<_, _> =
                    nodes.iter().map(|node| (node.id, node.clone())).collect();
                let spec = self.run.spec();
                let root = if spec.is_default() {
                    config::replace_field(staging, &graph, *root, &["spec"], None)?
                } else {
                    let capabilities = (!spec.capabilities.is_empty())
                        .then(|| serde_json::to_value(&spec.capabilities))
                        .transpose()
                        .map_err(|error| error.to_string())?;
                    config::replace_field(
                        staging,
                        &graph,
                        *root,
                        &["spec", "capabilities"],
                        capabilities,
                    )?
                };
                graph.extend(staging.nodes().iter().map(|(id, node)| (*id, node.clone())));
                let nodes: Arc<[_]> = lash_core_store::checkpoint_graph::closure(
                    &graph,
                    root,
                    &lash_core_store::checkpoint_graph::DecodeLimits::within_total(u64::MAX),
                )
                .map_err(|error| error.to_string())?
                .into_values()
                .collect::<Vec<_>>()
                .into();
                supplied.extend(nodes.iter().cloned());
                stored.run = Some(root);
                stored.prepared = Some((root, nodes));
                RunConfigReference::Resolved(root)
            }
            (None, None) => {
                let node = config::stage(staging, &*self.run)?;
                stored.run = Some(node);
                let nodes = lash_core_store::checkpoint_graph::closure(
                    staging,
                    node,
                    &lash_core_store::checkpoint_graph::DecodeLimits::within_total(u64::MAX),
                )
                .map_err(|error| error.to_string())?;
                stored.prepared = Some((node, nodes.into_values().collect::<Vec<_>>().into()));
                RunConfigReference::Resolved(node)
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
            prepared,
            supplied,
            stored,
        })
    }

    /// The configuration a PhaseRoot names, read back from its nodes'
    /// data, each with the node it is stored as.
    pub(super) fn load(
        run: (RunConfigReference, ResolvedRun),
        environments: std::collections::BTreeMap<
            u64,
            lash_core_store::environment::RoundEnvironment,
        >,
        source: &dyn lash_core_store::checkpoint_graph::NodeSource,
    ) -> Result<Self, String> {
        let session_config = match &run.0 {
            RunConfigReference::Snapshot { config, .. } => {
                use lash_core_store::checkpoint_graph::{DecodeLimits, EncodedNode, GraphRefusal};
                let stored = source
                    .fetch(config)
                    .ok_or_else(|| GraphRefusal::MissingNode(*config).to_string())?;
                Some(Arc::new(
                    VerifiedNode::check(
                        EncodedNode {
                            id: *config,
                            bytes: Arc::from(stored.bytes),
                        },
                        &DecodeLimits::within_total(u64::MAX),
                    )
                    .map_err(|error| error.to_string())?,
                ))
            }
            RunConfigReference::Resolved(_) => None,
        };
        Ok(Self {
            environments,
            publication: None,
            content_prepared: PhaseContentPreparations::default(),
            run: Arc::new(run.1),
            run_stored: match run.0 {
                RunConfigReference::Resolved(root) => Some(root),
                RunConfigReference::Snapshot { .. } => None,
            },
            session_config,
            run_prepared: None,
            configuration: None,
            surface: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_core_store::checkpoint_graph::{EncodeBudget, ExplicitLimit};

    /// CONFIG-STAGED-ONCE: each fresh default turn names the configuration
    /// its acknowledged head retains, without staging any configuration node.
    #[test]
    fn default_turns_reference_the_acknowledged_configuration_without_staging_nodes() {
        let config = lash_core_store::store::SessionHeadPayload::default().config;
        let mut head = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
        let node = config::stage(&mut head, &config).unwrap();
        for _ in 0..2 {
            let recorded = RecordedRunConfig::resolved(ResolvedRun::snapshot(config.clone()))
                .with_head_config(
                    &config,
                    Arc::new(
                        VerifiedNode::check(
                            head.get(&node).unwrap().clone(),
                            &lash_core_store::checkpoint_graph::DecodeLimits::within_total(
                                u64::MAX,
                            ),
                        )
                        .unwrap(),
                    ),
                );
            let mut delta = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
            let staged = recorded.stage(&mut delta).unwrap();
            assert!(
                delta.nodes().is_empty(),
                "a fresh default turn restaged {} config nodes",
                delta.nodes().len()
            );
            assert_eq!(staged.held, vec![node]);
        }
    }

    /// CONFIG-REFERENCE-IDENTITY: a head lends only its exact configuration,
    /// never a same-revision edit or a run's override.
    #[test]
    fn a_head_reference_cannot_replace_changed_or_overridden_configuration() {
        let config = lash_core_store::store::SessionHeadPayload::default().config;
        let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
        let node = config::stage(&mut staging, &config).unwrap();
        let mut changed = config.clone();
        changed.max_tool_calls = crate::MaxToolCalls::new(73);
        assert_eq!(config.config_revision, changed.config_revision);
        for run in [
            ResolvedRun::snapshot(changed.clone()),
            ResolvedRun::snapshot(config.clone()).with_config(changed),
        ] {
            let recorded = RecordedRunConfig::resolved(run).with_head_config(
                &config,
                Arc::new(
                    VerifiedNode::check(
                        staging.get(&node).unwrap().clone(),
                        &lash_core_store::checkpoint_graph::DecodeLimits::within_total(u64::MAX),
                    )
                    .unwrap(),
                ),
            );
            assert!(recorded.session_config.is_none());
            let mut delta = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
            let staged = recorded.stage(&mut delta).unwrap();
            assert!(matches!(staged.run, RunConfigReference::Resolved(_)));
            assert!(!delta.nodes().is_empty());
        }

        // FIG-7453: a new turn's authority changes the spec, while the
        // acknowledged configuration's encoded children stay reusable.
        let config = lash_core_store::store::SessionHeadPayload::default().config;
        let cache =
            lash_core_execution::runtime::actor::owner_state::SessionConfiguration::default();
        let mut spec = lash_core_store::run_spec::RunSpec::default();
        spec.capabilities.insert(
            lash_core_store::run_spec::SlotId::new("authority"),
            lash_core_store::run_spec::CapabilityRef {
                contract: lash_core_store::run_spec::ContractRef::new("host", 1),
                binding: lash_core_store::run_spec::BindingId::new("host"),
                args: serde_json::json!({"turn_id":"first"}),
            },
        );
        for ordinal in 0..2 {
            let run = ResolvedRun::snapshot(config.clone())
                .with_spec(spec.clone())
                .unwrap();
            let mut reference = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
            let expected = config::stage(&mut reference, &run).unwrap();
            let base = config::stage(&mut reference, run.base()).unwrap();
            let mut recorded =
                RecordedRunConfig::resolved(run).with_claim_configuration(cache.clone());
            let mut delta = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
            let staged = recorded.stage(&mut delta).unwrap();
            assert_eq!(
                staged.run,
                RunConfigReference::Resolved(expected),
                "run authority stays exact"
            );
            if ordinal != 0 {
                assert!(
                    !delta.nodes().contains_key(&base),
                    "FIG-7453: changing only turn authority re-encoded the unchanged base configuration"
                );
            }
            recorded.stored(&staged.stored);
            spec.capabilities
                .get_mut(&lash_core_store::run_spec::SlotId::new("authority"))
                .unwrap()
                .args = serde_json::json!({"turn_id":"second"});
        }
    }
}
