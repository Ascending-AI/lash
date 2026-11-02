//! A turn phase's checkpoint as checkpoint graph nodes (FIG-5887 §3): a
//! PhaseRoot (kind 33) over its run's configuration.
//!
//! The turn's machine is not stored: its journal rebuilds it (FIG-7585).
//! The root holds what a resume serves instead of deciding again, as
//! checkpoint text, with the window the turn started from as a separate
//! structured pin. The run's resolved shape rides beside it as
//! configuration graph nodes shared with retained revisions; its tool
//! surface is a TurnContent node. The first phase stages a nondefault run's
//! resolution. Default runs instead reference the configuration retained by
//! their acknowledged head, with their own resolved renderer (FIG-3843).

use lash_core_store::checkpoint_graph::EncodeBudget;
use lash_core_store::checkpoint_graph::ExplicitLimit;
use lash_core_store::checkpoint_graph::GraphDecoder;
use lash_core_store::checkpoint_graph::GraphRefusal;
use lash_core_store::checkpoint_graph::GraphStaging;
use lash_core_store::checkpoint_graph::NodeId;
use lash_core_store::checkpoint_graph::NodeKind;
use lash_core_store::checkpoint_graph::NodeSource;
use lash_core_store::checkpoint_graph::bodies::PhaseRoot as PhaseRootBody;
use lash_sansio::TurnWindowPin;

use super::run_config::{RecordedRunConfig, RunConfigStored};
use super::session::PhaseCheckpoint;

/// The phase's checkpoint text.
#[derive(serde::Serialize)]
struct PhaseText<'a> {
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    delivered: &'a [crate::AdmittedTurnInputs],
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    before_turn: &'a [crate::plugin::RecordedTurnContribution],
    #[serde(skip_serializing_if = "Option::is_none")]
    trace: &'a Option<lash_trace::DurableTraceScope>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cell: &'a Option<super::session::CellStart>,
}

/// The root's checkpoint text read back.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PhaseRead {
    #[serde(default)]
    delivered: Vec<crate::AdmittedTurnInputs>,
    #[serde(default)]
    before_turn: Vec<crate::plugin::RecordedTurnContribution>,
    #[serde(default)]
    trace: Option<lash_trace::DurableTraceScope>,
    #[serde(default)]
    cell: Option<super::session::CellStart>,
}

/// The host owns the opaque sans-io pin's spelling. At the graph boundary
/// it is a SessionHeadRef's fields, never JSON escaped inside a string.
fn structure_window_pin(pin: &TurnWindowPin) -> Result<serde_json::Value, String> {
    let head: crate::store::SessionHeadRef = serde_json::from_str(pin.as_str())
        .map_err(|e| format!("the window pin is not a head: {e}"))?;
    serde_json::to_value(head).map_err(|e| e.to_string())
}

/// The structured pin `value` as the machine spells it.
fn window_pin(value: serde_json::Value) -> Result<TurnWindowPin, String> {
    let head: crate::store::SessionHeadRef =
        serde_json::from_value(value).map_err(|e| format!("the window pin is not a head: {e}"))?;
    serde_json::to_string(&head)
        .map(TurnWindowPin::new)
        .map_err(|e| e.to_string())
}

/// A phase checkpoint staged as nodes, for the transaction that advances
/// the phase to install with it.
pub(in crate::runtime) struct StagedPhase {
    root: NodeId,
    environments: Vec<NodeId>,
    surface: Option<std::sync::Arc<crate::RecordedSurface>>,
    staging: GraphStaging,
    held: Vec<NodeId>,
    prepared: Vec<std::sync::Arc<lash_core_store::checkpoint_graph::node::VerifiedNode>>,
    supplied: Vec<lash_core_store::checkpoint_graph::EncodedNode>,
    stored: PhaseStored,
    publication: Option<std::sync::Arc<crate::runtime::session_environment::Publication>>,
    /// The journal rows the phase appends: the turn's build inputs with its
    /// first phase, then the machine inputs the phase observed, if any.
    journal: Vec<lash_durable::DomainWrite>,
}

/// What a staged phase stored: the nodes of its run's configuration it
/// added, which its drive is told once the phase's commit is acknowledged,
/// and the journal it made durable.
#[must_use = "the drive is told what its committed phase stored"]
pub(in crate::runtime) struct PhaseStored {
    config: RunConfigStored,
    publication: Option<std::sync::Arc<crate::runtime::session_environment::Publication>>,
    /// The machine's journal mark the phase's commit makes durable.
    inputs: Option<usize>,
    /// Whether the phase's commit journals the turn's build inputs.
    build: bool,
}

