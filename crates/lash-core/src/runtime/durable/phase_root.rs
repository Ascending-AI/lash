//! A turn phase's checkpoint as checkpoint graph nodes (FIG-5887 §3): a
//! PhaseRoot (kind 33) over the TurnContent (kind 39) and
//! ModelRequestFrame (kind 46) its machine checkpoint names.
//!
//! The machine's checkpoint is bounded state; the transcript it would
//! otherwise copy is content the checkpoint names by digest, in chains whose
//! closed chunks keep their digests as the turn appends. Each content blob
//! is one TurnContent node, and the PhaseRoot names every blob its
//! checkpoint needs. A phase stages only the blobs its machine's checkpoint
//! added (FIG-5955): the others an earlier phase of the turn stored, and the
//! root requires them by the node the machine was told each is stored as.
//! The root holds checkpoint text with its window pin as a separate structured field. The
//! run's resolved shape rides beside it as configuration graph nodes shared
//! with retained revisions; its tool surface is a TurnContent node. The
//! first phase that holds them stages them (FIG-6021, FIG-6599).

use lash_core_store::checkpoint_graph::bodies::PhaseRoot as PhaseRootBody;
use lash_core_store::checkpoint_graph::{
    EncodeBudget, ExplicitLimit, GraphDecoder, GraphRefusal, GraphStaging, NodeId, NodeKind,
    NodeSource, TokenCodec, TokenError, TokenReader, TokenWriter, TypedBody,
};
use lash_sansio::{
    CheckpointContentPin, CheckpointContentRef, SavedTurn, TurnCheckpoint, TurnCheckpointContent,
};

use super::run_config::{RecordedRunConfig, RunConfigStored};
use super::session::PhaseCheckpoint;
use crate::HostTurnProtocol;

/// The phase's checkpoint without its content.
#[derive(serde::Serialize)]
struct PhaseText<'a> {
    checkpoint: &'a TurnCheckpoint<HostTurnProtocol>,
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    delivered: &'a [crate::AdmittedTurnInputs],
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    before_turn: &'a [crate::plugin::RecordedTurnContribution],
    #[serde(skip_serializing_if = "Option::is_none")]
    trace: &'a Option<lash_trace::DurableTraceScope>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cell: &'a Option<super::session::CellStart>,
}

/// The root's checkpoint data read back.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PhaseRead {
    checkpoint: TurnCheckpoint<HostTurnProtocol>,
    #[serde(default)]
    delivered: Vec<crate::AdmittedTurnInputs>,
    #[serde(default)]
    before_turn: Vec<crate::plugin::RecordedTurnContribution>,
    #[serde(default)]
    trace: Option<lash_trace::DurableTraceScope>,
    #[serde(default)]
    cell: Option<super::session::CellStart>,
}

/// Kind 39's body: one content blob's canonical JSON text.
pub(super) struct ContentText(pub(super) String);

impl TokenCodec for ContentText {
    fn encode(&self, out: &mut TokenWriter) {
        out.text(&self.0);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(Self(input.text()?.to_owned()))
    }
}

/// Encode a canonical text slice without making an owned decode value.
fn stage_content(staging: &mut GraphStaging, text: &str) -> Result<NodeId, GraphRefusal> {
    let mut out = TokenWriter::new();
    out.text(text);
    let body = out.into_bytes();
    #[cfg(feature = "perf-witness")]
    crate::perf_witness::record_graph_serialized_body(body.len());
    staging
        .encode(NodeKind::TURN_CONTENT, TypedBody::logical(&body))
        .map_err(Into::into)
}

/// Kind 46: a request's changing scope over its immutable configuration.
/// The config is a TurnContent whose text is shared across sends; neither
/// the scope nor the checkpoint's content digest is part of that child.
/// The wrapper keeps its explicit reference intact when the graph decoder
/// expands references to placed text leaves.
struct ModelRequestFrame {
    config: NodeId,
    scope: serde_json::Value,
}

impl TokenCodec for ModelRequestFrame {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        self.config.encode(out);
        self.scope.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            config: NodeId::decode(input)?,
            scope: serde_json::Value::decode(input)?,
        })
    }
}

