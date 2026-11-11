//! A finished process's history in one self-contained archive on its own
//! row (FIG-7701, P2): its run records, its tool-call rows, its model-call
//! records and every graph node they reach, with their edges.
//!
//! The archive holds exact stored values: each row's columns as the store
//! wrote them, and each node's content address, encoding and bytes. It
//! never names a physical blob id, and nothing outside it is referenced, so
//! a peel of another owner's copy cannot break it. Rows keep the order the
//! store's own `ORDER BY` gave them when it packed, so a read answers in
//! the order it did before, whatever the database's collation.
//!
//! Layout (little-endian): a 20-byte header (magic, then the fact, call,
//! model-call and node counts); one `u32` offset per row plus an end offset
//! for each row section; a fixed-width node table sorted by content address
//! (address, bytes offset, length, edge offset, edge count, encoding); then
//! the row bytes, node bytes and edges (`u32` node indexes). A point read
//! decodes the header, binary-searches the tables and decodes only the rows
//! and nodes it selects: never the whole archive. Each store answers its
//! process's reads from it through the same row decoders it uses for rows,
//! and the selection rules here mirror the statements they replace.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_core_store::checkpoint_graph::NodeId;

/// The largest archive a process's history packs into, in exact bytes.
///
/// Measured (FIG-7701, `r59/evidence/pack-rows-progress.md` and
/// `r59/evidence/cenote-case-bytes.md`): a real-shaped run (one model call,
/// two host-tool reads, observers after the end) encodes to a median of
/// 34.5 KB (33,970 to 34,710 B over 100 runs), and a no-op run to 1.7 KB.
/// Cenote's finished demo cases retain 40.6 to 51.0 KB of history rows by
/// `pg_column_size`, which may be a compressed size; a longer document or a
/// retry crosses 64 KiB, so the cap leaves room for those. A larger or
/// unsupported history keeps its rows untouched: there is no second,
/// chunked form.
pub const PROCESS_ARCHIVE_CAP_BYTES: usize = 256 * 1024;

const MAGIC: &[u8; 4] = b"LPA1";
const HEADER: usize = 20;
const NODE_ENTRY: usize = 48;
const NONE_NODE: u32 = u32::MAX;

/// One `run_records` row of the process, its owner implied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchivedFact {
    pub run_seq: i64,
    pub ordinal: i64,
    pub kind: String,
    pub call_id: Option<String>,
    pub record: NodeId,
    pub written_epoch: i64,
}

/// One `tool_calls` row of the process, every column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchivedCall {
    pub owner_key: String,
    pub call_id: String,
    pub execution_owner: String,
    pub run_seq: i64,
    pub member: i64,
    pub admission_seq: i64,
    pub counted: bool,
    pub admitted_count: i64,
    pub cancellable: bool,
    pub session_id: Option<String>,
    pub turn_id: Option<String>,
    pub request_id: Option<String>,
    pub origin: Option<String>,
    pub reason: Option<String>,
    pub decision: Option<String>,
    pub decision_ref: Option<NodeId>,
    pub answer_outcome: Option<String>,
    pub body_ref: Option<NodeId>,
}

/// One `model_call_records` row of the process's session, every column but
/// the session, which is the process's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchivedModelCall {
    pub run: String,
    pub call: String,
    pub call_id: String,
    pub attempt: Option<i64>,
    pub answered: bool,
    pub enclosing_owner: Option<String>,
    pub enclosing_run: Option<i64>,
    pub enclosing_call: Option<String>,
    pub record_ref: Option<NodeId>,
    pub ended_at_ms: Option<i64>,
    pub written_epoch: Option<i64>,
}

/// One stored blob: its address, its stored encoding, its exact bytes and
/// the children its stored edges name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchivedNode {
    pub id: NodeId,
    pub encoding: u8,
    pub bytes: Vec<u8>,
    pub children: Vec<NodeId>,
}

/// Everything a process's history archive holds, rows in stored order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ArchiveContents {
    pub facts: Vec<ArchivedFact>,
    pub calls: Vec<ArchivedCall>,
    pub models: Vec<ArchivedModelCall>,
    pub nodes: Vec<ArchivedNode>,
}

/// Why a history does not pack: it stays as rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unpackable {
    /// The archive would exceed [`PROCESS_ARCHIVE_CAP_BYTES`].
    Oversize { bytes: usize },
    /// A row names a node the closure lacks, a node or row does not fit the
    /// format, or a value the format cannot carry.
    Unsupported(&'static str),
}

