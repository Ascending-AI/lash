//! The checkpoint graph's store laws (FIG-5887 §11: PC-ATOMIC, PC-ROOTS,
//! PC-READ and PC-SIZE; FIG-5949: R-LIVE, R-DEAD and the orphan audit),
//! written once and run by each dialect over its own store through a
//! [`GraphTier`].
//!
//! Each law takes a fresh store. Graphs are built from FrozenValue nodes
//! whose large texts are placed in shared TextLeaf nodes, so two roots that
//! name one text share one leaf.
//!
//! The reachability mark the stores no longer run is the laws' oracle
//! ([`exactly_the_reachable_nodes_are_stored`]): after a commit, every node
//! the catalog's roots reach along stored edges is stored (R-LIVE), and
//! every stored node is one they reach (R-DEAD).

use std::collections::{BTreeMap, BTreeSet};

use lash_core_store::checkpoint_graph::{
    CommitId, DecodeLimits, EncodeBudget, EncodedNode, ExplicitLimit, GraphCommitBudget,
    GraphRefusal, GraphStaging, NodeId, NodeKind, RootSource, TokenWriter, TypedBody,
};
use lash_sansio::{SessionId, ToolCallId, TurnId};

use super::{LABEL, LawBroken, LawOutcome, create, ensure, node, session};
use crate::domain::graph::{
    EffectAdmissionWrite, EffectKey, ExecRootWrite, HydratedGraph, ReleasePinWrite, RootOwner,
    RunFactKind, RunFactWrite,
};
use crate::domain::graph_commit::measure_owner_commit;
use crate::domain::namespace_value::NamespaceValue;
use crate::domain::{
    CellId, DomainRefusal, ExecKey, Ordinal, OwnerKey, RunSeq, SnapshotRev, StoredNamespaceWrite,
    StoredValues, TurnWrite, UnfinishedPhase,
};
use crate::{ActorKey, DomainWrite, DurableError, DurableStore, Epoch};

mod races;
pub use races::*;

/// Foreign data a law writes past the store's own checks, or a row of a
/// root source whose writer is not a graph commit yet.
#[derive(Clone, Debug)]
pub enum ForeignWrite {
    /// An opaque blob (encoding 0) at `hash`.
    OpaqueBlob { hash: String, bytes: Vec<u8> },
    /// Replace a stored blob's bytes.
    OverwriteBlob { node: NodeId, bytes: Vec<u8> },
    /// Point session revision `revision` at `root`, or drop it.
    RevisionRoot {
        config: bool,
        session: SessionId,
        revision: u64,
        root: Option<NodeId>,
    },
    /// Point the session's admission base at `root`, or drop its row.
    AdmissionBase {
        session: SessionId,
        root: Option<NodeId>,
    },
    /// Point an artifact bridge at `root`, or drop it.
    ArtifactBridge {
        namespace: String,
        artifact: String,
        root: Option<NodeId>,
    },
    /// Point run `run`'s terminal window's checkpoint at `root`, or drop
    /// the run's row.
    TerminalWindow {
        config: bool,
        session: SessionId,
        run: TurnId,
        root: Option<NodeId>,
    },
}

/// One dialect's store, its orphan audit and the raw access the laws need.
#[async_trait::async_trait]
pub trait GraphTier: Send + Sync {
    fn store(&self) -> &dyn DurableStore;
    /// Runs the orphan audit to its end: how many blobs it deleted.
    async fn audit(&self) -> Result<u64, GraphRefusal>;
    /// A stored blob's bytes and encoding.
    async fn blob(&self, hash: &str) -> Option<(Vec<u8>, u8)>;
    /// A node's stored edges, in digest order.
    async fn edges(&self, parent: &NodeId) -> Vec<NodeId>;
    /// Every stored blob's digest.
    async fn blobs(&self) -> BTreeSet<String>;
    /// Every root the catalog names, one per root row.
    async fn roots(&self) -> Vec<String>;
    async fn foreign(&self, write: ForeignWrite);
}

/// The nodes the catalog's roots reach along stored edges: the mark the
/// stores' collector ran, kept here as the laws' oracle.
async fn reached(tier: &dyn GraphTier) -> BTreeSet<String> {
    let mut marked = BTreeSet::new();
    let mut stack = tier.roots().await;
    while let Some(hash) = stack.pop() {
        if marked.insert(hash.clone())
            && let Some(id) = NodeId::from_hex(&hash)
        {
            stack.extend(tier.edges(&id).await.iter().map(NodeId::to_hex));
        }
    }
    marked
}

