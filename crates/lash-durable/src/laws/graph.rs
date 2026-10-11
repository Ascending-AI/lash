//! The checkpoint graph's store laws (FIG-5887 §11: PC-ATOMIC, PC-ROOTS,
//! PC-GC, PC-READ and PC-SIZE), written once and run by each dialect over
//! its own store through a [`GraphTier`].
//!
//! Each law takes a fresh store. Graphs are built from FrozenValue nodes
//! whose large texts are placed in shared TextLeaf nodes, so two roots that
//! name one text share one leaf.

use std::collections::BTreeMap;

use lash_core_store::checkpoint_graph::{
    CommitId, DecodeLimits, EncodeBudget, EncodedNode, ExplicitLimit, GraphCommitBudget,
    GraphRefusal, GraphStaging, NodeId, NodeKind, RootSource, TokenWriter, TypedBody,
};
use lash_sansio::{SessionId, ToolCallId, TurnId};

use super::{LABEL, LawBroken, LawOutcome, create, ensure, node, session};
use crate::domain::graph::{
    EffectAdmissionWrite, EffectKey, ExecRootWrite, FencedCommit, GraphCommitBatch, GraphStore,
    HydratedGraph, OwnerRevisionSet, PreparedGraph, ReadLease, ReadPin, ReleasePinWrite, RootOwner,
    RunFactKind, RunFactWrite,
};
use crate::domain::graph_commit::{CollectionLimits, GraphCollection, encode_commit};
use crate::domain::namespace_value::NamespaceValue;
use crate::domain::{
    CellId, ExecKey, Ordinal, OwnerKey, RunSeq, SnapshotRev, StoredNamespaceWrite, StoredValues,
    TurnWrite, UnfinishedPhase,
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
    /// Drop a node's stored edges.
    DeleteEdges { parent: NodeId },
    /// Drop a blob whatever references it.
    DeleteBlob { node: NodeId },
    /// Point session revision `revision` at `root`, or drop it.
    RevisionRoot {
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
    /// Make the root catalog's enumeration fail.
    BreakCatalog,
}

/// One dialect's store, its collector and the raw access the laws need.
#[async_trait::async_trait]
pub trait GraphTier: Send + Sync {
    fn store(&self) -> &dyn DurableStore;
    fn graph(&self) -> &dyn GraphStore;
    /// Moves the store's clock forward.
    fn advance(&self, millis: u64);
    async fn collect(&self, limits: CollectionLimits) -> Result<GraphCollection, GraphRefusal>;
    async fn release_read_pin(&self, pin: ReadPin) -> Result<(), GraphRefusal>;
    /// A stored blob's bytes and encoding.
    async fn blob(&self, hash: &str) -> Option<(Vec<u8>, u8)>;
    /// A node's stored edges, in digest order.
    async fn edges(&self, parent: &NodeId) -> Vec<NodeId>;
    async fn foreign(&self, write: ForeignWrite);
}

fn refused(what: &str, error: GraphRefusal) -> LawBroken {
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

fn batch(nodes: BTreeMap<NodeId, EncodedNode>, writes: Vec<DomainWrite>) -> GraphCommitBatch {
    GraphCommitBatch {
        commit_id: CommitId::mint(),
        expected: OwnerRevisionSet::new(),
        graph: PreparedGraph {
            nodes,
            reads: Default::default(),
            publications: writes.iter().flat_map(DomainWrite::publications).collect(),
            encoded: Default::default(),
        },
        domain_writes: writes,
        budget: roomy(),
    }
}

/// A graph refusal as the port carried it.
fn graph_refusal(error: DurableError) -> GraphRefusal {
    match error {
        DurableError::Graph(refusal) => refusal,
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
) -> Result<(), GraphRefusal> {
    let store = tier.store();
    let mut tx = store
        .begin(&owner.actor, owner.epoch)
        .await
        .map_err(graph_refusal)?;
    tx.stage(nodes.into_values());
    for write in writes {
        tx.write(write);
    }
    store
        .commit(tx, LABEL)
        .await
        .map(|_| ())
        .map_err(graph_refusal)
}

/// A session actor the law owns, and its fence.
struct Owned {
    actor: ActorKey,
    epoch: Epoch,
    session: SessionId,
}

impl Owned {
    fn fence(&self) -> FencedCommit {
        FencedCommit {
            actor: self.actor.clone(),
            epoch: self.epoch,
            label: LABEL,
            // The owner's own session history, as a session commit locks it.
            history_sessions: std::iter::once(self.session.clone()).collect(),
            read_pins: Default::default(),
        }
    }

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

async fn commit(
    tier: &dyn GraphTier,
    owner: &Owned,
    batch: GraphCommitBatch,
) -> Result<crate::domain::graph::PublishedGraph, GraphRefusal> {
    tier.graph().commit_graph(owner.fence(), batch).await
}

/// Commits `writes` as a batch when each is a graph-reference row, and
/// through the owner's transaction otherwise.
async fn commit_any(
    tier: &dyn GraphTier,
    owner: &Owned,
    nodes: BTreeMap<NodeId, EncodedNode>,
    writes: Vec<DomainWrite>,
) -> Result<(), GraphRefusal> {
    if writes.iter().all(DomainWrite::is_graph_reference) {
        commit(tier, owner, batch(nodes, writes)).await.map(|_| ())
    } else {
        owner_commit(tier, owner, nodes, writes).await
    }
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
/// root behind, whatever refused it, and leaves no receipt either: the same
/// commit identity is still free for the batch that succeeds.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_refused_graph_commit_writes_nothing(tier: &dyn GraphTier) -> LawOutcome {
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
            let hydrated = hydrate(
                tier,
                RootOwner::Exec(exec.clone()),
                None,
                DecodeLimits::within_total(1 << 30),
            )
            .await;
            ensure!(
                hydrated == Err(GraphRefusal::RootRetired),
                "{what}: a refused commit published the execution root: {hydrated:?}"
            );
            Ok::<_, LawBroken>(())
        }
    };

    // The last write conflicts, after every blob, edge, fact and admission
    // was written in the transaction.
    let late = batch(nodes.clone(), writes(Some((7, root))));
    let commit_id = late.commit_id;
    let answer = commit(tier, &owner, late).await;
    ensure!(
        answer == Err(GraphRefusal::RevisionConflict),
        "a stale execution revision answered {answer:?}"
    );
    assert_untouched("a late conflict").await?;

    // A stale owner fence.
    let stale = FencedCommit {
        epoch: Epoch(owner.epoch.0 - 1),
        ..owner.fence()
    };
    let answer = tier
        .graph()
        .commit_graph(stale, batch(nodes.clone(), writes(None)))
        .await;
    ensure!(
        answer == Err(GraphRefusal::FenceLost),
        "a stale fence answered {answer:?}"
    );
    assert_untouched("a stale fence").await?;

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
    let answer = commit(tier, &owner, batch(orphan_nodes, orphan_writes)).await;
    ensure!(
        answer == Err(GraphRefusal::MissingNode(absent)),
        "a dangling child answered {answer:?}"
    );
    assert_untouched("a dangling child").await?;

    // An opaque blob stored under a node's identity is never reclassified.
    tier.foreign(ForeignWrite::OpaqueBlob {
        hash: fact.to_hex(),
        bytes: b"opaque".to_vec(),
    })
    .await;
    let answer = commit(tier, &owner, batch(nodes.clone(), writes(None))).await;
    ensure!(
        answer == Err(GraphRefusal::EncodingConflict(fact)),
        "an opaque blob under a node's identity answered {answer:?}"
    );
    tier.foreign(ForeignWrite::DeleteBlob { node: fact }).await;
    assert_untouched("an encoding conflict").await?;

    // The refused identity left no receipt: it commits the valid batch.
    let mut valid = batch(nodes.clone(), writes(None));
    valid.commit_id = commit_id;
    let published = commit(tier, &owner, valid)
        .await
        .map_err(|error| refused("the valid batch", error))?;
    ensure!(
        published.installed.new_nodes == nodes.len() as u64,
        "the valid batch installed {} of {} nodes",
        published.installed.new_nodes,
        nodes.len()
    );
    for (id, node) in &nodes {
        ensure!(
            tier.edges(id).await == node.children().map_err(|e| refused("children", e))?,
            "node {id}'s stored edges are not its child table"
        );
    }
    Ok(())
}

/// PC-ATOMIC: a retry of an acknowledged commit under its identity answers
/// the first result and writes nothing again; another batch under that
/// identity is refused.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_lost_ack_retry_answers_once_and_a_reused_identity_refuses(
    tier: &dyn GraphTier,
) -> LawOutcome {
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
    let first = batch(nodes.into_nodes(), vec![append(0, outcome)]);
    let published = commit(tier, &owner, first.clone())
        .await
        .map_err(|error| refused("the first commit", error))?;
    let retried = commit(tier, &owner, first.clone())
        .await
        .map_err(|error| refused("the retry", error))?;
    ensure!(
        retried == published,
        "a retry answered {retried:?}, not the first result {published:?}"
    );
    let mut other = batch(first.graph.nodes.clone(), vec![append(1, outcome)]);
    other.commit_id = first.commit_id;
    let answer = commit(tier, &owner, other).await;
    ensure!(
        answer == Err(GraphRefusal::CommitIdentityConflict),
        "another batch under a used identity answered {answer:?}"
    );
    // A second outcome for the call under a fresh identity is a duplicate.
    let answer = commit(
        tier,
        &owner,
        batch(first.graph.nodes.clone(), vec![append(1, outcome)]),
    )
    .await;
    ensure!(
        answer == Err(GraphRefusal::DuplicateOutcome(call.clone())),
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

/// PC-ROOTS: every root source retains its whole graph on its own, and a
/// leaf two owners share outlives either one.
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
    let collect = || async {
        tier.collect(CollectionLimits::UNBOUNDED)
            .await
            .map_err(|error| refused("a collection", error))
    };

    // The anchor shares the leaf with every source's root.
    let mut anchor_nodes = staging();
    let anchor = value(&mut anchor_nodes, &[&shared], &[]);
    let install_anchor = DomainWrite::ReleasePin(ReleasePinWrite::Install {
        release: "anchor".into(),
        root: anchor,
    });
    commit(
        tier,
        &owner,
        batch(anchor_nodes.into_nodes(), vec![install_anchor]),
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
            RootSource::ReadPin
            | RootSource::SessionRevision
            | RootSource::SessionAdmissionBase
            | RootSource::ArtifactBridge => (None, None),
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
        commit_any(tier, &owner, nodes.clone(), writes)
            .await
            .map_err(|error| refused(source.catalog_name(), error))?;
        let mut pin = None;
        match source {
            RootSource::ReadPin => {
                pin = Some(
                    tier.graph()
                        .acquire_read_pin(
                            RootOwner::ReleasePin {
                                release: "carrier".into(),
                            },
                            ReadLease {
                                lease_owner: "roots".into(),
                                ttl_ms: 3_600_000,
                            },
                        )
                        .await
                        .map_err(|error| refused("a pin", error))?,
                );
            }
            RootSource::SessionRevision => {
                tier.foreign(ForeignWrite::RevisionRoot {
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
            _ => {}
        }
        if matches!(
            source,
            RootSource::ReadPin
                | RootSource::SessionRevision
                | RootSource::SessionAdmissionBase
                | RootSource::ArtifactBridge
        ) {
            commit(
                tier,
                &owner,
                batch(
                    BTreeMap::new(),
                    vec![DomainWrite::ReleasePin(ReleasePinWrite::Retire {
                        release: "carrier".into(),
                        expected: root,
                    })],
                ),
            )
            .await
            .map_err(|error| refused("the carrier", error))?;
        }
        let report = collect().await?;
        ensure!(
            report.roots.get(&source).copied().unwrap_or(0) >= 1,
            "{source:?} enumerated no root: {report:?}"
        );
        for id in nodes.keys() {
            ensure!(
                present(tier, id).await,
                "{source:?} alone did not retain {id}"
            );
        }
        // Retire this source's owner: its own material goes, the shared
        // leaf stays with the anchor.
        match (source, retire, pin) {
            (_, Some(retire), _) => {
                commit_any(tier, &owner, BTreeMap::new(), vec![retire])
                    .await
                    .map_err(|error| refused("a retirement", error))?;
            }
            (RootSource::ReadPin, None, Some(pin)) => {
                tier.release_read_pin(pin)
                    .await
                    .map_err(|error| refused("a pin's release", error))?;
            }
            (RootSource::SessionRevision, ..) => {
                tier.foreign(ForeignWrite::RevisionRoot {
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
            (RootSource::TurnPhase, ..) => {
                // The next phase root replaces this one.
                let mut next = staging();
                let replacement = value(&mut next, &["next phase"], &[]);
                owner_commit(
                    tier,
                    &owner,
                    next.into_nodes(),
                    vec![DomainWrite::Turn(TurnWrite::Advance {
                        session: owner.session.clone(),
                        run: run.clone(),
                        phase: UnfinishedPhase::Tools {
                            run: RunSeq(0),
                            checkpoint: replacement,
                        },
                        iteration: 1,
                    })],
                )
                .await
                .map_err(|error| refused("the next phase", error))?;
            }
            (source, ..) => return Err(LawBroken(format!("{source:?} has no retirement"))),
        }
        collect().await?;
        ensure!(
            !present(tier, &root).await && !present(tier, &unique_leaf).await,
            "{source:?}'s retired root kept its own material"
        );
        ensure!(
            present(tier, &shared_leaf).await,
            "retiring {source:?} took the leaf the anchor shares"
        );
    }
    // The last owner goes: so does the shared leaf.
    commit(
        tier,
        &owner,
        batch(
            BTreeMap::new(),
            vec![DomainWrite::ReleasePin(ReleasePinWrite::Retire {
                release: "anchor".into(),
                expected: anchor,
            })],
        ),
    )
    .await
    .map_err(|error| refused("the anchor's retirement", error))?;
    collect().await?;
    ensure!(
        !present(tier, &shared_leaf).await,
        "a leaf no root reaches outlived its last owner"
    );
    Ok(())
}

/// PC-GC: an unreachable subgraph is swept edges first, and a collection
/// that cannot prove its marks (a missing root or child, a corrupt node, a
/// stored edge set that is not a child table, a source that did not
/// enumerate, an exhausted budget) sweeps nothing.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_collection_sweeps_only_what_it_proved_unreachable(
    tier: &dyn GraphTier,
) -> LawOutcome {
    let owner = owned(tier, "graph-gc").await?;
    let mut nodes = staging();
    let child = value(&mut nodes, &[&big("gc-child")], &[]);
    let live = value(&mut nodes, &[&big("gc-live")], &[child]);
    let mut dead_nodes = staging();
    let dead = value(&mut dead_nodes, &[&big("gc-dead")], &[]);
    let dead_leaf = leaf(&big("gc-dead"));
    let pin = |release: &str, root| {
        DomainWrite::ReleasePin(ReleasePinWrite::Install {
            release: release.into(),
            root,
        })
    };
    commit(
        tier,
        &owner,
        batch(nodes.into_nodes(), vec![pin("live", live)]),
    )
    .await
    .map_err(|error| refused("the live graph", error))?;
    commit(
        tier,
        &owner,
        batch(dead_nodes.into_nodes(), vec![pin("dead", dead)]),
    )
    .await
    .map_err(|error| refused("the dead graph", error))?;
    commit(
        tier,
        &owner,
        batch(
            BTreeMap::new(),
            vec![DomainWrite::ReleasePin(ReleasePinWrite::Retire {
                release: "dead".into(),
                expected: dead,
            })],
        ),
    )
    .await
    .map_err(|error| refused("the dead graph's retirement", error))?;

    // Each failure leaves the unreachable graph where it is.
    let aborts = |what: &'static str, expected: GraphRefusal, limits: CollectionLimits| async move {
        let answer = tier.collect(limits).await;
        ensure!(
            answer.as_ref().err() == Some(&expected),
            "{what}: a collection answered {answer:?}, not {expected:?}"
        );
        ensure!(
            present(tier, &dead).await && present(tier, &dead_leaf).await,
            "{what}: a failed collection swept"
        );
        ensure!(
            !tier.edges(&dead).await.is_empty(),
            "{what}: a failed collection swept edges"
        );
        Ok::<_, LawBroken>(())
    };
    aborts(
        "an exhausted budget",
        GraphRefusal::CollectionIncomplete,
        CollectionLimits {
            nodes: 2,
            bytes: u64::MAX,
        },
    )
    .await?;
    aborts(
        "an exhausted byte budget",
        GraphRefusal::CollectionIncomplete,
        CollectionLimits {
            nodes: u64::MAX,
            bytes: 16,
        },
    )
    .await?;

    let (child_bytes, _) = tier
        .blob(&child.to_hex())
        .await
        .ok_or_else(|| LawBroken("the child was not stored".into()))?;
    let mut corrupt = child_bytes.clone();
    if let Some(last) = corrupt.last_mut() {
        *last ^= 1;
    }
    tier.foreign(ForeignWrite::OverwriteBlob {
        node: child,
        bytes: corrupt,
    })
    .await;
    aborts(
        "a corrupt node",
        GraphRefusal::HashMismatch(child),
        CollectionLimits::UNBOUNDED,
    )
    .await?;
    tier.foreign(ForeignWrite::OverwriteBlob {
        node: child,
        bytes: child_bytes,
    })
    .await;

    tier.foreign(ForeignWrite::DeleteEdges { parent: live })
        .await;
    aborts(
        "a node whose edges are not its child table",
        GraphRefusal::EdgeProjectionMismatch(live),
        CollectionLimits::UNBOUNDED,
    )
    .await?;
    let child_leaf = leaf(&big("gc-child"));
    tier.foreign(ForeignWrite::DeleteBlob { node: child_leaf })
        .await;
    tier.foreign(ForeignWrite::DeleteEdges { parent: child })
        .await;
    // The live root's edges are gone too; its child's leaf is missing.
    let answer = tier.collect(CollectionLimits::UNBOUNDED).await;
    ensure!(
        matches!(
            answer,
            Err(GraphRefusal::EdgeProjectionMismatch(_) | GraphRefusal::MissingNode(_))
        ),
        "a missing child answered {answer:?}"
    );
    ensure!(
        present(tier, &dead).await,
        "a missing child's collection swept"
    );

    let missing_root = leaf(&big("gc-missing-root"));
    tier.foreign(ForeignWrite::RevisionRoot {
        session: owner.session.clone(),
        revision: 3,
        root: Some(missing_root),
    })
    .await;
    // Repair the live graph so the missing root is the only fault.
    tier.foreign(ForeignWrite::DeleteBlob { node: live }).await;
    tier.foreign(ForeignWrite::DeleteBlob { node: child }).await;
    let mut repaired = staging();
    let child = value(&mut repaired, &[&big("gc-child")], &[]);
    let live = value(&mut repaired, &[&big("gc-live")], &[child]);
    commit(
        tier,
        &owner,
        batch(repaired.into_nodes(), vec![pin("repaired", live)]),
    )
    .await
    .map_err(|error| refused("the repaired graph", error))?;
    aborts(
        "a missing root",
        GraphRefusal::MissingNode(missing_root),
        CollectionLimits::UNBOUNDED,
    )
    .await?;
    tier.foreign(ForeignWrite::RevisionRoot {
        session: owner.session.clone(),
        revision: 3,
        root: None,
    })
    .await;

    // The proven collection sweeps the dead graph, edges first.
    let report = tier
        .collect(CollectionLimits::UNBOUNDED)
        .await
        .map_err(|error| refused("the proven collection", error))?;
    ensure!(
        !present(tier, &dead).await && !present(tier, &dead_leaf).await,
        "an unreachable graph survived its collection"
    );
    ensure!(
        tier.edges(&dead).await.is_empty(),
        "an unreachable node's edges survived it"
    );
    ensure!(
        report.swept_edges >= 1 && report.swept_blobs >= 2,
        "the collection reported {report:?}"
    );
    for id in [live, child, leaf(&big("gc-child")), leaf(&big("gc-live"))] {
        ensure!(present(tier, &id).await, "a live node {id} was swept");
    }

    // A source that cannot enumerate sweeps nothing.
    let mut more = staging();
    let doomed = value(&mut more, &[&big("gc-doomed")], &[]);
    commit(
        tier,
        &owner,
        batch(more.into_nodes(), vec![pin("doomed", doomed)]),
    )
    .await
    .map_err(|error| refused("the doomed graph", error))?;
    commit(
        tier,
        &owner,
        batch(
            BTreeMap::new(),
            vec![DomainWrite::ReleasePin(ReleasePinWrite::Retire {
                release: "doomed".into(),
                expected: doomed,
            })],
        ),
    )
    .await
    .map_err(|error| refused("the doomed graph's retirement", error))?;
    tier.foreign(ForeignWrite::BreakCatalog).await;
    let answer = tier.collect(CollectionLimits::UNBOUNDED).await;
    ensure!(
        matches!(answer, Err(GraphRefusal::RootSourceIncomplete(_))),
        "a source that cannot enumerate answered {answer:?}"
    );
    ensure!(
        present(tier, &doomed).await,
        "a collection with an incomplete source swept"
    );
    Ok(())
}

/// PC-READ: a hydration is one complete root, never a newer one; a read pin
/// keeps its root's closure through replacement and collection, fetches
/// only what that root reaches, renews by generation, refuses once expired,
/// and roots until it is retired.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_read_is_one_protected_root(tier: &dyn GraphTier) -> LawOutcome {
    let owner = owned(tier, "graph-read").await?;
    let exec = &owner.exec("read");
    let limits = DecodeLimits::within_total(1 << 30);
    let mut first = staging();
    let old_root = value(&mut first, &[&big("read-a"), &big("read-b")], &[]);
    let old_nodes = first.into_nodes();
    commit(
        tier,
        &owner,
        batch(old_nodes.clone(), vec![put_exec(exec, None, old_root)]),
    )
    .await
    .map_err(|error| refused("the first root", error))?;
    let hydrated = hydrate(tier, RootOwner::Exec(exec.clone()), None, limits)
        .await
        .map_err(|error| refused("a hydration", error))?;
    ensure!(
        hydrated.root == old_root
            && hydrated.nodes.len() == old_nodes.len()
            && old_nodes.iter().all(|(id, node)| hydrated
                .nodes
                .get(id)
                .is_some_and(|read| read.bytes == node.bytes)),
        "a hydration did not read exactly the root's closure"
    );
    let tight = DecodeLimits { nodes: 1, ..limits };
    let answer = hydrate(tier, RootOwner::Exec(exec.clone()), None, tight).await;
    ensure!(
        matches!(answer, Err(GraphRefusal::DecodeLimit { .. })),
        "a hydration past its limits answered {answer:?}"
    );

    let lease = ReadLease {
        lease_owner: "reader".into(),
        ttl_ms: 10_000,
    };
    let pin = tier
        .graph()
        .acquire_read_pin(RootOwner::Exec(exec.clone()), lease.clone())
        .await
        .map_err(|error| refused("a pin", error))?;
    ensure!(pin.root == old_root, "the pin is not on the current root");

    let mut second = staging();
    let new_root = value(&mut second, &[&big("read-c")], &[]);
    commit(
        tier,
        &owner,
        batch(
            second.into_nodes(),
            vec![put_exec(exec, Some((1, old_root)), new_root)],
        ),
    )
    .await
    .map_err(|error| refused("the replacement", error))?;
    tier.collect(CollectionLimits::UNBOUNDED)
        .await
        .map_err(|error| refused("a collection", error))?;
    let answer = hydrate(tier, RootOwner::Exec(exec.clone()), Some(old_root), limits).await;
    ensure!(
        answer.as_ref().err() == Some(&GraphRefusal::RevisionConflict),
        "a hydration expecting a replaced root answered {answer:?}"
    );

    let wanted: Vec<NodeId> = old_nodes.keys().copied().collect();
    let fetched = tier
        .graph()
        .fetch_pinned(pin.clone(), &wanted, Some(lease.clone()))
        .await
        .map_err(|error| refused("a pinned fetch", error))?;
    ensure!(
        old_nodes.iter().all(|(id, node)| fetched
            .nodes
            .get(id)
            .is_some_and(|read| read.bytes == node.bytes)),
        "a pinned fetch did not return the replaced root's exact nodes"
    );
    ensure!(
        fetched.pin.generation == pin.generation + 1,
        "a renewal did not advance the generation: {:?}",
        fetched.pin
    );
    let answer = tier.graph().fetch_pinned(pin.clone(), &wanted, None).await;
    ensure!(
        answer.as_ref().err() == Some(&GraphRefusal::ReadPinMismatch),
        "a superseded generation fetched: {answer:?}"
    );
    let renewed = fetched.pin;
    let answer = tier
        .graph()
        .fetch_pinned(renewed.clone(), &[leaf(&big("read-c"))], None)
        .await;
    ensure!(
        answer.as_ref().err() == Some(&GraphRefusal::ReadPinMismatch),
        "a pin fetched a node its root does not reach: {answer:?}"
    );

    tier.advance(10_001);
    let answer = tier
        .graph()
        .fetch_pinned(renewed.clone(), &wanted, None)
        .await;
    ensure!(
        answer.as_ref().err() == Some(&GraphRefusal::ReadPinExpired),
        "an expired pin fetched: {answer:?}"
    );
    // Expiry alone removes no root: the pin's row still holds it.
    let held = hydrate(
        tier,
        RootOwner::ReadPin(renewed.pin_id.clone()),
        None,
        limits,
    )
    .await
    .map_err(|error| refused("an expired pin's root", error))?;
    ensure!(held.root == old_root, "an expired pin lost its root");
    // A collection retires expired pins first, then sweeps what they held.
    tier.collect(CollectionLimits::UNBOUNDED)
        .await
        .map_err(|error| refused("a collection", error))?;
    for id in old_nodes.keys() {
        ensure!(
            !present(tier, id).await,
            "a retired pin's root {id} survived its collection"
        );
    }
    ensure!(present(tier, &new_root).await, "the current root was swept");

    commit(
        tier,
        &owner,
        batch(
            BTreeMap::new(),
            vec![DomainWrite::ExecRoot(ExecRootWrite::Retire {
                exec: exec.clone(),
                expected: new_root,
            })],
        ),
    )
    .await
    .map_err(|error| refused("the retirement", error))?;
    let answer = hydrate(tier, RootOwner::Exec(exec.clone()), None, limits).await;
    ensure!(
        answer.as_ref().err() == Some(&GraphRefusal::RootRetired),
        "a retired root hydrated: {answer:?}"
    );
    let answer = tier
        .graph()
        .acquire_read_pin(RootOwner::Exec(exec.clone()), lease)
        .await;
    ensure!(
        answer.as_ref().err() == Some(&GraphRefusal::RootRetired),
        "a retired root was pinned: {answer:?}"
    );
    Ok(())
}

/// The budget that admits `batch` exactly: its whole encoding (which names
/// the budget itself) plus the reserve.
fn exact_budget(batch: &mut GraphCommitBatch, reserve: u64) -> Result<u64, LawBroken> {
    batch.budget.failure_reserve_bytes = reserve;
    for _ in 0..8 {
        let measured = encode_commit(batch)
            .map_err(|error| refused("a measurement", error))?
            .measurement;
        let limit = measured.full_encoded_commit_bytes + reserve;
        if batch.budget.encoded_bytes.get() == limit {
            return Ok(limit);
        }
        batch.budget.encoded_bytes = ExplicitLimit::new(limit);
    }
    Err(LawBroken("the budget's own encoding did not settle".into()))
}

/// PC-SIZE: a commit is admitted on its whole encoding, whatever part is
/// large: a tiny root with large host material is measured whole. The limit
/// is admitted and one byte or one row less refuses with nothing written,
/// and the store refuses exactly where the preparer's count says.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_commit_is_admitted_on_its_whole_encoding(tier: &dyn GraphTier) -> LawOutcome {
    let owner = owned(tier, "graph-size").await?;
    let exec = &owner.exec("size");
    let prepare = |tag: &str| {
        let mut nodes = staging();
        // A small root naming large host material: three 20 KB texts.
        let history: Vec<String> = (0..3)
            .map(|index| format!("{tag}-{index}:{}", "h".repeat(20_000)))
            .collect();
        let texts: Vec<&str> = history.iter().map(String::as_str).collect();
        let host = value(&mut nodes, &texts, &[]);
        let root = value(&mut nodes, &["root"], &[host]);
        let size = nodes.nodes().get(&root).map_or(0, EncodedNode::len);
        (nodes.into_nodes(), root, size)
    };

    let (nodes, root, root_size) = prepare("over");
    let mut over = batch(nodes.clone(), vec![put_exec(exec, None, root)]);
    let limit = exact_budget(&mut over, 512)?;
    ensure!(
        root_size < 512 && limit > 60_000,
        "the law's root is not small beside its host material"
    );
    over.budget.encoded_bytes = ExplicitLimit::new(limit - 1);
    let measured = encode_commit(&over)
        .map_err(|error| refused("a measurement", error))?
        .measurement;
    let answer = commit(tier, &owner, over).await;
    ensure!(
        answer
            == Err(GraphRefusal::CommitTooLarge {
                bytes: measured.full_encoded_commit_bytes + 512,
                limit: limit - 1,
            })
            && measured.full_encoded_commit_bytes + 512 == limit,
        "one byte under the whole encoding answered {answer:?} (measured {measured:?}, limit {limit})"
    );
    for id in nodes.keys() {
        ensure!(!present(tier, id).await, "a refused commit installed {id}");
    }

    let mut rows = batch(nodes.clone(), vec![put_exec(exec, None, root)]);
    exact_budget(&mut rows, 0)?;
    let planned = encode_commit(&rows)
        .map_err(|error| refused("a measurement", error))?
        .measurement
        .planned_mutation_rows;
    rows.budget.mutation_rows = ExplicitLimit::new(planned - 1);
    exact_budget(&mut rows, 0)?;
    let answer = commit(tier, &owner, rows).await;
    ensure!(
        answer
            == Err(GraphRefusal::TooManyMutations {
                rows: planned,
                limit: planned - 1,
            }),
        "one row under the plan answered {answer:?}"
    );

    let mut exact = batch(nodes.clone(), vec![put_exec(exec, None, root)]);
    exact.budget.mutation_rows = ExplicitLimit::new(planned);
    exact_budget(&mut exact, 512)?;
    let published = commit(tier, &owner, exact)
        .await
        .map_err(|error| refused("the commit at its limit", error))?;
    let installed: u64 = nodes.values().map(EncodedNode::len).sum();
    ensure!(
        published.installed.newly_installed_canonical_bytes == installed
            && published.installed.new_nodes == nodes.len() as u64,
        "the commit at its limit reported {:?}, not {installed} bytes in {} nodes",
        published.installed,
        nodes.len()
    );
    Ok(())
}
