//! A turn phase's checkpoint as checkpoint graph nodes (FIG-5887 §3): a
//! PhaseRoot (kind 33) over the TurnContent (kind 39) its machine
//! checkpoint names.
//!
//! The machine's checkpoint is bounded state; the transcript it would
//! otherwise copy is content the checkpoint names by digest, in chains whose
//! closed chunks keep their digests as the turn appends. Each content blob
//! is one TurnContent node, and the PhaseRoot names every blob its
//! checkpoint needs. A phase stages only the blobs its machine's checkpoint
//! added (FIG-5955): the others an earlier phase of the turn stored, and the
//! root requires them by the node the machine was told each is stored as.
//! The root's checkpoint text is the phase's JSON without its content.

use lash_core_store::checkpoint_graph::{
    EncodeBudget, ExplicitLimit, GraphDecoder, GraphRefusal, GraphStaging, NodeId, NodeKind,
    NodeSource, TokenCodec, TokenError, TokenReader, TokenWriter,
};
use lash_sansio::{
    CheckpointContentPin, CheckpointContentRef, SavedTurn, TurnCheckpoint, TurnCheckpointContent,
};

use super::session::PhaseCheckpoint;
use crate::HostTurnProtocol;

/// The phase's checkpoint without its content, as the root's text holds it.
#[derive(serde::Serialize)]
struct PhaseText<'a> {
    checkpoint: &'a TurnCheckpoint<HostTurnProtocol>,
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    delivered: &'a [crate::AdmittedTurnInputs],
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    before_turn: &'a [crate::plugin::RecordedTurnContribution],
    #[serde(skip_serializing_if = "Option::is_none")]
    trace: &'a Option<lash_trace::DurableTraceScope>,
}

/// The root's text read back.
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
}

/// Kind 33's body: the checkpoint text, and each content blob the
/// checkpoint names by its digest, ascending.
struct PhaseRootBody {
    checkpoint: String,
    content: Vec<(String, NodeId)>,
}

impl TokenCodec for PhaseRootBody {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        out.text(&self.checkpoint);
        out.seq(self.content.len() as u64);
        for (digest, node) in &self.content {
            out.seq(2);
            out.text(digest);
            node.encode(out);
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        let checkpoint = input.text()?.to_owned();
        let count = input.seq()?;
        let mut content = Vec::new();
        for _ in 0..count {
            input.expect_struct(2)?;
            let digest = input.text()?.to_owned();
            content.push((digest, NodeId::decode(input)?));
        }
        Ok(Self {
            checkpoint,
            content,
        })
    }
}

/// Kind 39's body: one content blob's canonical JSON text.
struct ContentText(String);

impl TokenCodec for ContentText {
    fn encode(&self, out: &mut TokenWriter) {
        out.text(&self.0);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(Self(input.text()?.to_owned()))
    }
}

/// A phase checkpoint staged as nodes, for the transaction that advances
/// the phase to install with it.
pub(super) struct StagedPhase {
    root: NodeId,
    staging: GraphStaging,
    held: Vec<NodeId>,
    stored: PhaseStored,
}

/// The blobs a staged phase added, each by the node it is stored as: what
/// its machine is told once the phase's commit is acknowledged
/// ([`lash_sansio::TurnMachine::checkpoint_stored`]).
#[must_use = "the machine is told what its committed phase stored"]
pub(super) struct PhaseStored(Vec<(CheckpointContentRef, CheckpointContentPin)>);

impl PhaseStored {
    /// Tell `machine` the phase is stored.
    pub(super) fn committed(self, machine: &mut crate::TurnMachine) {
        machine.checkpoint_stored(self.0);
    }
}

impl StagedPhase {
    /// Stages `checkpoint`: a TurnContent node per content blob it holds
    /// and the PhaseRoot over them and over `held`, the blobs an earlier
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
        let text = serde_json::to_string(&PhaseText {
            checkpoint: &checkpoint.saved.checkpoint,
            delivered: &checkpoint.delivered,
            before_turn: &checkpoint.before_turn,
            trace: &checkpoint.trace,
        })
        .map_err(|error| format!("the turn checkpoint does not encode: {error}"))?;
        let refused = |error: GraphRefusal| format!("the turn checkpoint does not stage: {error}");
        let mut content = Vec::new();
        let mut stored = Vec::new();
        for (digest, bytes) in checkpoint.saved.content.iter() {
            let text = std::str::from_utf8(bytes)
                .map_err(|error| format!("turn content {digest} is not text: {error}"))?;
            let node = staging
                .encode_value(NodeKind::TURN_CONTENT, &ContentText(text.to_owned()))
                .map_err(refused)?;
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
                    checkpoint: text,
                    content,
                },
            )
            .map_err(refused)?;
        Ok(Self {
            root,
            staging,
            held: held
                .iter()
                .map(|(_, pin)| NodeId(*pin.as_bytes()))
                .collect(),
            stored: PhaseStored(stored),
        })
    }

    /// Stages the nodes in `tx`, requiring the held ones, and returns the
    /// PhaseRoot the phase names with what it stored.
    pub(super) fn staged_in(self, tx: &mut lash_durable::ActorTx) -> (NodeId, PhaseStored) {
        for node in self.held {
            tx.require(node);
        }
        tx.stage_all(self.staging);
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
    body.checkpoint = edit(&body.checkpoint);
    let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
    let root = staging
        .encode_value(NodeKind::PHASE_ROOT, &body)
        .map_err(|error| error.to_string())?;
    for (_, node) in &body.content {
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
) -> Result<(PhaseCheckpoint, PhaseStored), String> {
    let mut decoder = GraphDecoder::new(source, lash_durable::domain::graph::OWNER_READ_LIMITS);
    let text = |error: GraphRefusal| error.to_string();
    let body: PhaseRootBody = decoder.decode(root, NodeKind::PHASE_ROOT).map_err(text)?;
    let read: PhaseRead = serde_json::from_str(&body.checkpoint)
        .map_err(|error| format!("the turn checkpoint does not decode: {error}"))?;
    let mut content = TurnCheckpointContent::default();
    let mut stored = Vec::new();
    for (digest, node) in body.content {
        decoder
            .child_kind(root, node, &[NodeKind::TURN_CONTENT])
            .map_err(text)?;
        let ContentText(blob) = decoder.decode(node, NodeKind::TURN_CONTENT).map_err(text)?;
        let digest = CheckpointContentRef::stored(digest);
        content
            .insert_stored(digest.clone(), blob.into_bytes())
            .map_err(|error| error.to_string())?;
        stored.push((digest, CheckpointContentPin::new(node.0)));
    }
    Ok((
        PhaseCheckpoint {
            saved: SavedTurn {
                checkpoint: read.checkpoint,
                content,
            },
            delivered: read.delivered,
            before_turn: read.before_turn,
            trace: read.trace,
        },
        PhaseStored(stored),
    ))
}