/// R-LIVE and R-DEAD after `what` committed: every node a root reaches is
/// stored, and every stored node is one a root reaches, but `orphans`: what
/// the law expects left for the orphan audit.
///
/// # Errors
///
/// The first node out of place.
pub async fn exactly_the_reachable_nodes_are_stored(
    tier: &dyn GraphTier,
    what: &str,
    orphans: &BTreeSet<String>,
) -> LawOutcome {
    let live = reached(tier).await;
    let stored = tier.blobs().await;
    if let Some(missing) = live.difference(&stored).next() {
        return Err(LawBroken(format!(
            "R-LIVE after {what}: {missing} is reachable and not stored"
        )));
    }
    if let Some(dead) = stored
        .difference(&live)
        .find(|hash| !orphans.contains(*hash))
    {
        return Err(LawBroken(format!(
            "R-DEAD after {what}: {dead} is stored and no root reaches it"
        )));
    }
    Ok(())
}

/// [`exactly_the_reachable_nodes_are_stored`] with no orphan expected.
async fn exact(tier: &dyn GraphTier, what: &str) -> LawOutcome {
    exactly_the_reachable_nodes_are_stored(tier, what, &BTreeSet::new()).await
}

fn refused(what: &str, error: impl std::fmt::Display) -> LawBroken {
    LawBroken(format!("{what}: unexpected refusal {error}"))
}

fn staging() -> GraphStaging {
    GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)))
}

/// A text long enough to be placed in its own shared leaf.
fn big(tag: &str) -> String {
    format!("{tag}:{}", "x".repeat(600))
}

/// One FrozenValue node naming `texts` (placed when large) and `refs`.
fn value(staging: &mut GraphStaging, texts: &[&str], refs: &[NodeId]) -> NodeId {
    let mut out = TokenWriter::new();
    out.seq((texts.len() + refs.len()) as u64);
    for text in texts {
        out.text(text);
    }
    for node in refs {
        out.child(node.child_ref());
    }
    staging
        .encode(NodeKind::FROZEN_VALUE, TypedBody::logical(out.as_bytes()))
        .unwrap_or_else(|error| panic!("a law's node encodes: {error}"))
}

/// The leaf a large text is placed in.
fn leaf(text: &str) -> NodeId {
    let mut out = TokenWriter::new();
    out.text(text);
    staging()
        .encode(NodeKind::TEXT_LEAF, TypedBody::logical(out.as_bytes()))
        .unwrap_or_else(|error| panic!("a leaf encodes: {error}"))
}

fn roomy() -> GraphCommitBudget {
    GraphCommitBudget {
        encoded_bytes: ExplicitLimit::new(u64::MAX / 4),
        mutation_rows: ExplicitLimit::new(u64::MAX / 4),
        encode_memory: ExplicitLimit::new(u64::MAX / 4),
        failure_reserve_bytes: 0,
    }
}

/// A graph refusal as the port carried it.
fn graph_refusal(error: DurableError) -> GraphRefusal {
    match error {
        DurableError::Graph(refusal) => refusal,
        DurableError::OwnershipLost(_) => GraphRefusal::FenceLost,
        DurableError::Store(failure) => GraphRefusal::Backend(failure),
        other => GraphRefusal::Backend(crate::StoreFailure {
            kind: crate::StoreFailureKind::Corrupt,
            message: other.to_string(),
        }),
    }
}

/// `owner`'s complete root graph, as the port hydrates it.
async fn hydrate(
    tier: &dyn GraphTier,
    owner: RootOwner,
    expected: Option<NodeId>,
    limits: DecodeLimits,
) -> Result<HydratedGraph, GraphRefusal> {
    tier.store()
        .hydrate(owner, expected, limits)
        .await
        .map_err(graph_refusal)
}

/// Commits `writes` with `nodes` staged through the owner's own fenced
/// transaction: how turn rows publish their roots.
async fn owner_commit(
    tier: &dyn GraphTier,
    owner: &Owned,
    nodes: BTreeMap<NodeId, EncodedNode>,
    writes: Vec<DomainWrite>,
) -> Result<(), DurableError> {
    let store = tier.store();
    let mut tx = store.begin(&owner.actor, owner.epoch).await?;
    tx.stage(nodes.into_values());
    for write in writes {
        tx.write(write);
    }
    store.commit(tx, LABEL).await.map(|_| ())
}

/// A session actor the law owns, and its fence.
struct Owned {
    actor: ActorKey,
    epoch: Epoch,
    session: SessionId,
}

impl Owned {
    fn exec(&self, cell: &str) -> ExecKey {
        ExecKey::Cell(self.session.clone(), turn("graph-turn"), CellId::new(cell))
    }
}

fn turn(id: &str) -> TurnId {
    TurnId::try_from(id.to_owned()).unwrap_or_else(|_| panic!("a constant turn id"))
}

async fn owned(tier: &dyn GraphTier, id: &str) -> Result<Owned, LawBroken> {
    let store = tier.store();
    let actor = session(id)?;
    create(store, std::slice::from_ref(&actor)).await?;
    let lease = node(store, &format!("{id}-node")).await?;
    let claimed = store.claim(&lease, 1).await?;
    let epoch = claimed
        .first()
        .map(|claimed| claimed.epoch)
        .ok_or_else(|| LawBroken(format!("{id} was not claimed")))?;
    Ok(Owned {
        actor,
        epoch,
        session: SessionId::try_from(id.to_owned())
            .map_err(|_| LawBroken("a constant session id".into()))?,
    })
}

