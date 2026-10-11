//! The checkpoint graph's gate law (PC-GATE) and its race laws: reads,
//! collections and references racing replacement, commits and retirement
//! (FIG-5887 §8, §11).

use std::collections::BTreeMap;

use lash_core_store::checkpoint_graph::{DecodeLimits, EncodedNode, GraphRefusal, NodeId};

use super::{
    GraphTier, batch, big, commit, hydrate, leaf, owned, present, put_exec, refused, staging, value,
};
use crate::domain::graph::{ExecRootWrite, ReadLease, ReleasePinWrite, RootOwner};
use crate::domain::graph_commit::CollectionLimits;
use crate::laws::{LawBroken, LawOutcome, ensure};
use crate::{ActorKey, DomainWrite};

/// A held graph gate, taken outside any store operation.
#[async_trait::async_trait]
pub trait GateHold: Send {
    /// Whether `actor`'s row can be locked now: no transaction holds it.
    async fn owner_unlocked(&mut self, actor: &ActorKey) -> bool;
    /// Ends the holding transaction.
    async fn release(self: Box<Self>);
}

/// Takes the store's graph gate as a collection (`exclusive`) or a guarded
/// transaction would.
#[async_trait::async_trait]
pub trait GateControl: Send + Sync {
    async fn hold(&self, exclusive: bool) -> Box<dyn GateHold>;
}

/// How long a law holds the gate while the operation it orders waits.
const GATE_HOLD: std::time::Duration = std::time::Duration::from_millis(300);

fn release_pin(release: &str, root: NodeId) -> DomainWrite {
    DomainWrite::ReleasePin(ReleasePinWrite::Install {
        release: release.into(),
        root,
    })
}

fn retire_release(release: &str, expected: NodeId) -> DomainWrite {
    DomainWrite::ReleasePin(ReleasePinWrite::Retire {
        release: release.into(),
        expected,
    })
}

/// PC-GATE: a collection excludes every guarded transaction and they
/// exclude it, while guarded transactions share the gate. A commit waiting
/// on a collection holds no owner lock: the gate is its first lock.
///
/// # Errors
///
/// The first rule broken.
pub async fn the_graph_gate_orders_collection_against_every_guarded_transaction(
    tier: &dyn GraphTier,
    gate: &dyn GateControl,
) -> LawOutcome {
    use std::sync::atomic::{AtomicBool, Ordering};
    let owner = owned(tier, "graph-gate").await?;
    let limits = DecodeLimits::within_total(1 << 30);
    let mut nodes = staging();
    let read = value(&mut nodes, &[&big("gate-read")], &[]);
    let retired = value(&mut nodes, &[&big("gate-retired")], &[]);
    commit(
        tier,
        &owner,
        batch(
            nodes.into_nodes(),
            vec![release_pin("read", read), release_pin("retired", retired)],
        ),
    )
    .await
    .map_err(|error| refused("the gated graph", error))?;

    // A collection holds the gate: a commit and a hydration wait for it,
    // and the waiting commit has not taken its owner's row.
    let mut held = gate.hold(true).await;
    let released = AtomicBool::new(false);
    let (committed, hydrated, unlocked) = tokio::join!(
        async {
            let answer = commit(
                tier,
                &owner,
                batch(BTreeMap::new(), vec![retire_release("retired", retired)]),
            )
            .await;
            (answer, released.load(Ordering::SeqCst))
        },
        async {
            let answer = hydrate(
                tier,
                RootOwner::ReleasePin {
                    release: "read".into(),
                },
                None,
                limits,
            )
            .await;
            (answer, released.load(Ordering::SeqCst))
        },
        async {
            tokio::time::sleep(GATE_HOLD).await;
            let unlocked = held.owner_unlocked(&owner.actor).await;
            released.store(true, Ordering::SeqCst);
            held.release().await;
            unlocked
        },
    );
    ensure!(
        unlocked,
        "a commit waiting on the gate held its owner's row: the gate was not its first lock"
    );
    let (answer, after) = committed;
    answer.map_err(|error| refused("the gated commit", error))?;
    ensure!(after, "a commit ran beside a collection");
    let (answer, after) = hydrated;
    ensure!(
        answer.as_ref().is_ok_and(|graph| graph.root == read),
        "the gated hydration answered {answer:?}"
    );
    ensure!(after, "a hydration ran beside a collection");

    // A guarded transaction holds the gate: another commit shares it, and
    // a collection waits for it.
    let held = gate.hold(false).await;
    let released = AtomicBool::new(false);
    let (shared, collected, ()) = tokio::join!(
        async {
            let mut more = staging();
            let root = value(&mut more, &[&big("gate-shared")], &[]);
            let answer = commit(
                tier,
                &owner,
                batch(more.into_nodes(), vec![release_pin("shared", root)]),
            )
            .await;
            (answer, released.load(Ordering::SeqCst))
        },
        async {
            let answer = tier.collect(CollectionLimits::UNBOUNDED).await;
            (answer, released.load(Ordering::SeqCst))
        },
        async {
            tokio::time::sleep(GATE_HOLD).await;
            released.store(true, Ordering::SeqCst);
            held.release().await;
        },
    );
    let (answer, after) = shared;
    answer.map_err(|error| refused("a commit beside a shared holder", error))?;
    ensure!(!after, "a commit waited for another guarded transaction");
    let (answer, after) = collected;
    let report = answer.map_err(|error| refused("the gated collection", error))?;
    ensure!(after, "a collection ran beside a guarded transaction");
    ensure!(
        report.swept_blobs >= 2 && !present(tier, &retired).await && present(tier, &read).await,
        "the gated collection reported {report:?}"
    );
    Ok(())
}

