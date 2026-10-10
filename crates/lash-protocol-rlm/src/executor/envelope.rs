//! A code cell's envelope: the host's half of its parked run (ADR 0132 §8).
//!
//! Every park of a cell commits the machine's state with the broker ledger
//! that matches it, and this envelope beside them as the checkpoint's host
//! state. It carries what the activation that resumes the cell needs to
//! carry on as if the cell had never stopped: the document the cell was
//! lowered to, the effects it was lowered against with the tool each is,
//! the grants its deferred resolutions recorded, its prints, and the calls
//! it admitted. All of it is plain data, so nothing keeps a cell on the
//! process that started it.
//!
//! The document is recorded, never lowered again: a resumed cell runs the
//! program its first activation admitted, whatever the session's catalog
//! has become since.

use std::collections::{BTreeMap, BTreeSet};

use lash_core::RuntimeExecutionContext;
use lash_kernel_doc::{Datum, Document, EffectName, Signature};
use lash_vm_broker::kernel::{CheckpointPhase, ParkedCheckpoint};
use lash_vm_protocol::OpaqueVmState;
use lash_vm_runtime::{HostBoundary, HostEffect};

use super::host::CellHostLedgers;

/// version_surface = "coexist"
/// version_guard(items(LASH_CODEMODE_CELL_SEGMENT_CODE_DOMAIN_VERSION, code_digest))
const LASH_CODEMODE_CELL_SEGMENT_CODE_DOMAIN_VERSION: &str = "lash-codemode-cell-segment-code/v1";

/// One effect the cell was lowered against.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordedEffect {
    pub tool: lash_core::ToolId,
    pub signature: Signature,
    /// The turn controls the tool declared when the cell was lowered: what
    /// the cell's control admission reads at every park.
    pub controls: lash_core::TurnControls,
}

/// What a cell is, recorded when it was lowered and the same at every park.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CellEnvelope {
    /// The digest of the cell's source: a parked run under the cell's
    /// execution holds this source's state.
    pub code: String,
    /// The dialect the source was lowered from.
    pub dialect: String,
    /// The kernel document the cell runs, as JSON.
    pub document: String,
    /// The document's annotations, as JSON: where each statement came from
    /// in the source.
    pub annotations: String,
    /// What the dialect's function values are, where its sessions keep the
    /// functions a cell binds ([`lash_kernel_dialect::Package::function_values`]).
    pub function_values: Option<lash_kernel_dialect::FunctionValues>,
    /// The effects the document was lowered against.
    pub effects: BTreeMap<EffectName, RecordedEffect>,
    /// The grants the cell's deferred resolutions recorded.
    pub grants: BTreeMap<lash_core::ToolId, lash_core::ToolExecutionGrant>,
    /// The read-only host bindings the cell started with: they are the
    /// host's, and are not the session's when the cell ends.
    pub projected: BTreeSet<String>,
}

/// A cell's envelope at a park, as the activation that resumes the cell
/// reads it.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields, try_from = "CellSegmentStateWire")]
pub(super) struct CellSegmentState {
    pub cell: CellEnvelope,
    pub prints: Vec<Datum>,
    pub host: CellHostLedgers,
    pub started_process_ids: Vec<lash_core::ProcessId>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CellSegmentStateWire {
    cell: CellEnvelope,
    prints: Vec<Datum>,
    host: CellHostLedgers,
    started_process_ids: Vec<lash_core::ProcessId>,
}

impl TryFrom<CellSegmentStateWire> for CellSegmentState {
    type Error = String;

    fn try_from(wire: CellSegmentStateWire) -> Result<Self, Self::Error> {
        for call in &wire.host.calls {
            if !wire.cell.effects.contains_key(&call.operation) {
                return Err(format!(
                    "the cell ledger names an unrecorded effect: {}",
                    call.operation
                ));
            }
        }
        Ok(Self {
            cell: wire.cell,
            prints: wire.prints,
            host: wire.host,
            started_process_ids: wire.started_process_ids,
        })
    }
}

impl CellEnvelope {
    pub(super) fn code_digest(code: &str) -> String {
        lash_sansio::core_support::blake3_domain_hash_hex(
            LASH_CODEMODE_CELL_SEGMENT_CODE_DOMAIN_VERSION,
            code.as_bytes(),
        )
    }

    /// The effects `boundary` offers, as the envelope records them.
    pub(super) fn record_effects(boundary: &HostBoundary) -> BTreeMap<EffectName, RecordedEffect> {
        boundary
            .signatures()
            .into_iter()
            .filter_map(|(name, signature)| {
                let effect = boundary.effect(&name)?;
                Some((
                    name,
                    RecordedEffect {
                        tool: effect.tool.clone(),
                        signature,
                        controls: effect.controls.clone(),
                    },
                ))
            })
            .collect()
    }

