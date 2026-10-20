//! The checkpoint graph's race laws: reads, the orphan audit and references
//! racing replacement, commits and the peel a retirement runs (FIG-5887
//! §11; FIG-5949). No gate orders any of them.

use std::collections::BTreeMap;

use lash_core_store::checkpoint_graph::{DecodeLimits, EncodedNode, GraphRefusal, NodeId};

use super::{
    ForeignWrite, GraphTier, big, exact, hydrate, leaf, owned, owner_commit, present, put_exec,
    refused, staging, value,
};
use crate::domain::graph::{ExecRootWrite, HydratedGraph, RootOwner};
use crate::laws::{LawBroken, LawOutcome, ensure};
use crate::{DomainWrite, DurableError};

/// A row lock a law holds in a transaction of its own.
#[async_trait::async_trait]
pub trait LockHold: Send {
    /// Waits until the server observes a transaction blocked by this holder.
    async fn wait_for_blocked(&mut self);
    /// Ends the holding transaction.
    async fn release(self: Box<Self>);
}

/// What a law about a server store's locks and statements needs beyond a
/// [`GraphTier`].
#[async_trait::async_trait]
pub trait ServerProbe: Send + Sync {
    /// Key-share locks blob `hash`, as a commit that references it does,
    /// outside any store operation.
    async fn hold(&self, hash: &str) -> Box<dyn LockHold>;
    /// Hydrates `owner`'s root, answering the round trips it made to the
    /// server, the checkout's ping included.
    async fn hydration_round_trips(
        &self,
        owner: RootOwner,
        limits: DecodeLimits,
    ) -> (Result<HydratedGraph, GraphRefusal>, u64);
}

/// Rounds a race law runs.
const RACE_ROUNDS: u64 = 16;

/// PC-READ under a race: hydrations racing root replacement, the peel it
/// runs and the orphan audit each read one whole root, never a node peeled
/// from under them.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_read_racing_replacement_and_its_peel_reads_one_whole_root(
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
    owner_commit(tier, &owner, nodes, vec![put_exec(&exec, None, first)])
        .await
        .map_err(|error| refused("the first root", error))?;
    let done = AtomicBool::new(false);
    let writer = async {
        let rounds = async {
            let mut current = first;
            for round in 1..=RACE_ROUNDS {
                let (nodes, root) = graph(round);
                owner_commit(
                    tier,
                    &owner,
                    nodes,
                    vec![put_exec(&exec, Some((round, current)), root)],
                )
                .await
                .map_err(|error| refused("a racing replacement", error))?;
                current = root;
                tier.audit()
                    .await
                    .map_err(|error| refused("a racing audit", error))?;
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
            reads += 1;
            tokio::task::yield_now().await;
        }
        Ok::<_, LawBroken>(())
    };
    let (written, read) = tokio::join!(writer, reader);
    written?;
    read?;
    exact(tier, "the racing replacements").await
}

/// The orphan audit under a race: audits racing commits, and the peels
/// their retirements run, never take a node a commit installed or relied
/// on; afterwards exactly the last graph is stored.
///
/// # Errors
///
/// The first rule broken.
pub async fn an_audit_racing_commits_peels_nothing_they_rely_on(
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
                let mut writes = vec![super::pin(&release(round), root)];
                if let (Some(previous), Some(before)) = (previous, round.checked_sub(1)) {
                    writes.push(super::unpin(&release(before), previous));
                }
                owner_commit(tier, &owner, nodes.into_nodes(), writes)
                    .await
                    .map_err(|error| refused("a racing commit", error))?;
                let hydrated = hydrate(tier, super::pinned(&release(round)), None, limits)
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
    let auditor = async {
        let mut audits = 0_u64;
        while !done.load(Ordering::SeqCst) || audits == 0 {
            tier.audit()
                .await
                .map_err(|error| refused("an audit racing commits", error))?;
            audits += 1;
            tokio::task::yield_now().await;
        }
        Ok::<_, LawBroken>(())
    };
    let (written, audited) = tokio::join!(writer, auditor);
    let last = written?.ok_or_else(|| LawBroken("no round ran".into()))?;
    audited?;
    ensure!(
        present(tier, &last).await && present(tier, &leaf(&big("gc-race-shared"))).await,
        "the last graph was peeled"
    );
    for round in 0..RACE_ROUNDS - 1 {
        ensure!(
            !present(tier, &leaf(&unique(round))).await,
            "round {round}'s retired graph outlived its retirement"
        );
    }
    exact(tier, "the racing commits").await
}

