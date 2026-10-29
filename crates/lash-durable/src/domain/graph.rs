//! Checkpoint graph rows and the graph commit contract (FIG-5887 §5–§8).
//!
//! The format itself (envelopes, kinds, encoding, decoding, refusals) is
//! `lash_core_store::checkpoint_graph`. This module is what a durable commit
//! does with it: the closed set of root owners, the graph-reference rows
//! that own roots, the owner transaction, and whole graph hydration.
//!
//! # Contracts
//!
//! An owner's [`ActorTx`](crate::ActorTx) stages its graph and publishes it
//! in one fenced transaction. The graph gate precedes the owner fence;
//! nodes are checked for hashes, closure, acyclicity and body ownership,
//! then installed before edges and domain rows publish their roots. A
//! refusal rolls back the nodes, edges and domain rows together.
//!
//! Root changes are derived directly from [`DomainWrite::publications`],
//! and the owning domain adapter writes each column once. A park is never
//! split across visible commits. Graph hydration and collection share the
//! graph gate.

use std::collections::BTreeMap;
use std::sync::Arc;

use lash_core_store::checkpoint_graph::{
    BlobEncoding, CommitId, DecodeLimits, EncodedNode, NodeId, NodeSource, RootSource, StoredNode,
};
use lash_kernel_value::identity::SessionId;
use lash_protocol_types::ToolCallId;
use lash_vocabulary::identity::TurnId;

use super::DomainWrite;
use super::keys::{ExecKey, Ordinal, OwnerKey, RunSeq};
use super::snapshots::SnapshotRev;
use super::turns::{StoredValues, TurnWrite};
use crate::ids::Epoch;

/// The PostgreSQL transaction advisory key of the graph gate: the one bigint
/// `0x4c41534847524150` ("LASHGRAP"). Ordinary graph transactions take it
/// shared, the collector exclusive, always first.
pub const GRAPH_GATE_ADVISORY_KEY: i64 = 0x4c41_5348_4752_4150;

/// The limits a store reads its own rows' graphs within (a run's facts, a
/// namespace's value): every one was admitted under a commit budget when it
/// was written, so these bound only corruption.
pub const OWNER_READ_LIMITS: DecodeLimits = DecodeLimits::within_total(1 << 32);

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
    /// Blob insert and key-share locks, in digest order.
    Blobs,
    /// Edge, root and domain writes.
    Writes,
}

impl LockStep {
    /// Every step, in acquisition order.
    pub const ORDER: [Self; 7] = [
        Self::GraphGate,
        Self::FleetGuard,
        Self::NodeLiveness,
        Self::SessionHistory,
        Self::OwnerFence,
        Self::Blobs,
        Self::Writes,
    ];
}

/// A canonical binary `EffectId`: the token encoding of
/// [`lash_kernel_value::sites::EffectId`], the `effect_admissions` key.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EffectKey(pub Arc<[u8]>);

/// Who owns a root: one row of one [`RootSource`]. Closed: there are no
/// free-form owners.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RootOwner {
    /// `exec_snapshots`.
    Exec(ExecKey),
    /// A retained model call, addressed exactly as its prompt.
    ModelCall(super::PromptCallKey),
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
    /// `turn_phases.answer_ref`.
    ModelAnswer { session: SessionId, run: TurnId },
    /// `session_runs.terminal_transcript_ref`.
    RunTranscript { session: SessionId, run: TurnId },
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
    /// SQLite `artifact_refs`; PostgreSQL `lash_lash_vm_artifacts` with
    /// `storage_kind = 1`: a kernel document's or a helper declaration's
    /// row, which roots its node for the referrers whose edges hold the row
    /// (FIG-6469).
    ArtifactBridge { namespace: String, artifact: String },
}