    /// The boundary the cell was lowered against.
    pub(super) fn boundary(&self) -> Result<HostBoundary, String> {
        let mut boundary = HostBoundary::new();
        for (name, effect) in &self.effects {
            boundary
                .offer(
                    name.as_str(),
                    HostEffect {
                        tool: effect.tool.clone(),
                        signature: effect.signature.clone(),
                        controls: effect.controls.clone(),
                    },
                )
                .map_err(|error| error.to_string())?;
        }
        Ok(boundary)
    }

    /// The document the cell runs.
    pub(super) fn document(&self) -> Result<Document, String> {
        Document::from_json(&self.document).map_err(|error| error.to_string())
    }

    /// The cell's envelope now: what its parent holds of it at a park.
    pub(super) fn at_park(
        &self,
        ctx: &RuntimeExecutionContext<'_>,
        host: CellHostLedgers,
        prints: &[Datum],
    ) -> CellSegmentState {
        CellSegmentState {
            cell: self.clone(),
            prints: prints.to_vec(),
            host,
            started_process_ids: ctx.started_process_ids(),
        }
    }
}

impl CellSegmentState {
    pub(super) fn encode(&self) -> Result<Vec<u8>, String> {
        serde_json::to_vec(self).map_err(|error| error.to_string())
    }

    fn decode(bytes: &[u8]) -> Result<Self, String> {
        serde_json::from_slice(bytes).map_err(|error| error.to_string())
    }

