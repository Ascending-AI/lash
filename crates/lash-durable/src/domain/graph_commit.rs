//! Owner graph validation and full commit measurement (FIG-5887 §5, §9),
//! and a candidate's streamed install (FIG-6191).
//!
//! Both backends validate the graph staged in an [`ActorTx`](crate::ActorTx)
//! and measure its supplied nodes and domain writes inside the owner fence.
//! Nodes are keyed by identity, and references to stored nodes carry only
//! their digest. The same measurement admits a commit on either backend.
//!
//! A persistent-heap candidate is too large to stage in the transaction:
//! [`CandidateInstall`] merges its manifest stream with the commit's own
//! graph in global digest order, a bounded batch at a time. The inductive
//! rule is the same: every supplied envelope and its exact child table is
//! checked, and only immediate references to stored nodes are, never their
//! closures. Each supplied body is read and checked once: the edges follow
//! from an authenticated log of the checked nodes (FIG-6596).

use std::collections::{BTreeMap, BTreeSet};

use lash_core_store::checkpoint_graph::stage::{
    CandidateManifest, EdgeLog, ManifestSummary, StageFault,
};
use lash_core_store::checkpoint_graph::{
    EncodedCommitMeasurement, GraphRefusal, NodeId, TokenWriter,
};

use super::DomainWrite;
use super::graph::{
    AdmissionMember, EffectAdmissionWrite, ExecRootWrite, ReleasePinWrite, RootChange, RootOwner,
    RunFactWrite,
};
use super::pages::CandidateOwner;

fn encode_node(out: &mut TokenWriter, node: Option<&NodeId>) {
    match node {
        None => out.none(),
        Some(node) => {
            out.some();
            out.bytes(&node.0);
        }
    }
}

fn encode_change(out: &mut TokenWriter, change: &RootChange) {
    match change {
        RootChange::Put {
            owner,
            expected,
            root,
        } => {
            out.sum(0, 3);
            encode_owner(out, owner);
            encode_node(out, expected.as_ref());
            out.bytes(&root.0);
        }
        RootChange::Retire { owner, expected } => {
            out.sum(1, 2);
            encode_owner(out, owner);
            out.bytes(&expected.0);
        }
    }
}

fn encode_member(out: &mut TokenWriter, member: Option<&AdmissionMember>) {
    match member {
        None => out.none(),
        Some(member) => {
            out.some();
            out.seq(3);
            out.text(&member.owner.stored());
            out.unsigned(member.run.0);
            out.unsigned(member.member);
        }
    }
}

/// Each graph-reference write, tagged by its table and operation.
fn encode_graph_write(out: &mut TokenWriter, write: &DomainWrite) -> Result<(), GraphRefusal> {
    match write {
        DomainWrite::ExecRoot(ExecRootWrite::Put {
            exec,
            expected,
            expected_root,
            root,
            commit_id,
        }) => {
            out.sum(0, 5);
            out.text(&exec.stored());
            match expected {
                None => out.none(),
                Some(rev) => {
                    out.some();
                    out.unsigned(rev.0);
                }
            }
            encode_node(out, expected_root.as_ref());
            out.bytes(&root.0);
            out.bytes(&commit_id.0);
        }
        DomainWrite::ExecRoot(ExecRootWrite::Retire { exec, expected }) => {
            out.sum(1, 2);
            out.text(&exec.stored());
            out.bytes(&expected.0);
        }
        DomainWrite::ExecRoot(ExecRootWrite::Clear { .. }) => {
            return Err(GraphRefusal::InvalidBody {
                kind: 0,
                reason: "a graph batch retires only the root it names".to_owned(),
            });
        }
        DomainWrite::RunFact(RunFactWrite::Append {
            owner,
            run,
            ordinal,
            kind,
            call,
            record,
        }) => {
            out.sum(2, 6);
            out.text(&owner.stored());
            out.unsigned(run.0);
            out.unsigned(ordinal.0);
            out.text(kind.as_str());
            match call {
                None => out.none(),
                Some(call) => {
                    out.some();
                    out.text(call.as_str());
                }
            }
            out.bytes(&record.0);
        }
        DomainWrite::RunFact(RunFactWrite::Prune { owner, before }) => {
            out.sum(3, 2);
            out.text(&owner.stored());
            out.unsigned(before.0);
        }
        DomainWrite::EffectAdmission(EffectAdmissionWrite::Put {
            exec,
            effect,
            member,
            admission,
        }) => {
            out.sum(4, 4);
            out.text(&exec.stored());
            out.bytes(&effect.0);
            encode_member(out, member.as_ref());
            out.bytes(&admission.0);
        }
        DomainWrite::EffectAdmission(EffectAdmissionWrite::Remove {
            exec,
            effect,
            expected,
        }) => {
            out.sum(5, 3);
            out.text(&exec.stored());
            out.bytes(&effect.0);
            out.bytes(&expected.0);
        }
        DomainWrite::ReleasePin(ReleasePinWrite::Install { release, root }) => {
            out.sum(8, 2);
            out.text(release);
            out.bytes(&root.0);
        }
        DomainWrite::ReleasePin(ReleasePinWrite::Retire { release, expected }) => {
            out.sum(9, 2);
            out.text(release);
            out.bytes(&expected.0);
        }
        other => {
            return Err(GraphRefusal::InvalidBody {
                kind: 0,
                reason: format!("a batch commit carries graph-reference rows only, not {other:?}"),
            });
        }
    }
    Ok(())
}

