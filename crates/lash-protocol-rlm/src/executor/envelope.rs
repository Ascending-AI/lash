//! A code cell's envelope: the host's half of its parked run (ADR 0132 §8).
//!
//! Every park of a cell commits the machine's state with the effect
//! membership that matches it, and this envelope and the cell's HostRoot
//! beside them in the cell's execution root. Together they carry what the
//! activation that resumes the cell needs to carry on as if the cell had
//! never stopped: the document the cell was lowered to, the effects it was
//! lowered against with the tool each is, the grants its deferred
//! resolutions recorded, its prints, and the calls it admitted. All of it
//! is graph nodes, so nothing keeps a cell on the process that started it.
//!
//! The document is recorded, never lowered again: a resumed cell runs the
//! program its first activation admitted, whatever the session's catalog
//! has become since.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_dialect::Written;
use lash_kernel_doc::{Datum, Document, EffectName, Signature, Site};
use lash_vm_runtime::{HostBoundary, HostEffect};

use super::host::{CellHostLedgers, HostGraph, LedgerCall, LedgerCallState};
use super::host_graph::{CallStatus, CellEnd, HostState};

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
    /// Where each statement of the document came from in the source, when
    /// the dialect gave a map: what a failure locates its tasks by, at
    /// whichever activation the cell ends. Nothing a run or a save needs is
    /// in it.
    pub source_map: Option<lash_kernel_doc::SourceMap>,
    /// What the source wrote of each function the document makes, by the
    /// site of its closure: what the cell saves a function it leaves with
    /// ([`lash_kernel_dialect::Lowered::written`]).
    #[serde(with = "lash_kernel_dialect::written_table")]
    pub written: BTreeMap<Site, Written>,
    /// What the dialect's function values are, where its sessions keep the
    /// functions a cell binds ([`lash_kernel_dialect::Package::function_values`]).
    pub function_values: Option<lash_kernel_dialect::FunctionValues>,
    /// The effects the document was lowered against.
    pub effects: BTreeMap<EffectName, RecordedEffect>,
    /// The grants the cell's deferred resolutions recorded.
    pub grants: BTreeMap<lash_core::ToolId, lash_core::ToolExecutionGrant>,
    /// Recorded host explanations for unavailable call paths; no authority.
    pub unavailable_notes: BTreeMap<String, String>,
    /// The read-only host bindings the cell started with: they are the
    /// host's, and are not the session's when the cell ends.
    pub projected: BTreeSet<String>,
    /// The cell's read plan (ADR 0144): each effect `main` reaches whose
    /// tool declares a read, with the provider the run's surface retained
    /// for it. The cell performs these as reads, answered inside their
    /// statements and recorded nowhere but the cell's saved state; its
    /// roots owe each provider's reader while the cell lives.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub reads: BTreeMap<EffectName, lash_core::ProviderKey>,
}

impl CellEnvelope {
    pub(super) fn code_digest(code: &str) -> String {
        lash_sansio::core_support::blake3_domain_hash_hex(
            LASH_CODEMODE_CELL_SEGMENT_CODE_DOMAIN_VERSION,
            code.as_bytes(),
        )
    }

    /// The grants the cell's committed deferred matches recorded, by the
    /// effect path each matched tool's binding names: what a start the
    /// cell declares captures an effect the run's surface does not offer
    /// from, never resolving it again. A path two grants name is left out,
    /// so neither is captured in place of the other.
    pub(super) fn recorded_grants(&self) -> BTreeMap<EffectName, lash_core::ToolExecutionGrant> {
        let mut grants = BTreeMap::new();
        let mut conflicted = BTreeSet::new();
        for grant in self.grants.values() {
            let Some(effect) = lash_core::effect_path_of(grant.manifest())
                .and_then(|path| EffectName::new(path.as_str()).ok())
            else {
                continue;
            };
            if grants.insert(effect.clone(), grant.clone()).is_some() {
                conflicted.insert(effect);
            }
        }
        for effect in conflicted {
            grants.remove(&effect);
        }
        grants
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
}

/// The helper release manifest the envelope of a cell written in `dialect`
/// pins: the build's own for that dialect, which its workers run a cell
/// started here on.
pub(super) fn cell_release(dialect: &str) -> Result<NodeId, String> {
    static RELEASES: std::sync::OnceLock<Result<BTreeMap<String, NodeId>, String>> =
        std::sync::OnceLock::new();
    RELEASES
        .get_or_init(|| {
            let releases = lash_vm_broker::kernel::state::release_manifests()
                .map_err(|failure| failure.to_string())?;
            Ok(releases
                .manifests
                .dialects()
                .filter_map(|dialect| {
                    let release = releases.manifests.current(dialect)?;
                    Some((dialect.to_owned(), NodeId(release.0)))
                })
                .collect())
        })
        .as_ref()
        .map_err(Clone::clone)?
        .get(dialect)
        .copied()
        .ok_or_else(|| format!("the build holds no helper release of the dialect `{dialect}`"))
}

/// Each tool request read back through the projection it was frozen
/// under, over the cell's document's entries.
pub(super) struct EntryProjections<'a>(pub &'a super::processes::CellEntries);