    /// The parent ledgers of `ctx` a park hands over.
    pub(super) fn restore_context(&self, ctx: &RuntimeExecutionContext<'_>) {
        ctx.restore_started_process_ids(&self.started_process_ids);
    }
}

/// The envelope `checkpoint` holds, if it holds one.
pub(super) fn envelope_of(
    checkpoint: &ParkedCheckpoint<OpaqueVmState>,
) -> Result<Option<CellSegmentState>, String> {
    checkpoint
        .host
        .as_ref()
        .map(|host| CellSegmentState::decode(&host.0))
        .transpose()
}

pub(super) fn stored_checkpoint(snapshot: &str) -> Result<ParkedCheckpoint<OpaqueVmState>, String> {
    serde_json::from_str(snapshot).map_err(|error| error.to_string())
}

/// The records of the tool calls a cell completed, in call order, as its
/// stored `snapshot` holds them in its envelope's call ledger.
pub(crate) fn snapshot_tool_calls(
    snapshot: &str,
) -> Result<Vec<lash_core::ToolCallRecord>, String> {
    Ok(envelope_of(&stored_checkpoint(snapshot)?)?
        .map(|state| state.host.tool_call_records())
        .unwrap_or_default())
}

/// Whether this build resumes `snapshot`, a cell's stored checkpoint: the
/// broker's checkpoint and ledger, the envelope the resumed cell runs on
/// with, the document it names, and the kernel version its parked state
/// was written under, which its bytes state as its seal does. The account
/// of whichever does not decode is the `Err`.
pub(crate) fn check_cell_snapshot(snapshot: &str) -> Result<(), String> {
    let checkpoint = stored_checkpoint(snapshot)?;
    let envelope = envelope_of(&checkpoint)?;
    if let Some(envelope) = &envelope {
        envelope.cell.document()?;
        envelope.cell.boundary()?;
    }
    if let CheckpointPhase::Parked { state, .. } = &checkpoint.phase {
        let reads = lash_vm_client::kernel_reads();
        if !reads.contains(state.kernel()) {
            return Err(
                lash_vm_protocol::OpaqueStateRefusal::KernelOutsideReadRange {
                    found: state.kernel(),
                    reads,
                }
                .to_string(),
            );
        }
        lash_vm_runtime::check_sealed_kernel(state).map_err(|refusal| refusal.to_string())?;
    }
    Ok(())
}

/// A cell resumed from its latest park: the envelope its host runs on
/// with, and the requests of the calls its ledger holds open. The broker
/// reads the checkpoint itself.
pub(super) struct ResumedCell {
    pub state: CellSegmentState,
    pub open_calls: Vec<lash_vm_protocol::EncodedPayload>,
    /// The effect identity each open call was performed at.
    pub open_sites:
        std::collections::BTreeMap<lash_core::ToolCallId, lash_kernel_doc::EffectIdentity>,
}

/// The cell running `code` as its latest park holds it, if it has one.
pub(super) async fn resumed_cell(
    snapshots: &lash_vm_broker::DurableSnapshotStore,
    code: &str,
) -> Result<Option<ResumedCell>, String> {
    let Some((_, checkpoint)) = snapshots
        .latest_park::<OpaqueVmState>()
        .await
        .map_err(|refusal| refusal.0)?
    else {
        return Ok(None);
    };
    let state = envelope_of(&checkpoint)?
        .ok_or_else(|| "the cell's checkpoint has no envelope".to_owned())?;
    if state.cell.code != CellEnvelope::code_digest(code) {
        return Err("the checkpoint under this cell's execution holds other source".to_owned());
    }
    let (open_calls, open_sites) = match &checkpoint.phase {
        CheckpointPhase::Parked { ledger, .. } => (
            ledger
                .executions()
                .map(|effect| effect.request.clone())
                .collect(),
            ledger
                .pending()
                .filter_map(|(identity, pending)| match &pending.standing {
                    lash_vm_broker::kernel::Standing::Admitted(effect) => {
                        Some((effect.call.clone(), identity.clone()))
                    }
                    _ => None,
                })
                .collect(),
        ),
        CheckpointPhase::Ended { .. } => (Vec::new(), std::collections::BTreeMap::new()),
    };
    Ok(Some(ResumedCell {
        state,
        open_calls,
        open_sites,
    }))
}

impl lash_core::store::DurableRecord for CellSegmentState {
    const SURFACE: lash_core::store::SurfaceFormat =
        lash_core::surface_format!(crate::executor::CODEMODE_SNAPSHOT_VERSION);
}

// --- The envelope as checkpoint graph nodes (FIG-5887 §3) -------------------

use lash_core_store::checkpoint_graph::{
    GraphDecoder, GraphRefusal, GraphStaging, NodeId, NodeKind, TokenCodec, TokenError,
    TokenReader, TokenWriter,
};
use lash_vm_broker::round_graph::{decode_opt_tree, decode_tree, encode_opt_tree, encode_tree};

fn graph_invalid(kind: NodeKind, reason: impl std::fmt::Display) -> GraphRefusal {
    GraphRefusal::InvalidBody {
        kind: kind.0,
        reason: reason.to_string(),
    }
}

/// Kind 12: the effects a cell was lowered against, each with its tool,
/// signature and declared controls; the grants its deferred resolutions
/// recorded; and its projected binding names. All sorted.
#[derive(Clone, Debug, PartialEq)]
pub struct EffectTable {
    pub effects: BTreeMap<EffectName, RecordedEffect>,
    pub grants: BTreeMap<lash_core::ToolId, lash_core::ToolExecutionGrant>,
    pub projected: BTreeSet<String>,
}

impl TokenCodec for EffectTable {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(3);
        out.map(self.effects.len() as u64);
        for (name, effect) in &self.effects {
            name.encode(out);
            out.seq(3);
            out.text(effect.tool.as_str());
            effect.signature.encode(out);
            encode_tree(&effect.controls, out);
        }
        out.map(self.grants.len() as u64);
        for (tool, grant) in &self.grants {
            out.text(tool.as_str());
            encode_tree(grant, out);
        }
        out.seq(self.projected.len() as u64);
        for name in &self.projected {
            out.text(name);
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let unsorted = || {
            TokenError::NonCanonical(
                lash_core_store::checkpoint_graph::CanonicalFault::UnsortedOrDuplicateKey,
            )
        };
        input.expect_struct(3)?;
        let mut effects = BTreeMap::new();
        for _ in 0..input.map()? {
            let name = EffectName::decode(input)?;
            input.expect_struct(3)?;
            let effect = RecordedEffect {
                tool: lash_core::ToolId::new(input.text()?),
                signature: Signature::decode(input)?,
                controls: decode_tree(input)?,
            };
            if effects
                .last_key_value()
                .is_some_and(|(last, _)| last >= &name)
            {
                return Err(unsorted());
            }
            effects.insert(name, effect);
        }
        let mut grants = BTreeMap::new();
        for _ in 0..input.map()? {
            let tool = lash_core::ToolId::new(input.text()?);
            let grant = decode_tree(input)?;
            if grants
                .last_key_value()
                .is_some_and(|(last, _)| last >= &tool)
            {
                return Err(unsorted());
            }
            grants.insert(tool, grant);
        }
        let mut projected = BTreeSet::new();
        for _ in 0..input.seq()? {
            let name = input.text()?.to_owned();
            if projected.last().is_some_and(|last: &String| last >= &name) {
                return Err(unsorted());
            }
            projected.insert(name);
        }
        Ok(Self {
            effects,
            grants,
            projected,
        })
    }
}

/// Kind 15: what a cell is, staged once when it is lowered and named by
/// every park's root: no park copies it.
struct EnvelopeBody {
    code: String,
    dialect: String,
    document_id: lash_kernel_doc::DocumentId,
    document: NodeId,
    annotations: NodeId,
    function_values: Option<lash_kernel_dialect::FunctionValues>,
    effects: NodeId,
    release: NodeId,
}

impl TokenCodec for EnvelopeBody {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(8);
        out.text(&self.code);
        out.text(&self.dialect);
        self.document_id.encode(out);
        self.document.encode(out);
        self.annotations.encode(out);
        encode_opt_tree(self.function_values.as_ref(), out);
        self.effects.encode(out);
        self.release.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(8)?;
        Ok(Self {
            code: input.text()?.to_owned(),
            dialect: input.text()?.to_owned(),
            document_id: lash_kernel_doc::DocumentId::decode(input)?,
            document: NodeId::decode(input)?,
            annotations: NodeId::decode(input)?,
            function_values: decode_opt_tree(input)?,
            effects: NodeId::decode(input)?,
            release: NodeId::decode(input)?,
        })
    }
}