/// A root owner's canonical encoding: its source's variant number and its
/// key.
pub fn encode_owner(out: &mut TokenWriter, owner: &RootOwner) {
    match owner {
        RootOwner::ModelCall(key) => {
            out.sum(17, 3);
            out.text(key.session.as_str());
            out.text(&key.call.owner_column());
            out.text(&key.call.call_column());
        }
        RootOwner::Exec(exec) => {
            out.sum(0, 1);
            out.text(&exec.stored());
        }
        RootOwner::RunFact {
            owner,
            run,
            ordinal,
        } => {
            out.sum(1, 3);
            out.text(&owner.stored());
            out.unsigned(run.0);
            out.unsigned(ordinal.0);
        }
        RootOwner::EffectAdmission { exec, effect } => {
            out.sum(2, 2);
            out.text(&exec.stored());
            out.bytes(&effect.0);
        }
        RootOwner::TurnPhase { session, run } => {
            out.sum(3, 2);
            out.text(session.as_str());
            out.text(run.as_str());
        }
        RootOwner::WorkingExecution { session, run } => {
            out.sum(4, 2);
            out.text(session.as_str());
            out.text(run.as_str());
        }
        RootOwner::ModelAnswer { session, run } => {
            out.sum(14, 2);
            out.text(session.as_str());
            out.text(run.as_str());
        }
        RootOwner::TurnNamespace {
            session,
            run,
            plugin,
        } => {
            out.sum(5, 3);
            out.text(session.as_str());
            out.text(run.as_str());
            out.text(plugin);
        }
        RootOwner::SessionRevision { session, revision } => {
            out.sum(6, 2);
            out.text(session.as_str());
            out.unsigned(*revision);
        }
        RootOwner::SessionAdmissionBase { session } => {
            out.sum(7, 1);
            out.text(session.as_str());
        }
        RootOwner::ArtifactBridge {
            namespace,
            artifact,
        } => {
            out.sum(9, 2);
            out.text(namespace);
            out.text(artifact);
        }
        RootOwner::ReleasePin { release } => {
            out.sum(7, 1);
            out.text(release);
        }
    }
}

/// A commit's graph checked before its transaction writes anything: every
/// supplied node's envelope and the closure it relies on.
#[derive(Clone, Debug, Default)]
pub struct CheckedGraph {
    /// Every supplied node, in digest order (the blob lock order), with its
    /// exact child table.
    pub nodes: Vec<CheckedNode>,
    /// Nodes the commit references without supplying: required children and
    /// required reads. Each must already be stored.
    pub external: BTreeSet<NodeId>,
    /// Roots the commit publishes that it does not supply. Each must already
    /// be stored as a graph node.
    pub external_roots: BTreeSet<NodeId>,
}

/// One supplied node.
#[derive(Clone, Debug)]
pub struct CheckedNode {
    pub id: NodeId,
    pub bytes: std::sync::Arc<[u8]>,
    pub children: Vec<NodeId>,
}

/// Checks a commit's graph without a store: every supplied node hashes to
/// its identity, parses, and keeps the node bound (a published root kind
/// the root bound); the supplied subgraph has no storage cycle; and what it
/// references without supplying is listed for the store to find.
///
/// # Errors
///
/// Any refusal of
/// [`check_envelope`](lash_core_store::checkpoint_graph::check_envelope),
/// [`GraphRefusal::HashMismatch`], [`GraphRefusal::RootTooLarge`] or
/// [`GraphRefusal::StorageCycle`].
pub fn check_graph(
    supplied_nodes: &BTreeMap<NodeId, lash_core_store::checkpoint_graph::EncodedNode>,
    reads: &BTreeSet<NodeId>,
    publications: &[RootChange],
) -> Result<CheckedGraph, GraphRefusal> {
    use lash_core_store::checkpoint_graph::{
        DecodeLimits, MAX_NODE_BYTES, SizeBound, check_envelope,
    };
    let limits = DecodeLimits::within_total(u64::MAX);
    debug_assert_eq!(limits.node_bytes, MAX_NODE_BYTES);
    let mut nodes = Vec::with_capacity(supplied_nodes.len());
    for (id, node) in supplied_nodes {
        if node.id != *id {
            return Err(GraphRefusal::HashMismatch(*id));
        }
        let envelope = check_envelope(&node.bytes, *id, &limits)?;
        nodes.push(CheckedNode {
            id: *id,
            bytes: node.bytes.clone(),
            children: envelope.children.iter().collect(),
        });
    }
    let supplied: BTreeMap<NodeId, usize> = nodes
        .iter()
        .enumerate()
        .map(|(index, node)| (node.id, index))
        .collect();
    let mut external: BTreeSet<NodeId> = reads
        .iter()
        .filter(|read| !supplied.contains_key(read))
        .copied()
        .collect();
    for node in &nodes {
        external.extend(
            node.children
                .iter()
                .filter(|child| !supplied.contains_key(child)),
        );
    }
    let mut external_roots = BTreeSet::new();
    for change in publications {
        let RootChange::Put { root, .. } = change else {
            continue;
        };
        match supplied.get(root) {
            None => {
                external_roots.insert(*root);
            }
            Some(index) => {
                let node = &nodes[*index];
                let kind =
                    lash_core_store::checkpoint_graph::parse_envelope(&node.bytes, Some(*root))?
                        .kind;
                if let Some(spec) = kind.spec()
                    && spec.bound == SizeBound::Root
                    && node.bytes.len() as u64 > SizeBound::Root.bytes()
                {
                    return Err(GraphRefusal::RootTooLarge {
                        bytes: node.bytes.len() as u64,
                        limit: SizeBound::Root.bytes(),
                    });
                }
            }
        }
    }
    check_acyclic(&nodes, &supplied)?;
    Ok(CheckedGraph {
        nodes,
        external,
        external_roots,
    })
}