/// A stored archive that does not decode.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("process history archive is corrupt: {0}")]
pub struct ArchiveCorrupt(pub &'static str);

type Decoded<T> = Result<T, ArchiveCorrupt>;

impl ArchiveContents {
    /// The archive's exact bytes, or why this history stays as rows. The
    /// cap is checked on the exact encoded size, before any caller mutates.
    ///
    /// # Errors
    ///
    /// [`Unpackable`] for an oversize or unsupported history.
    pub fn encode(&self) -> Result<Vec<u8>, Unpackable> {
        let mut order: Vec<&ArchivedNode> = self.nodes.iter().collect();
        order.sort_by(|a, b| a.id.0.cmp(&b.id.0));
        if order.windows(2).any(|pair| pair[0].id == pair[1].id) {
            return Err(Unpackable::Unsupported("a node appears twice"));
        }
        let index: BTreeMap<NodeId, u32> = order
            .iter()
            .enumerate()
            .map(|(at, node)| Ok((node.id, u32::try_from(at).map_err(|_| too_large())?)))
            .collect::<Result<_, Unpackable>>()?;
        let node_ref = |id: &NodeId| {
            index.get(id).copied().ok_or(Unpackable::Unsupported(
                "a row names a node outside the closure",
            ))
        };
        let mut rows = Vec::new();
        let mut offsets = [Vec::new(), Vec::new(), Vec::new()];
        for fact in &self.facts {
            offsets[0].push(rows.len());
            put_i64(&mut rows, fact.run_seq);
            put_i64(&mut rows, fact.ordinal);
            put_str(&mut rows, &fact.kind)?;
            put_opt_str(&mut rows, fact.call_id.as_deref())?;
            put_u32(&mut rows, node_ref(&fact.record)?);
            put_i64(&mut rows, fact.written_epoch);
        }
        offsets[0].push(rows.len());
        for call in &self.calls {
            offsets[1].push(rows.len());
            put_i64(&mut rows, call.admission_seq);
            put_str(&mut rows, &call.call_id)?;
            put_str(&mut rows, &call.owner_key)?;
            put_str(&mut rows, &call.execution_owner)?;
            put_i64(&mut rows, call.run_seq);
            put_i64(&mut rows, call.member);
            rows.push(u8::from(call.counted));
            put_i64(&mut rows, call.admitted_count);
            rows.push(u8::from(call.cancellable));
            for text in [
                &call.session_id,
                &call.turn_id,
                &call.request_id,
                &call.origin,
                &call.reason,
                &call.decision,
            ] {
                put_opt_str(&mut rows, text.as_deref())?;
            }
            put_u32(&mut rows, opt_ref(call.decision_ref.as_ref(), &node_ref)?);
            put_opt_str(&mut rows, call.answer_outcome.as_deref())?;
            put_u32(&mut rows, opt_ref(call.body_ref.as_ref(), &node_ref)?);
        }
        offsets[1].push(rows.len());
        for model in &self.models {
            offsets[2].push(rows.len());
            put_str(&mut rows, &model.run)?;
            put_str(&mut rows, &model.call)?;
            put_str(&mut rows, &model.call_id)?;
            put_opt_i64(&mut rows, model.attempt);
            rows.push(u8::from(model.answered));
            put_opt_str(&mut rows, model.enclosing_owner.as_deref())?;
            put_opt_i64(&mut rows, model.enclosing_run);
            put_opt_str(&mut rows, model.enclosing_call.as_deref())?;
            put_u32(&mut rows, opt_ref(model.record_ref.as_ref(), &node_ref)?);
            put_opt_i64(&mut rows, model.ended_at_ms);
            put_opt_i64(&mut rows, model.written_epoch);
        }
        offsets[2].push(rows.len());

        let row_tables = (self.facts.len() + self.calls.len() + self.models.len() + 3) * 4;
        let node_table = order.len() * NODE_ENTRY;
        let rows_at = HEADER + row_tables + node_table;
        let bytes_at = rows_at + rows.len();
        let content: usize = order.iter().map(|node| node.bytes.len()).sum();
        let edges_at = bytes_at + content;
        let edges: usize = order.iter().map(|node| node.children.len() * 4).sum();
        let total = edges_at + edges;
        debug_assert_eq!(total, self.size_with(order.len(), content, edges / 4));
        if total > PROCESS_ARCHIVE_CAP_BYTES {
            return Err(Unpackable::Oversize { bytes: total });
        }
        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(MAGIC);
        for count in [
            self.facts.len(),
            self.calls.len(),
            self.models.len(),
            order.len(),
        ] {
            put_u32(&mut out, u32::try_from(count).map_err(|_| too_large())?);
        }
        for table in &offsets {
            for offset in table {
                put_u32(&mut out, absolute(rows_at + offset)?);
            }
        }
        let (mut content_at, mut edge_at) = (bytes_at, edges_at);
        for node in &order {
            out.extend_from_slice(&node.id.0);
            put_u32(&mut out, absolute(content_at)?);
            put_u32(&mut out, absolute(node.bytes.len())?);
            put_u32(&mut out, absolute(edge_at)?);
            let count = u16::try_from(node.children.len()).map_err(|_| {
                Unpackable::Unsupported("a node has more children than the format holds")
            })?;
            out.extend_from_slice(&count.to_le_bytes());
            out.push(node.encoding);
            out.push(0);
            content_at += node.bytes.len();
            edge_at += node.children.len() * 4;
        }
        out.extend_from_slice(&rows);
        for node in &order {
            out.extend_from_slice(&node.bytes);
        }
        for node in &order {
            for child in &node.children {
                put_u32(&mut out, node_ref(child)?);
            }
        }
        debug_assert_eq!(out.len(), total);
        Ok(out)
    }
}

/// A read view of one stored archive. Opening it checks the header and the
/// table bounds; each read decodes only what it selects, and counts it.
pub struct ProcessArchive<'a> {
    bytes: &'a [u8],
    counts: [usize; 4],
    tables: [usize; 3],
    nodes_at: usize,
    decoded: AtomicUsize,
}

/// One node read from an archive.
#[derive(Clone, Copy, Debug)]
pub struct ArchiveNode<'a> {
    pub id: NodeId,
    pub encoding: u8,
    pub bytes: &'a [u8],
    index: usize,
}

