//! Checkpoint graph rows and the graph commit contract (FIG-5887 §5–§8).
//!
//! The format itself (envelopes, kinds, encoding, decoding, refusals) is
//! `lash_core_store::checkpoint_graph`. This module is what a durable commit
//! does with it: the closed set of root owners, the prepared graph a commit
//! publishes, the graph-reference rows that own roots, the fenced commit and
//! the read-pin contract every store implements.
//!
//! # Contracts
//!
//! - **One fenced transaction.** [`GraphStore::commit_graph`] runs inside a
//!   transaction opened through the lock-order prelude ([`LockStep`]): the
//!   graph gate first, then the fleet guard, node liveness, session history
//!   in sorted session order, the owner fence and revision compare-and-set,
//!   read-pin rows in sorted order, blob inserts in digest order, and only
//!   then edges, roots and domain rows. Every key it will lock is named in
//!   its [`FencedCommit`] before it opens; no category is acquired after a
//!   later one is held.
//! - **Commit order.** Within it: validate revisions, commit identity and
//!   budget; validate hashes, closure, acyclicity and body owners; insert
//!   unique blobs comparing bytes on conflict; insert every child edge once
//!   all blobs exist; append admission, start, wait and outcome facts;
//!   acquire destination roots, update namespace, phase and execution rows,
//!   retire source roots and prune consumed records; commit, then
//!   acknowledge and dispatch. A park is never split across visible commits.
//! - **Root changes are a projection.** [`PreparedGraph::publications`] is
//!   derived from the commit's [`GraphDomainWrite`]s by
//!   [`GraphDomainWrite::publication`]; the commit refuses a batch whose
//!   publications are not exactly that projection
//!   ([`PreparedGraph::check_projection`]), and the owning domain adapter
//!   writes each column once.
//! - **Retry by identity.** The same [`CommitId`] with the same batch digest
//!   is an idempotent retry answered from its receipt; with another digest
//!   it is refused. A receipt never authorizes hydrating a retired root.
//!
//! These types sit beside the inline snapshot, run-record and turn rows they
//! replace; the cutover that removes those is the integrator's.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use lash_core_store::checkpoint_graph::{
    BatchDigest, BlobEncoding, CommitId, DecodeLimits, EncodedCommitMeasurement, EncodedNode,
    GraphCommitBudget, GraphRefusal, InstalledGraphMeasurement, NodeId, NodeSource, RootSource,
    StoredNode,
};
use lash_sansio::{SessionId, ToolCallId, TurnId};

use super::DomainWrite;
use super::keys::{ExecKey, Ordinal, OwnerKey, RunSeq};
use super::snapshots::SnapshotRev;
use crate::ids::{ActorKey, CommitLabel, DurableInstant, Epoch};

/// The PostgreSQL transaction advisory key of the graph gate: the one bigint
/// `0x4c41534847524150` ("LASHGRAP"). Ordinary graph transactions take it
/// shared, the collector exclusive, always first.
pub const GRAPH_GATE_ADVISORY_KEY: i64 = 0x4c41_5348_4752_4150;

/// The prelude's lock categories, in the only order they are acquired.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LockStep {
    /// The graph gate: shared for writers and readers, exclusive for the
    /// collector.
    GraphGate,
    /// Fleet and writer compatibility.
    FleetGuard,
    /// Node liveness and target identity.
    NodeLiveness,
    /// Session-history locks, in sorted session order.
    SessionHistory,
    /// The actor's epoch fence and revision compare-and-set.
    OwnerFence,
    /// Read-pin row locks, in sorted pin order.
    ReadPins,
    /// Blob insert and key-share locks, in digest order.
    Blobs,
    /// Edge, root and domain writes.
    Writes,
}

impl LockStep {
    /// Every step, in acquisition order.
    pub const ORDER: [Self; 8] = [
        Self::GraphGate,
        Self::FleetGuard,
        Self::NodeLiveness,
        Self::SessionHistory,
        Self::OwnerFence,
        Self::ReadPins,
        Self::Blobs,
        Self::Writes,
    ];
}

