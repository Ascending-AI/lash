//! A graph commit's canonical encoding (FIG-5887 §5, §9): the batch digest a
//! retry is recognized by, the full encoded measurement a commit is admitted
//! on, and the receipt's result bytes.
//!
//! Both backends call these, so a batch measures and digests the same on
//! SQLite and PostgreSQL, and a preparer measures exactly what the store
//! rechecks inside its fence.
//!
//! The encoding uses the graph token grammar. Every node, root key,
//! expected revision, publication and graph-reference row of the batch is
//! in it, with its table tag; derived measurements and the digest itself
//! are not. Supplied nodes are keyed by identity, so a duplicate is encoded
//! once; a reference to an existing node is its 32-byte digest, never its
//! body again. A batch carries graph-reference rows only
//! ([`DomainWrite::is_graph_reference`]); an owner's other rows commit
//! through its [`ActorTx`](crate::ActorTx), whose graph the same check
//! ([`check_graph`]) validates.

use std::collections::{BTreeMap, BTreeSet};

use lash_core_store::checkpoint_graph::{
    BatchDigest, CommitId, EncodedCommitMeasurement, GraphRefusal, InstalledGraphMeasurement,
    NodeId, TokenError, TokenReader, TokenWriter,
};
use lash_sansio::{SessionId, TurnId};

use super::DomainWrite;
use super::graph::{
    AdmissionMember, EffectAdmissionWrite, EffectKey, ExecRootWrite, GraphCommitBatch,
    OwnerRevisionSet, PublishedGraph, ReadPin, ReadPinId, ReadPinWrite, ReleasePinWrite,
    RootChange, RootOwner, RunFactWrite,
};
use super::keys::{ExecKey, OwnerKey};

/// The canonical encoding of everything `batch` intends, measured and
/// digested.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodedCommit {
    pub digest: BatchDigest,
    pub measurement: EncodedCommitMeasurement,
}

/// Encodes `batch` once: its digest and its measurement.
///
/// # Errors
///
/// A supplied node whose envelope does not parse (its children are counted
/// as edge rows), or [`GraphRefusal::InvalidBody`] for a domain write that
/// is not a graph-reference row.
pub fn encode_commit(batch: &GraphCommitBatch) -> Result<EncodedCommit, GraphRefusal> {
    let mut nodes = TokenWriter::new();
    let mut supplied_node_bytes = 0_u64;
    let mut edge_rows = 0_u64;
    nodes.seq(batch.graph.nodes.len() as u64);
    for (id, node) in &batch.graph.nodes {
        nodes.seq(2);
        nodes.bytes(&id.0);
        nodes.bytes(&node.bytes);
        supplied_node_bytes += node.len();
        edge_rows += node.children()?.len() as u64;
    }

    let mut mutations = TokenWriter::new();
    mutations.seq(6);
    mutations.bytes(&batch.commit_id.0);
    encode_revisions(&mut mutations, &batch.expected);
    mutations.seq(batch.graph.reads.len() as u64);
    for read in &batch.graph.reads {
        mutations.bytes(&read.0);
    }
    mutations.seq(batch.graph.publications.len() as u64);
    for change in &batch.graph.publications {
        encode_change(&mut mutations, change);
    }
    mutations.seq(batch.domain_writes.len() as u64);
    for write in &batch.domain_writes {
        encode_graph_write(&mut mutations, write)?;
    }
    mutations.seq(4);
    mutations.unsigned(batch.budget.encoded_bytes.get());
    mutations.unsigned(batch.budget.mutation_rows.get());
    mutations.unsigned(batch.budget.encode_memory.get());
    mutations.unsigned(batch.budget.failure_reserve_bytes);

    let mut whole = TokenWriter::with_capacity(nodes.len() + mutations.len() + 2);
    whole.seq(2);
    whole.raw(nodes.as_bytes());
    whole.raw(mutations.as_bytes());

    // One row per supplied blob and per child edge, per graph-reference
    // write, and the receipt.
    let planned_mutation_rows = (batch.graph.nodes.len() as u64)
        .saturating_add(edge_rows)
        .saturating_add(batch.domain_writes.len() as u64)
        .saturating_add(1);
    Ok(EncodedCommit {
        digest: BatchDigest::of_canonical(whole.as_bytes()),
        measurement: EncodedCommitMeasurement {
            supplied_node_bytes,
            mutation_encoding_bytes: mutations.len() as u64,
            full_encoded_commit_bytes: whole.len() as u64,
            planned_mutation_rows,
        },
    })
}