/// The measurement of an owner commit (FIG-5887 §9): its supplied nodes,
/// and its writes by their canonical encoding where they own a root
/// (graph-reference rows and turn writes), and the rows the commit plans:
/// one per new blob and edge, one per domain write.
///
/// # Errors
///
/// A supplied node whose envelope does not parse.
pub fn measure_owner_commit(
    nodes: &BTreeMap<NodeId, lash_core_store::checkpoint_graph::EncodedNode>,
    writes: &[DomainWrite],
) -> Result<EncodedCommitMeasurement, GraphRefusal> {
    let mut supplied_node_bytes = 0_u64;
    let mut edge_rows = 0_u64;
    for node in nodes.values() {
        supplied_node_bytes += node.len();
        edge_rows += node.children()?.len() as u64;
    }
    let mut mutations = TokenWriter::new();
    mutations.seq(writes.len() as u64);
    for write in writes {
        if encode_graph_write(&mut mutations, write).is_err() {
            encode_root_changes(&mut mutations, &write.publications());
        }
    }
    let mutation_encoding_bytes = mutations.len() as u64;
    Ok(EncodedCommitMeasurement {
        supplied_node_bytes,
        mutation_encoding_bytes,
        full_encoded_commit_bytes: supplied_node_bytes
            .saturating_add(nodes.len() as u64 * 34)
            .saturating_add(mutation_encoding_bytes),
        planned_mutation_rows: (nodes.len() as u64)
            .saturating_add(edge_rows)
            .saturating_add(writes.len() as u64),
    })
}

fn encode_root_changes(out: &mut TokenWriter, changes: &[RootChange]) {
    out.seq(changes.len() as u64);
    for change in changes {
        encode_change(out, change);
    }
}