impl<'a> ProcessArchive<'a> {
    /// Open `bytes`.
    ///
    /// # Errors
    ///
    /// [`ArchiveCorrupt`] when the header or the tables do not fit.
    pub fn open(bytes: &'a [u8]) -> Decoded<Self> {
        if bytes.len() < HEADER || &bytes[..4] != MAGIC {
            return Err(ArchiveCorrupt("header"));
        }
        let count = |at: usize| u32_at(bytes, at).map(|value| value as usize);
        let counts = [count(4)?, count(8)?, count(12)?, count(16)?];
        let mut at = HEADER;
        let mut tables = [0; 3];
        for (section, table) in tables.iter_mut().enumerate() {
            *table = at;
            at = counts[section]
                .checked_add(1)
                .and_then(|entries| entries.checked_mul(4))
                .and_then(|size| at.checked_add(size))
                .ok_or(ArchiveCorrupt("row tables"))?;
        }
        let nodes_at = at;
        let end = counts[3]
            .checked_mul(NODE_ENTRY)
            .and_then(|size| nodes_at.checked_add(size))
            .ok_or(ArchiveCorrupt("node table"))?;
        if end > bytes.len() {
            return Err(ArchiveCorrupt("tables overrun the archive"));
        }
        Ok(Self {
            bytes,
            counts,
            tables,
            nodes_at,
            decoded: AtomicUsize::new(HEADER),
        })
    }

    /// How many bytes the reads so far decoded, the header included.
    #[must_use]
    pub fn decoded_bytes(&self) -> usize {
        self.decoded.load(Ordering::Relaxed)
    }