/// A canonical binary `EffectIdentity`: the token encoding of
/// [`lash_kernel_doc::EffectIdentity`], the `effect_admissions` key.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EffectKey(pub Arc<[u8]>);

/// A read pin's identity.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ReadPinId(pub String);

/// Who owns a root: one row of one [`RootSource`]. Closed: there are no
/// free-form owners.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RootOwner {
    /// `exec_snapshots`.
    Exec(ExecKey),
    /// `run_records`.
    RunFact {
        owner: OwnerKey,
        run: RunSeq,
        ordinal: Ordinal,
    },
    /// `effect_admissions`.
    EffectAdmission { exec: ExecKey, effect: EffectKey },
    /// `turn_phases.checkpoint_ref`.
    TurnPhase { session: SessionId, run: TurnId },
    /// `turn_phases.working_execution_ref`.
    WorkingExecution { session: SessionId, run: TurnId },
    /// `turn_namespaces`.
    TurnNamespace {
        session: SessionId,
        run: TurnId,
        plugin: String,
    },
    /// `session_revisions`.
    SessionRevision { session: SessionId, revision: u64 },
    /// `session_meta.admission_base_checkpoint_ref`.
    SessionAdmissionBase { session: SessionId },
    /// `blob_read_pins`.
    ReadPin(ReadPinId),
    /// SQLite `artifact_refs`; PostgreSQL `lash_lash_vm_artifacts` with
    /// `storage_kind = 1`.
    ArtifactBridge { namespace: String, artifact: String },
    /// `graph_release_pins`.
    ReleasePin { release: String },
}

impl RootOwner {
    /// The source this owner's row belongs to.
    #[must_use]
    pub fn source(&self) -> RootSource {
        match self {
            Self::Exec(_) => RootSource::Exec,
            Self::RunFact { .. } => RootSource::RunFact,
            Self::EffectAdmission { .. } => RootSource::EffectAdmission,
            Self::TurnPhase { .. } => RootSource::TurnPhase,
            Self::WorkingExecution { .. } => RootSource::WorkingExecution,
            Self::TurnNamespace { .. } => RootSource::TurnNamespace,
            Self::SessionRevision { .. } => RootSource::SessionRevision,
            Self::SessionAdmissionBase { .. } => RootSource::SessionAdmissionBase,
            Self::ReadPin(_) => RootSource::ReadPin,
            Self::ArtifactBridge { .. } => RootSource::ArtifactBridge,
            Self::ReleasePin { .. } => RootSource::ReleasePin,
        }
    }
}

/// One root publication or retirement: the checked graph projection of a
/// domain write, never a second instruction to write the same column.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RootChange {
    /// Point `owner` at `root`, replacing `expected` (none for a new row).
    Put {
        owner: RootOwner,
        expected: Option<NodeId>,
        root: NodeId,
    },
    /// Retire `owner`'s root, which must be `expected`.
    Retire { owner: RootOwner, expected: NodeId },
}

/// The owner revisions a commit expects, and after it the ones it produced:
/// each expected revision plus one (none becomes 1).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OwnerRevisionSet {
    revisions: BTreeMap<RootOwner, Option<u64>>,
}

impl OwnerRevisionSet {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Expects `owner` at `revision` (none when it has no row yet).
    #[must_use]
    pub fn expect(mut self, owner: RootOwner, revision: Option<u64>) -> Self {
        self.revisions.insert(owner, revision);
        self
    }

    #[must_use]
    pub fn get(&self, owner: &RootOwner) -> Option<Option<u64>> {
        self.revisions.get(owner).copied()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&RootOwner, Option<u64>)> {
        self.revisions
            .iter()
            .map(|(owner, revision)| (owner, *revision))
    }

    /// The revisions a successful commit leaves: each expected one plus one.
    ///
    /// # Errors
    ///
    /// [`GraphRefusal::LengthOverflow`] at `u64::MAX`.
    pub fn advanced(&self) -> Result<Self, GraphRefusal> {
        let mut revisions = BTreeMap::new();
        for (owner, revision) in &self.revisions {
            let next = revision
                .unwrap_or(0)
                .checked_add(1)
                .ok_or(GraphRefusal::LengthOverflow)?;
            revisions.insert(owner.clone(), Some(next));
        }
        Ok(Self { revisions })
    }
}