impl PhaseStored {
    /// Tell `drive`, and its machine, the phase is stored.
    pub(super) fn committed(self, drive: &mut dyn super::session::TurnDrive) {
        if let Some(publication) = self.publication {
            publication.committed();
        }
        if let Some(mark) = self.inputs {
            drive.machine().inputs_committed(mark);
        }
        if self.build {
            drive.build_journaled();
        }
        drive.run_config_stored(&self.config);
    }
}

impl StagedPhase {
    /// Stages `checkpoint`: its PhaseRoot over its run's configuration.
    ///
    /// # Errors
    ///
    /// The account of a checkpoint that does not encode.
    pub(in crate::runtime) fn of(checkpoint: &PhaseCheckpoint) -> Result<Self, String> {
        let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
        let refused = |error: GraphRefusal| format!("the turn checkpoint does not stage: {error}");
        let config = checkpoint
            .config
            .as_ref()
            .map(|config| config.stage(&mut staging))
            .transpose()?;
        let text = serde_json::to_string(&PhaseText {
            delivered: &checkpoint.delivered,
            before_turn: &checkpoint.before_turn,
            trace: &checkpoint.trace,
            cell: &checkpoint.cell,
        })
        .map_err(|error| format!("the turn checkpoint does not encode: {error}"))?;
        let body = PhaseRootBody {
            checkpoint: text,
            window_pin: checkpoint
                .window_pin
                .as_ref()
                .map(structure_window_pin)
                .transpose()?,
            run: config.as_ref().map(|config| config.run.clone()),
            environments: config
                .as_ref()
                .map(|config| config.environments.clone())
                .unwrap_or_default(),
        };
        let root = staging
            .encode_value(NodeKind::PHASE_ROOT, &body)
            .map_err(|error| refused(error.into()))?;
        let publication = config
            .as_ref()
            .and_then(|config| config.publication.clone());
        let mut config_stored = RunConfigStored::default();
        let mut held = Vec::new();
        let mut prepared = Vec::new();
        let mut supplied = Vec::new();
        if let Some(config) = config {
            held = config.held;
            prepared = config.prepared;
            supplied = config.supplied;
            config_stored = config.stored;
        }
        Ok(Self {
            publication: publication.clone(),
            root,
            environments: body
                .environments
                .into_iter()
                .map(|(_, node)| node)
                .collect(),
            surface: checkpoint
                .config
                .as_ref()
                .and_then(|config| config.surface().cloned()),
            staging,
            held,
            prepared,
            supplied,
            stored: PhaseStored {
                config: config_stored,
                publication,
                inputs: None,
                build: false,
            },
            journal: Vec::new(),
        })
    }

    /// Journal the turn's `build` inputs, unless an earlier phase journaled
    /// them, and `machine`'s inputs since its last committed journal row
    /// with the phase: the phase's commit appends them, and its
    /// acknowledgement makes them durable.
    ///
    /// # Errors
    ///
    /// The account of inputs that do not encode.
    pub(in crate::runtime) fn journaling(
        mut self,
        build: Option<String>,
        machine: &mut crate::TurnMachine,
        session: &lash_kernel_value::identity::SessionId,
        run: &lash_vocabulary::identity::TurnId,
    ) -> Result<Self, String> {
        let journal = |inputs| {
            lash_durable::DomainWrite::Turn(lash_durable::domain::TurnWrite::Journal {
                session: session.clone(),
                run: run.clone(),
                inputs,
            })
        };
        if let Some(build) = build {
            self.journal.push(journal(build));
            self.stored.build = true;
        }
        let (row, mark) = machine.stage_inputs();
        if !row.is_empty() {
            let inputs = serde_json::to_string(&row)
                .map_err(|error| format!("the turn's machine inputs do not encode: {error}"))?;
            self.journal.push(journal(inputs));
        }
        self.stored.inputs = Some(mark);
        Ok(self)
    }

    /// Stages the nodes in `tx`, requiring the held ones, and returns the
    /// PhaseRoot the phase names with what it stored.
    pub(in crate::runtime) fn staged_in(
        self,
        tx: &mut lash_durable::ActorTx,
    ) -> (NodeId, PhaseStored) {
        if let Some(publication) = self.publication {
            publication.stage(tx);
        }
        for journal in self.journal {
            tx.write(journal);
        }
        for node in self.held {
            tx.require(node);
        }
        for node in self.prepared {
            tx.require_exact(node);
        }
        tx.stage(self.supplied);
        tx.stage_all(self.staging);
        tx.phase(self.root, self.surface, self.environments);
        (self.root, self.stored)
    }
}