    /// The archive's exact size.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Whether the archive is empty (never, for one that opened).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    fn charge(&self, bytes: usize) {
        self.decoded.fetch_add(bytes, Ordering::Relaxed);
    }

    fn row(&self, section: usize, at: usize) -> Decoded<Reader<'a>> {
        let table = self.tables[section] + at * 4;
        let start = u32_at(self.bytes, table)? as usize;
        let end = u32_at(self.bytes, table + 4)? as usize;
        self.charge(8);
        if start > end || end > self.bytes.len() {
            return Err(ArchiveCorrupt("row bounds"));
        }
        self.charge(end - start);
        Ok(Reader {
            bytes: &self.bytes[start..end],
        })
    }

    fn fact_at(&self, at: usize) -> Decoded<ArchivedFact> {
        let mut row = self.row(0, at)?;
        let fact = ArchivedFact {
            run_seq: row.i64()?,
            ordinal: row.i64()?,
            kind: row.string()?,
            call_id: row.opt_string()?,
            record: self.node_id(row.u32()?)?,
            written_epoch: row.i64()?,
        };
        row.end()?;
        Ok(fact)
    }

    /// The `(run, ordinal)` key of fact `at`, read without its other columns.
    fn fact_key(&self, at: usize) -> Decoded<(i64, i64)> {
        let mut row = self.row(0, at)?;
        Ok((row.i64()?, row.i64()?))
    }

    /// The facts of run `run` (every run when `None`), in stored order: by
    /// run and ordinal. At most `limit` of them.
    ///
    /// # Errors
    ///
    /// [`ArchiveCorrupt`] for a row that does not decode.
    pub fn facts(&self, run: Option<i64>, limit: Option<usize>) -> Decoded<Vec<ArchivedFact>> {
        let (from, to) = match run {
            None => (0, self.counts[0]),
            Some(run) => (
                self.partition(self.counts[0], |at| Ok(self.fact_key(at)?.0 < run))?,
                self.partition(self.counts[0], |at| Ok(self.fact_key(at)?.0 <= run))?,
            ),
        };
        let to = limit.map_or(to, |limit| to.min(from.saturating_add(limit)));
        (from..to).map(|at| self.fact_at(at)).collect()
    }

    /// Fact `ordinal` of run `run`.
    ///
    /// # Errors
    ///
    /// [`ArchiveCorrupt`] for a row that does not decode.
    pub fn fact(&self, run: i64, ordinal: i64) -> Decoded<Option<ArchivedFact>> {
        let at = self.partition(self.counts[0], |at| Ok(self.fact_key(at)? < (run, ordinal)))?;
        if at < self.counts[0] && self.fact_key(at)? == (run, ordinal) {
            return self.fact_at(at).map(Some);
        }
        Ok(None)
    }

    fn call_at(&self, at: usize) -> Decoded<ArchivedCall> {
        let mut row = self.row(1, at)?;
        let admission_seq = row.i64()?;
        let call_id = row.string()?;
        let owner_key = row.string()?;
        let execution_owner = row.string()?;
        let run_seq = row.i64()?;
        let member = row.i64()?;
        let counted = row.bool()?;
        let admitted_count = row.i64()?;
        let cancellable = row.bool()?;
        let session_id = row.opt_string()?;
        let turn_id = row.opt_string()?;
        let request_id = row.opt_string()?;
        let origin = row.opt_string()?;
        let reason = row.opt_string()?;
        let decision = row.opt_string()?;
        let decision_ref = self.opt_node_id(row.u32()?)?;
        let answer_outcome = row.opt_string()?;
        let body_ref = self.opt_node_id(row.u32()?)?;
        row.end()?;
        Ok(ArchivedCall {
            owner_key,
            call_id,
            execution_owner,
            run_seq,
            member,
            admission_seq,
            counted,
            admitted_count,
            cancellable,
            session_id,
            turn_id,
            request_id,
            origin,
            reason,
            decision,
            decision_ref,
            answer_outcome,
            body_ref,
        })
    }

    /// The `(admission sequence, call id)` key of call `at`.
    fn call_key(&self, at: usize) -> Decoded<(i64, String)> {
        let mut row = self.row(1, at)?;
        Ok((row.i64()?, row.string()?))
    }

    /// The calls whose ids are in `calls`, in stored order (by admission
    /// sequence, then call id).
    ///
    /// # Errors
    ///
    /// [`ArchiveCorrupt`] for a row that does not decode.
    pub fn calls_named(
        &self,
        owner_key: &str,
        calls: &BTreeSet<&str>,
    ) -> Decoded<Vec<ArchivedCall>> {
        let mut found = Vec::new();
        for at in 0..self.counts[1] {
            if calls.contains(self.call_key(at)?.1.as_str()) {
                let call = self.call_at(at)?;
                if call.owner_key == owner_key {
                    found.push(call);
                }
            }
        }
        Ok(found)
    }

    /// One page of calls after `(sequence, call)` in stored order, at most
    /// `limit`; with `saved_reads`, only uncancellable calls answered
    /// `"Read"`.
    ///
    /// # Errors
    ///
    /// [`ArchiveCorrupt`] for a row that does not decode.
    pub fn call_page(
        &self,
        owner_key: &str,
        after: (i64, &str),
        limit: usize,
        saved_reads: bool,
    ) -> Decoded<Vec<ArchivedCall>> {
        let from = self.partition(self.counts[1], |at| {
            let (sequence, call) = self.call_key(at)?;
            Ok((sequence, call.as_str()) <= after)
        })?;
        let mut page = Vec::new();
        for at in from..self.counts[1] {
            if page.len() >= limit {
                break;
            }
            let call = self.call_at(at)?;
            if call.owner_key != owner_key {
                continue;
            }
            if saved_reads
                && (call.cancellable || call.answer_outcome.as_deref() != Some("\"Read\""))
            {
                continue;
            }
            page.push(call);
        }
        Ok(page)
    }

    /// The call `call` of execution `execution`.
    ///
    /// # Errors
    ///
    /// [`ArchiveCorrupt`] for a row that does not decode.
    pub fn call_of(&self, execution: &str, call: &str) -> Decoded<Option<ArchivedCall>> {
        for at in 0..self.counts[1] {
            if self.call_key(at)?.1 == call {
                let found = self.call_at(at)?;
                if found.execution_owner == execution {
                    return Ok(Some(found));
                }
            }
        }
        Ok(None)
    }

    fn model_at(&self, at: usize) -> Decoded<ArchivedModelCall> {
        let mut row = self.row(2, at)?;
        let model = ArchivedModelCall {
            run: row.string()?,
            call: row.string()?,
            call_id: row.string()?,
            attempt: row.opt_i64()?,
            answered: row.bool()?,
            enclosing_owner: row.opt_string()?,
            enclosing_run: row.opt_i64()?,
            enclosing_call: row.opt_string()?,
            record_ref: self.opt_node_id(row.u32()?)?,
            ended_at_ms: row.opt_i64()?,
            written_epoch: row.opt_i64()?,
        };
        row.end()?;
        Ok(model)
    }

    /// The model-call rows `keep` selects, in stored order (by run, then
    /// call).
    ///
    /// # Errors
    ///
    /// [`ArchiveCorrupt`] for a row that does not decode.
    pub fn models(
        &self,
        mut keep: impl FnMut(&ArchivedModelCall) -> bool,
    ) -> Decoded<Vec<ArchivedModelCall>> {
        let mut found = Vec::new();
        for at in 0..self.counts[2] {
            let model = self.model_at(at)?;
            if keep(&model) {
                found.push(model);
            }
        }
        Ok(found)
    }

    fn entry(&self, index: usize) -> Decoded<ArchiveNode<'a>> {
        let at = self.nodes_at + index * NODE_ENTRY;
        let id = NodeId(
            self.bytes[at..at + 32]
                .try_into()
                .map_err(|_| ArchiveCorrupt("node address"))?,
        );
        let start = u32_at(self.bytes, at + 32)? as usize;
        let len = u32_at(self.bytes, at + 36)? as usize;
        let encoding = self.bytes[at + 46];
        let end = start
            .checked_add(len)
            .ok_or(ArchiveCorrupt("node bounds"))?;
        if end > self.bytes.len() {
            return Err(ArchiveCorrupt("node bounds"));
        }
        self.charge(NODE_ENTRY);
        Ok(ArchiveNode {
            id,
            encoding,
            bytes: &self.bytes[start..end],
            index,
        })
    }

    fn node_id(&self, index: u32) -> Decoded<NodeId> {
        let index = index as usize;
        if index >= self.counts[3] {
            return Err(ArchiveCorrupt("node index"));
        }
        let at = self.nodes_at + index * NODE_ENTRY;
        self.charge(32);
        Ok(NodeId(
            self.bytes[at..at + 32]
                .try_into()
                .map_err(|_| ArchiveCorrupt("node address"))?,
        ))
    }

    fn opt_node_id(&self, index: u32) -> Decoded<Option<NodeId>> {
        (index != NONE_NODE)
            .then(|| self.node_id(index))
            .transpose()
    }

    /// The node at address `id`.
    ///
    /// # Errors
    ///
    /// [`ArchiveCorrupt`] for a table entry that does not decode.
    pub fn node(&self, id: &NodeId) -> Decoded<Option<ArchiveNode<'a>>> {
        let address = |index: usize| {
            let at = self.nodes_at + index * NODE_ENTRY;
            self.charge(32);
            &self.bytes[at..at + 32]
        };
        let at = self.partition(self.counts[3], |index| Ok(address(index) < &id.0[..]))?;
        if at < self.counts[3] && address(at) == &id.0[..] {
            return self.entry(at).map(Some);
        }
        Ok(None)
    }

    fn children(&self, node: &ArchiveNode<'_>) -> Decoded<Vec<ArchiveNode<'a>>> {
        let at = self.nodes_at + node.index * NODE_ENTRY;
        let start = u32_at(self.bytes, at + 40)? as usize;
        let count = usize::from(u16::from_le_bytes([
            self.bytes[at + 44],
            self.bytes[at + 45],
        ]));
        (0..count)
            .map(|edge| {
                let index = u32_at(self.bytes, start + edge * 4)? as usize;
                self.charge(4);
                if index >= self.counts[3] {
                    return Err(ArchiveCorrupt("edge target"));
                }
                self.entry(index)
            })
            .collect()
    }

    /// Every node `roots` reach along stored edges, roots included, with
    /// its stored encoding and bytes: what the store's reach statement
    /// answers. Unless `whole`, nothing under a persistent-heap page
    /// (envelope kind 1000 to 1999) is reached: the page is a leaf. A root
    /// the archive lacks is not answered; the walk reports it missing.
    ///
    /// # Errors
    ///
    /// [`ArchiveCorrupt`] for a table entry or edge that does not decode.
    pub fn reach(&self, roots: &[NodeId], whole: bool) -> Decoded<Vec<ArchiveNode<'a>>> {
        let mut seen = BTreeSet::new();
        let mut level = Vec::new();
        for root in roots {
            if let Some(node) = self.node(root)?
                && seen.insert(node.index)
            {
                level.push(node);
            }
        }
        let mut reached = Vec::new();
        while !level.is_empty() {
            let mut next = Vec::new();
            for node in level {
                if whole || !is_heap_page(node.encoding, node.bytes) {
                    for child in self.children(&node)? {
                        if seen.insert(child.index) {
                            next.push(child);
                        }
                    }
                }
                self.charge(node.bytes.len());
                reached.push(node);
            }
            level = next;
        }
        Ok(reached)
    }

    /// Every row and node, decoded: what a pack wrote.
    ///
    /// # Errors
    ///
    /// [`ArchiveCorrupt`] for anything that does not decode.
    pub fn contents(&self) -> Decoded<ArchiveContents> {
        let mut contents = ArchiveContents {
            facts: self.facts(None, None)?,
            calls: (0..self.counts[1])
                .map(|at| self.call_at(at))
                .collect::<Decoded<_>>()?,
            models: self.models(|_| true)?,
            nodes: Vec::with_capacity(self.counts[3]),
        };
        for index in 0..self.counts[3] {
            let node = self.entry(index)?;
            contents.nodes.push(ArchivedNode {
                id: node.id,
                encoding: node.encoding,
                bytes: node.bytes.to_vec(),
                children: self.children(&node)?.iter().map(|child| child.id).collect(),
            });
        }
        Ok(contents)
    }

    /// The first index in `0..len` for which `before` is false; `before`
    /// holds for a prefix.
    fn partition(
        &self,
        len: usize,
        mut before: impl FnMut(usize) -> Decoded<bool>,
    ) -> Decoded<usize> {
        let (mut low, mut high) = (0, len);
        while low < high {
            let mid = low + (high - low) / 2;
            if before(mid)? {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        Ok(low)
    }
}

/// Whether a stored blob is a persistent-heap page, exactly as the reach
/// statements test it: a graph envelope whose kind (a two-byte varint after
/// the four-byte magic) is 1000 to 1999.
#[must_use]
pub fn is_heap_page(encoding: u8, bytes: &[u8]) -> bool {
    if encoding != 1 || bytes.len() < 6 || bytes[4] < 128 || bytes[5] >= 128 {
        return false;
    }
    let kind = u32::from(bytes[4] - 128) + u32::from(bytes[5]) * 128;
    (1000..=1999).contains(&kind)
}

/// The model-call owner filter the stores' reads apply: `run` is `owner`,
/// or a turn owner's owned calls. `ascii_case_like` is SQLite's `LIKE`,
/// which ignores ASCII case.
#[must_use]
pub fn model_owner_matches(run: &str, owner: &str, ascii_case_like: bool) -> bool {
    if run == owner {
        return true;
    }
    let prefix = owner.get(..5);
    let turn = match prefix {
        Some(prefix) if ascii_case_like => prefix.eq_ignore_ascii_case("turn:"),
        Some(prefix) => prefix == "turn:",
        None => false,
    };
    turn && run.strip_prefix("owned:") == owner.get(5..)
}

struct Reader<'a> {
    bytes: &'a [u8],
}