fn stage_request(staging: &mut GraphStaging, text: &str) -> Result<NodeId, String> {
    let mut config: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let scope = config
        .as_object_mut()
        .and_then(|config| config.remove("scope"))
        .ok_or_else(|| "the checkpoint's model request has no scope".to_owned())?;
    let config = stage_content(staging, &config.to_string()).map_err(|e| e.to_string())?;
    staging
        .encode_value(
            NodeKind::MODEL_REQUEST_FRAME,
            &ModelRequestFrame { config, scope },
        )
        .map_err(|e| e.to_string())
}

fn load_content(
    decoder: &mut GraphDecoder<'_>,
    parent: NodeId,
    node: NodeId,
) -> Result<String, String> {
    let kind = decoder
        .child_kind(
            parent,
            node,
            &[NodeKind::TURN_CONTENT, NodeKind::MODEL_REQUEST_FRAME],
        )
        .map_err(|e| e.to_string())?;
    if kind == NodeKind::TURN_CONTENT {
        return decoder
            .decode::<ContentText>(node, kind)
            .map(|text| text.0)
            .map_err(|e| e.to_string());
    }
    let frame: ModelRequestFrame = decoder.decode(node, kind).map_err(|e| e.to_string())?;
    decoder
        .child_kind(node, frame.config, &[NodeKind::TURN_CONTENT])
        .map_err(|e| e.to_string())?;
    let ContentText(text) = decoder
        .decode(frame.config, NodeKind::TURN_CONTENT)
        .map_err(|e| e.to_string())?;
    let mut config: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    if serde_json::to_string(&config).map_err(|e| e.to_string())? != text {
        return Err("the request config is not canonical JSON".to_owned());
    }
    let fields = config
        .as_object_mut()
        .ok_or_else(|| "the request config is not an object".to_owned())?;
    if fields.insert("scope".to_owned(), frame.scope).is_some() {
        return Err("the request config holds its changing scope".to_owned());
    }
    // Re-serialize through the owning type, preserving the machine's exact
    // canonical field order and bytes, which insert_stored checks by digest.
    let request: crate::llm::types::LlmRequest =
        serde_json::from_value(config.clone()).map_err(|e| e.to_string())?;
    if serde_json::to_value(&request).map_err(|e| e.to_string())? != config {
        return Err("the model request is not canonical".to_owned());
    }
    serde_json::to_string(&request).map_err(|e| e.to_string())
}

/// The host owns the opaque sans-io pin's spelling. At the graph boundary
/// it is a SessionHeadRef, never JSON escaped inside a JSON string.
fn structure_window_pin(
    value: &mut serde_json::Value,
) -> Result<Option<serde_json::Value>, String> {
    let Some(window) = value
        .pointer_mut("/checkpoint/window")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return Ok(None);
    };
    let pin = window
        .remove("pin")
        .ok_or_else(|| "the checkpoint window has no pin".to_owned())?;
    let head: crate::store::SessionHeadRef = serde_json::from_str(
        pin.as_str()
            .ok_or_else(|| "the window pin is not text".to_owned())?,
    )
    .map_err(|e| e.to_string())?;
    serde_json::to_value(head)
        .map(Some)
        .map_err(|e| e.to_string())
}

fn checkpoint_value(
    mut value: serde_json::Value,
    pin: Option<serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let window = value
        .pointer_mut("/checkpoint/window")
        .and_then(serde_json::Value::as_object_mut);
    match (window, pin) {
        (Some(window), Some(pin)) => {
            if window.contains_key("pin") {
                return Err("the checkpoint text embeds its window pin".to_owned());
            }
            let head: crate::store::SessionHeadRef =
                serde_json::from_value(pin).map_err(|e| e.to_string())?;
            window.insert(
                "pin".to_owned(),
                serde_json::Value::String(serde_json::to_string(&head).map_err(|e| e.to_string())?),
            );
        }
        (None, None) => {}
        _ => return Err("the checkpoint window and its structured pin disagree".to_owned()),
    }
    Ok(value)
}

fn read_checkpoint(text: &str, pin: Option<serde_json::Value>) -> Result<PhaseRead, String> {
    let value = serde_json::from_str(text)
        .map_err(|e| format!("the turn checkpoint does not decode: {e}"))?;
    serde_json::from_value(checkpoint_value(value, pin)?)
        .map_err(|e| format!("the turn checkpoint does not decode: {e}"))
}