async fn present(tier: &dyn GraphTier, node: &NodeId) -> bool {
    tier.blob(&node.to_hex()).await.is_some()
}

fn put_exec(exec: &ExecKey, expected: Option<(u64, NodeId)>, root: NodeId) -> DomainWrite {
    DomainWrite::ExecRoot(ExecRootWrite::Put {
        exec: exec.clone(),
        expected: expected.map(|(rev, _)| SnapshotRev(rev)),
        expected_root: expected.map(|(_, root)| root),
        root,
        commit_id: CommitId::mint(),
    })
}

/// PC-ATOMIC: a refused commit leaves no node, edge, fact, admission or
/// root behind, whatever refused it.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_refused_owner_graph_commit_writes_nothing(tier: &dyn GraphTier) -> LawOutcome {
    let owner = owned(tier, "graph-atomic").await?;
    let exec = &owner.exec("cell");
    let facts = OwnerKey::Cell(
        owner.session.clone(),
        turn("graph-turn"),
        CellId::new("cell"),
    );
    let mut nodes = staging();
    let root = value(&mut nodes, &[&big("atomic-a"), &big("atomic-b")], &[]);
    let admission = value(&mut nodes, &[&big("atomic-request")], &[]);
    let fact = value(&mut nodes, &["fact"], &[admission]);
    let nodes = nodes.into_nodes();
    let writes = |expected: Option<(u64, NodeId)>| {
        vec![
            DomainWrite::RunFact(RunFactWrite::Append {
                owner: facts.clone(),
                run: RunSeq(0),
                ordinal: Ordinal(0),
                kind: RunFactKind::Admit,
                call: None,
                record: fact,
            }),
            DomainWrite::EffectAdmission(EffectAdmissionWrite::Put {
                exec: exec.clone(),
                effect: EffectKey(vec![1, 2, 3].into()),
                member: None,
                admission,
            }),
            put_exec(exec, expected, root),
        ]
    };
    let assert_untouched = |what: &'static str| {
        let nodes = nodes.clone();
        let facts = facts.clone();
        async move {
            for id in nodes.keys() {
                ensure!(
                    !present(tier, id).await,
                    "{what}: node {id} was installed by a refused commit"
                );
                ensure!(
                    tier.edges(id).await.is_empty(),
                    "{what}: edges of {id} were installed by a refused commit"
                );
            }
            for root_owner in [
                RootOwner::Exec(exec.clone()),
                RootOwner::RunFact {
                    owner: facts.clone(),
                    run: RunSeq(0),
                    ordinal: Ordinal(0),
                },
                RootOwner::EffectAdmission {
                    exec: exec.clone(),
                    effect: EffectKey(vec![1, 2, 3].into()),
                },
            ] {
                let hydrated = hydrate(
                    tier,
                    root_owner.clone(),
                    None,
                    DecodeLimits::within_total(1 << 30),
                )
                .await;
                ensure!(
                    hydrated == Err(GraphRefusal::RootRetired),
                    "{what}: a refused commit published {root_owner:?}: {hydrated:?}"
                );
            }
            Ok::<_, LawBroken>(())
        }
    };

    // The last write conflicts, after every blob, edge, fact and admission
    // was written in the transaction.
    let answer = owner_commit(tier, &owner, nodes.clone(), writes(Some((7, root)))).await;
    ensure!(
        answer
            == Err(DurableError::Domain(DomainRefusal::SnapshotRevConflict {
                exec: exec.clone(),
                expected: Some(SnapshotRev(7)),
                found: None,
            })),
        "a stale execution revision answered {answer:?}"
    );
    assert_untouched("a late conflict").await?;

    // A child neither supplied nor stored.
    let mut orphan = staging();
    let absent = leaf(&big("never-stored"));
    let dangling = value(&mut orphan, &["dangling"], &[absent]);
    let mut orphan_nodes = nodes.clone();
    if let Some(node) = orphan.get(&dangling) {
        orphan_nodes.insert(dangling, node.clone());
    }
    let mut orphan_writes = writes(None);
    orphan_writes.push(DomainWrite::ReleasePin(ReleasePinWrite::Install {
        release: "dangling".into(),
        root: dangling,
    }));
    let answer = owner_commit(tier, &owner, orphan_nodes, orphan_writes).await;
    ensure!(
        answer == Err(DurableError::Graph(GraphRefusal::MissingNode(absent))),
        "a dangling child answered {answer:?}"
    );
    assert_untouched("a dangling child").await?;
    ensure!(
        !present(tier, &dangling).await && tier.edges(&dangling).await.is_empty(),
        "a refused owner commit installed the dangling node or its edges"
    );
    let released = hydrate(
        tier,
        RootOwner::ReleasePin {
            release: "dangling".into(),
        },
        None,
        DecodeLimits::within_total(1 << 30),
    )
    .await;
    ensure!(
        released == Err(GraphRefusal::RootRetired),
        "a refused owner commit published a dangling pin"
    );

    // An opaque blob stored under a node's identity is never reclassified.
    tier.foreign(ForeignWrite::OpaqueBlob {
        hash: fact.to_hex(),
        bytes: b"opaque".to_vec(),
    })
    .await;
    let answer = owner_commit(tier, &owner, nodes.clone(), writes(None)).await;
    ensure!(
        answer == Err(DurableError::Graph(GraphRefusal::EncodingConflict(fact))),
        "an opaque blob under a node's identity answered {answer:?}"
    );
    // The opaque blob is an orphan no root names: the audit takes it.
    tier.audit()
        .await
        .map_err(|error| refused("the audit", error))?;
    assert_untouched("an encoding conflict").await?;

    owner_commit(tier, &owner, nodes.clone(), writes(None))
        .await
        .map_err(|error| refused("the valid graph", error))?;
    for (id, node) in &nodes {
        ensure!(
            tier.edges(id).await == node.children().map_err(|e| refused("children", e))?,
            "node {id}'s stored edges are not its child table"
        );
    }
    Ok(())
}