impl Reader<'_> {
    fn take(&mut self, len: usize) -> Decoded<&[u8]> {
        if self.bytes.len() < len {
            return Err(ArchiveCorrupt("row ends early"));
        }
        let (head, rest) = self.bytes.split_at(len);
        self.bytes = rest;
        Ok(head)
    }
    fn u32(&mut self) -> Decoded<u32> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }
    fn i64(&mut self) -> Decoded<i64> {
        let bytes = self.take(8)?;
        let mut word = [0; 8];
        word.copy_from_slice(bytes);
        Ok(i64::from_le_bytes(word))
    }
    fn bool(&mut self) -> Decoded<bool> {
        match self.take(1)?[0] {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(ArchiveCorrupt("boolean")),
        }
    }
    fn present(&mut self) -> Decoded<bool> {
        self.bool()
    }
    fn string(&mut self) -> Decoded<String> {
        let len = self.u32()? as usize;
        String::from_utf8(self.take(len)?.to_vec()).map_err(|_| ArchiveCorrupt("text"))
    }
    fn opt_string(&mut self) -> Decoded<Option<String>> {
        if self.present()? {
            self.string().map(Some)
        } else {
            Ok(None)
        }
    }
    fn opt_i64(&mut self) -> Decoded<Option<i64>> {
        if self.present()? {
            self.i64().map(Some)
        } else {
            Ok(None)
        }
    }
    fn end(&self) -> Decoded<()> {
        if self.bytes.is_empty() {
            Ok(())
        } else {
            Err(ArchiveCorrupt("row has trailing bytes"))
        }
    }
}