/// A phase checkpoint staged as nodes, for the transaction that advances
/// the phase to install with it.
pub(super) struct StagedPhase {
    root: NodeId,
    surface: Option<std::sync::Arc<crate::RecordedSurface>>,
    staging: GraphStaging,
    held: Vec<NodeId>,
    stored: PhaseStored,
}

/// The blobs a staged phase added, each by the node it is stored as: what
/// its machine is told once the phase's commit is acknowledged
/// ([`lash_sansio::TurnMachine::checkpoint_stored`]), and the nodes of its
/// run's configuration it added, which its drive is told.
#[must_use = "the machine is told what its committed phase stored"]
pub(super) struct PhaseStored {
    content: Vec<(CheckpointContentRef, CheckpointContentPin)>,
    config: RunConfigStored,
}

impl PhaseStored {
    /// Tell `drive`, and its machine, the phase is stored.
    pub(super) fn committed(
        self,
        drive: &mut dyn super::session::TurnDrive,
    ) -> Result<(), crate::RuntimeError> {
        drive
            .machine()
            .checkpoint_stored(self.content)
            .map_err(crate::RuntimeError::allocation)?;
        drive.run_config_stored(&self.config);
        Ok(())
    }

    /// Tell `machine`, restored from this phase, what the phase stored: its
    /// configuration's nodes came back with the checkpoint.
    pub(super) fn restored(
        self,
        machine: &mut crate::TurnMachine,
    ) -> Result<(), lash_protocol_types::allocation::AllocationError> {
        machine.checkpoint_stored(self.content)
    }
}

impl StagedPhase {
    /// Stages `checkpoint`: a ModelRequestFrame for its request and a
    /// TurnContent per other content blob, with the PhaseRoot over them
    /// and over `held`, the blobs an earlier
    /// phase stored, by their nodes.
    ///
    /// # Errors
    ///
    /// The account of a checkpoint that does not encode.
    pub(super) fn of(
        checkpoint: &PhaseCheckpoint,
        held: &[(CheckpointContentRef, CheckpointContentPin)],
    ) -> Result<Self, String> {
        let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
        let refused = |error: GraphRefusal| format!("the turn checkpoint does not stage: {error}");
        let config = checkpoint
            .config
            .as_ref()
            .map(|config| config.stage(&mut staging))
            .transpose()?;
        let mut value = serde_json::to_value(&PhaseText {
            checkpoint: &checkpoint.saved.checkpoint,
            delivered: &checkpoint.delivered,
            before_turn: &checkpoint.before_turn,
            trace: &checkpoint.trace,
            cell: &checkpoint.cell,
        })
        .map_err(|error| format!("the turn checkpoint does not encode: {error}"))?;
        let window_pin = structure_window_pin(&mut value)?;
        let request = checkpoint
            .saved
            .checkpoint
            .pending_request()
            .map(|(request, _, _)| request);
        let mut content = Vec::new();
        let mut stored = Vec::new();
        for (digest, bytes) in checkpoint.saved.content.iter() {
            let text = std::str::from_utf8(bytes)
                .map_err(|error| format!("turn content {digest} is not text: {error}"))?;
            let node = if Some(digest) == request {
                stage_request(&mut staging, text)?
            } else {
                stage_content(&mut staging, text).map_err(refused)?
            };
            content.push((digest.as_str().to_owned(), node));
            stored.push((digest.clone(), CheckpointContentPin::new(node.0)));
        }
        for (digest, pin) in held {
            content.push((digest.as_str().to_owned(), NodeId(*pin.as_bytes())));
        }
        content.sort_by(|left, right| left.0.cmp(&right.0));
        let root = staging
            .encode_value(
                NodeKind::PHASE_ROOT,
                &PhaseRootBody {
                    checkpoint: value.to_string(),
                    window_pin,
                    content,
                    run: config.as_ref().map(|config| config.run),
                    surface: config.as_ref().and_then(|config| config.surface),
                },
            )
            .map_err(|error| refused(error.into()))?;
        let mut held_nodes = held
            .iter()
            .map(|(_, pin)| NodeId(*pin.as_bytes()))
            .collect::<Vec<_>>();
        let mut config_stored = RunConfigStored::default();
        if let Some(config) = config {
            held_nodes.extend(config.held);
            config_stored = config.stored;
        }
        Ok(Self {
            root,
            surface: checkpoint
                .config
                .as_ref()
                .and_then(|config| config.surface().cloned()),
            staging,
            held: held_nodes,
            stored: PhaseStored {
                content: stored,
                config: config_stored,
            },
        })
    }