impl RootOwner {
    /// The source this owner's row belongs to.
    #[must_use]
    pub fn source(&self) -> RootSource {
        match self {
            Self::Exec(_) => RootSource::Exec,
            Self::ModelCall(_) => RootSource::ModelCall,
            Self::RunFact { .. } => RootSource::RunFact,
            Self::EffectAdmission { .. } => RootSource::EffectAdmission,
            Self::TurnPhase { .. } => RootSource::TurnPhase,
            Self::WorkingExecution { .. } => RootSource::WorkingExecution,
            Self::ModelAnswer { .. } => RootSource::ModelAnswer,
            Self::RunTranscript { .. } => RootSource::RunTranscript,
            Self::TurnNamespace { .. } => RootSource::TurnNamespace,
            Self::SessionRevision { .. } => RootSource::SessionRevision,
            Self::SessionAdmissionBase { .. } => RootSource::SessionAdmissionBase,
            Self::ArtifactBridge { .. } => RootSource::ArtifactBridge,
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

/// One stored blob a hydration read.
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

/// One execution's root row (`exec_snapshots`). Executable identity and
/// format live in the root's ExecRoot descriptor, not in columns.
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
    /// Delete the execution's row, whose root must be `expected`: its
    /// consumer took what it ended with. A cell's own run facts and effect
    /// admissions go with it; its report is among its turn's facts.
    Retire { exec: ExecKey, expected: NodeId },
    /// Delete the execution's row, whichever root it holds, if it has one:
    /// its owner's terminal, under the owner's fence. An owner commit only.
    Clear { exec: ExecKey },
}

/// What a run fact is: the stored `kind` spellings of `run_records`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RunFactKind {
    Admit,
    XStart,
    XOutcome,
    XWait,
    XCancel,
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
        Self::XCancel,
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
            Self::XCancel => "x_cancel",
            Self::Present => "present",
            Self::Retry => "retry",
            Self::CellReport => "cell_report",
        }
    }

    /// The RoundFact variant number a fact of this kind holds
    /// (Admit/Start/Wait/Retry are 0–3, Present/Outcome/CellReport 5–7;
    /// 4 is retired).
    #[must_use]
    pub const fn round_fact_variant(self) -> u32 {
        match self {
            Self::Admit => 0,
            Self::XStart => 1,
            Self::XWait => 2,
            Self::Retry => 3,
            Self::Present => 5,
            Self::XOutcome => 6,
            Self::CellReport => 7,
            Self::XCancel => 8,
        }
    }

    /// The stored spelling read back.
    #[must_use]
    pub fn parse(stored: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == stored)
    }
}

/// One stored run fact (`run_records`).
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

/// Every run fact of one owner, ordered by run and ordinal, and every node
/// they reach, read in one protected read: what a fold decodes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunFacts {
    pub read_limits: DecodeLimits,
    pub rows: Vec<RunFactRow>,
    pub nodes: BTreeMap<NodeId, HydratedNode>,
}

impl Default for RunFacts {
    fn default() -> Self {
        Self {
            rows: Vec::new(),
            nodes: BTreeMap::new(),
            read_limits: OWNER_READ_LIMITS,
        }
    }
}
impl RunFacts {
    /// The facts `writes` append, with `staged`, the nodes their commit
    /// stages, as the store holds them once the commit lands at `epoch`.
    #[must_use]
    pub fn appended(
        writes: &[DomainWrite],
        staged: &BTreeMap<NodeId, EncodedNode>,
        epoch: Epoch,
    ) -> Self {
        let rows: Vec<_> = writes
            .iter()
            .filter_map(|write| match write {
                DomainWrite::RunFact(RunFactWrite::Append {
                    owner,
                    run,
                    ordinal,
                    kind,
                    call,
                    record,
                }) => Some(RunFactRow {
                    owner: owner.clone(),
                    run: *run,
                    ordinal: *ordinal,
                    kind: *kind,
                    call: call.clone(),
                    record: *record,
                    written_epoch: epoch,
                }),
                _ => None,
            })
            .collect();
        let nodes = if rows.is_empty() {
            BTreeMap::new()
        } else {
            staged
                .iter()
                .map(|(id, node)| {
                    (
                        *id,
                        HydratedNode {
                            encoding: BlobEncoding::Graph,
                            bytes: Arc::clone(&node.bytes),
                        },
                    )
                })
                .collect()
        };
        Self {
            rows,
            nodes,
            read_limits: OWNER_READ_LIMITS,
        }
    }

    /// These facts with `node`, a page their commit installs from a
    /// candidate's manifest rather than its staged nodes, as the store
    /// holds it once the commit lands.
    #[must_use]
    pub fn with_published(mut self, node: &EncodedNode) -> Self {
        self.nodes.insert(
            node.id,
            HydratedNode {
                encoding: BlobEncoding::Graph,
                bytes: Arc::clone(&node.bytes),
            },
        );
        self
    }

    /// These facts and `other`'s, rows in order.
    pub fn extend(&mut self, other: Self) {
        self.rows.extend(other.rows);
        self.rows.sort_by_key(|row| (row.run, row.ordinal));
        self.nodes.extend(other.nodes);
    }

    /// Only the facts of run `run`.
    #[must_use]
    pub fn of_run(mut self, run: RunSeq) -> Self {
        self.rows.retain(|row| row.run == run);
        self
    }

    /// Whether there are no facts.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

impl NodeSource for RunFacts {
    fn fetch(&self, id: &NodeId) -> Option<StoredNode<'_>> {
        self.nodes.get(id).map(|node| StoredNode {
            encoding: node.encoding,
            bytes: &node.bytes,
        })
    }
}

