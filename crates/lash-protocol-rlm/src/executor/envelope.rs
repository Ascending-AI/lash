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

use lash_core_store::tool_surface::ExecutionEnvironment;
use lash_execution_contracts::process::linking::LinkInspection;
use lash_kernel_dialect::Written;
use lash_kernel_doc::{Datum, IdentifiedDocument};
use lash_vocabulary::package::LinkId;
use lash_vocabulary::sites::Site;

use super::host::{CellHostLedgers, HostGraph, LedgerCall, LedgerCallState};
use super::host_graph::{CallStatus, CellEnd, HostState};

/// version_surface = "coexist"
/// version_guard(items(LASH_CODEMODE_CELL_SEGMENT_CODE_DOMAIN_VERSION, code_digest))
const LASH_CODEMODE_CELL_SEGMENT_CODE_DOMAIN_VERSION: &str = "lash-codemode-cell-segment-code/v1";

/// What a cell is, recorded when it was lowered and the same at every park.
#[derive(Clone)]
pub struct CellEnvelope {
    /// The digest of the cell's source: a parked run under the cell's
    /// execution holds this source's state.
    pub code: String,
    /// The dialect the source was lowered from.
    pub dialect: String,
    /// The kernel document the cell runs, decoded once where it was lowered
    /// or read back, with its identity and token body.
    pub document: IdentifiedDocument,
    pub link: LinkId,
    pub environment: ExecutionEnvironment,
    pub inspection: LinkInspection,
    /// Where each statement of the document came from in the source, when
    /// the dialect gave a map: what a failure locates its tasks by, at
    /// whichever activation the cell ends. Nothing a run or a save needs is
    /// in it.
    pub source_map: Option<lash_kernel_doc::SourceMap>,
    /// What the source wrote of each function the document makes, by the
    /// site of its closure: what the cell saves a function it leaves with
    /// ([`lash_kernel_dialect::Lowered::written`]).
    pub written: BTreeMap<Site, Written>,
    pub saved_origins: BTreeMap<lash_kernel_doc::heap::EntityId, lash_vocabulary::SignatureOrigin>,
    /// What the dialect's function values are, where its sessions keep the
    /// functions a cell binds ([`lash_kernel_dialect::Dialect::function_values`]).
    pub function_values: Option<lash_kernel_doc::FunctionRepresentation>,
    /// The read-only host bindings the cell started with: they are the
    /// host's, and are not the session's when the cell ends.
    pub projected: BTreeSet<String>,
}

impl CellEnvelope {
    pub(super) fn code_digest(code: &str) -> String {
        lash_protocol_types::core_support::blake3_domain_hash_hex(
            LASH_CODEMODE_CELL_SEGMENT_CODE_DOMAIN_VERSION,
            code.as_bytes(),
        )
    }
}