/// Everything one commit publishes, frozen before the commit opens: new
/// requests, whole kernel metadata, host summaries and envelope references
/// all encoded already. Every root is a complete graph.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PreparedGraph {
    /// Unique nodes, staged until the commit is acknowledged. A node an
    /// acknowledged root already installed may be omitted; the commit
    /// rechecks every reference and refuses a missing one
    /// ([`GraphRefusal::MissingNode`]) so the caller resends its bytes.
    pub nodes: BTreeMap<NodeId, EncodedNode>,
    /// Existing nodes the commit requires: a requirement, not a proof that
    /// they exist.
    pub reads: BTreeSet<NodeId>,
    /// The root changes, exactly the projection of the commit's writes.
    pub publications: Vec<RootChange>,
    pub encoded: EncodedCommitMeasurement,
}

impl PreparedGraph {
    /// Refuses publications that are not exactly the projection of `writes`,
    /// in order.
    ///
    /// # Errors
    ///
    /// [`GraphRefusal::PublicationMismatch`].
    pub fn check_projection(&self, writes: &[GraphDomainWrite]) -> Result<(), GraphRefusal> {
        let projected: Vec<RootChange> = writes
            .iter()
            .filter_map(GraphDomainWrite::publication)
            .collect();
        if projected == self.publications {
            Ok(())
        } else {
            Err(GraphRefusal::PublicationMismatch)
        }
    }
}

/// One graph commit: identity, expected revisions, the prepared graph, the
/// domain rows it writes, and its explicit budget.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphCommitBatch {
    pub commit_id: CommitId,
    pub expected: OwnerRevisionSet,
    pub graph: PreparedGraph,
    /// Existing domain writes the commit carries: waits, namespaces, mail
    /// and the rest.
    pub domain_writes: Vec<DomainWrite>,
    /// The graph-reference rows the commit writes. (A seam-lane addition:
    /// they join [`DomainWrite`] when the integrator retires the inline
    /// rows they replace.)
    pub graph_writes: Vec<GraphDomainWrite>,
    pub budget: GraphCommitBudget,
}

/// What a successful graph commit published.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishedGraph {
    pub commit_id: CommitId,
    /// The expected revisions, each advanced by one.
    pub revisions: OwnerRevisionSet,
    pub roots: Vec<(RootOwner, NodeId)>,
    pub installed: InstalledGraphMeasurement,
}

/// The transaction a graph commit runs in: the owner fence and every key
/// the prelude locks, known before it opens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FencedCommit {
    pub actor: ActorKey,
    pub epoch: Epoch,
    pub label: CommitLabel,
    /// Sessions whose history the commit locks, locked in this order.
    pub history_sessions: BTreeSet<SessionId>,
    /// Read pins the commit acquires, renews or releases, locked in this
    /// order.
    pub read_pins: BTreeSet<ReadPinId>,
}

/// One stored blob a hydration or pinned fetch read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HydratedNode {
    pub encoding: BlobEncoding,
    pub bytes: Arc<[u8]>,
}

/// An owner's complete root graph, read under one protection: every node
/// reachable from `root`, owned, needing no continuing database pin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HydratedGraph {
    pub owner: RootOwner,
    pub root: NodeId,
    pub nodes: BTreeMap<NodeId, HydratedNode>,
}

impl NodeSource for HydratedGraph {
    fn fetch(&self, id: &NodeId) -> Option<StoredNode<'_>> {
        self.nodes.get(id).map(|node| StoredNode {
            encoding: node.encoding,
            bytes: &node.bytes,
        })
    }
}

/// A reader's lease request for a durable read pin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadLease {
    pub lease_owner: String,
    /// How long the pin lives from the database's clock.
    pub ttl_ms: u64,
}

/// A durable pin on one owner's root, for streaming hydration. Each fetch
/// checks the exact `(pin_id, lease_owner, generation, root)` and expiry
/// under the pin's row lock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadPin {
    pub pin_id: ReadPinId,
    pub lease_owner: String,
    pub generation: u64,
    pub root: NodeId,
    pub expires_at: DurableInstant,
}