/// A run fact write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunFactWrite {
    /// Append one fact. Refused with
    /// [`DomainRefusal::RunOrdinalTaken`](super::DomainRefusal::RunOrdinalTaken)
    /// when its ordinal is taken, with
    /// [`DomainRefusal::RunOrdinalGap`](super::DomainRefusal::RunOrdinalGap)
    /// when the run has no fact at the ordinal before it, and an outcome with
    /// [`DomainRefusal::OutcomeExists`](super::DomainRefusal::OutcomeExists)
    /// when its call already has one.
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

impl DomainWrite {
    /// The root changes this write makes, in order: none for a write that
    /// owns no root, or for a range delete the collector sees through the
    /// catalog.
    #[must_use]
    pub fn publications(&self) -> Vec<RootChange> {
        let one = |change: Option<RootChange>| change.into_iter().collect();
        match self {
            Self::ModelCall(write) => one(change(
                RootOwner::ModelCall(write.key.clone()),
                None,
                Some(write.record),
            )),
            Self::ExecRoot(ExecRootWrite::Put {
                exec,
                expected_root,
                root,
                ..
            }) => one(change(
                RootOwner::Exec(exec.clone()),
                *expected_root,
                Some(*root),
            )),
            Self::ExecRoot(ExecRootWrite::Retire { exec, expected }) => {
                one(change(RootOwner::Exec(exec.clone()), Some(*expected), None))
            }
            Self::ExecRoot(ExecRootWrite::Clear { .. }) => Vec::new(),
            Self::RunFact(RunFactWrite::Append {
                owner,
                run,
                ordinal,
                record,
                ..
            }) => one(change(
                RootOwner::RunFact {
                    owner: owner.clone(),
                    run: *run,
                    ordinal: *ordinal,
                },
                None,
                Some(*record),
            )),
            Self::EffectAdmission(EffectAdmissionWrite::Put {
                exec,
                effect,
                admission,
                ..
            }) => one(change(
                RootOwner::EffectAdmission {
                    exec: exec.clone(),
                    effect: effect.clone(),
                },
                None,
                Some(*admission),
            )),
            Self::EffectAdmission(EffectAdmissionWrite::Remove {
                exec,
                effect,
                expected,
            }) => one(change(
                RootOwner::EffectAdmission {
                    exec: exec.clone(),
                    effect: effect.clone(),
                },
                Some(*expected),
                None,
            )),
            Self::Turn(TurnWrite::Advance {
                session,
                run,
                phase,
                ..
            }) => one(change(
                RootOwner::TurnPhase {
                    session: session.clone(),
                    run: run.clone(),
                },
                None,
                Some(phase.checkpoint()),
            )),
            Self::Turn(TurnWrite::Answer {
                session,
                run,
                answer,
            }) => one(change(
                RootOwner::ModelAnswer {
                    session: session.clone(),
                    run: run.clone(),
                },
                None,
                Some(*answer),
            )),
            Self::Turn(TurnWrite::Working {
                session,
                run,
                expected,
                working,
            }) => one(change(
                RootOwner::WorkingExecution {
                    session: session.clone(),
                    run: run.clone(),
                },
                *expected,
                *working,
            )),
            Self::Turn(TurnWrite::Namespaces {
                session,
                run,
                namespaces,
            }) => namespaces
                .iter()
                .filter_map(|namespace| match &namespace.values {
                    StoredValues::Root(root) => change(
                        RootOwner::TurnNamespace {
                            session: session.clone(),
                            run: run.clone(),
                            plugin: namespace.plugin.clone(),
                        },
                        None,
                        Some(*root),
                    ),
                    StoredValues::Base | StoredValues::Held => None,
                })
                .collect(),
            Self::Turn(TurnWrite::Terminal {
                session,
                run,
                transcript,
                ..
            }) => one(change(
                RootOwner::RunTranscript {
                    session: session.clone(),
                    run: run.clone(),
                },
                None,
                *transcript,
            )),
            Self::Environment(_)
            | Self::RunFact(RunFactWrite::Prune { .. })
            | Self::Turn(_)
            | Self::SessionCommit(_)
            | Self::Wait(_)
            | Self::Process(_)
            | Self::SessionClose(_)
            | Self::ActorPark { .. }
            | Self::SessionMail(_)
            | Self::Prompt(_)
            | Self::ModelCallPin(_)
            | Self::RegisterExecution(_)
            | Self::EndExecution(_)
            | Self::AdoptExecution(_)
            | Self::SeedResolution(_) => Vec::new(),
        }
    }
}