/// Restages PhaseRoot `root` from `source` with its checkpoint text made
/// `edit` of it, as another build would have written the phase.
///
/// # Errors
///
/// The account of a root that does not read or a body that does not stage.
#[cfg(any(test, feature = "testing"))]
pub(super) fn restaged_text(
    source: &dyn NodeSource,
    root: NodeId,
    edit: fn(&str) -> String,
    tx: &mut lash_durable::ActorTx,
) -> Result<NodeId, String> {
    let mut decoder = GraphDecoder::new(source, lash_durable::domain::graph::OWNER_READ_LIMITS);
    let mut body: PhaseRootBody = decoder
        .decode(root, NodeKind::PHASE_ROOT)
        .map_err(|error| error.to_string())?;
    body.checkpoint = edit(&body.checkpoint);
    let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
    let root = staging
        .encode_value(NodeKind::PHASE_ROOT, &body)
        .map_err(|error| error.to_string())?;
    for (_, node) in &body.environments {
        tx.require(*node);
    }
    if let Some(run) = &body.run {
        tx.require(run.root());
    }
    tx.stage_all(staging);
    Ok(root)
}

/// Reads PhaseRoot `root` from `source` back into the phase checkpoint it
/// was staged from.
///
/// # Errors
///
/// The account of a node that does not read or a checkpoint text that does
/// not decode.
pub(in crate::runtime) fn load(
    source: &dyn NodeSource,
    root: NodeId,
    limits: lash_core_store::checkpoint_graph::DecodeLimits,
) -> Result<PhaseCheckpoint, String> {
    let mut decoder = GraphDecoder::new(source, limits);
    let text = |error: GraphRefusal| error.to_string();
    let body: PhaseRootBody = decoder.decode(root, NodeKind::PHASE_ROOT).map_err(text)?;
    let read: PhaseRead = serde_json::from_str(&body.checkpoint)
        .map_err(|e| format!("the turn checkpoint does not decode: {e}"))?;
    let config = match body.run {
        None => None,
        Some(run) => {
            decoder
                .child_kind(root, run.root(), &[NodeKind::CONFIG_VALUE])
                .map_err(text)?;
            let resolved = match &run {
                lash_core_store::checkpoint_graph::bodies::RunConfigReference::Resolved(node) => {
                    lash_core_store::checkpoint_graph::config::load(&mut decoder, *node)?
                }
                lash_core_store::checkpoint_graph::bodies::RunConfigReference::Snapshot {
                    config,
                    render,
                } => {
                    let config =
                        lash_core_store::checkpoint_graph::config::load(&mut decoder, *config)?;
                    let mut resolved = crate::ResolvedRun::snapshot(config);
                    resolved.render = render.clone();
                    resolved
                }
            };
            let environments = body
                .environments
                .iter()
                .map(|(iteration, node)| {
                    decoder
                        .child_kind(root, *node, &[NodeKind::ROUND_ENVIRONMENT])
                        .map_err(text)?;
                    lash_durable::environment::decode_round(&mut decoder, *node)
                        .map(|round| (*iteration, round))
                        .map_err(|error| error.to_string())
                })
                .collect::<Result<_, _>>()?;
            Some(RecordedRunConfig::load(
                (run, resolved),
                environments,
                source,
            )?)
        }
    };
    Ok(PhaseCheckpoint {
        window_pin: body.window_pin.map(window_pin).transpose()?,
        delivered: read.delivered,
        before_turn: read.before_turn,
        trace: read.trace,
        config,
        cell: read.cell,
    })
}

/// The retained plugin decisions and surface in a phase, projected without
/// restoring a runtime or executing the checkpoint.
#[derive(Clone, Debug)]
pub struct RecordedPluginPhase {
    pub before_turn: Vec<crate::plugin::RecordedTurnContribution>,
    pub surface: Option<crate::RecordedSurface>,
}

/// Read only retained evidence. Absence is not evidence of a pruning history.
pub async fn read_plugin_phase(
    store: &dyn lash_durable::DurableStore,
    session: &lash_kernel_value::identity::SessionId,
    run: &lash_vocabulary::identity::TurnId,
) -> Result<Option<RecordedPluginPhase>, lash_durable::DurableError> {
    let Some(phase) = read_phase(store, session, run).await? else {
        return Ok(None);
    };
    let round = phase.config.as_ref().and_then(|c| c.current_environment());
    let surface = match round {
        None => None,
        Some(round) => {
            let body = crate::runtime::session_environment::load_body(
                store.environments().as_ref(),
                round,
            )
            .await?;
            Some(
                body.recorded_surface()
                    .map_err(lash_durable::environment::corrupt)?,
            )
        }
    };
    Ok(Some(RecordedPluginPhase {
        before_turn: phase.before_turn,
        surface,
    }))
}