fn u32_at(bytes: &[u8], at: usize) -> Decoded<u32> {
    let word = bytes
        .get(at..at + 4)
        .ok_or(ArchiveCorrupt("offset past the end"))?;
    Ok(u32::from_le_bytes([word[0], word[1], word[2], word[3]]))
}

fn too_large() -> Unpackable {
    Unpackable::Unsupported("a count does not fit the format")
}

fn absolute(at: usize) -> Result<u32, Unpackable> {
    u32::try_from(at).map_err(|_| too_large())
}

fn opt_ref(
    id: Option<&NodeId>,
    node_ref: &impl Fn(&NodeId) -> Result<u32, Unpackable>,
) -> Result<u32, Unpackable> {
    id.map_or(Ok(NONE_NODE), node_ref)
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}
fn put_i64(out: &mut Vec<u8>, value: i64) {
    out.extend_from_slice(&value.to_le_bytes());
}
fn put_str(out: &mut Vec<u8>, text: &str) -> Result<(), Unpackable> {
    put_u32(out, absolute(text.len())?);
    out.extend_from_slice(text.as_bytes());
    Ok(())
}
fn put_opt_str(out: &mut Vec<u8>, text: Option<&str>) -> Result<(), Unpackable> {
    match text {
        None => out.push(0),
        Some(text) => {
            out.push(1);
            put_str(out, text)?;
        }
    }
    Ok(())
}
fn put_opt_i64(out: &mut Vec<u8>, value: Option<i64>) {
    match value {
        None => out.push(0),
        Some(value) => {
            out.push(1);
            put_i64(out, value);
        }
    }
}

/// The process whose archive answers a read of `owner`'s run facts.
#[must_use]
pub fn fact_process(owner: &super::OwnerKey) -> Option<&lash_vocabulary::identity::ProcessId> {
    match owner {
        super::OwnerKey::Process(process) => Some(process),
        _ => None,
    }
}

/// The process whose archive answers a read of `owner`'s tool calls.
#[must_use]
pub fn call_process(owner: &super::CallOwner) -> Option<&lash_vocabulary::identity::ProcessId> {
    match owner {
        super::CallOwner::Process(process) => Some(process),
        _ => None,
    }
}

/// The session a process's model calls are recorded under.
#[must_use]
pub fn model_session(
    process: &lash_vocabulary::identity::ProcessId,
) -> lash_kernel_value::identity::SessionId {
    lash_kernel_value::identity::SessionId::prefixed(MODEL_SESSION_PREFIX, process.as_str())
}

const MODEL_SESSION_PREFIX: &str = "process:";

/// The process id whose archive answers a read of `session`'s model calls:
/// its id as stored, unchecked (a lookup by an id no process has finds none).
#[must_use]
pub fn model_process(session: &lash_kernel_value::identity::SessionId) -> Option<&str> {
    session.as_str().strip_prefix(MODEL_SESSION_PREFIX)
}

impl From<ArchiveCorrupt> for crate::DurableError {
    fn from(error: ArchiveCorrupt) -> Self {
        crate::DurableError::Store(crate::StoreFailure {
            code: None,
            kind: crate::StoreFailureKind::Corrupt,
            message: error.to_string(),
        })
    }
}

/// The stores' corruption refusal for a stored value, word for word.
fn corrupt(what: &str, value: &str) -> crate::DurableError {
    crate::DurableError::Store(crate::StoreFailure {
        code: None,
        kind: crate::StoreFailureKind::Corrupt,
        message: format!("stored {what} `{value}` does not decode"),
    })
}

fn unsigned(value: i64) -> Result<u64, crate::DurableError> {
    u64::try_from(value).map_err(|error| corrupt("integer", &error.to_string()))
}

/// An archived fact as the run-fact read answers it: the same decoding and
/// refusals as a stored row.
///
/// # Errors
///
/// A corruption refusal for a value a stored row would refuse.
pub fn fact_row(
    owner: &super::OwnerKey,
    fact: ArchivedFact,
) -> Result<super::RunFactRow, crate::DurableError> {
    let kind = super::RunFactKind::parse(&fact.kind)
        .ok_or_else(|| corrupt("run record kind", &fact.kind))?;
    let call = fact
        .call_id
        .map(|call| {
            lash_protocol_types::ToolCallId::parse(&call)
                .map_err(|_| corrupt("tool call id", &call))
        })
        .transpose()?;
    Ok(super::RunFactRow {
        owner: owner.clone(),
        run: super::RunSeq(unsigned(fact.run_seq)?),
        ordinal: super::Ordinal(unsigned(fact.ordinal)?),
        kind,
        call,
        record: fact.record,
        written_epoch: crate::Epoch(fact.written_epoch),
    })
}

/// An archived call as the tool-call read answers it.
///
/// # Errors
///
/// A corruption refusal for a value a stored row would refuse.
pub fn call_row(
    owner: &super::CallOwner,
    call: ArchivedCall,
) -> Result<super::ToolCallRow, crate::DurableError> {
    use super::{ToolCallAnswer, ToolCallDecision};
    Ok(super::ToolCallRow {
        sequence: u64::try_from(call.admission_seq)
            .map_err(|_| corrupt("tool call order", "negative"))?,
        owner: owner.clone(),
        call: lash_protocol_types::ToolCallId::parse(&call.call_id)
            .map_err(|_| corrupt("tool call id", &call.call_id))?,
        execution: super::OwnerKey::parse(&call.execution_owner)
            .map_err(|_| corrupt("tool call execution", &call.execution_owner))?,
        run: super::RunSeq(
            u64::try_from(call.run_seq).map_err(|_| corrupt("tool call run", "negative"))?,
        ),
        member: u64::try_from(call.member).map_err(|_| corrupt("tool call member", "negative"))?,
        decision: match call.decision.as_deref() {
            None => None,
            Some("final") => Some(ToolCallDecision::Final),
            Some("cancel") => Some(ToolCallDecision::Cancel),
            Some(other) => return Err(corrupt("tool call decision", other)),
        },
        answer: call
            .answer_outcome
            .map(|outcome| {
                ToolCallAnswer::from_stored(&outcome, call.body_ref)
                    .ok_or_else(|| corrupt("tool call answer", &outcome))
            })
            .transpose()?,
    })
}