    /// Stages the nodes in `tx`, requiring the held ones, and returns the
    /// PhaseRoot the phase names with what it stored.
    pub(super) fn staged_in(self, tx: &mut lash_durable::ActorTx) -> (NodeId, PhaseStored) {
        for node in self.held {
            tx.require(node);
        }
        tx.stage_all(self.staging);
        tx.phase_surface(self.root, self.surface);
        (self.root, self.stored)
    }
}

/// Restages PhaseRoot `root` from `source` with its checkpoint text made
/// `edit` of it, as another build would have written the phase: its content
/// stays the installed nodes it names.
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
    let logical = checkpoint_value(
        serde_json::from_str(&body.checkpoint).map_err(|error| error.to_string())?,
        body.window_pin,
    )?;
    let mut value =
        serde_json::from_str(&edit(&logical.to_string())).map_err(|error| error.to_string())?;
    body.window_pin = structure_window_pin(&mut value)?;
    body.checkpoint = value.to_string();
    let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
    let root = staging
        .encode_value(NodeKind::PHASE_ROOT, &body)
        .map_err(|error| error.to_string())?;
    for node in body
        .content
        .iter()
        .map(|(_, node)| node)
        .chain(body.run.iter())
        .chain(body.surface.iter())
    {
        tx.require(*node);
    }
    tx.stage_all(staging);
    Ok(root)
}

/// Reads PhaseRoot `root` from `source` back into the phase checkpoint it
/// was staged from, with the node each content blob is stored as.
///
/// # Errors
///
/// The account of a node that does not read, a checkpoint text that does
/// not decode, or content that is not the bytes its digest names.
pub(super) fn load(
    source: &dyn NodeSource,
    root: NodeId,
    limits: lash_core_store::checkpoint_graph::DecodeLimits,
) -> Result<(PhaseCheckpoint, PhaseStored), String> {
    let mut decoder = GraphDecoder::new(source, limits);
    let text = |error: GraphRefusal| error.to_string();
    let body: PhaseRootBody = decoder.decode(root, NodeKind::PHASE_ROOT).map_err(text)?;
    let read = read_checkpoint(&body.checkpoint, body.window_pin)?;
    let mut content = TurnCheckpointContent::default();
    let mut stored = Vec::new();
    for (digest, node) in body.content {
        let blob = load_content(&mut decoder, root, node)?;
        let digest = CheckpointContentRef::stored(digest);
        content
            .insert_stored(digest.clone(), blob.into_bytes())
            .map_err(|error| error.to_string())?;
        stored.push((digest, CheckpointContentPin::new(node.0)));
    }
    let config = match body.run {
        None => None,
        Some(run) => {
            decoder
                .child_kind(root, run, &[NodeKind::CONFIG_VALUE])
                .map_err(text)?;
            let resolved = lash_core_store::checkpoint_graph::config::load(&mut decoder, run)?;
            let surface = body
                .surface
                .map(|node| {
                    decoder
                        .child_kind(root, node, &[NodeKind::TURN_CONTENT])
                        .map_err(text)?;
                    let ContentText(body) =
                        decoder.decode(node, NodeKind::TURN_CONTENT).map_err(text)?;
                    Ok::<_, String>((node, body))
                })
                .transpose()?;
            Some(RecordedRunConfig::load(
                (run, resolved),
                surface.as_ref().map(|(node, text)| (*node, text.as_str())),
            )?)
        }
    };
    Ok((
        PhaseCheckpoint {
            saved: SavedTurn {
                checkpoint: read.checkpoint,
                content,
            },
            delivered: read.delivered,
            before_turn: read.before_turn,
            trace: read.trace,
            config,
            cell: read.cell,
        },
        PhaseStored {
            content: stored,
            config: RunConfigStored::default(),
        },
    ))
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
    reads: &dyn lash_durable::DurableReads,
    session: &crate::SessionId,
    run: &crate::TurnId,
) -> Result<Option<RecordedPluginPhase>, lash_durable::DurableError> {
    Ok(read_phase(reads, session, run)
        .await?
        .map(|phase| RecordedPluginPhase {
            before_turn: phase.before_turn,
            surface: phase
                .config
                .and_then(|config| config.surface().map(|surface| (**surface).clone())),
        }))
}