impl lash_vm_broker::kernel::graph::Projections for EntryProjections<'_> {
    fn project(&self, projection: &str, args: &[Datum]) -> Result<serde_json::Value, String> {
        use lash_vm_broker::kernel::graph::ArgumentProjection as _;
        super::host::CellArgs::named(projection, self.0)?.project(args)
    }
}

/// A cell as its execution root holds it: the envelope it runs on, its
/// host's records, prints and started processes, and the host graph its
/// next park builds on.
pub(super) struct ResumedCell {
    pub cell: CellEnvelope,
    pub prints: Vec<Datum>,
    pub ledgers: CellHostLedgers,
    pub started_process_ids: Vec<lash_core::ProcessId>,
    pub graph: HostGraph,
    /// The effect identity each open call was performed at.
    pub open_sites: BTreeMap<lash_core::ToolCallId, lash_kernel_doc::EffectIdentity>,
    /// The session an ended root's end names, as the activation that
    /// recorded the end left it: its descriptor page, which the session
    /// adopts (FIG-6315). None for a parked root.
    pub session: Option<lash_core_store::execution_state::HydratedExecutionGraph>,
}

/// The cell root `root` (an ExecRoot) in `source` holds: parked, its root's
/// host and envelope; ended, its CellEnd's.
///
/// # Errors
///
/// The account of a node that does not read, or a root that is not a
/// cell's.
pub(super) fn load_cell(
    source: &dyn lash_core_store::checkpoint_graph::NodeSource,
    root: NodeId,
    limits: lash_core_store::checkpoint_graph::DecodeLimits,
) -> Result<ResumedCell, String> {
    use lash_core_store::checkpoint_graph::bodies::{ExecRoot, ExecStatus};
    let text = |error: GraphRefusal| error.to_string();
    let mut decoder = GraphDecoder::new(source, limits);
    let body: ExecRoot = decoder.decode(root, NodeKind::EXEC_ROOT).map_err(text)?;
    let (host, envelope, session) = match body.status {
        ExecStatus::Parked => (
            body.host.ok_or("a parked cell root names no host state")?,
            body.envelope
                .ok_or("a parked cell root names no envelope")?,
            None,
        ),
        ExecStatus::Ended => {
            let end = CellEnd::load(&mut decoder, body.state).map_err(text)?;
            let descriptor = source
                .fetch(&end.session)
                .ok_or("an ended cell root omits its session's descriptor")?;
            let session = lash_core_store::execution_state::HydratedExecutionGraph {
                root: end.session,
                nodes: BTreeMap::from([(
                    end.session,
                    lash_core_store::checkpoint_graph::EncodedNode {
                        id: end.session,
                        bytes: descriptor.bytes.into(),
                    },
                )]),
            };
            (end.host, end.envelope, Some(session))
        }
    };
    let (cell, _) = CellEnvelope::load(&mut decoder, envelope).map_err(text)?;
    let state = HostState::load(&mut decoder, host).map_err(text)?;
    let document = cell.document()?;
    let entries = super::processes::CellEntries {
        identity: document.identity().map_err(|error| error.to_string())?,
        document,
    };
    let projections = EntryProjections(&entries);
    let mut ledgers = CellHostLedgers {
        calls: Vec::with_capacity(state.calls.items().len()),
        tool_call_limit: state.tool_call_limit,
        reads: state.reads,
    };
    let mut graph = HostGraph {
        envelope: Some(envelope),
        ..HostGraph::default()
    };
    graph.installed.insert(envelope);
    let mut open_sites = BTreeMap::new();
    for summary in state.calls.items() {
        if !cell.effects.contains_key(&summary.operation) {
            return Err(format!(
                "the cell ledger names an unrecorded effect: {}",
                summary.operation
            ));
        }
        graph
            .admissions
            .insert(summary.call_id.clone(), summary.admission);
        let call_state = match &summary.status {
            CallStatus::Pending => {
                open_sites.insert(summary.call_id.clone(), summary.effect.clone());
                LedgerCallState::Pending {
                    call_id: summary.call_id.clone(),
                }
            }
            CallStatus::Completed { outcome, .. } => {
                let loaded = lash_vm_broker::round_graph::CallOutcome::load(&mut decoder, *outcome)
                    .map_err(text)?;
                let admitted = lash_vm_broker::kernel::graph::Admission::load(
                    &mut decoder,
                    summary.admission,
                    &projections,
                )
                .map_err(text)?;
                let lash_vm_broker::kernel::graph::FrozenRequest::Tool { call, .. } =
                    admitted.request
                else {
                    return Err("a summarized call's admission holds no tool call".to_owned());
                };
                if call.args != loaded.args {
                    return Err(
                        "a cell call body's arguments differ from its recorded request projection"
                            .to_owned(),
                    );
                }
                graph.outcomes.insert(summary.call_id.clone(), *outcome);
                graph.installed.insert(*outcome);
                LedgerCallState::Completed {
                    record: Box::new(lash_core::ToolCallRecord {
                        call_id: loaded.call_id,
                        provider_call_id: loaded.provider_call_id,
                        tool: loaded.tool_name,
                        args: call.args,
                        output: loaded.output,
                    }),
                }
            }
        };
        ledgers.calls.push(LedgerCall {
            operation: summary.operation.clone(),
            effect: summary.effect.clone(),
            state: call_state,
        });
    }
    let prints = state
        .prints
        .items()
        .iter()
        .map(|print| print.0.clone())
        .collect();
    let started_process_ids = state
        .processes
        .items()
        .iter()
        .map(|process| process.0.clone())
        .collect();
    graph.state = state;
    Ok(ResumedCell {
        cell,
        prints,
        ledgers,
        started_process_ids,
        graph,
        open_sites,
        session,
    })
}