/// The archived calls `selection` reads, in the order the stored read
/// answers them.
///
/// # Errors
///
/// [`ArchiveCorrupt`] for a row that does not decode.
pub fn select_calls(
    archive: &ProcessArchive<'_>,
    owner: &super::CallOwner,
    selection: &super::ToolCallSelection,
) -> Result<Vec<ArchivedCall>, ArchiveCorrupt> {
    use super::ToolCallSelection;
    let key = owner.stored();
    match selection {
        ToolCallSelection::Calls(calls) => {
            let named = calls.iter().map(|call| call.as_str()).collect();
            archive.calls_named(&key, &named)
        }
        ToolCallSelection::List { after, limit }
        | ToolCallSelection::SavedReads { after, limit } => {
            let (sequence, call) = after.as_ref().map_or((-1, ""), |cursor| {
                (
                    i64::try_from(cursor.sequence).unwrap_or(i64::MAX),
                    cursor.call.as_str(),
                )
            });
            archive.call_page(
                &key,
                (sequence, call),
                *limit as usize,
                matches!(selection, ToolCallSelection::SavedReads { .. }),
            )
        }
    }
}

/// What a model-call read selects from an archive.
#[derive(Clone, Copy, Debug)]
pub enum ModelSelection<'a> {
    /// Sealed records, every one or those with the given ids.
    Records(Option<&'a [lash_protocol_types::llm::types::LlmCallId]>),
    /// Reservations, every one, those with the given ids, or the one at a
    /// call key (matched on its exact run).
    Pins {
        ids: Option<&'a [lash_protocol_types::llm::types::LlmCallId]>,
        call: Option<&'a str>,
    },
}

/// The archived model calls of `owner` that `selection` reads, in stored
/// order. `ascii_case_like` is the store's `LIKE`.
///
/// # Errors
///
/// [`ArchiveCorrupt`] for a row that does not decode.
pub fn select_models(
    archive: &ProcessArchive<'_>,
    owner: &str,
    selection: ModelSelection<'_>,
    ascii_case_like: bool,
) -> Result<Vec<ArchivedModelCall>, ArchiveCorrupt> {
    let named = |ids: Option<&[lash_protocol_types::llm::types::LlmCallId]>, id: &str| {
        ids.is_none_or(|ids| ids.iter().any(|named| named.0 == id))
    };
    archive.models(|model| match selection {
        ModelSelection::Records(ids) => {
            model.record_ref.is_some()
                && model_owner_matches(&model.run, owner, ascii_case_like)
                && named(ids, &model.call_id)
        }
        ModelSelection::Pins {
            call: Some(call), ..
        } => model.record_ref.is_none() && model.run == owner && model.call == call,
        ModelSelection::Pins { ids, call: None } => {
            model.record_ref.is_none()
                && model_owner_matches(&model.run, owner, ascii_case_like)
                && named(ids, &model.call_id)
        }
    })
}

/// An archived reservation as the pin read answers it.
///
/// # Errors
///
/// The refusal a stored reservation row would give.
pub fn pin_row(
    session: &lash_kernel_value::identity::SessionId,
    model: ArchivedModelCall,
) -> Result<super::ModelCallPinRow, crate::DurableError> {
    let attempt = model
        .attempt
        .ok_or_else(|| corrupt("model call reservation", "no attempt"))?;
    super::model_calls::pin_row(
        session,
        &model.run,
        model.call,
        model.call_id,
        attempt,
        model.answered,
        (
            model.enclosing_owner,
            model.enclosing_run,
            model.enclosing_call,
        ),
    )
}

/// An archived sealed record's id, root and end instant, as the record read
/// selects them.
///
/// # Errors
///
/// A corruption refusal for a row a stored read could not select.
pub fn record_parts(
    model: ArchivedModelCall,
) -> Result<(String, NodeId, i64), crate::DurableError> {
    match (model.record_ref, model.ended_at_ms) {
        (Some(root), Some(ended)) => Ok((model.call_id, root, ended)),
        _ => Err(corrupt("model call", "a sealed record without its end")),
    }
}

impl ArchiveContents {
    /// Every root the rows name, in row order, each once.
    #[must_use]
    pub fn roots(&self) -> Vec<NodeId> {
        let mut seen = BTreeSet::new();
        self.facts
            .iter()
            .map(|fact| Some(fact.record))
            .chain(
                self.calls
                    .iter()
                    .flat_map(|call| [call.decision_ref, call.body_ref]),
            )
            .chain(self.models.iter().map(|model| model.record_ref))
            .flatten()
            .filter(|root| seen.insert(*root))
            .collect()
    }