impl CellEnvelope {
    /// Stages the cell's envelope under the exact helper release `release`
    /// (a ReleaseManifest), its document and annotations as typed nodes,
    /// and returns the envelope's node.
    ///
    /// # Errors
    ///
    /// [`GraphRefusal::InvalidBody`] for a document or annotations that do
    /// not decode, or any refusal of the staging area.
    pub fn stage(
        &self,
        staging: &mut GraphStaging,
        release: NodeId,
    ) -> Result<NodeId, GraphRefusal> {
        let document = self
            .document()
            .map_err(|error| graph_invalid(NodeKind::DOCUMENT, error))?;
        let annotations = lash_kernel_doc::Annotations::from_json(&self.annotations)
            .map_err(|error| graph_invalid(NodeKind::ANNOTATIONS, error))?;
        let document_id = document
            .identity()
            .map_err(|error| graph_invalid(NodeKind::DOCUMENT, error))?;
        let body = EnvelopeBody {
            code: self.code.clone(),
            dialect: self.dialect.clone(),
            document_id,
            document: staging.encode_value(NodeKind::DOCUMENT, &document)?,
            annotations: staging.encode_value(NodeKind::ANNOTATIONS, &annotations)?,
            function_values: self.function_values.clone(),
            effects: staging.encode_value(
                NodeKind::EFFECT_TABLE,
                &EffectTable {
                    effects: self.effects.clone(),
                    grants: self.grants.clone(),
                    projected: self.projected.clone(),
                },
            )?,
            release,
        };
        staging.encode_value(NodeKind::CELL_ENVELOPE, &body)
    }

    /// Reads envelope `id` back, with the release manifest it pins,
    /// checking every child's kind and that the document is the one its
    /// semantic identity names.
    ///
    /// # Errors
    ///
    /// Any refusal of a node read, [`GraphRefusal::WrongChildKind`], or
    /// [`GraphRefusal::InvalidBody`] for a document of another identity.
    pub fn load(
        decoder: &mut GraphDecoder<'_>,
        id: NodeId,
    ) -> Result<(Self, NodeId), GraphRefusal> {
        let body: EnvelopeBody = decoder.decode(id, NodeKind::CELL_ENVELOPE)?;
        decoder.child_kind(id, body.release, &[NodeKind::RELEASE_MANIFEST])?;
        let document: Document = decoder.decode(body.document, NodeKind::DOCUMENT)?;
        if document.identity().ok() != Some(body.document_id) {
            return Err(graph_invalid(
                NodeKind::CELL_ENVELOPE,
                "the document is not the one its identity names",
            ));
        }
        let annotations: lash_kernel_doc::Annotations =
            decoder.decode(body.annotations, NodeKind::ANNOTATIONS)?;
        let table: EffectTable = decoder.decode(body.effects, NodeKind::EFFECT_TABLE)?;
        let envelope = Self {
            code: body.code,
            dialect: body.dialect,
            document: document
                .to_json()
                .map_err(|error| graph_invalid(NodeKind::DOCUMENT, error))?,
            annotations: annotations
                .to_json()
                .map_err(|error| graph_invalid(NodeKind::ANNOTATIONS, error))?,
            function_values: body.function_values,
            effects: table.effects,
            grants: table.grants,
            projected: table.projected,
        };
        Ok((envelope, body.release))
    }
}