async fn read_phase(
    reads: &dyn lash_durable::DurableReads,
    session: &lash_kernel_value::identity::SessionId,
    run: &lash_vocabulary::identity::TurnId,
) -> Result<Option<PhaseCheckpoint>, lash_durable::DurableError> {
    lash_core_ids::operational_metrics::sql::owner_state_read(
        lash_core_ids::operational_metrics::sql::OwnerStateRead::Phase,
    );
    let limits = lash_core_store::checkpoint_graph::DecodeLimits::within_total(16 * 1024 * 1024);
    let graph = match reads
        .hydrate(
            lash_durable::domain::RootOwner::TurnPhase {
                session: session.clone(),
                run: run.clone(),
            },
            None,
            limits,
        )
        .await
    {
        Ok(graph) => graph,
        Err(lash_durable::DurableError::Graph(GraphRefusal::RootRetired)) => return Ok(None),
        Err(error) => return Err(error),
    };
    let phase = load(&graph, graph.root, limits).map_err(|reason| {
        lash_durable::DurableError::Graph(GraphRefusal::InvalidBody {
            kind: NodeKind::PHASE_ROOT.0,
            reason,
        })
    })?;
    Ok(Some(phase))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CONFIG-REFERENCE-RESUME: the reference keeps its original renderer,
    /// including its parameters, and missing configuration remains refused.
    #[test]
    fn a_snapshot_reference_retains_its_renderer_and_requires_its_configuration() {
        use lash_core_store::checkpoint_graph::{bodies::RunConfigReference, config};
        let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
        let config = config::stage(
            &mut staging,
            &crate::store::SessionHeadPayload::default().config,
        )
        .unwrap();
        let reference = RunConfigReference::Snapshot {
            config,
            render: Some(crate::RecordedRender {
                renderer_id: "original-renderer".into(),
                params: std::sync::Arc::new(serde_json::json!({"audience": "recorded"})),
            }),
        };
        let root = staging
            .encode_value(
                NodeKind::PHASE_ROOT,
                &PhaseRootBody {
                    checkpoint: "{}".into(),
                    window_pin: None,
                    run: Some(reference.clone()),
                    environments: Vec::new(),
                },
            )
            .unwrap();
        let limits = lash_core_store::checkpoint_graph::DecodeLimits::within_total(1 << 20);
        let mut nodes = staging.into_nodes();
        let body: PhaseRootBody = GraphDecoder::new(&nodes, limits)
            .decode(root, NodeKind::PHASE_ROOT)
            .unwrap();
        assert_eq!(body.run, Some(reference));
        nodes.remove(&config);
        let mut decoder = GraphDecoder::new(&nodes, limits);
        assert_eq!(
            config::load::<crate::PersistedSessionConfig>(&mut decoder, config).unwrap_err(),
            GraphRefusal::MissingNode(config).to_string()
        );
    }

    /// PHASE-PIN-STRUCTURE: the stored pin is fields, and a graph read
    /// returns exactly the host's original opaque spelling to the machine.
    #[test]
    fn phase_window_pins_are_structured_fields_and_read_back_identically() {
        let head = crate::store::SessionHeadRef {
            generation: 0,
            revision: 17,
            leaf: None,
            checkpoint: None,
        };
        let pin = TurnWindowPin::new(serde_json::to_string(&head).unwrap());
        let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
        let root = staging
            .encode_value(
                NodeKind::PHASE_ROOT,
                &PhaseRootBody {
                    checkpoint: "{}".into(),
                    window_pin: Some(structure_window_pin(&pin).unwrap()),
                    run: None,
                    environments: Vec::new(),
                },
            )
            .unwrap();
        let limits = lash_core_store::checkpoint_graph::DecodeLimits::within_total(1 << 20);
        let stored: PhaseRootBody = GraphDecoder::new(&staging, limits)
            .decode(root, NodeKind::PHASE_ROOT)
            .unwrap();
        assert_eq!(
            stored.window_pin,
            Some(serde_json::to_value(&head).unwrap())
        );
        assert_eq!(load(&staging, root, limits).unwrap().window_pin, Some(pin));
    }
}