/// PC-ATOMIC: an owner cannot append a second outcome for one call;
/// the refused transaction publishes no second fact.
///
/// # Errors
///
/// The first rule broken.
pub async fn an_owner_refuses_a_second_outcome_for_one_call(tier: &dyn GraphTier) -> LawOutcome {
    let owner = owned(tier, "graph-retry").await?;
    let facts = OwnerKey::Cell(
        owner.session.clone(),
        turn("graph-turn"),
        CellId::new("retry"),
    );
    let mut nodes = staging();
    let outcome = value(&mut nodes, &[&big("retry-outcome")], &[]);
    let call = ToolCallId::parse(&format!("tc_{}", "ab".repeat(32)))
        .map_err(|e| LawBroken(e.to_string()))?;
    let append = |ordinal, record| {
        DomainWrite::RunFact(RunFactWrite::Append {
            owner: facts.clone(),
            run: RunSeq(0),
            ordinal: Ordinal(ordinal),
            kind: RunFactKind::XOutcome,
            call: Some(call.clone()),
            record,
        })
    };
    let nodes = nodes.into_nodes();
    owner_commit(tier, &owner, nodes.clone(), vec![append(0, outcome)])
        .await
        .map_err(|error| refused("the first outcome", error))?;
    let answer = owner_commit(tier, &owner, nodes, vec![append(1, outcome)]).await;
    ensure!(
        answer
            == Err(DurableError::Domain(DomainRefusal::OutcomeExists {
                owner: facts.clone(),
                run: RunSeq(0),
                call: call.clone()
            })),
        "a second outcome for one call answered {answer:?}"
    );
    let hydrated = hydrate(
        tier,
        RootOwner::RunFact {
            owner: facts.clone(),
            run: RunSeq(0),
            ordinal: Ordinal(1),
        },
        None,
        DecodeLimits::within_total(1 << 30),
    )
    .await;
    ensure!(
        hydrated == Err(GraphRefusal::RootRetired),
        "a refused or retried commit appended a second fact: {hydrated:?}"
    );
    Ok(())
}

