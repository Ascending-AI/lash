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
    EffectAdmissionWrite, EffectKey, ExecRootWrite, GraphStore, HydratedGraph, ReadLease, ReadPin,
    ReleasePinWrite, RootOwner, RunFactKind, RunFactWrite,
};
use crate::domain::graph_commit::{CollectionLimits, GraphCollection, measure_owner_commit};
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
    /// Point run `run`'s terminal window's checkpoint at `root`, or drop
    /// the run's row.
    TerminalWindow {
        session: SessionId,
        run: TurnId,
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
    tier.foreign(ForeignWrite::DeleteBlob { node: fact }).await;
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
            | RootSource::ArtifactBridge
            | RootSource::RunTerminalWindow => (None, None),
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
            RootSource::RunTerminalWindow => {
                tier.foreign(ForeignWrite::TerminalWindow {
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
            RootSource::ReadPin
                | RootSource::SessionRevision
                | RootSource::SessionAdmissionBase
                | RootSource::ArtifactBridge
                | RootSource::RunTerminalWindow
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
                owner_commit(tier, &owner, BTreeMap::new(), vec![retire])
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
            (RootSource::RunTerminalWindow, ..) => {
                tier.foreign(ForeignWrite::TerminalWindow {
                    session: owner.session.clone(),
                    run: turn("ended"),
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
    owner_commit(tier, &owner, nodes.into_nodes(), vec![pin("live", live)])
        .await
        .map_err(|error| refused("the live graph", error))?;
    owner_commit(
        tier,
        &owner,
        dead_nodes.into_nodes(),
        vec![pin("dead", dead)],
    )
    .await
    .map_err(|error| refused("the dead graph", error))?;
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
    owner_commit(
        tier,
        &owner,
        repaired.into_nodes(),
        vec![pin("repaired", live)],
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
    owner_commit(tier, &owner, more.into_nodes(), vec![pin("doomed", doomed)])
        .await
        .map_err(|error| refused("the doomed graph", error))?;
    owner_commit(
        tier,
        &owner,
        BTreeMap::new(),
        vec![DomainWrite::ReleasePin(ReleasePinWrite::Retire {
            release: "doomed".into(),
            expected: doomed,
        })],
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
    owner_commit(
        tier,
        &owner,
        old_nodes.clone(),
        vec![put_exec(exec, None, old_root)],
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
    owner_commit(
        tier,
        &owner,
        second.into_nodes(),
        vec![put_exec(exec, Some((1, old_root)), new_root)],
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

    owner_commit(
        tier,
        &owner,
        BTreeMap::new(),
        vec![DomainWrite::ExecRoot(ExecRootWrite::Retire {
            exec: exec.clone(),
            expected: new_root,
        })],
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

/// PC-SIZE: an owner's whole graph encoding and reserve fit exactly at
/// the byte and row limits; one byte or row less publishes nothing.
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