/// R-RACE: a commit that publishes a reference to another owner's root
/// while that owner's retirement peels it either installs a whole graph or
/// is refused the missing root; it never publishes a root whose closure the
/// peel deleted, and the peel never leaves the retired closure behind.
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
        owner_commit(
            tier,
            &source,
            nodes.into_nodes(),
            vec![put_exec(&source_exec, None, base)],
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
                // Every other round, the fork's commit starts first.
                if round % 2 == 1 {
                    tokio::task::yield_now().await;
                }
                owner_commit(
                    tier,
                    &source,
                    BTreeMap::new(),
                    vec![DomainWrite::ExecRoot(ExecRootWrite::Retire {
                        exec: source_exec.clone(),
                        expected: base,
                    })],
                )
                .await
                .map_err(|error| refused("the base's retirement", error))
            },
            owner_commit(
                tier,
                &fork,
                carried,
                vec![put_exec(&fork_exec, None, forked)],
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
            Err(DurableError::Graph(GraphRefusal::MissingNode(missing))) if missing == base => {}
            Err(error) => return Err(refused("a fork racing its base's retirement", error)),
        }
        exact(tier, &format!("fork round {round}")).await?;
    }
    ensure!(
        installed > 0 || RACE_ROUNDS == 0,
        "no fork ever won its race"
    );
    Ok(())
}

/// C2 (FIG-5949): a hydration is one statement, with no gate, fence or
/// transaction around it: a checkout and the statement that reads the
/// owner's root with everything it reaches.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_hydration_is_one_statement(
    tier: &dyn GraphTier,
    probe: &dyn ServerProbe,
) -> LawOutcome {
    let owner = owned(tier, "graph-one-statement").await?;
    let exec = owner.exec("one-statement");
    let mut nodes = staging();
    let root = value(&mut nodes, &[&big("one-a"), &big("one-b")], &[]);
    let nodes = nodes.into_nodes();
    owner_commit(
        tier,
        &owner,
        nodes.clone(),
        vec![put_exec(&exec, None, root)],
    )
    .await
    .map_err(|error| refused("the root", error))?;
    let (answer, trips) = probe
        .hydration_round_trips(
            RootOwner::Exec(exec.clone()),
            DecodeLimits::within_total(1 << 30),
        )
        .await;
    let hydrated = answer.map_err(|error| refused("the hydration", error))?;
    ensure!(
        hydrated.root == root && hydrated.nodes.len() == nodes.len(),
        "the hydration read {} of {} nodes",
        hydrated.nodes.len(),
        nodes.len()
    );
    ensure!(
        trips <= 2,
        "a hydration made {trips} round trips, not a checkout and one statement"
    );
    Ok(())
}

/// C2 (FIG-5949): the orphan audit takes no lock that orders commits or
/// reads against it, and waits on none. While another transaction holds an
/// orphan, a commit and a hydration beside the audit complete, and the audit
/// completes too, leaving the held node; once it is released, the next
/// audit peels it.
///
/// # Errors
///
/// The first rule broken.
pub async fn commits_and_reads_progress_while_the_audit_runs(
    tier: &dyn GraphTier,
    probe: &dyn ServerProbe,
) -> LawOutcome {
    let owner = owned(tier, "graph-audit-load").await?;
    let limits = DecodeLimits::within_total(1 << 30);
    let mut nodes = staging();
    let read = value(&mut nodes, &[&big("load-read")], &[]);
    owner_commit(
        tier,
        &owner,
        nodes.into_nodes(),
        vec![super::pin("read", read)],
    )
    .await
    .map_err(|error| refused("the read root", error))?;
    let orphan = "f".repeat(64);
    tier.foreign(ForeignWrite::OpaqueBlob {
        hash: orphan.clone(),
        bytes: b"held orphan".to_vec(),
    })
    .await;
    let mut held = probe.hold(&orphan).await;
    // Keep the lock until all three operations finish or the server observes
    // a waiter. Scheduling latency is not evidence of a database lock wait.
    // The timeout is only a watchdog; it never releases the lock to let a
    // blocked operation finish successfully.
    let progress = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        tokio::select! {
            () = held.wait_for_blocked() => Err(LawBroken(
                "the audit or a commit beside it waited for a transaction that held an orphan"
                    .into(),
            )),
            answers = async {
                tokio::join!(tier.audit(), async {
                    let mut more = staging();
                    let root = value(&mut more, &[&big("load-write")], &[]);
                    let committed = owner_commit(
                        tier,
                        &owner,
                        more.into_nodes(),
                        vec![super::pin("write", root)],
                    )
                    .await;
                    let hydrated = hydrate(
                        tier,
                        super::pinned("read"),
                        None,
                        limits,
                    )
                    .await;
                    (committed, hydrated)
                })
            } => Ok(answers),
        }
    })
    .await;
    held.release().await;
    let (answer, (committed, hydrated)) = progress.map_err(|_| {
        LawBroken("the audit, commit and read did not finish with the orphan held".into())
    })??;
    committed.map_err(|error| refused("a commit beside the audit", error))?;
    ensure!(
        hydrated.as_ref().is_ok_and(|graph| graph.root == read),
        "a hydration beside the audit answered {hydrated:?}"
    );
    let deleted = answer.map_err(|error| refused("the audit", error))?;
    ensure!(
        deleted == 0,
        "the audit peeled {deleted} blobs another transaction held"
    );
    let deleted = tier
        .audit()
        .await
        .map_err(|error| refused("the audit after the release", error))?;
    ensure!(
        deleted == 1 && tier.blob(&orphan).await.is_none(),
        "the audit after the release peeled {deleted} blobs, not the orphan"
    );
    exact(tier, "the audit under load").await
}