    /// Why `process`'s rows cannot be answered from an archive, if they
    /// cannot: a row its read would refuse, or a call another execution
    /// answers through. Those histories keep their rows.
    ///
    /// # Errors
    ///
    /// [`Unpackable::Unsupported`] naming the first such row.
    pub fn check_supported(
        &self,
        process: &lash_vocabulary::identity::ProcessId,
    ) -> Result<(), Unpackable> {
        let owner = super::OwnerKey::Process(process.clone());
        let calls = super::CallOwner::Process(process.clone());
        let key = owner.stored();
        let session = model_session(process);
        for fact in &self.facts {
            fact_row(&owner, fact.clone())
                .map_err(|_| Unpackable::Unsupported("a run fact does not decode"))?;
        }
        for call in &self.calls {
            if call.owner_key != key || call.execution_owner != key {
                return Err(Unpackable::Unsupported(
                    "a call is executed by another owner",
                ));
            }
            call_row(&calls, call.clone())
                .map_err(|_| Unpackable::Unsupported("a tool call does not decode"))?;
        }
        for model in &self.models {
            if model.record_ref.is_some() {
                record_parts(model.clone())
                    .map_err(|_| Unpackable::Unsupported("a model call record does not decode"))?;
            } else {
                pin_row(&session, model.clone()).map_err(|_| {
                    Unpackable::Unsupported("a model call reservation does not decode")
                })?;
            }
        }
        Ok(())
    }
}

impl ArchiveContents {
    /// The exact size an archive of these rows and a closure of `nodes`
    /// nodes, `bytes` content bytes and `edges` edges encodes to, without
    /// encoding it: what a packer checks against the cap before it reads
    /// the closure's bytes.
    #[must_use]
    pub fn size_with(&self, nodes: usize, bytes: usize, edges: usize) -> usize {
        let text = |text: &str| 4 + text.len();
        let opt_text = |value: &Option<String>| 1 + value.as_deref().map_or(0, text);
        let opt_int = |value: &Option<i64>| 1 + value.map_or(0, |_| 8);
        let facts: usize = self
            .facts
            .iter()
            .map(|fact| 8 + 8 + text(&fact.kind) + opt_text(&fact.call_id) + 4 + 8)
            .sum();
        let calls: usize = self
            .calls
            .iter()
            .map(|call| {
                8 + text(&call.call_id)
                    + text(&call.owner_key)
                    + text(&call.execution_owner)
                    + 8
                    + 8
                    + 1
                    + 8
                    + 1
                    + [
                        &call.session_id,
                        &call.turn_id,
                        &call.request_id,
                        &call.origin,
                        &call.reason,
                        &call.decision,
                        &call.answer_outcome,
                    ]
                    .into_iter()
                    .map(opt_text)
                    .sum::<usize>()
                    + 4
                    + 4
            })
            .sum();
        let models: usize = self
            .models
            .iter()
            .map(|model| {
                text(&model.run)
                    + text(&model.call)
                    + text(&model.call_id)
                    + opt_int(&model.attempt)
                    + 1
                    + opt_text(&model.enclosing_owner)
                    + opt_int(&model.enclosing_run)
                    + opt_text(&model.enclosing_call)
                    + 4
                    + opt_int(&model.ended_at_ms)
                    + opt_int(&model.written_epoch)
            })
            .sum();
        let tables = (self.facts.len() + self.calls.len() + self.models.len() + 3) * 4;
        HEADER
            .saturating_add(tables)
            .saturating_add(facts + calls + models)
            .saturating_add(nodes.saturating_mul(NODE_ENTRY))
            .saturating_add(bytes)
            .saturating_add(edges.saturating_mul(4))
    }
}

/// The most rows of one table a history within the cap can hold: a packer
/// reads one more and stops.
pub const PROCESS_ARCHIVE_MAX_ROWS: usize = PROCESS_ARCHIVE_CAP_BYTES / 24;

/// The most nodes a closure within the cap can hold.
pub const PROCESS_ARCHIVE_MAX_NODES: usize = PROCESS_ARCHIVE_CAP_BYTES / NODE_ENTRY;

/// The process whose archive holds `owner`'s root once its history is
/// packed, as a process id: a run fact or tool call of a process, or a
/// model call of a process's session.
#[must_use]
pub fn root_process(owner: &super::RootOwner) -> Option<String> {
    match owner {
        super::RootOwner::RunFact { owner, .. } | super::RootOwner::ToolCall { owner, .. } => {
            fact_process(owner).map(|process| process.as_str().to_owned())
        }
        super::RootOwner::ModelCall(key) => model_process(&key.session).map(str::to_owned),
        _ => None,
    }
}

/// The root `owner` names in `archive`: what the stored root statement
/// answers once the rows are packed.
///
/// # Errors
///
/// [`ArchiveCorrupt`] for a row that does not decode.
pub fn archived_root(
    archive: &ProcessArchive<'_>,
    owner: &super::RootOwner,
) -> Result<Option<NodeId>, ArchiveCorrupt> {
    Ok(match owner {
        super::RootOwner::RunFact {
            owner,
            run,
            ordinal,
        } => {
            let (Ok(run), Ok(ordinal)) = (i64::try_from(run.0), i64::try_from(ordinal.0)) else {
                return Ok(None);
            };
            if fact_process(owner).is_none() {
                return Ok(None);
            }
            archive.fact(run, ordinal)?.map(|fact| fact.record)
        }
        super::RootOwner::ToolCall { owner, call, part } => archive
            .call_of(&owner.stored(), call.as_str())?
            .and_then(|found| match part {
                super::tool_calls::ToolCallRoot::Decision => found.decision_ref,
                super::tool_calls::ToolCallRoot::Answer => found.body_ref,
            }),
        super::RootOwner::ModelCall(key) => {
            let (run, call) = (key.call.owner_column(), key.call.call_column());
            archive
                .models(|model| {
                    model.record_ref.is_some() && model.run == run && model.call == call
                })?
                .into_iter()
                .next()
                .and_then(|model| model.record_ref)
        }
        _ => None,
    })
}