/// Nodes one pinned fetch returned, and the pin as it stands after it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinnedBatch {
    pub pin: ReadPin,
    pub nodes: BTreeMap<NodeId, HydratedNode>,
}

impl NodeSource for PinnedBatch {
    fn fetch(&self, id: &NodeId) -> Option<StoredNode<'_>> {
        self.nodes.get(id).map(|node| StoredNode {
            encoding: node.encoding,
            bytes: &node.bytes,
        })
    }
}

/// The graph half of a durable store. Every method takes the graph gate on
/// PostgreSQL; none is sequenced by its caller from smaller operations.
#[async_trait::async_trait]
pub trait GraphStore: Send + Sync {
    /// Applies one graph commit atomically in the order of this module's
    /// contracts, or refuses with nothing written.
    async fn commit_graph(
        &self,
        tx: FencedCommit,
        batch: GraphCommitBatch,
    ) -> Result<PublishedGraph, GraphRefusal>;

    /// Reads `owner`'s complete root graph in one protected read. With
    /// `expected`, refuses a root that is not it
    /// ([`GraphRefusal::RevisionConflict`]); never reads a newer root
    /// mid-hydration.
    async fn hydrate(
        &self,
        owner: RootOwner,
        expected: Option<NodeId>,
        limits: DecodeLimits,
    ) -> Result<HydratedGraph, GraphRefusal>;

    /// Pins `owner`'s current root under the gate and the owner's check.
    async fn acquire_read_pin(
        &self,
        owner: RootOwner,
        lease: ReadLease,
    ) -> Result<ReadPin, GraphRefusal>;

    /// Fetches `nodes`, each reachable from the pinned root, in one
    /// transaction under the pin's lock, optionally renewing it. An expired
    /// pin is refused before anything is read
    /// ([`GraphRefusal::ReadPinExpired`]).
    async fn fetch_pinned(
        &self,
        pin: ReadPin,
        nodes: &[NodeId],
        renew: Option<ReadLease>,
    ) -> Result<PinnedBatch, GraphRefusal>;
}

/// One execution's root row (`exec_snapshots`): the graph successor of
/// [`SnapshotRow`](super::SnapshotRow). Executable identity and format live
/// in the root's ExecRoot descriptor, not in columns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecRootRow {
    pub exec: ExecKey,
    pub rev: SnapshotRev,
    /// An ExecRoot node.
    pub root: NodeId,
    pub written_epoch: Epoch,
    pub commit_id: CommitId,
}

/// An execution root write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecRootWrite {
    /// Replace the execution's root at revision `expected` (root
    /// `expected_root`) with `root` at the next revision.
    Put {
        exec: ExecKey,
        expected: Option<SnapshotRev>,
        expected_root: Option<NodeId>,
        root: NodeId,
        commit_id: CommitId,
    },
    /// Delete the execution's row, whose root must be `expected`.
    Retire { exec: ExecKey, expected: NodeId },
}

/// What a run fact is: the stored `kind` spellings of `run_records`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RunFactKind {
    Admit,
    XStart,
    XOutcome,
    XWait,
    Decide,
    Present,
    Retry,
    /// A cell's report: a reference-only fact naming the original
    /// admissions and outcomes, never a re-admission.
    CellReport,
}

impl RunFactKind {
    pub const ALL: [Self; 8] = [
        Self::Admit,
        Self::XStart,
        Self::XOutcome,
        Self::XWait,
        Self::Decide,
        Self::Present,
        Self::Retry,
        Self::CellReport,
    ];