/// Rounds a race law runs.
const RACE_ROUNDS: u64 = 16;

/// PC-READ under a race: hydrations and pinned fetches racing root
/// replacement and collection each read one whole root, never a node a
/// collection swept from under them.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_read_racing_replacement_and_collection_reads_one_whole_root(
    tier: &dyn GraphTier,
) -> LawOutcome {
    use std::sync::atomic::{AtomicBool, Ordering};
    let owner = owned(tier, "graph-read-race").await?;
    let exec = owner.exec("race");
    let limits = DecodeLimits::within_total(1 << 30);
    let graph = |round: u64| {
        let mut nodes = staging();
        let unique = big(&format!("read-race-{round}"));
        let root = value(&mut nodes, &[&big("read-race-shared"), &unique], &[]);
        (nodes.into_nodes(), root)
    };
    let (nodes, first) = graph(0);
    commit(
        tier,
        &owner,
        batch(nodes, vec![put_exec(&exec, None, first)]),
    )
    .await
    .map_err(|error| refused("the first root", error))?;
    let done = AtomicBool::new(false);
    let writer = async {
        let rounds = async {
            let mut current = first;
            for round in 1..=RACE_ROUNDS {
                let (nodes, root) = graph(round);
                commit(
                    tier,
                    &owner,
                    batch(nodes, vec![put_exec(&exec, Some((round, current)), root)]),
                )
                .await
                .map_err(|error| refused("a racing replacement", error))?;
                current = root;
                tier.collect(CollectionLimits::UNBOUNDED)
                    .await
                    .map_err(|error| refused("a racing collection", error))?;
            }
            Ok::<_, LawBroken>(())
        }
        .await;
        done.store(true, Ordering::SeqCst);
        rounds
    };
    let reader = async {
        let mut reads = 0_u64;
        while !done.load(Ordering::SeqCst) || reads == 0 {
            let hydrated = hydrate(tier, RootOwner::Exec(exec.clone()), None, limits)
                .await
                .map_err(|error| refused("a racing hydration", error))?;
            ensure!(
                hydrated.nodes.len() == 3,
                "a racing hydration read {} nodes of a three-node root",
                hydrated.nodes.len()
            );
            let pin = tier
                .graph()
                .acquire_read_pin(
                    RootOwner::Exec(exec.clone()),
                    ReadLease {
                        lease_owner: "racer".into(),
                        ttl_ms: 3_600_000,
                    },
                )
                .await
                .map_err(|error| refused("a racing pin", error))?;
            let pinned = hydrate(tier, RootOwner::ReadPin(pin.pin_id.clone()), None, limits)
                .await
                .map_err(|error| refused("a racing pinned hydration", error))?;
            let wanted: Vec<NodeId> = pinned.nodes.keys().copied().collect();
            tier.graph()
                .fetch_pinned(pin.clone(), &wanted, None)
                .await
                .map_err(|error| refused("a racing pinned fetch", error))?;
            tier.release_read_pin(pin)
                .await
                .map_err(|error| refused("a racing pin's release", error))?;
            reads += 1;
            tokio::task::yield_now().await;
        }
        Ok::<_, LawBroken>(())
    };
    let (written, read) = tokio::join!(writer, reader);
    written?;
    read
}