/// Protocol events retained by a turn's phase, decoded without restoring a runtime.
/// A retired phase has no retained events.
pub async fn read_protocol_events(
    reads: &dyn lash_durable::DurableReads,
    session: &crate::SessionId,
    run: &crate::TurnId,
) -> Result<Vec<crate::ProtocolEvent>, lash_durable::DurableError> {
    let Some(phase) = read_phase(reads, session, run).await? else {
        return Ok(Vec::new());
    };
    let events = phase.saved.recorded_events().map_err(|error| {
        lash_durable::DurableError::Graph(GraphRefusal::InvalidBody {
            kind: NodeKind::PHASE_ROOT.0,
            reason: error.to_string(),
        })
    })?;
    Ok(events
        .into_iter()
        .filter_map(|event| match event {
            crate::SessionHistoryRecord::Protocol(event) => Some(event),
            crate::SessionHistoryRecord::Conversation(_) => None,
        })
        .collect())
}

async fn read_phase(
    reads: &dyn lash_durable::DurableReads,
    session: &crate::SessionId,
    run: &crate::TurnId,
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
    let (phase, _) = load(&graph, graph.root, limits).map_err(|reason| {
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

    /// BorrowedEncodingIsIdentical: borrowing canonical phase text keeps every
    /// staged envelope and its digest, including placed and chunked UTF-8 text.
    #[test]
    fn borrowed_content_encoding_keeps_nodes_and_digests() {
        let limits = lash_core_store::checkpoint_graph::DecodeLimits::within_total(1 << 22);
        for text in [
            String::new(),
            r#"{"escaped":"\n\"\\","number":123456789012345678901234567890}"#.into(),
            "é日🦀".repeat(8192),
        ] {
            let budget = || EncodeBudget::new(ExplicitLimit::new(u64::MAX));
            let mut owned = GraphStaging::new(budget());
            let expected = owned
                .encode_value(NodeKind::TURN_CONTENT, &ContentText(text.clone()))
                .unwrap();
            let mut borrowed = GraphStaging::new(budget());
            let actual = stage_content(&mut borrowed, &text).unwrap();
            assert_eq!(actual, expected);
            assert_eq!(borrowed.nodes(), owned.nodes());
            let ContentText(decoded) = GraphDecoder::new(&borrowed, limits)
                .decode(actual, NodeKind::TURN_CONTENT)
                .unwrap();
            assert_eq!(decoded, text);
        }
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
        let original = serde_json::json!({"checkpoint": {"window": {
            "pin": serde_json::to_string(&head).unwrap(), "messages": 3, "events": 2
        }}});
        let mut structured = original.clone();
        let window_pin = structure_window_pin(&mut structured).unwrap();
        let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
        let root = staging
            .encode_value(
                NodeKind::PHASE_ROOT,
                &PhaseRootBody {
                    checkpoint: structured.to_string(),
                    window_pin,
                    content: Vec::new(),
                    run: None,
                    surface: None,
                },
            )
            .unwrap();
        let mut decoder = GraphDecoder::new(
            &staging,
            lash_core_store::checkpoint_graph::DecodeLimits::within_total(1 << 20),
        );
        let stored: PhaseRootBody = decoder.decode(root, NodeKind::PHASE_ROOT).unwrap();
        assert_eq!(
            stored.window_pin,
            Some(serde_json::to_value(&head).unwrap())
        );
        let value: serde_json::Value = serde_json::from_str(&stored.checkpoint).unwrap();
        assert!(
            value.pointer("/checkpoint/window/pin").is_none(),
            "the text contains no escaped pin"
        );
        assert_eq!(
            checkpoint_value(value, stored.window_pin).unwrap(),
            original
        );
    }
}