/// The cell running `code` as its latest root holds it, if it has one.
pub(super) async fn resumed_cell(
    snapshots: &lash_vm_broker::DurableSnapshotStore,
    code: &str,
) -> Result<Option<ResumedCell>, String> {
    let Some(read) = snapshots.latest_root().await.map_err(|refusal| refusal.0)? else {
        return Ok(None);
    };
    let cell = load_cell(&read.graph, read.held.root, read.read_limits)?;
    if cell.cell.code != CellEnvelope::code_digest(code) {
        return Err("the root under this cell's execution holds other source".to_owned());
    }
    Ok(Some(cell))
}

/// The records of the tool calls a cell completed, in call order, as its
/// stored root holds them in its host state.
pub(crate) fn snapshot_tool_calls(
    cell: &lash_core::plugin::StoredCell,
) -> Result<
    (
        Vec<lash_core::ToolCallRecord>,
        lash_vm_broker::round_graph::ReportView,
    ),
    String,
> {
    let loaded = load_cell(&cell.graph, cell.row.root, cell.read_limits)?;
    let calls = loaded.ledgers.tool_call_records();
    let mut decoder = GraphDecoder::new(&cell.graph, cell.read_limits);
    let entries = calls
        .iter()
        .map(|call| {
            let outcome = loaded
                .graph
                .outcomes
                .get(&call.call_id)
                .copied()
                .ok_or_else(|| format!("call {} has no recorded outcome", call.call_id))?;
            lash_vm_broker::round_graph::CallOutcome::body_node(&mut decoder, outcome)
                .map_err(|error| error.to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok((calls, lash_vm_broker::round_graph::ReportView { entries }))
}

/// Read the original archival component. Its arguments are the exact
/// projection recorded at settlement; no live execution machinery is needed.
pub(crate) fn recorded_call(
    decoder: &mut GraphDecoder<'_>,
    body: NodeId,
) -> Result<lash_core::ToolCallRecord, GraphRefusal> {
    lash_vm_broker::round_graph::CallBody::load(decoder, body).map(|body| body.record)
}

/// Offline call reads while a cell is still saved under its execution root.
pub(crate) fn recorded_cell_ledger(
    decoder: &mut GraphDecoder<'_>,
    root: NodeId,
) -> Result<(Vec<lash_core::ToolCallRecord>, Vec<lash_core::ToolCallId>), GraphRefusal> {
    use lash_core_store::checkpoint_graph::bodies::{ExecRoot, ExecStatus};
    let root: ExecRoot = decoder.decode(root, NodeKind::EXEC_ROOT)?;
    let host = match root.status {
        ExecStatus::Parked => root.host.ok_or_else(|| GraphRefusal::InvalidBody {
            kind: NodeKind::EXEC_ROOT.0,
            reason: "a parked cell has no host ledger".to_owned(),
        })?,
        ExecStatus::Ended => CellEnd::load(decoder, root.state)?.host,
    };
    let state = HostState::load(decoder, host)?;
    let mut records = Vec::new();
    let mut pending = Vec::new();
    for call in state.calls.items() {
        match call.status {
            CallStatus::Pending => pending.push(call.call_id.clone()),
            CallStatus::Completed { outcome, .. } => {
                let body = lash_vm_broker::round_graph::CallOutcome::body_node(decoder, outcome)?;
                records.push(recorded_call(decoder, body)?);
            }
        }
    }
    Ok((records, pending))
}

/// Whether this build resumes `cell`, a cell's stored root: its host state,
/// the envelope the resumed cell runs on with, the document it names, and
/// the kernel version its parked state was written under. The account of
/// whichever does not decode is the `Err`.
pub(crate) fn check_cell_snapshot(cell: &lash_core::plugin::StoredCell) -> Result<(), String> {
    use lash_core_store::checkpoint_graph::bodies::{ExecRoot, ExecStatus};
    let loaded = load_cell(&cell.graph, cell.row.root, cell.read_limits)?;
    loaded.cell.document()?;
    loaded.cell.boundary()?;
    let mut decoder = GraphDecoder::new(&cell.graph, cell.read_limits);
    let body: ExecRoot = decoder
        .decode(cell.row.root, NodeKind::EXEC_ROOT)
        .map_err(|error| error.to_string())?;
    if body.status == ExecStatus::Parked {
        let reads = lash_vm_client::kernel_reads();
        if !reads.contains(body.format) {
            return Err(format!(
                "the parked run was written under kernel version {}, outside read range {reads}",
                body.format
            ));
        }
        // The root is where the parent reads the version from: a state that
        // states another would resume under a version it was not parked
        // under (FIG-5716).
        let page = cell.graph.nodes.get(&body.state).ok_or_else(|| {
            format!(
                "the parked run's checkpoint root {} is not in its graph",
                body.state
            )
        })?;
        let header = lash_vm_broker::kernel::state::checkpoint_header(body.state, &page.bytes)
            .map_err(|error| error.to_string())?;
        if header.kernel != body.format {
            return Err(format!(
                "the parked run's root records kernel version {}, and its checkpoint root states kernel version {}",
                body.format, header.kernel
            ));
        }
    }
    Ok(())
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
/// recorded, including unavailable notes; its projected binding names;
/// and its read plan. All sorted.
#[derive(Clone, Debug, PartialEq)]
pub struct EffectTable {
    pub effects: BTreeMap<EffectName, RecordedEffect>,
    pub grants: BTreeMap<lash_core::ToolId, lash_core::ToolExecutionGrant>,
    /// Recorded host explanations for unavailable call paths; no authority.
    pub unavailable_notes: BTreeMap<String, String>,
    pub projected: BTreeSet<String>,
    /// Each planned read's effect, with its retained provider.
    pub reads: BTreeMap<EffectName, lash_core::ProviderKey>,
}

impl TokenCodec for EffectTable {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(5);
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
        encode_tree(&self.unavailable_notes, out);
        out.seq(self.projected.len() as u64);
        for name in &self.projected {
            out.text(name);
        }
        encode_tree(&self.reads, out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let unsorted = || {
            TokenError::NonCanonical(
                lash_core_store::checkpoint_graph::CanonicalFault::UnsortedOrDuplicateKey,
            )
        };
        input.expect_struct(5)?;
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
        let unavailable_notes = decode_tree(input)?;
        let mut projected = BTreeSet::new();
        for _ in 0..input.seq()? {
            let name = input.text()?.to_owned();
            if projected.last().is_some_and(|last: &String| last >= &name) {
                return Err(unsorted());
            }
            projected.insert(name);
        }
        let reads: BTreeMap<EffectName, lash_core::ProviderKey> = decode_tree(input)?;
        if reads.keys().any(|effect| !effects.contains_key(effect)) {
            return Err(TokenError::Invalid(
                "the read plan names an effect the cell was not lowered against".to_owned(),
            ));
        }
        Ok(Self {
            effects,
            grants,
            unavailable_notes,
            projected,
            reads,
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
    source_map: Option<NodeId>,
    written: NodeId,
    function_values: Option<lash_kernel_dialect::FunctionValues>,
    effects: NodeId,
    release: NodeId,
}

impl TokenCodec for EnvelopeBody {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(9);
        out.text(&self.code);
        out.text(&self.dialect);
        self.document_id.encode(out);
        self.document.encode(out);
        self.source_map.encode(out);
        self.written.encode(out);
        encode_opt_tree(self.function_values.as_ref(), out);
        self.effects.encode(out);
        self.release.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(9)?;
        Ok(Self {
            code: input.text()?.to_owned(),
            dialect: input.text()?.to_owned(),
            document_id: lash_kernel_doc::DocumentId::decode(input)?,
            document: NodeId::decode(input)?,
            source_map: Option::decode(input)?,
            written: NodeId::decode(input)?,
            function_values: decode_opt_tree(input)?,
            effects: NodeId::decode(input)?,
            release: NodeId::decode(input)?,
        })
    }
}

impl CellEnvelope {
    /// Stages the cell's envelope under the exact helper release `release`
    /// (a ReleaseManifest), its document, source map and written table as
    /// typed nodes, and returns the envelope's node.
    ///
    /// # Errors
    ///
    /// [`GraphRefusal::InvalidBody`] for a document that does not decode or
    /// a map of another document, or any refusal of the staging area.
    pub fn stage(
        &self,
        staging: &mut GraphStaging,
        release: NodeId,
    ) -> Result<NodeId, GraphRefusal> {
        let document = self
            .document()
            .map_err(|error| graph_invalid(NodeKind::DOCUMENT, error))?;
        let document_id = document
            .identity()
            .map_err(|error| graph_invalid(NodeKind::DOCUMENT, error))?;
        if self
            .source_map
            .as_ref()
            .is_some_and(|map| map.document != document_id)
        {
            return Err(graph_invalid(
                NodeKind::SOURCE_MAP,
                "the source map names another document",
            ));
        }
        let body = EnvelopeBody {
            code: self.code.clone(),
            dialect: self.dialect.clone(),
            document_id,
            document: staging.encode_value(NodeKind::DOCUMENT, &document)?,
            source_map: self
                .source_map
                .as_ref()
                .map(|map| staging.encode_value(NodeKind::SOURCE_MAP, map))
                .transpose()?,
            written: staging.encode_value(NodeKind::WRITTEN_TABLE, &self.written)?,
            function_values: self.function_values.clone(),
            effects: staging.encode_value(
                NodeKind::EFFECT_TABLE,
                &EffectTable {
                    effects: self.effects.clone(),
                    grants: self.grants.clone(),
                    unavailable_notes: self.unavailable_notes.clone(),
                    projected: self.projected.clone(),
                    reads: self.reads.clone(),
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
        let source_map: Option<lash_kernel_doc::SourceMap> = body
            .source_map
            .map(|map| decoder.decode(map, NodeKind::SOURCE_MAP))
            .transpose()?;
        if source_map
            .as_ref()
            .is_some_and(|map| map.document != body.document_id)
        {
            return Err(graph_invalid(
                NodeKind::SOURCE_MAP,
                "the source map names another document",
            ));
        }
        let written: BTreeMap<Site, Written> =
            decoder.decode(body.written, NodeKind::WRITTEN_TABLE)?;
        let table: EffectTable = decoder.decode(body.effects, NodeKind::EFFECT_TABLE)?;
        let envelope = Self {
            code: body.code,
            dialect: body.dialect,
            document: document
                .to_json()
                .map_err(|error| graph_invalid(NodeKind::DOCUMENT, error))?,
            source_map,
            written,
            function_values: body.function_values,
            effects: table.effects,
            grants: table.grants,
            unavailable_notes: table.unavailable_notes,
            projected: table.projected,
            reads: table.reads,
        };
        Ok((envelope, body.release))
    }
}