    /// The stored spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Admit => "admit",
            Self::XStart => "x_start",
            Self::XOutcome => "x_outcome",
            Self::XWait => "x_wait",
            Self::Decide => "decide",
            Self::Present => "present",
            Self::Retry => "retry",
            Self::CellReport => "cell_report",
        }
    }

    /// The RoundFact variant number a fact of this kind holds
    /// (Admit/Start/Wait/Retry/Decide/Present/Outcome/CellReport are 0–7).
    #[must_use]
    pub const fn round_fact_variant(self) -> u32 {
        match self {
            Self::Admit => 0,
            Self::XStart => 1,
            Self::XWait => 2,
            Self::Retry => 3,
            Self::Decide => 4,
            Self::Present => 5,
            Self::XOutcome => 6,
            Self::CellReport => 7,
        }
    }

    /// The stored spelling read back.
    #[must_use]
    pub fn parse(stored: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == stored)
    }
}

/// One stored run fact (`run_records`): the graph successor of
/// [`RunRecordRow`](super::RunRecordRow).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunFactRow {
    pub owner: OwnerKey,
    pub run: RunSeq,
    pub ordinal: Ordinal,
    pub kind: RunFactKind,
    pub call: Option<ToolCallId>,
    /// A RoundFact node.
    pub record: NodeId,
    pub written_epoch: Epoch,
}

/// A run fact write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunFactWrite {
    /// Append one fact, with the ordinal, gap and outcome rules of
    /// [`RunRecordWrite::Append`](super::RunRecordWrite::Append).
    Append {
        owner: OwnerKey,
        run: RunSeq,
        ordinal: Ordinal,
        kind: RunFactKind,
        call: Option<ToolCallId>,
        record: NodeId,
    },
    /// Delete `owner`'s facts of every run before `before`. A range delete:
    /// its retirements are not projected as root changes, and the collector
    /// sees them through the catalog.
    Prune { owner: OwnerKey, before: RunSeq },
}

/// The round member an admission belongs to, when a round admitted it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AdmissionMember {
    pub owner: OwnerKey,
    pub run: RunSeq,
    pub member: u64,
}

/// One effect admission lookup row (`effect_admissions`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectAdmissionRow {
    pub exec: ExecKey,
    pub effect: EffectKey,
    pub member: Option<AdmissionMember>,
    /// An Admission node.
    pub admission: NodeId,
    pub written_epoch: Epoch,
}

/// An effect admission write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EffectAdmissionWrite {
    /// Insert the admission of `effect` under `exec`. Refused with
    /// [`GraphRefusal::DuplicateEffect`] when it exists.
    Put {
        exec: ExecKey,
        effect: EffectKey,
        member: Option<AdmissionMember>,
        admission: NodeId,
    },
    /// Remove the lookup once no live kernel or released member needs it;
    /// outcome and admission edges keep the historical request.
    Remove {
        exec: ExecKey,
        effect: EffectKey,
        expected: NodeId,
    },
}

/// A turn's graph references (`turn_phases`, `turn_namespaces`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnGraphWrite {
    /// Point the unfinished phase at its PhaseRoot, or clear it.
    Phase {
        session: SessionId,
        run: TurnId,
        expected: Option<NodeId>,
        checkpoint: Option<NodeId>,
    },
    /// Replace the post-cell working SessionExecution, or retire it at the
    /// final session transfer.
    Working {
        session: SessionId,
        run: TurnId,
        expected: Option<NodeId>,
        working: Option<NodeId>,
    },
    /// Point a run namespace at its NamespaceValue (or checked opaque
    /// node); none keeps the base-values meaning.
    Namespace {
        session: SessionId,
        run: TurnId,
        plugin: String,
        expected: Option<NodeId>,
        graph_root: Option<NodeId>,
    },
}

/// A read pin write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadPinWrite {
    /// Insert a pin on its root.
    Acquire { pin: ReadPin },
    /// Retire a pin, expired or released, under its row lock.
    Retire { pin: ReadPin },
}

/// A release manifest pin write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReleasePinWrite {
    /// Install a runnable manifest.
    Install { release: String, root: NodeId },
    /// Retire it; live envelope and bundle children keep their own owners.
    Retire { release: String, expected: NodeId },
}

/// One commit receipt (`graph_commit_receipts`): kept for the owner's retry
/// lifetime. It owns no root and authorizes no hydration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitReceiptRow {
    /// The stored key of the owner whose retries it answers.
    pub owner: String,
    pub commit_id: CommitId,
    pub batch_digest: BatchDigest,
    /// The canonical encoding of the [`PublishedGraph`] it answers with.
    pub result_bytes: Arc<[u8]>,
    pub written_epoch: Epoch,
}