/// Refuses a storage cycle among supplied nodes. Content addressing makes
/// one unbuildable from real digests; the check is what keeps a forged
/// batch from installing one.
fn check_acyclic(
    nodes: &[CheckedNode],
    supplied: &BTreeMap<NodeId, usize>,
) -> Result<(), GraphRefusal> {
    // 0: unvisited, 1: on the path, 2: done.
    let mut state = vec![0_u8; nodes.len()];
    for start in 0..nodes.len() {
        if state[start] != 0 {
            continue;
        }
        let mut stack = vec![(start, 0_usize)];
        state[start] = 1;
        while let Some((index, next)) = stack.pop() {
            let children = &nodes[index].children;
            if next == children.len() {
                state[index] = 2;
                continue;
            }
            stack.push((index, next + 1));
            if let Some(&child) = supplied.get(&children[next]) {
                match state[child] {
                    0 => {
                        state[child] = 1;
                        stack.push((child, 0));
                    }
                    1 => return Err(GraphRefusal::StorageCycle(nodes[child].id)),
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

/// The logical SQL bytes of one installed blob row (hash text, content,
/// encoding) and of one edge row (two hash texts), by the declared
/// cross-backend convention (FIG-5887 §9).
#[must_use]
pub const fn blob_row_logical_bytes(content: u64) -> u64 {
    64 + content + 8
}

/// See [`blob_row_logical_bytes`].
pub const EDGE_ROW_LOGICAL_BYTES: u64 = 128;

/// How much of a candidate one install batch buffers (FIG-6191): a store
/// holds one batch of the manifest's entries beside the commit's own small
/// graph, never the candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CandidateBuffers {
    /// The entries one manifest read asks for.
    pub page: usize,
    /// A batch ends once it holds this many entries,
    pub entries: usize,
    /// or this many supplied node bytes.
    pub bytes: u64,
    /// An edge batch holds at most this many `(parent, child)` pairs.
    pub edges: usize,
}

impl CandidateBuffers {
    /// The stores' batches: 64-entry manifest reads, batches of at most 256
    /// entries or 4 MiB of node bytes, and edge batches of 4096 pairs.
    pub const DEFAULT: Self = Self {
        page: 64,
        entries: 256,
        bytes: 4 << 20,
        edges: 4096,
    };

    /// The most manifest bytes a batch holds at once: its byte bound plus
    /// one manifest read of maximal nodes. An edge batch holds one child
    /// table, within one maximal node.
    #[must_use]
    pub const fn bound(self) -> u64 {
        self.bytes + self.page as u64 * lash_core_store::checkpoint_graph::MAX_NODE_BYTES
    }
}

/// One entry of a candidate's install, in global digest order.
#[derive(Clone, Debug)]
pub enum InstallEntry {
    /// A stored node the commit names without supplying: key-share locked
    /// and checked present, never walked. `root` when the commit publishes
    /// it as a root, which must be a graph node.
    Named { id: NodeId, root: bool },
    /// A node the commit supplies, its envelope and exact child table
    /// checked.
    Supplied(CheckedNode),
}

impl InstallEntry {
    #[must_use]
    pub fn id(&self) -> NodeId {
        match self {
            Self::Named { id, .. } => *id,
            Self::Supplied(node) => node.id,
        }
    }

    fn bytes(&self) -> u64 {
        match self {
            Self::Named { .. } => 0,
            Self::Supplied(node) => node.bytes.len() as u64,
        }
    }
}

/// A manifest's failure as the publication's refusal: infrastructure, and
/// the candidate stays staged.
#[must_use]
pub fn stage_refusal(fault: &StageFault) -> GraphRefusal {
    GraphRefusal::Backend(lash_core_store::store_failure::StoreFailure {
        kind: lash_core_store::store_failure::StoreFailureKind::Unavailable,
        message: format!("the candidate's manifest failed: {fault}"),
    })
}

/// Checks a candidate against the owner commit it travels with, before
/// anything is written: one publication identity (FIG-6197 F1). `owner`
/// names exactly one write of the commit, whose root the commit supplies
/// and which reaches the candidate's semantic root through supplied nodes
/// (an execution root's wrapper, or a step outcome's record). An
/// execution root's manifest was built against the root the row holds now
/// (`summary.base` is the row's `expected_root`, a wrapper too) and its
/// row records the candidate's commit identity, which is how an ambiguous
/// publication resolves against the row ([`PageStore::candidate_state`]).
/// A step outcome's manifest has no base. A session's candidate is owned
/// by the one write that names the session's execution root (a turn's
/// working state or a head commit's checkpoint), and that root is the
/// candidate's own: the session descriptor, with no base (FIG-6418).
///
/// # Errors
///
/// [`GraphRefusal::PublicationMismatch`].
///
/// [`PageStore::candidate_state`]: super::pages::PageStore::candidate_state
pub fn check_candidate(
    writes: &[DomainWrite],
    checked: &CheckedGraph,
    owner: &CandidateOwner,
    summary: &ManifestSummary,
) -> Result<(), GraphRefusal> {
    if let CandidateOwner::Session(session) = owner {
        return check_session_candidate(writes, session, summary);
    }
    let mut owned = writes.iter().filter_map(|write| match (owner, write) {
        (
            CandidateOwner::ExecRootPut(exec),
            DomainWrite::ExecRoot(ExecRootWrite::Put {
                exec: put,
                root,
                expected_root,
                commit_id,
                ..
            }),
        ) if put == exec => Some((*root, *expected_root, Some(*commit_id))),
        (
            CandidateOwner::RunFactAppend {
                owner,
                run,
                ordinal,
            },
            DomainWrite::RunFact(RunFactWrite::Append {
                owner: appended,
                run: at,
                ordinal: place,
                record,
                ..
            }),
        ) if appended == owner && at == run && place == ordinal => Some((*record, None, None)),
        _ => None,
    });
    let (Some((root, base, row_commit)), None) = (owned.next(), owned.next()) else {
        return Err(GraphRefusal::PublicationMismatch);
    };
    if root != summary.root
        && reaches(checked, root, summary.root)
        && base == summary.base
        && row_commit.is_none_or(|commit| commit == summary.commit)
    {
        Ok(())
    } else {
        Err(GraphRefusal::PublicationMismatch)
    }
}

/// [`check_candidate`] for a session's candidate: exactly one write names
/// the session's execution root, and it is the candidate's root.
fn check_session_candidate(
    writes: &[DomainWrite],
    session: &lash_sansio::SessionId,
    summary: &ManifestSummary,
) -> Result<(), GraphRefusal> {
    let mut owned = writes.iter().filter_map(|write| match write {
        DomainWrite::Turn(super::TurnWrite::Working {
            session: wrote,
            working: Some(root),
            ..
        })
        | DomainWrite::SessionCommit(super::SessionCommitWrite {
            session: wrote,
            execution: Some(root),
            ..
        }) if wrote == session => Some(*root),
        _ => None,
    });
    let (Some(root), None) = (owned.next(), owned.next()) else {
        return Err(GraphRefusal::PublicationMismatch);
    };
    if root == summary.root && summary.base.is_none() {
        Ok(())
    } else {
        Err(GraphRefusal::PublicationMismatch)
    }
}

/// Whether `from`, a node `checked` supplies, reaches `to` through nodes
/// it supplies: bounded by the commit's own graph, never the store's.
fn reaches(checked: &CheckedGraph, from: NodeId, to: NodeId) -> bool {
    let supplied: BTreeMap<NodeId, &CheckedNode> =
        checked.nodes.iter().map(|node| (node.id, node)).collect();
    let mut seen = BTreeSet::new();
    let mut stack = vec![from];
    while let Some(id) = stack.pop() {
        let Some(node) = supplied.get(&id) else {
            continue;
        };
        for child in &node.children {
            if *child == to {
                return true;
            }
            if seen.insert(*child) {
                stack.push(*child);
            }
        }
    }
    false
}

/// A candidate's install (FIG-6191, S6): the owner commit's own checked
/// graph merged with the candidate's manifest stream in one ascending digest
/// order, the blob lock order, read a bounded batch at a time.
///
/// The store reads it once to lock what the commit names and insert what it
/// supplies, then takes its [`edges`](Self::edges) to insert every supplied
/// node's edges once all node rows exist. Each supplied manifest node's
/// envelope is checked against its digest; content addressing makes a
/// storage cycle unbuildable from checked digests, so the stream needs no
/// cycle check. Nothing a named node reaches is read.
///
/// Each body is read and checked once (FIG-6596): as it checks a manifest
/// node with children, the install appends a record of it to the
/// manifest's [`EdgeLog`], authenticated under a key this install alone
/// holds, and its edge pass reads back only those records and the child
/// tables they name.
pub struct CandidateInstall<'a> {
    own: Vec<InstallEntry>,
    manifest: &'a mut dyn CandidateManifest,
    summary: ManifestSummary,
    buffers: CandidateBuffers,
    own_next: usize,
    after: Option<NodeId>,
    page: std::collections::VecDeque<lash_core_store::checkpoint_graph::stage::ManifestEntry>,
    exhausted: bool,
    peak: u64,
    log: EdgeRecords,
}

impl<'a> CandidateInstall<'a> {
    /// The install of `manifest` beside the commit's own checked graph.
    #[must_use]
    pub fn new(
        own: &CheckedGraph,
        manifest: &'a mut dyn CandidateManifest,
        buffers: CandidateBuffers,
    ) -> Self {
        let mut named: BTreeMap<NodeId, bool> = BTreeMap::new();
        for id in &own.external {
            named.insert(*id, false);
        }
        for id in &own.external_roots {
            named.insert(*id, true);
        }
        let mut entries: Vec<InstallEntry> = own
            .nodes
            .iter()
            .cloned()
            .map(InstallEntry::Supplied)
            .chain(
                named
                    .into_iter()
                    .map(|(id, root)| InstallEntry::Named { id, root }),
            )
            .collect();
        entries.sort_by_key(InstallEntry::id);
        let summary = manifest.summary();
        Self {
            own: entries,
            manifest,
            summary,
            buffers,
            own_next: 0,
            after: None,
            page: std::collections::VecDeque::new(),
            exhausted: false,
            peak: 0,
            log: EdgeRecords::new(),
        }
    }

    /// What the manifest stated before it was streamed.
    #[must_use]
    pub fn summary(&self) -> ManifestSummary {
        self.summary
    }

    /// The edge pass, once [`next_batch`](Self::next_batch) has answered
    /// none: every supplied node's edges, the commit's own from memory and
    /// the manifest's from the install's edge log.
    ///
    /// # Errors
    ///
    /// A node stream not yet spent, or the log's last records failing to
    /// reach it.
    pub fn edges(mut self) -> Result<CandidateEdges<'a>, GraphRefusal> {
        if !(self.exhausted && self.page.is_empty() && self.own_next == self.own.len()) {
            return Err(corrupt(
                "the edge pass began before every node was installed",
            ));
        }
        self.log.flush(&mut *self.manifest)?;
        self.own.retain(
            |entry| matches!(entry, InstallEntry::Supplied(node) if !node.children.is_empty()),
        );
        Ok(CandidateEdges {
            own: self.own,
            own_next: 0,
            manifest: self.manifest,
            log: self.log,
            read: 0,
            ahead: std::collections::VecDeque::new(),
            last: None,
            parent: None,
            pending: Vec::new(),
            pairs: self.buffers.edges,
            peak: self.peak,
        })
    }

    /// The most manifest bytes the install held at once.
    #[must_use]
    pub fn peak_buffered(&self) -> u64 {
        self.peak
    }

    fn refill(&mut self) -> Result<(), GraphRefusal> {
        use lash_core_store::checkpoint_graph::stage::ManifestEntry;
        if !self.page.is_empty() || self.exhausted {
            return Ok(());
        }
        let entries = self
            .manifest
            .entries(self.after, self.buffers.page)
            .map_err(|fault| stage_refusal(&fault))?;
        if entries.is_empty() {
            self.exhausted = true;
            return Ok(());
        }
        for entry in &entries {
            let id = match entry {
                ManifestEntry::External(id) => *id,
                ManifestEntry::Fresh(node) => node.id,
            };
            // The stream must ascend strictly: the blob lock order.
            if self.after.is_some_and(|after| id <= after) {
                return Err(corrupt(&format!(
                    "the candidate's manifest does not ascend at {id}"
                )));
            }
            self.after = Some(id);
        }
        self.page.extend(entries);
        Ok(())
    }

    fn page_bytes(&self) -> u64 {
        use lash_core_store::checkpoint_graph::stage::ManifestEntry;
        self.page
            .iter()
            .map(|entry| match entry {
                ManifestEntry::External(_) => 0,
                ManifestEntry::Fresh(node) => node.bytes.len() as u64,
            })
            .sum()
    }

    /// The manifest's next entry, checked, and where a supplied node's
    /// child table starts.
    fn take_manifest(&mut self) -> Result<(InstallEntry, u8), GraphRefusal> {
        use lash_core_store::checkpoint_graph::stage::ManifestEntry;
        use lash_core_store::checkpoint_graph::{DecodeLimits, SizeBound, check_envelope};
        match self.page.pop_front() {
            Some(ManifestEntry::External(id)) => Ok((InstallEntry::Named { id, root: false }, 0)),
            Some(ManifestEntry::Fresh(node)) => {
                let limits = DecodeLimits::within_total(u64::MAX);
                let envelope = check_envelope(&node.bytes, node.id, &limits)?;
                if node.id == self.summary.root
                    && let Some(spec) = envelope.kind.spec()
                    && spec.bound == SizeBound::Root
                    && node.bytes.len() as u64 > SizeBound::Root.bytes()
                {
                    return Err(GraphRefusal::RootTooLarge {
                        bytes: node.bytes.len() as u64,
                        limit: SizeBound::Root.bytes(),
                    });
                }
                let table = lash_core_store::checkpoint_graph::node::child_table_offset(
                    envelope.kind,
                    envelope.children.len(),
                );
                let children = envelope.children.iter().collect();
                Ok((
                    InstallEntry::Supplied(CheckedNode {
                        id: node.id,
                        bytes: node.bytes,
                        children,
                    }),
                    u8::try_from(table).map_err(|_| GraphRefusal::LengthOverflow)?,
                ))
            }
            None => Err(GraphRefusal::Truncated),
        }
    }

    /// The next batch, ascending by digest and after every earlier batch;
    /// none once both streams are spent.
    ///
    /// # Errors
    ///
    /// A manifest failure or disorder, a refused envelope, two different
    /// bodies supplied under one digest, a root over its bound, or an edge
    /// log that refuses its records.
    pub fn next_batch(&mut self) -> Result<Option<Vec<InstallEntry>>, GraphRefusal> {
        let mut batch: Vec<InstallEntry> = Vec::new();
        let mut bytes = 0_u64;
        while batch.len() < self.buffers.entries && bytes < self.buffers.bytes {
            self.refill()?;
            self.peak = self.peak.max(bytes + self.page_bytes());
            let manifest = self.page.front().map(|entry| match entry {
                lash_core_store::checkpoint_graph::stage::ManifestEntry::External(id) => *id,
                lash_core_store::checkpoint_graph::stage::ManifestEntry::Fresh(node) => node.id,
            });
            let own = self.own.get(self.own_next).map(InstallEntry::id);
            let entry = match (own, manifest) {
                (None, None) => break,
                (Some(_), None) => self.take_own(),
                (Some(own), Some(manifest)) if own < manifest => self.take_own(),
                (None, Some(_)) => {
                    let (entry, table) = self.take_manifest()?;
                    self.log.record(&entry, table);
                    entry
                }
                (Some(own), Some(manifest)) if manifest < own => {
                    let (entry, table) = self.take_manifest()?;
                    self.log.record(&entry, table);
                    entry
                }
                (Some(_), Some(_)) => {
                    let own = self.take_own();
                    let own_supplies = matches!(own, InstallEntry::Supplied(_));
                    let (manifest, table) = self.take_manifest()?;
                    let entry = merge(own, manifest)?;
                    // The commit's own node keeps its edges in memory.
                    if !own_supplies {
                        self.log.record(&entry, table);
                    }
                    entry
                }
            };
            bytes += entry.bytes();
            batch.push(entry);
        }
        self.peak = self.peak.max(bytes + self.page_bytes());
        self.log.flush(&mut *self.manifest)?;
        Ok((!batch.is_empty()).then_some(batch))
    }

    fn take_own(&mut self) -> InstallEntry {
        let entry = self.own[self.own_next].clone();
        self.own_next += 1;
        entry
    }
}

fn corrupt(message: &str) -> GraphRefusal {
    GraphRefusal::Backend(lash_core_store::store_failure::StoreFailure {
        kind: lash_core_store::store_failure::StoreFailureKind::Corrupt,
        message: message.to_owned(),
    })
}

/// One edge-log record: a checked manifest node's digest, where its child
/// table starts and how many children it has, and the record's tag.
const EDGE_RECORD: usize = 32 + 1 + 4 + EDGE_TAG;
/// A record's tag: the leading bytes of its keyed BLAKE3 hash.
const EDGE_TAG: usize = 16;
const _: () =
    assert!(EDGE_RECORD as u64 == lash_core_store::checkpoint_graph::stage::EDGE_RECORD_BYTES);
/// The records the edge pass reads from its log at once.
const EDGE_READ_AHEAD: usize = 64;

/// The install's edge log and what authenticates it: a key fresh for this
/// install, and how many records it wrote. Each record's tag covers its
/// place in the log, its fields and the child table they name, so a record
/// another install wrote, reordered, altered or naming a changed table is
/// refused, and a log that lost records ends before the count.
struct EdgeRecords {
    key: [u8; 32],
    log: Option<Box<dyn EdgeLog>>,
    /// Records not yet appended: at most one batch's.
    staged: Vec<u8>,
    written: u64,
}

impl EdgeRecords {
    fn new() -> Self {
        Self {
            // 256 fresh random bits.
            key: lash_core_store::checkpoint_graph::CommitId::mint().0,
            log: None,
            staged: Vec::new(),
            written: 0,
        }
    }

    fn tag(&self, at: u64, head: &[u8], table: &[u8]) -> [u8; EDGE_TAG] {
        let mut hasher = blake3::Hasher::new_keyed(&self.key);
        hasher.update(&at.to_le_bytes());
        hasher.update(head);
        hasher.update(table);
        let mut tag = [0; EDGE_TAG];
        tag.copy_from_slice(&hasher.finalize().as_bytes()[..EDGE_TAG]);
        tag
    }

    /// Stages the record of `entry`, a node the manifest supplies, if it
    /// has children; its child table starts at `table`.
    fn record(&mut self, entry: &InstallEntry, table: u8) {
        let InstallEntry::Supplied(node) = entry else {
            return;
        };
        if node.children.is_empty() {
            return;
        }
        let at = self.written + (self.staged.len() / EDGE_RECORD) as u64;
        let mut head = [0; 37];
        head[..32].copy_from_slice(&node.id.0);
        head[32] = table;
        head[33..].copy_from_slice(&(node.children.len() as u32).to_le_bytes());
        let start = usize::from(table);
        let tag = self.tag(
            at,
            &head,
            &node.bytes[start..start + 32 * node.children.len()],
        );
        self.staged.extend_from_slice(&head);
        self.staged.extend_from_slice(&tag);
    }

    /// Appends the staged records to the log, opening it at the first.
    fn flush(&mut self, manifest: &mut dyn CandidateManifest) -> Result<(), GraphRefusal> {
        if self.staged.is_empty() {
            return Ok(());
        }
        let log = match &mut self.log {
            Some(log) => log,
            None => self
                .log
                .insert(manifest.edge_log().map_err(|fault| stage_refusal(&fault))?),
        };
        log.append(&self.staged)
            .map_err(|fault| stage_refusal(&fault))?;
        self.written += (self.staged.len() / EDGE_RECORD) as u64;
        self.staged.clear();
        Ok(())
    }
}

/// A logged record, its fields not yet authenticated.
struct EdgeRecord {
    at: u64,
    id: NodeId,
    head: [u8; 37],
    tag: [u8; EDGE_TAG],
}

impl EdgeRecord {
    fn parse(at: u64, bytes: &[u8; EDGE_RECORD]) -> Self {
        let mut head = [0; 37];
        head.copy_from_slice(&bytes[..37]);
        let mut id = [0; 32];
        id.copy_from_slice(&head[..32]);
        let mut tag = [0; EDGE_TAG];
        tag.copy_from_slice(&bytes[37..EDGE_RECORD]);
        Self {
            at,
            id: NodeId(id),
            head,
            tag,
        }
    }

    fn table(&self) -> (u64, usize) {
        let mut count = [0; 4];
        count.copy_from_slice(&self.head[33..37]);
        (
            u64::from(self.head[32]),
            32 * u32::from_le_bytes(count) as usize,
        )
    }
}

/// A candidate's edge pass (FIG-6596): every supplied node's
/// `(parent, child)` pairs in ascending parent order, a bounded batch at a
/// time, the commit's own nodes' from memory and the manifest's from the
/// install's authenticated edge log, each child table read once and never
/// a body.
pub struct CandidateEdges<'a> {
    /// The commit's own supplied nodes with children, ascending.
    own: Vec<InstallEntry>,
    own_next: usize,
    manifest: &'a mut dyn CandidateManifest,
    log: EdgeRecords,
    /// Records read from the log.
    read: u64,
    ahead: std::collections::VecDeque<EdgeRecord>,
    last: Option<NodeId>,
    /// The parent whose remaining children are `pending`, reversed.
    parent: Option<NodeId>,
    pending: Vec<NodeId>,
    pairs: usize,
    peak: u64,
}

impl CandidateEdges<'_> {
    /// The next batch of at most [`CandidateBuffers::edges`] pairs; none
    /// once every supplied node's edges were answered.
    ///
    /// # Errors
    ///
    /// The edge log or a child table failing to read, or a record that does
    /// not authenticate.
    pub fn next_batch(&mut self) -> Result<Option<Vec<(NodeId, NodeId)>>, GraphRefusal> {
        let mut batch = Vec::new();
        while batch.len() < self.pairs {
            if let (Some(parent), Some(child)) = (self.parent, self.pending.pop()) {
                batch.push((parent, child));
                continue;
            }
            let Some((parent, mut children)) = self.next_parent()? else {
                break;
            };
            children.reverse();
            self.parent = Some(parent);
            self.pending = children;
        }
        Ok((!batch.is_empty()).then_some(batch))
    }

    /// The most manifest bytes the install held at once, its edge pass
    /// included.
    #[must_use]
    pub fn peak_buffered(&self) -> u64 {
        self.peak
    }

    /// The next supplied node with children and its children, ascending by
    /// digest across the commit's own nodes and the log.
    fn next_parent(&mut self) -> Result<Option<(NodeId, Vec<NodeId>)>, GraphRefusal> {
        let own = self.own.get(self.own_next).map(InstallEntry::id);
        let logged = self.peek()?.map(|record| record.id);
        let parent = match (own, logged) {
            (None, None) => return Ok(None),
            (Some(own), Some(logged)) if logged < own => self.take_logged()?,
            (Some(_), _) => {
                let InstallEntry::Supplied(node) = &self.own[self.own_next] else {
                    unreachable!("only supplied nodes are kept");
                };
                self.own_next += 1;
                (node.id, node.children.clone())
            }
            (None, Some(_)) => self.take_logged()?,
        };
        if self.last.is_some_and(|last| parent.0 <= last) {
            return Err(corrupt(&format!(
                "the install's edge log does not ascend at {}",
                parent.0
            )));
        }
        self.last = Some(parent.0);
        Ok(Some(parent))
    }

    fn peek(&mut self) -> Result<Option<&EdgeRecord>, GraphRefusal> {
        if self.ahead.is_empty() && self.read < self.log.written {
            let count = (self.log.written - self.read).min(EDGE_READ_AHEAD as u64);
            let mut bytes = vec![0; count as usize * EDGE_RECORD];
            let log = self
                .log
                .log
                .as_mut()
                .ok_or_else(|| corrupt("the install's edge log is missing"))?;
            log.read_at(self.read * EDGE_RECORD as u64, &mut bytes)
                .map_err(|fault| stage_refusal(&fault))?;
            for (offset, record) in bytes.as_chunks::<EDGE_RECORD>().0.iter().enumerate() {
                self.ahead
                    .push_back(EdgeRecord::parse(self.read + offset as u64, record));
            }
            self.read += count;
        }
        Ok(self.ahead.front())
    }

    /// The front record's node and children, read and authenticated.
    fn take_logged(&mut self) -> Result<(NodeId, Vec<NodeId>), GraphRefusal> {
        let record = self
            .ahead
            .pop_front()
            .ok_or_else(|| corrupt("the install's edge log ended early"))?;
        let (offset, len) = record.table();
        let table = self
            .manifest
            .fresh_range(&record.id, offset, len)
            .map_err(|fault| stage_refusal(&fault))?;
        self.peak = self.peak.max(table.len() as u64);
        if table.len() != len || self.log.tag(record.at, &record.head, &table) != record.tag {
            return Err(corrupt(&format!(
                "the install's edge record for {} does not authenticate",
                record.id
            )));
        }
        let children = table
            .as_chunks::<32>()
            .0
            .iter()
            .map(|digest| NodeId(*digest))
            .collect();
        Ok((record.id, children))
    }
}

/// One digest named by the commit's own graph and by the manifest: a node
/// either supplies is supplied, and two supplied bodies must agree.
fn merge(own: InstallEntry, manifest: InstallEntry) -> Result<InstallEntry, GraphRefusal> {
    match (own, manifest) {
        (InstallEntry::Supplied(own), InstallEntry::Supplied(manifest)) => {
            if own.bytes == manifest.bytes {
                Ok(InstallEntry::Supplied(own))
            } else {
                Err(GraphRefusal::ContentConflict(own.id))
            }
        }
        (InstallEntry::Supplied(node), InstallEntry::Named { .. })
        | (InstallEntry::Named { .. }, InstallEntry::Supplied(node)) => {
            Ok(InstallEntry::Supplied(node))
        }
        (InstallEntry::Named { id, root }, InstallEntry::Named { root: other, .. }) => {
            Ok(InstallEntry::Named {
                id,
                root: root || other,
            })
        }
    }
}