fn encode_revisions(out: &mut TokenWriter, revisions: &OwnerRevisionSet) {
    let entries: Vec<_> = revisions.iter().collect();
    out.seq(entries.len() as u64);
    for (owner, revision) in entries {
        out.seq(2);
        encode_owner(out, owner);
        match revision {
            None => out.none(),
            Some(revision) => {
                out.some();
                out.unsigned(revision);
            }
        }
    }
}

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

fn encode_pin(out: &mut TokenWriter, pin: &ReadPin) {
    out.seq(5);
    out.text(&pin.pin_id.0);
    out.text(&pin.lease_owner);
    out.unsigned(pin.generation);
    out.bytes(&pin.root.0);
    out.signed(pin.expires_at.0);
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
        DomainWrite::ReadPin(ReadPinWrite::Acquire { pin }) => {
            out.sum(6, 1);
            encode_pin(out, pin);
        }
        DomainWrite::ReadPin(ReadPinWrite::Retire { pin }) => {
            out.sum(7, 1);
            encode_pin(out, pin);
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
        RootOwner::ReadPin(pin) => {
            out.sum(8, 1);
            out.text(&pin.0);
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

fn invalid(what: &str) -> TokenError {
    TokenError::Invalid(format!("not a stored {what}"))
}

fn session(input: &mut TokenReader<'_>) -> Result<SessionId, TokenError> {
    SessionId::try_from(input.text()?.to_owned()).map_err(|_| invalid("session id"))
}

fn turn(input: &mut TokenReader<'_>) -> Result<TurnId, TokenError> {
    TurnId::try_from(input.text()?.to_owned()).map_err(|_| invalid("turn id"))
}

fn digest(input: &mut TokenReader<'_>) -> Result<[u8; 32], TokenError> {
    <[u8; 32]>::try_from(input.bytes()?).map_err(|_| invalid("32-byte digest"))
}

fn expect(found: u64, expected: u64) -> Result<(), TokenError> {
    TokenReader::expect_fields(found, expected)
}

/// Reads back [`encode_owner`].
///
/// # Errors
///
/// Any token refusal, or a key no owner stores.
pub fn decode_owner(input: &mut TokenReader<'_>) -> Result<RootOwner, TokenError> {
    let (variant, fields) = input.sum()?;
    let owner = match variant {
        0 => {
            expect(fields, 1)?;
            RootOwner::Exec(ExecKey::parse(input.text()?).map_err(|_| invalid("exec key"))?)
        }
        1 => {
            expect(fields, 3)?;
            RootOwner::RunFact {
                owner: OwnerKey::parse(input.text()?).map_err(|_| invalid("owner key"))?,
                run: super::keys::RunSeq(input.unsigned()?),
                ordinal: super::keys::Ordinal(input.unsigned()?),
            }
        }
        2 => {
            expect(fields, 2)?;
            RootOwner::EffectAdmission {
                exec: ExecKey::parse(input.text()?).map_err(|_| invalid("exec key"))?,
                effect: EffectKey(input.bytes()?.into()),
            }
        }
        3 => {
            expect(fields, 2)?;
            RootOwner::TurnPhase {
                session: session(input)?,
                run: turn(input)?,
            }
        }
        4 => {
            expect(fields, 2)?;
            RootOwner::WorkingExecution {
                session: session(input)?,
                run: turn(input)?,
            }
        }
        5 => {
            expect(fields, 3)?;
            RootOwner::TurnNamespace {
                session: session(input)?,
                run: turn(input)?,
                plugin: input.text()?.to_owned(),
            }
        }
        6 => {
            expect(fields, 2)?;
            RootOwner::SessionRevision {
                session: session(input)?,
                revision: input.unsigned()?,
            }
        }
        7 => {
            expect(fields, 1)?;
            RootOwner::SessionAdmissionBase {
                session: session(input)?,
            }
        }
        8 => {
            expect(fields, 1)?;
            RootOwner::ReadPin(ReadPinId(input.text()?.to_owned()))
        }
        9 => {
            expect(fields, 2)?;
            RootOwner::ArtifactBridge {
                namespace: input.text()?.to_owned(),
                artifact: input.text()?.to_owned(),
            }
        }
        10 => {
            expect(fields, 1)?;
            RootOwner::ReleasePin {
                release: input.text()?.to_owned(),
            }
        }
        variant => return Err(TokenError::UnsupportedVariant { variant }),
    };
    Ok(owner)
}

/// A receipt's result bytes: the canonical encoding of what the commit
/// published.
#[must_use]
pub fn encode_published(published: &PublishedGraph) -> Vec<u8> {
    let mut out = TokenWriter::new();
    out.seq(4);
    out.bytes(&published.commit_id.0);
    encode_revisions(&mut out, &published.revisions);
    out.seq(published.roots.len() as u64);
    for (owner, root) in &published.roots {
        out.seq(2);
        encode_owner(&mut out, owner);
        out.bytes(&root.0);
    }
    let installed = &published.installed;
    out.seq(4);
    out.unsigned(installed.newly_installed_canonical_bytes);
    out.unsigned(installed.new_nodes);
    out.unsigned(installed.new_edges);
    out.unsigned(installed.sql_logical_bytes);
    out.into_bytes()
}

/// Reads back [`encode_published`].
///
/// # Errors
///
/// [`GraphRefusal::Backend`]-free token refusals: a receipt that does not
/// decode is corrupt storage.
pub fn decode_published(bytes: &[u8]) -> Result<PublishedGraph, TokenError> {
    let mut input = TokenReader::new(bytes);
    input.expect_struct(4)?;
    let commit_id = CommitId(digest(&mut input)?);
    let count = input.seq()?;
    let mut revisions = OwnerRevisionSet::new();
    for _ in 0..count {
        input.expect_struct(2)?;
        let owner = decode_owner(&mut input)?;
        let revision = if input.option()? {
            Some(input.unsigned()?)
        } else {
            None
        };
        revisions = revisions.expect(owner, revision);
    }
    let count = input.seq()?;
    let mut roots = Vec::new();
    for _ in 0..count {
        input.expect_struct(2)?;
        let owner = decode_owner(&mut input)?;
        roots.push((owner, NodeId(digest(&mut input)?)));
    }
    input.expect_struct(4)?;
    let installed = InstalledGraphMeasurement {
        newly_installed_canonical_bytes: input.unsigned()?,
        new_nodes: input.unsigned()?,
        new_edges: input.unsigned()?,
        sql_logical_bytes: input.unsigned()?,
    };
    input.finish()?;
    Ok(PublishedGraph {
        commit_id,
        revisions,
        roots,
        installed,
    })
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

/// A batch checked before its transaction writes anything: its graph and
/// its measurement.
#[derive(Clone, Debug)]
pub struct CheckedBatch {
    pub encoded: EncodedCommit,
    pub graph: CheckedGraph,
}

/// One supplied node.
#[derive(Clone, Debug)]
pub struct CheckedNode {
    pub id: NodeId,
    pub bytes: std::sync::Arc<[u8]>,
    pub children: Vec<NodeId>,
}

/// Checks `batch` without a store: it carries graph-reference rows only;
/// its publications are exactly the projection of its writes; its graph
/// passes [`check_graph`]. It measures the whole batch once.
///
/// # Errors
///
/// [`GraphRefusal::PublicationMismatch`], [`GraphRefusal::InvalidBody`] for
/// a write that is not a graph-reference row, or any refusal of
/// [`check_graph`].
pub fn check_batch(batch: &GraphCommitBatch) -> Result<CheckedBatch, GraphRefusal> {
    batch.graph.check_projection(&batch.domain_writes)?;
    let graph = check_graph(
        &batch.graph.nodes,
        &batch.graph.reads,
        &batch.graph.publications,
    )?;
    Ok(CheckedBatch {
        encoded: encode_commit(batch)?,
        graph,
    })
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

/// The logical SQL bytes a graph-reference write puts in its row: text as
/// UTF-8, blobs raw, integers 8, NULL 0, each column once; a delete counts
/// its key.
#[must_use]
pub fn graph_write_logical_bytes(write: &DomainWrite) -> u64 {
    const HASH: u64 = 64;
    const INT: u64 = 8;
    let text = |value: &str| value.len() as u64;
    let opt = |node: Option<&NodeId>| node.map_or(0, |_| HASH);
    match write {
        DomainWrite::ExecRoot(ExecRootWrite::Put { exec, .. }) => {
            // exec_key, rev, root_ref, commit_id, written_epoch.
            text(&exec.stored()) + INT + HASH + 32 + INT
        }
        DomainWrite::ExecRoot(ExecRootWrite::Retire { exec, expected: _ }) => {
            text(&exec.stored()) + HASH
        }
        DomainWrite::ExecRoot(ExecRootWrite::Clear { exec }) => text(&exec.stored()),
        DomainWrite::RunFact(RunFactWrite::Append {
            owner, kind, call, ..
        }) => {
            text(&owner.stored())
                + INT
                + INT
                + text(kind.as_str())
                + call.as_ref().map_or(0, |call| text(call.as_str()))
                + HASH
                + INT
        }
        DomainWrite::RunFact(RunFactWrite::Prune { owner, .. }) => text(&owner.stored()) + INT,
        DomainWrite::EffectAdmission(EffectAdmissionWrite::Put {
            exec,
            effect,
            member,
            ..
        }) => {
            text(&exec.stored())
                + effect.0.len() as u64
                + member
                    .as_ref()
                    .map_or(0, |member| text(&member.owner.stored()) + INT + INT)
                + HASH
                + INT
        }
        DomainWrite::EffectAdmission(EffectAdmissionWrite::Remove { exec, effect, .. }) => {
            text(&exec.stored()) + effect.0.len() as u64
        }
        DomainWrite::Turn(super::TurnWrite::Advance { phase, .. }) => {
            opt(phase.checkpoint().as_ref())
        }
        DomainWrite::Turn(super::TurnWrite::Working { working, .. }) => opt(working.as_ref()),
        DomainWrite::Turn(super::TurnWrite::Namespaces { namespaces, .. }) => namespaces
            .iter()
            .map(|namespace| match &namespace.values {
                super::StoredValues::Root(root) => opt(Some(root)),
                super::StoredValues::Base | super::StoredValues::Held => 0,
            })
            .sum(),
        DomainWrite::ReadPin(ReadPinWrite::Acquire { pin }) => {
            text(&pin.pin_id.0) + text(&pin.lease_owner) + INT + HASH + INT
        }
        DomainWrite::ReadPin(ReadPinWrite::Retire { pin }) => text(&pin.pin_id.0),
        DomainWrite::ReleasePin(ReleasePinWrite::Install { release, .. }) => text(release) + HASH,
        DomainWrite::ReleasePin(ReleasePinWrite::Retire { release, .. }) => text(release),
        _ => 0,
    }
}

/// What a collector may spend before it gives up with nothing swept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CollectionLimits {
    /// Blobs it may mark.
    pub nodes: u64,
    /// Blob bytes it may read while marking.
    pub bytes: u64,
}

impl CollectionLimits {
    /// No bound but the catalog's own size.
    pub const UNBOUNDED: Self = Self {
        nodes: u64::MAX,
        bytes: u64::MAX,
    };
}

/// What one complete collection did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GraphCollection {
    /// Root rows enumerated, per source, every source exhausted.
    pub roots: BTreeMap<lash_core_store::checkpoint_graph::RootSource, u64>,
    /// Blobs marked live.
    pub marked: u64,
    /// Edge rows swept (each from an unmarked parent).
    pub swept_edges: u64,
    /// Blobs swept.
    pub swept_blobs: u64,
}