/// PC-GC under a race: collections racing commits each prove their marks
/// and never sweep a node a commit installed or relied on; afterwards only
/// the last graph remains.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_collection_racing_commits_sweeps_nothing_they_rely_on(
    tier: &dyn GraphTier,
) -> LawOutcome {
    use std::sync::atomic::{AtomicBool, Ordering};
    let owner = owned(tier, "graph-gc-race").await?;
    let limits = DecodeLimits::within_total(1 << 30);
    let done = AtomicBool::new(false);
    let release = |round: u64| format!("gc-race-{round}");
    let unique = |round: u64| big(&format!("gc-race-{round}"));
    let writer = async {
        let rounds = async {
            let mut previous: Option<NodeId> = None;
            for round in 0..RACE_ROUNDS {
                let mut nodes = staging();
                let root = value(&mut nodes, &[&big("gc-race-shared"), &unique(round)], &[]);
                let mut writes = vec![release_pin(&release(round), root)];
                if let (Some(previous), Some(before)) = (previous, round.checked_sub(1)) {
                    writes.push(retire_release(&release(before), previous));
                }
                commit(tier, &owner, batch(nodes.into_nodes(), writes))
                    .await
                    .map_err(|error| refused("a racing commit", error))?;
                let hydrated = hydrate(
                    tier,
                    RootOwner::ReleasePin {
                        release: release(round),
                    },
                    None,
                    limits,
                )
                .await
                .map_err(|error| refused("a racing commit's graph", error))?;
                ensure!(
                    hydrated.nodes.len() == 3,
                    "a racing commit's graph read {} nodes",
                    hydrated.nodes.len()
                );
                previous = Some(root);
            }
            Ok::<_, LawBroken>(previous)
        }
        .await;
        done.store(true, Ordering::SeqCst);
        rounds
    };
    let collector = async {
        let mut collections = 0_u64;
        while !done.load(Ordering::SeqCst) || collections == 0 {
            tier.collect(CollectionLimits::UNBOUNDED)
                .await
                .map_err(|error| refused("a collection racing commits", error))?;
            collections += 1;
            tokio::task::yield_now().await;
        }
        Ok::<_, LawBroken>(())
    };
    let (written, collected) = tokio::join!(writer, collector);
    let last = written?.ok_or_else(|| LawBroken("no round ran".into()))?;
    collected?;
    tier.collect(CollectionLimits::UNBOUNDED)
        .await
        .map_err(|error| refused("the last collection", error))?;
    ensure!(
        present(tier, &last).await && present(tier, &leaf(&big("gc-race-shared"))).await,
        "the last graph was swept"
    );
    for round in 0..RACE_ROUNDS - 1 {
        ensure!(
            !present(tier, &leaf(&unique(round))).await,
            "round {round}'s retired graph outlived its collection"
        );
    }
    Ok(())
}

/// PC-ROOTS under a race: a commit that publishes a reference to another
/// owner's root while that owner retires it and a collection runs either
/// installs a whole graph or is refused the missing root; it never
/// publishes a root whose closure a collection swept.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_reference_racing_its_roots_retirement_installs_whole_or_refuses(
    tier: &dyn GraphTier,
) -> LawOutcome {
    let source = owned(tier, "graph-fork-source").await?;
    let fork = owned(tier, "graph-fork").await?;
    let limits = DecodeLimits::within_total(1 << 30);
    let mut installed = 0_u64;
    for round in 0..RACE_ROUNDS {
        let cell = format!("fork-{round}");
        let source_exec = source.exec(&cell);
        let fork_exec = fork.exec(&cell);
        let mut nodes = staging();
        let base = value(&mut nodes, &[&big(&format!("fork-base-{round}"))], &[]);
        commit(
            tier,
            &source,
            batch(nodes.into_nodes(), vec![put_exec(&source_exec, None, base)]),
        )
        .await
        .map_err(|error| refused("a fork's base", error))?;
        // The fork carries only its own root: the base is a reference.
        let mut carried = staging();
        let forked = value(&mut carried, &[&format!("fork-{round}")], &[base]);
        let carried: BTreeMap<NodeId, EncodedNode> = carried
            .into_nodes()
            .into_iter()
            .filter(|(id, _)| *id == forked)
            .collect();
        let (retired, published) = tokio::join!(
            async {
                commit(
                    tier,
                    &source,
                    batch(
                        BTreeMap::new(),
                        vec![DomainWrite::ExecRoot(ExecRootWrite::Retire {
                            exec: source_exec.clone(),
                            expected: base,
                        })],
                    ),
                )
                .await
                .map_err(|error| refused("the base's retirement", error))?;
                tier.collect(CollectionLimits::UNBOUNDED)
                    .await
                    .map_err(|error| refused("a collection racing a fork", error))
            },
            commit(
                tier,
                &fork,
                batch(carried, vec![put_exec(&fork_exec, None, forked)]),
            ),
        );
        retired?;
        match published {
            Ok(_) => {
                installed += 1;
                let hydrated = hydrate(tier, RootOwner::Exec(fork_exec.clone()), None, limits)
                    .await
                    .map_err(|error| refused("a published fork", error))?;
                ensure!(
                    hydrated.nodes.len() == 3,
                    "a published fork read {} of its three nodes",
                    hydrated.nodes.len()
                );
            }
            Err(GraphRefusal::MissingNode(missing)) if missing == base => {}
            Err(error) => return Err(refused("a fork racing its base's retirement", error)),
        }
    }
    // A fork published before every collection still holds its base.
    tier.collect(CollectionLimits::UNBOUNDED)
        .await
        .map_err(|error| refused("the last collection", error))?;
    ensure!(
        installed > 0 || RACE_ROUNDS == 0,
        "no fork ever won its race"
    );
    Ok(())
}