/// One graph-reference row write inside a graph commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphDomainWrite {
    ExecRoot(ExecRootWrite),
    RunFact(RunFactWrite),
    EffectAdmission(EffectAdmissionWrite),
    Turn(TurnGraphWrite),
    ReadPin(ReadPinWrite),
    ReleasePin(ReleasePinWrite),
}

fn change(owner: RootOwner, expected: Option<NodeId>, next: Option<NodeId>) -> Option<RootChange> {
    match (expected, next) {
        (expected, Some(root)) => Some(RootChange::Put {
            owner,
            expected,
            root,
        }),
        (Some(expected), None) => Some(RootChange::Retire { owner, expected }),
        (None, None) => None,
    }
}

impl GraphDomainWrite {
    /// The root change this write makes, when it makes exactly one.
    #[must_use]
    pub fn publication(&self) -> Option<RootChange> {
        match self {
            Self::ExecRoot(ExecRootWrite::Put {
                exec,
                expected_root,
                root,
                ..
            }) => change(RootOwner::Exec(exec.clone()), *expected_root, Some(*root)),
            Self::ExecRoot(ExecRootWrite::Retire { exec, expected }) => {
                change(RootOwner::Exec(exec.clone()), Some(*expected), None)
            }
            Self::RunFact(RunFactWrite::Append {
                owner,
                run,
                ordinal,
                record,
                ..
            }) => change(
                RootOwner::RunFact {
                    owner: owner.clone(),
                    run: *run,
                    ordinal: *ordinal,
                },
                None,
                Some(*record),
            ),
            Self::RunFact(RunFactWrite::Prune { .. }) => None,
            Self::EffectAdmission(EffectAdmissionWrite::Put {
                exec,
                effect,
                admission,
                ..
            }) => change(
                RootOwner::EffectAdmission {
                    exec: exec.clone(),
                    effect: effect.clone(),
                },
                None,
                Some(*admission),
            ),
            Self::EffectAdmission(EffectAdmissionWrite::Remove {
                exec,
                effect,
                expected,
            }) => change(
                RootOwner::EffectAdmission {
                    exec: exec.clone(),
                    effect: effect.clone(),
                },
                Some(*expected),
                None,
            ),
            Self::Turn(TurnGraphWrite::Phase {
                session,
                run,
                expected,
                checkpoint,
            }) => change(
                RootOwner::TurnPhase {
                    session: session.clone(),
                    run: run.clone(),
                },
                *expected,
                *checkpoint,
            ),
            Self::Turn(TurnGraphWrite::Working {
                session,
                run,
                expected,
                working,
            }) => change(
                RootOwner::WorkingExecution {
                    session: session.clone(),
                    run: run.clone(),
                },
                *expected,
                *working,
            ),
            Self::Turn(TurnGraphWrite::Namespace {
                session,
                run,
                plugin,
                expected,
                graph_root,
            }) => change(
                RootOwner::TurnNamespace {
                    session: session.clone(),
                    run: run.clone(),
                    plugin: plugin.clone(),
                },
                *expected,
                *graph_root,
            ),
            Self::ReadPin(ReadPinWrite::Acquire { pin }) => {
                change(RootOwner::ReadPin(pin.pin_id.clone()), None, Some(pin.root))
            }
            Self::ReadPin(ReadPinWrite::Retire { pin }) => {
                change(RootOwner::ReadPin(pin.pin_id.clone()), Some(pin.root), None)
            }
            Self::ReleasePin(ReleasePinWrite::Install { release, root }) => change(
                RootOwner::ReleasePin {
                    release: release.clone(),
                },
                None,
                Some(*root),
            ),
            Self::ReleasePin(ReleasePinWrite::Retire { release, expected }) => change(
                RootOwner::ReleasePin {
                    release: release.clone(),
                },
                Some(*expected),
                None,
            ),
        }
    }
}

#[cfg(test)]
#[path = "graph_tests.rs"]
mod tests;