/// A cell as its execution root holds it: the envelope it runs on, its
/// host's records, prints and started processes, and the host graph its
/// next park builds on.
pub(super) struct ResumedCell {
    /// The cell's root this was read from.
    pub root: NodeId,
    pub cell: CellEnvelope,
    pub prints: Vec<Datum>,
    pub ledgers: CellHostLedgers,
    pub started_process_ids: Vec<lash_vocabulary::identity::ProcessId>,
    pub graph: HostGraph,
    /// The effect identity each open call was performed at.
    pub open_sites: BTreeMap<lash_core::ToolCallId, lash_vocabulary::sites::EffectId>,
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
                descriptor: descriptor.bytes.into(),
            };
            (end.host, end.envelope, Some(session))
        }
    };
    let cell = CellEnvelope::load(&mut decoder, envelope).map_err(text)?;
    let state = HostState::load(&mut decoder, host).map_err(text)?;
    let projections = super::host::DataProjections;
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
        if !cell.environment.uses.contains_key(&summary.operation) {
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
            control: state.control_attempt == Some(summary.ordinal),
            operation: summary.operation,
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
        root,
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
    let Some(read) = snapshots
        .latest_root()
        .await
        .map_err(|refusal| refusal.to_string())?
    else {
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
    if loaded.cell.link != loaded.cell.inspection.link
        || !loaded.cell.environment.roots.contains(&loaded.cell.link)
    {
        return Err("cell envelope Link and environment disagree".into());
    }
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

use lash_core_store::checkpoint_graph::GraphDecoder;
use lash_core_store::checkpoint_graph::GraphRefusal;
use lash_core_store::checkpoint_graph::GraphStaging;
use lash_core_store::checkpoint_graph::NodeId;
use lash_core_store::checkpoint_graph::NodeKind;
use lash_core_store::checkpoint_graph::TypedBody;
use lash_vm_broker::round_graph::{decode_opt_tree, decode_tree, encode_opt_tree, encode_tree};
use lash_vocabulary::tokens::TokenCodec;
use lash_vocabulary::tokens::TokenError;
use lash_vocabulary::tokens::TokenReader;
use lash_vocabulary::tokens::TokenWriter;

fn graph_invalid(kind: NodeKind, reason: impl std::fmt::Display) -> GraphRefusal {
    GraphRefusal::InvalidBody {
        kind: kind.0,
        reason: reason.to_string(),
    }
}

/// Kind 12: immutable per-use routes and selected reach, outside mutable resolution rows.
#[derive(Clone, Debug, PartialEq)]
pub struct EffectTable {
    pub environment: ExecutionEnvironment,
    pub inspection: LinkInspection,
    pub projected: BTreeSet<String>,
}
impl TokenCodec for EffectTable {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(3);
        encode_tree(&self.environment, out);
        encode_tree(&self.inspection, out);
        encode_tree(&self.projected, out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(3)?;
        Ok(Self {
            environment: decode_tree(input)?,
            inspection: decode_tree(input)?,
            projected: decode_tree(input)?,
        })
    }
}

/// Kind 15: what a cell is, staged once when it is lowered and named by
/// every park's root: no park copies it. It retains the Document and its
/// immutable Link alongside its source and execution metadata.
struct EnvelopeBody {
    code: String,
    dialect: String,
    document_id: lash_vocabulary::names::DocumentId,
    document: NodeId,
    link: LinkId,
    source_map: Option<NodeId>,
    written: NodeId,
    saved_origins: BTreeMap<lash_kernel_doc::heap::EntityId, lash_vocabulary::SignatureOrigin>,
    function_values: Option<lash_kernel_doc::FunctionRepresentation>,
    effects: NodeId,
}

impl TokenCodec for EnvelopeBody {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(10);
        out.text(&self.code);
        out.text(&self.dialect);
        self.document_id.encode(out);
        self.document.encode(out);
        self.link.encode(out);
        self.source_map.encode(out);
        self.written.encode(out);
        self.saved_origins.encode(out);
        encode_opt_tree(self.function_values.as_ref(), out);
        self.effects.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(10)?;
        Ok(Self {
            code: input.text()?.to_owned(),
            dialect: input.text()?.to_owned(),
            document_id: lash_vocabulary::names::DocumentId::decode(input)?,
            document: NodeId::decode(input)?,
            link: LinkId::decode(input)?,
            source_map: Option::decode(input)?,
            written: NodeId::decode(input)?,
            saved_origins: BTreeMap::decode(input)?,
            function_values: decode_opt_tree(input)?,
            effects: NodeId::decode(input)?,
        })
    }
}

impl CellEnvelope {
    /// Stages the cell's envelope, its document, source map and written
    /// table as typed nodes, and returns the envelope's node.
    ///
    /// # Errors
    ///
    /// [`GraphRefusal::InvalidBody`] for a map of another document, or any
    /// refusal of the staging area.
    pub fn stage(&self, staging: &mut GraphStaging) -> Result<NodeId, GraphRefusal> {
        // The document's identity is the digest of the logical body its
        // node is encoded from (`K-ID-001`): the envelope holds both.
        let logical = TypedBody::logical(self.document.body());
        let document_id = self.document.id();
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
            document: staging.encode(NodeKind::DOCUMENT, logical)?,
            link: self.link,
            source_map: self
                .source_map
                .as_ref()
                .map(|map| staging.encode_value(NodeKind::SOURCE_MAP, map))
                .transpose()?,
            written: staging.encode_value(NodeKind::WRITTEN_TABLE, &self.written)?,
            saved_origins: self.saved_origins.clone(),
            function_values: self.function_values.clone(),
            effects: staging.encode_value(
                NodeKind::EFFECT_TABLE,
                &EffectTable {
                    environment: self.environment.clone(),
                    inspection: self.inspection.clone(),
                    projected: self.projected.clone(),
                },
            )?,
        };
        Ok(staging.encode_value(NodeKind::CELL_ENVELOPE, &body)?)
    }

    /// Reads envelope `id` back, checking every child's kind and that the document is the one its
    /// semantic identity names.
    ///
    /// # Errors
    ///
    /// Any refusal of a node read, [`GraphRefusal::WrongChildKind`], or
    /// [`GraphRefusal::InvalidBody`] for a document of another identity.
    pub fn load(decoder: &mut GraphDecoder<'_>, id: NodeId) -> Result<Self, GraphRefusal> {
        let body: EnvelopeBody = decoder.decode(id, NodeKind::CELL_ENVELOPE)?;
        let logical = decoder.logical_body(body.document, NodeKind::DOCUMENT)?;
        let document = IdentifiedDocument::decode(&logical, decoder.limits().kernel_data).map_err(
            |error| {
                GraphRefusal::from_token(error, Some(body.document), Some(NodeKind::DOCUMENT.0))
            },
        )?;
        if document.id() != body.document_id {
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
            document,
            link: body.link,
            source_map,
            written,
            saved_origins: body.saved_origins,
            function_values: body.function_values,
            environment: table.environment,
            inspection: table.inspection,
            projected: table.projected,
        };
        Ok(envelope)
    }
}