/// PC-ROOTS, R-LIVE and R-DEAD (FIG-5949): every root source retains its
/// whole graph on its own; the transaction that retires its root peels the
/// closure only that root reached, so after every commit exactly the
/// reachable nodes are stored; and a leaf two owners share outlives either
/// one. A source whose rows no graph commit writes is retired by a foreign
/// write here, which peels nothing: the orphan audit takes its closure.
///
/// # Errors
///
/// The first rule broken.
pub async fn every_root_source_retains_its_graph_and_shared_leaves_outlive_one_owner(
    tier: &dyn GraphTier,
) -> LawOutcome {
    let owner = owned(tier, "graph-roots").await?;
    let shared = big("shared");
    let shared_leaf = leaf(&shared);
    let run = turn("graph-turn");

    // The anchor shares the leaf with every source's root.
    let mut anchor_nodes = staging();
    let anchor = value(&mut anchor_nodes, &[&shared], &[]);
    let install_anchor = DomainWrite::ReleasePin(ReleasePinWrite::Install {
        release: "anchor".into(),
        root: anchor,
    });
    owner_commit(
        tier,
        &owner,
        anchor_nodes.into_nodes(),
        vec![install_anchor],
    )
    .await
    .map_err(|error| refused("the anchor", error))?;

    // A turn with a phase row and a namespace row to hang roots on.
    let admit = DomainWrite::Turn(TurnWrite::Admit {
        session: owner.session.clone(),
        run: run.clone(),
        admission: lash_core_store::store::RunAdmissionRecord::Turn {
            took: lash_core_store::store::AdmittedTurnRows::Inputs {
                ids: lash_core_store::store::AdmittedInputIds::one(lash_sansio::InputId::from(
                    "graph-roots-batch",
                )),
            },
            trace: None,
        },
        turn_deadline: None,
    });
    let mut first_phase = staging();
    let phase0 = value(&mut first_phase, &["first phase"], &[]);
    let advance = DomainWrite::Turn(TurnWrite::Advance {
        session: owner.session.clone(),
        run: run.clone(),
        phase: UnfinishedPhase::Tools {
            run: RunSeq(0),
            checkpoint: phase0,
        },
        iteration: 1,
    });
    let (entry, body) = {
        let values = BTreeMap::from([("key".to_owned(), "value".into())]);
        let body = lash_core_store::plugin_state::NamespaceBody::encode(&values);
        let entry =
            lash_core_store::plugin_state::PluginNamespaceState::default().entry(body.values);
        (entry, body.bytes)
    };
    let namespace_root = NamespaceValue {
        plugin: "memory".into(),
        body,
    }
    .stage(&mut first_phase)
    .map_err(|error| refused("a namespace value", error))?;
    let namespace = |values: StoredValues| {
        DomainWrite::Turn(TurnWrite::Namespaces {
            session: owner.session.clone(),
            run: run.clone(),
            namespaces: vec![StoredNamespaceWrite {
                plugin: "memory".into(),
                entry: entry.clone(),
                values,
            }],
        })
    };
    owner_commit(
        tier,
        &owner,
        first_phase.into_nodes(),
        vec![
            admit,
            advance,
            namespace(StoredValues::Root(namespace_root)),
        ],
    )
    .await
    .map_err(|error| refused("the turn", error))?;

    let exec = owner.exec("roots");
    let facts = OwnerKey::Cell(owner.session.clone(), run.clone(), CellId::new("roots"));
    let effect = EffectKey(vec![9].into());
    // The turn's phase root as it stands: a receipt is recorded beside it,
    // in its model phase, and the next phase over the same root consumes it.
    let mut phase_root = phase0;
    let phase_over = |phase_root: NodeId, model: bool| {
        DomainWrite::Turn(TurnWrite::Advance {
            session: owner.session.clone(),
            run: run.clone(),
            phase: if model {
                UnfinishedPhase::Model {
                    pin: crate::domain::ModelPin {
                        call: 1,
                        attempt: 1,
                        request_ref: "graph-roots-request".into(),
                        deadline: crate::DurableInstant(1_000),
                        stream_from: "graph-roots-stream".into(),
                    },
                    checkpoint: phase_root,
                    call: crate::domain::ModelCall::Requested,
                }
            } else {
                UnfinishedPhase::Tools {
                    run: RunSeq(0),
                    checkpoint: phase_root,
                }
            },
            iteration: 1,
        })
    };
    for source in RootSource::ALL {
        let mut nodes = staging();
        let unique = big(source.catalog_name());
        let root = value(&mut nodes, &[&shared, &unique], &[]);
        let unique_leaf = leaf(&unique);
        let nodes = nodes.into_nodes();
        let (publish, retire): (Option<DomainWrite>, Option<DomainWrite>) = match source {
            RootSource::Exec => (
                Some(put_exec(&exec, None, root)),
                Some(DomainWrite::ExecRoot(ExecRootWrite::Retire {
                    exec: exec.clone(),
                    expected: root,
                })),
            ),
            RootSource::RunFact => (
                Some(DomainWrite::RunFact(RunFactWrite::Append {
                    owner: facts.clone(),
                    run: RunSeq(0),
                    ordinal: Ordinal(0),
                    kind: RunFactKind::CellReport,
                    call: None,
                    record: root,
                })),
                Some(DomainWrite::RunFact(RunFactWrite::Prune {
                    owner: facts.clone(),
                    before: RunSeq(1),
                })),
            ),
            RootSource::EffectAdmission => (
                Some(DomainWrite::EffectAdmission(EffectAdmissionWrite::Put {
                    exec: exec.clone(),
                    effect: effect.clone(),
                    member: None,
                    admission: root,
                })),
                Some(DomainWrite::EffectAdmission(EffectAdmissionWrite::Remove {
                    exec: exec.clone(),
                    effect: effect.clone(),
                    expected: root,
                })),
            ),
            RootSource::TurnPhase => (
                Some(DomainWrite::Turn(TurnWrite::Advance {
                    session: owner.session.clone(),
                    run: run.clone(),
                    phase: UnfinishedPhase::Tools {
                        run: RunSeq(0),
                        checkpoint: root,
                    },
                    iteration: 1,
                })),
                // A phase root goes with its row, at the turn's end; here
                // the next phase's root replaces it.
                None,
            ),
            RootSource::WorkingExecution => (
                Some(DomainWrite::Turn(TurnWrite::Working {
                    session: owner.session.clone(),
                    run: run.clone(),
                    expected: None,
                    working: Some(root),
                })),
                Some(DomainWrite::Turn(TurnWrite::Working {
                    session: owner.session.clone(),
                    run: run.clone(),
                    expected: Some(root),
                    working: None,
                })),
            ),
            RootSource::TurnNamespace => (
                Some(namespace(StoredValues::Root(root))),
                Some(namespace(StoredValues::Base)),
            ),
            RootSource::ModelAnswer => {
                owner_commit(
                    tier,
                    &owner,
                    BTreeMap::new(),
                    vec![phase_over(phase_root, true)],
                )
                .await
                .map_err(|error| refused("the model phase", error))?;
                (
                    Some(DomainWrite::Turn(TurnWrite::Answer {
                        session: owner.session.clone(),
                        run: run.clone(),
                        answer: root,
                    })),
                    // The next phase consumes the receipt.
                    Some(phase_over(phase_root, false)),
                )
            }
            RootSource::ReleasePin => (
                Some(DomainWrite::ReleasePin(ReleasePinWrite::Install {
                    release: "release".into(),
                    root,
                })),
                Some(DomainWrite::ReleasePin(ReleasePinWrite::Retire {
                    release: "release".into(),
                    expected: root,
                })),
            ),
            // Sources whose writers are not graph commits: installed nodes
            // first, then their rows written directly.
            RootSource::SessionRevision
            | RootSource::RevisionConfig
            | RootSource::SessionAdmissionBase
            | RootSource::ArtifactBridge
            | RootSource::RunTerminalWindow
            | RootSource::TerminalConfig => (None, None),
        };
        let mut writes: Vec<DomainWrite> = publish.into_iter().collect();
        if writes.is_empty() {
            // Root the nodes through the anchor-free release pin for the
            // install, then hand them to the source and retire the pin.
            writes.push(DomainWrite::ReleasePin(ReleasePinWrite::Install {
                release: "carrier".into(),
                root,
            }));
        }
        owner_commit(tier, &owner, nodes.clone(), writes)
            .await
            .map_err(|error| refused(source.catalog_name(), error))?;
        match source {
            RootSource::SessionRevision | RootSource::RevisionConfig => {
                tier.foreign(ForeignWrite::RevisionRoot {
                    config: matches!(
                        source,
                        RootSource::RevisionConfig | RootSource::TerminalConfig
                    ),
                    session: owner.session.clone(),
                    revision: 1,
                    root: Some(root),
                })
                .await;
            }
            RootSource::SessionAdmissionBase => {
                tier.foreign(ForeignWrite::AdmissionBase {
                    session: owner.session.clone(),
                    root: Some(root),
                })
                .await;
            }
            RootSource::ArtifactBridge => {
                tier.foreign(ForeignWrite::ArtifactBridge {
                    namespace: "kernel_document".into(),
                    artifact: "bridge".into(),
                    root: Some(root),
                })
                .await;
            }
            RootSource::RunTerminalWindow | RootSource::TerminalConfig => {
                tier.foreign(ForeignWrite::TerminalWindow {
                    config: matches!(
                        source,
                        RootSource::RevisionConfig | RootSource::TerminalConfig
                    ),
                    session: owner.session.clone(),
                    run: turn("ended"),
                    root: Some(root),
                })
                .await;
            }
            _ => {}
        }
        if matches!(
            source,
            RootSource::SessionRevision
                | RootSource::RevisionConfig
                | RootSource::SessionAdmissionBase
                | RootSource::ArtifactBridge
                | RootSource::RunTerminalWindow
                | RootSource::TerminalConfig
        ) {
            owner_commit(
                tier,
                &owner,
                BTreeMap::new(),
                vec![DomainWrite::ReleasePin(ReleasePinWrite::Retire {
                    release: "carrier".into(),
                    expected: root,
                })],
            )
            .await
            .map_err(|error| refused("the carrier", error))?;
        }
        ensure!(
            tier.roots().await.contains(&root.to_hex()),
            "{source:?}'s root is not in the catalog"
        );
        exact(tier, &format!("{source:?}'s publication")).await?;
        for id in nodes.keys() {
            ensure!(
                present(tier, id).await,
                "{source:?} alone did not retain {id}"
            );
        }
        // Retire this source's owner: its own material goes, the shared
        // leaf stays with the anchor.
        let foreign = retire.is_none() && source != RootSource::TurnPhase;
        match (source, retire) {
            (_, Some(retire)) => {
                owner_commit(tier, &owner, BTreeMap::new(), vec![retire])
                    .await
                    .map_err(|error| refused("a retirement", error))?;
            }
            (RootSource::SessionRevision | RootSource::RevisionConfig, ..) => {
                tier.foreign(ForeignWrite::RevisionRoot {
                    config: matches!(
                        source,
                        RootSource::RevisionConfig | RootSource::TerminalConfig
                    ),
                    session: owner.session.clone(),
                    revision: 1,
                    root: None,
                })
                .await;
            }
            (RootSource::SessionAdmissionBase, ..) => {
                tier.foreign(ForeignWrite::AdmissionBase {
                    session: owner.session.clone(),
                    root: None,
                })
                .await;
            }
            (RootSource::ArtifactBridge, ..) => {
                tier.foreign(ForeignWrite::ArtifactBridge {
                    namespace: "kernel_document".into(),
                    artifact: "bridge".into(),
                    root: None,
                })
                .await;
            }
            (RootSource::RunTerminalWindow | RootSource::TerminalConfig, ..) => {
                tier.foreign(ForeignWrite::TerminalWindow {
                    config: matches!(
                        source,
                        RootSource::RevisionConfig | RootSource::TerminalConfig
                    ),
                    session: owner.session.clone(),
                    run: turn("ended"),
                    root: None,
                })
                .await;
            }
            (RootSource::TurnPhase, ..) => {
                // The next phase root replaces this one.
                let mut next = staging();
                let replacement = value(&mut next, &[&big("next phase")], &[]);
                owner_commit(
                    tier,
                    &owner,
                    next.into_nodes(),
                    vec![phase_over(replacement, false)],
                )
                .await
                .map_err(|error| refused("the next phase", error))?;
                phase_root = replacement;
            }
            (source, ..) => return Err(LawBroken(format!("{source:?} has no retirement"))),
        }
        if foreign {
            let audited = tier
                .audit()
                .await
                .map_err(|error| refused("the audit", error))?;
            // Everything but the leaf the anchor shares.
            let own = nodes.len() as u64 - 1;
            ensure!(
                audited == own,
                "the audit took {audited} of {source:?}'s {own} orphaned blobs"
            );
        }
        exact(tier, &format!("{source:?}'s retirement")).await?;
        ensure!(
            !present(tier, &root).await && !present(tier, &unique_leaf).await,
            "{source:?}'s retired root kept its own material"
        );
        ensure!(
            present(tier, &shared_leaf).await,
            "retiring {source:?} took the leaf the anchor shares"
        );
    }
    // The turn's end retires its phase row: the PhaseRoot is peeled alone
    // (C.7), and the turn content beneath it is left for the next turn to
    // name again, or for the audit.
    owner_commit(
        tier,
        &owner,
        BTreeMap::new(),
        vec![DomainWrite::Turn(TurnWrite::Terminal {
            session: owner.session.clone(),
            run: run.clone(),
            cause: Box::new(
                lash_core_store::store::RunTerminalCause::OperatorCancelled {
                    intent: lash_core_store::store::ControlIntentId::from_sequence(1),
                },
            ),
            head_revision: None,
        })],
    )
    .await
    .map_err(|error| refused("the turn's end", error))?;
    let content = leaf(&big("next phase"));
    exactly_the_reachable_nodes_are_stored(
        tier,
        "the turn's end",
        &BTreeSet::from([content.to_hex()]),
    )
    .await?;
    let audited = tier
        .audit()
        .await
        .map_err(|error| refused("the audit", error))?;
    ensure!(
        audited == 1,
        "the audit took {audited} blobs, not the turn content"
    );
    exact(tier, "the turn content's audit").await?;
    // The last owner goes: so does the shared leaf.
    owner_commit(
        tier,
        &owner,
        BTreeMap::new(),
        vec![DomainWrite::ReleasePin(ReleasePinWrite::Retire {
            release: "anchor".into(),
            expected: anchor,
        })],
    )
    .await
    .map_err(|error| refused("the anchor's retirement", error))?;
    exact(tier, "the anchor's retirement").await?;
    ensure!(
        !present(tier, &shared_leaf).await,
        "a leaf no root reaches outlived its last owner"
    );
    Ok(())
}

/// The orphan audit (FIG-5949): it peels exactly the closures no root
/// reaches, an opaque orphan and a graph whose root lost its row alike, and
/// keeps every node a root still reaches, the leaf an orphan shared with a
/// live root included. A second audit finds nothing.
///
/// # Errors
///
/// The first rule broken.
pub async fn the_orphan_audit_peels_only_what_no_root_reaches(tier: &dyn GraphTier) -> LawOutcome {
    let owner = owned(tier, "graph-audit").await?;
    let shared = big("audit-shared");
    let mut nodes = staging();
    let live = value(&mut nodes, &[&shared, &big("audit-live")], &[]);
    let dead = value(&mut nodes, &[&shared, &big("audit-dead")], &[]);
    let nodes = nodes.into_nodes();
    owner_commit(
        tier,
        &owner,
        nodes.clone(),
        vec![
            DomainWrite::ReleasePin(ReleasePinWrite::Install {
                release: "live".into(),
                root: live,
            }),
            DomainWrite::ReleasePin(ReleasePinWrite::Install {
                release: "dead".into(),
                root: dead,
            }),
        ],
    )
    .await
    .map_err(|error| refused("the roots", error))?;
    // The dead root moves to a row no graph commit retires, which then
    // loses it: nothing peels it.
    tier.foreign(ForeignWrite::RevisionRoot {
        config: false,
        session: owner.session.clone(),
        revision: 1,
        root: Some(dead),
    })
    .await;
    owner_commit(
        tier,
        &owner,
        BTreeMap::new(),
        vec![DomainWrite::ReleasePin(ReleasePinWrite::Retire {
            release: "dead".into(),
            expected: dead,
        })],
    )
    .await
    .map_err(|error| refused("the release's retirement", error))?;
    tier.foreign(ForeignWrite::RevisionRoot {
        config: false,
        session: owner.session.clone(),
        revision: 1,
        root: None,
    })
    .await;
    let opaque = "0".repeat(64);
    tier.foreign(ForeignWrite::OpaqueBlob {
        hash: opaque.clone(),
        bytes: b"orphan".to_vec(),
    })
    .await;
    let orphans = BTreeSet::from([
        opaque.clone(),
        dead.to_hex(),
        leaf(&big("audit-dead")).to_hex(),
    ]);
    exactly_the_reachable_nodes_are_stored(tier, "the foreign retirements", &orphans).await?;

    let audited = tier
        .audit()
        .await
        .map_err(|error| refused("the audit", error))?;
    ensure!(
        audited == orphans.len() as u64,
        "the audit peeled {audited} blobs, not the {} orphaned",
        orphans.len()
    );
    exact(tier, "the audit").await?;
    ensure!(
        present(tier, &live).await && present(tier, &leaf(&shared)).await,
        "the audit took a node a root reaches"
    );
    let again = tier
        .audit()
        .await
        .map_err(|error| refused("a second audit", error))?;
    ensure!(again == 0, "a second audit peeled {again} blobs");
    Ok(())
}

///
/// # Errors
///
/// The first rule broken.
pub async fn an_owner_commit_is_admitted_on_its_whole_encoding(tier: &dyn GraphTier) -> LawOutcome {
    let owner = owned(tier, "graph-size").await?;
    let exec = owner.exec("size");
    let mut staging = staging();
    let history: Vec<String> = (0..3)
        .map(|index| format!("host-{index}:{}", "h".repeat(20_000)))
        .collect();
    let texts: Vec<&str> = history.iter().map(String::as_str).collect();
    let host = value(&mut staging, &texts, &[]);
    let root = value(&mut staging, &["root"], &[host]);
    let root_size = staging.get(&root).map_or(0, EncodedNode::len);
    let nodes = staging.into_nodes();
    let writes = vec![put_exec(&exec, None, root)];
    let measured = measure_owner_commit(&nodes, &writes)
        .map_err(|error| refused("the owner measurement", error))?;
    let reserve = 512;
    let limit = measured.full_encoded_commit_bytes + reserve;
    ensure!(
        root_size < 512 && limit > 60_000,
        "the law's root is not small beside its host material"
    );
    let planned = measured.planned_mutation_rows;
    let budgets = [
        (
            limit - 1,
            planned,
            Some(GraphRefusal::CommitTooLarge {
                bytes: limit,
                limit: limit - 1,
            }),
        ),
        (
            limit,
            planned - 1,
            Some(GraphRefusal::TooManyMutations {
                rows: planned,
                limit: planned - 1,
            }),
        ),
        (limit, planned, None),
    ];
    for (bytes, rows, refusal) in budgets {
        let store = tier.store();
        let mut tx = store.begin(&owner.actor, owner.epoch).await?;
        tx.stage(nodes.values().cloned());
        for write in &writes {
            tx.write(write.clone());
        }
        tx.within(GraphCommitBudget {
            encoded_bytes: ExplicitLimit::new(bytes),
            mutation_rows: ExplicitLimit::new(rows),
            failure_reserve_bytes: reserve,
            ..roomy()
        });
        let answer = store
            .commit(tx, LABEL)
            .await
            .map(|_| ())
            .map_err(graph_refusal);
        ensure!(
            answer == refusal.clone().map_or(Ok(()), Err),
            "owner byte limit {bytes} and row limit {rows} answered {answer:?}"
        );
        for (id, node) in &nodes {
            let stored = tier.blob(&id.to_hex()).await;
            if refusal.is_some() {
                ensure!(
                    stored.is_none() && tier.edges(id).await.is_empty(),
                    "a refused owner commit installed node or edges {id}"
                );
            } else {
                ensure!(
                    stored.is_some_and(
                        |(bytes, encoding)| bytes == node.bytes.as_ref() && encoding == 1
                    ),
                    "the admitted owner commit did not install {id}"
                );
            }
        }
        let hydrated = hydrate(
            tier,
            RootOwner::Exec(exec.clone()),
            None,
            DecodeLimits::within_total(1 << 30),
        )
        .await;
        if refusal.is_some() {
            ensure!(
                hydrated == Err(GraphRefusal::RootRetired),
                "a refused owner commit published a root"
            );
        } else {
            ensure!(
                hydrated.is_ok_and(|graph| graph.root == root && graph.nodes.len() == nodes.len()),
                "the admitted owner commit did not publish its whole graph"
            );
        }
    }
    Ok(())
}
