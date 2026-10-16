//! The page store's lock order under load (FIG-6193): publications, lease
//! releases and retirement steps on two nodes over one database, sharing
//! pages, never deadlock and never reclaim a page a lease or a root keeps.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};

use lash_core_store::checkpoint_graph::{CommitId, EncodedNode, NodeId, NodeKind};

use super::super::graph::LockHold;
use super::super::{LABEL, LawBroken, LawOutcome, create, ensure, node, session};
use super::{Candidate, Owned, PageTier, drain, encoded, exec_root, lease_request, page};
use crate::domain::graph::{EffectAdmissionWrite, EffectKey, ExecRootWrite};
use crate::domain::pages::{CandidateOwner, CandidatePublication, PageStore, RetirementBudget};
use crate::domain::{ExecKey, SnapshotRev};
use crate::{DomainWrite, DurableError, NodeLease};

/// Owners publishing at once, split across the two nodes.
const OWNERS: usize = 6;

/// The shared payload leaves every candidate draws from.
const LEAVES: usize = 8;

/// The shared admission nodes every owner admits and retires, two per
/// round: each is an effect admission's root on some owners and a page
/// under others' candidates at once.
const ADMISSIONS: usize = 6;

/// The nodes one racing retirement step may delete: small, so a step
/// often ends with a frontier it did not reach.
const STEP_NODES: u64 = 2;

/// Choreographed rounds that put a publication's hold and a retirement
/// step's peel in the order that would close a lock cycle.
const CHOREOGRAPHED: usize = 4;

/// What the choreographed rounds need of a server store's row locks: a
/// foreign transaction holding a blob the way an inline peel or a writer
/// does, and the server's view of who waits.
#[async_trait::async_trait]
pub trait PageLockProbe: Send + Sync {
    /// Locks blob `hash` for update, as an inline peel does, outside any
    /// store operation.
    async fn peel_hold(&self, hash: &str) -> Box<dyn LockHold>;
    /// Key-share locks blob `hash`, as a commit that names it does.
    async fn name_hold(&self, hash: &str) -> Box<dyn LockHold>;
    /// Waits until `count` of the database's transactions wait on a lock.
    async fn wait_for_lock_waiters(&self, count: usize);
}

/// D-RET, D-LEASE under load: owners on two nodes publish candidates over
/// shared pages, each publication retiring its execution root and two
/// effect admissions, while each owner reads its previous root under a
/// lease and then releases it, and both nodes run bounded retirement steps
/// throughout. Publications hold what they retire and steps peel the same
/// pages, in opposite roles, every round; under one global lock order no
/// write meets a deadlock or a lock timeout (a tier whose store retries
/// none surfaces either as a failed write), every leased read answers
/// every page with its bytes, and once the steps drain, every live root's
/// closure is stored and every retired candidate is reclaimed.
///
/// A server store's `probe` first choreographs the interleaving a lock
/// cycle needs, [`CHOREOGRAPHED`] times: a step has taken the hold row of
/// a page a publication on the other node retires, its frontier names a
/// page that publication holds first, and a foreign inline peel delays
/// the step. A step that waited for anything there would close the
/// cycle: one of the two would die of a deadlock.
///
/// # Errors
///
/// The first rule broken.
pub async fn publications_and_retirement_on_two_nodes_never_deadlock(
    tier: &dyn PageTier,
    probe: Option<&dyn PageLockProbe>,
    rounds: usize,
) -> LawOutcome {
    let nodes = [tier.pages(), tier.peer()];
    let mut leases = Vec::new();
    for (index, store) in nodes.iter().enumerate() {
        leases.push(node(*store, &format!("pages-race-node-{index}")).await?);
    }
    if let Some(probe) = probe {
        for round in 0..CHOREOGRAPHED {
            choreographed(tier, probe, &leases, round).await?;
        }
    }
    let mut owners = Vec::new();
    for index in 0..OWNERS {
        let store = nodes[index % nodes.len()];
        let lease = &leases[index % leases.len()];
        owners.push((
            store,
            owned_on(store, lease, &format!("pages-race-{index}")).await?,
        ));
    }
    let leaves: Vec<EncodedNode> = (0..LEAVES)
        .map(|leaf| encoded(NodeKind::PAYLOAD_LEAF, &format!("race leaf {leaf}"), &[]))
        .collect();
    let admissions: Vec<EncodedNode> = (0..ADMISSIONS)
        .map(|admission| {
            encoded(
                NodeKind::FROZEN_VALUE,
                &format!("race admission {admission}"),
                &[],
            )
        })
        .collect();
    let pool = Pool { leaves, admissions };
    let done = AtomicBool::new(false);
    let racers = futures_util::future::join_all(
        owners
            .iter()
            .enumerate()
            .map(|(index, (store, owner))| race(*store, owner, index, rounds, &pool)),
    );
    let steppers =
        futures_util::future::join_all(nodes.iter().map(|store| step_until(*store, &done)));
    let (raced, stepped) = futures_util::join!(
        async {
            let raced = racers.await;
            done.store(true, Ordering::SeqCst);
            raced
        },
        steppers,
    );
    for stepped in stepped {
        stepped?;
    }
    let mut lives = Vec::new();
    for raced in raced {
        lives.push(raced?);
    }
    drain(tier.pages(), 4_096).await?;
    let backlog = tier.pages().retirement_backlog().await?;
    ensure!(
        backlog.holds == 0,
        "{} holds outlived the drain",
        backlog.holds
    );
    let stored = tier.stored().await;
    for live in &lives {
        for page in &live.keeps {
            ensure!(
                stored.contains(page),
                "{page} was reclaimed under its live root"
            );
        }
        for page in &live.retired {
            ensure!(!stored.contains(page), "{page} outlived its last owner");
        }
    }
    Ok(())
}

/// One choreographed round (see the law). Owner A on the first node and
/// owner B on the second share pages `held < taken` in digest order and a
/// page `stalled`. B admits `held` and `taken`; A admits `taken`,
/// `stalled` and a page `dead` over `stalled` and `held`, then retires
/// `taken`, then `dead`, so a step takes `taken`'s hold first. A foreign
/// peel holds `stalled` and foreign writers name `held` and `taken` while
/// the step runs: it takes `taken`'s hold row, deletes `dead`, skips all
/// three pages and ends with them in its frontier. B's publication then
/// retires `held` and `taken`: it holds `held`, then meets the hold row
/// the step took.
async fn choreographed(
    tier: &dyn PageTier,
    probe: &dyn PageLockProbe,
    leases: &[NodeLease],
    round: usize,
) -> LawOutcome {
    let (first, second) = (tier.pages(), tier.peer());
    let a = owned_on(first, &leases[0], &format!("pages-choreo-a-{round}")).await?;
    let b = owned_on(second, &leases[1], &format!("pages-choreo-b-{round}")).await?;
    let (a_exec, b_exec) = (a.exec("choreo"), b.exec("choreo"));
    let mut shared: Vec<EncodedNode> = (0..2)
        .map(|index| {
            encoded(
                NodeKind::FROZEN_VALUE,
                &format!("choreo {round} {index}"),
                &[],
            )
        })
        .collect();
    shared.sort_by_key(|node| node.id);
    let [held, taken] =
        <[EncodedNode; 2]>::try_from(shared).map_err(|_| LawBroken("two shared pages".into()))?;
    let stalled = encoded(
        NodeKind::FROZEN_VALUE,
        &format!("choreo {round} stalled"),
        &[],
    );
    let dead = encoded(
        NodeKind::FROZEN_VALUE,
        &format!("choreo {round} dead"),
        &[stalled.id, held.id],
    );
    admit(
        second,
        &b,
        &b_exec,
        &[],
        &[
            (named("held"), held.clone()),
            (named("taken"), taken.clone()),
        ],
    )
    .await?;
    admit(
        first,
        &a,
        &a_exec,
        &[],
        &[
            (named("taken"), taken.clone()),
            (named("stalled"), stalled.clone()),
            (named("dead"), dead.clone()),
        ],
    )
    .await?;
    drain(first, 4_096).await?;
    tier.advance(10);
    admit(first, &a, &a_exec, &[(named("taken"), taken.id)], &[]).await?;
    tier.advance(10);
    admit(first, &a, &a_exec, &[(named("dead"), dead.id)], &[]).await?;
    let mut peeling = probe.peel_hold(&stalled.id.to_hex()).await;
    let naming = [
        probe.name_hold(&held.id.to_hex()).await,
        probe.name_hold(&taken.id.to_hex()).await,
    ];
    let mut fresh = BTreeMap::new();
    let leaf = page(
        &mut fresh,
        NodeKind::PAYLOAD_LEAF,
        &format!("choreo {round} leaf"),
        &[],
    );
    let root = page(
        &mut fresh,
        NodeKind::CHECKPOINT_ROOT,
        &format!("choreo {round} root"),
        &[leaf],
    );
    let candidate = Candidate { root, fresh };
    let removes = [(named("held"), held.id), (named("taken"), taken.id)];
    let step = first.retire_step(RetirementBudget { nodes: STEP_NODES });
    let publication = publish(second, &b, &b_exec, None, &candidate, &removes, &[]);
    tokio::pin!(step, publication);
    // The step either ends without waiting, or waits behind the foreign
    // peel holding everything it locked.
    let stepped = tokio::select! {
        stepped = &mut step => Some(stepped),
        () = peeling.wait_for_blocked() => None,
    };
    for hold in naming {
        hold.release().await;
    }
    let (stepped, published) = match stepped {
        Some(stepped) => {
            let published = (&mut publication).await;
            peeling.release().await;
            (stepped, published)
        }
        None => {
            // The publication either commits, or waits behind the step.
            let published = tokio::select! {
                published = &mut publication => Some(published),
                () = probe.wait_for_lock_waiters(2) => None,
            };
            peeling.release().await;
            match published {
                Some(published) => ((&mut step).await, published),
                None => futures_util::join!(&mut step, &mut publication),
            }
        }
    };
    stepped.map_err(|error| {
        LawBroken(format!(
            "choreographed round {round}: the retirement step failed: {error}"
        ))
    })?;
    published.map_err(|error| {
        LawBroken(format!(
            "choreographed round {round}: the publication failed: {error}"
        ))
    })?;
    Ok(())
}

/// The shared pages every candidate draws from.
struct Pool {
    leaves: Vec<EncodedNode>,
    admissions: Vec<EncodedNode>,
}

impl Pool {
    /// Owner `owner`'s candidate of round `round`: a CheckpointRoot of its
    /// own over two shared leaves and two shared admission nodes.
    fn candidate(&self, owner: usize, round: usize) -> Candidate {
        let mut fresh = BTreeMap::new();
        let mut children = BTreeSet::new();
        for node in [
            &self.leaves[(owner + round) % LEAVES],
            &self.leaves[(owner + 2 * round + 1) % LEAVES],
            &self.admissions[(owner + round) % ADMISSIONS],
            &self.admissions[(owner + round + 2) % ADMISSIONS],
        ] {
            fresh.insert(node.id, node.clone());
            children.insert(node.id);
        }
        let children: Vec<NodeId> = children.into_iter().collect();
        let root = page(
            &mut fresh,
            NodeKind::CHECKPOINT_ROOT,
            &format!("race {owner} {round}"),
            &children,
        );
        Candidate { root, fresh }
    }

    /// The two admission nodes owner `owner` admits in round `round`.
    fn admissions(&self, owner: usize, round: usize) -> [EncodedNode; 2] {
        let first = (3 * owner + round) % ADMISSIONS;
        [
            self.admissions[first].clone(),
            self.admissions[(first + ADMISSIONS / 2) % ADMISSIONS].clone(),
        ]
    }
}

/// What an owner's race left: the pages its live roots keep, and the
/// pages only its retired roots kept.
struct Live {
    keeps: BTreeSet<NodeId>,
    retired: BTreeSet<NodeId>,
}

/// A session actor created through `store` and claimed by `lease`, its
/// node.
async fn owned_on(store: &dyn PageStore, lease: &NodeLease, id: &str) -> Result<Owned, LawBroken> {
    let actor = session(id)?;
    create(store, std::slice::from_ref(&actor)).await?;
    let claimed = store.claim(lease, 1).await?;
    let epoch = claimed
        .iter()
        .find(|claimed| claimed.actor == actor)
        .map(|claimed| claimed.epoch)
        .ok_or_else(|| LawBroken(format!("{id} was not claimed")))?;
    Ok(Owned {
        actor,
        epoch,
        node: lease.clone(),
        session: id.to_owned(),
    })
}

/// Owner `index`'s rounds on `store`: each publishes the round's candidate
/// over the previous root, retiring it and the previous effect admissions,
/// then reads the previous root's pages under the lease it took on it,
/// and releases the lease.
async fn race(
    store: &dyn PageStore,
    owner: &Owned,
    index: usize,
    rounds: usize,
    pool: &Pool,
) -> Result<Live, LawBroken> {
    let exec = owner.exec("race");
    let mut previous: Option<(Candidate, [EncodedNode; 2])> = None;
    let mut retired = BTreeSet::new();
    for round in 0..rounds {
        let candidate = pool.candidate(index, round);
        let admissions = pool.admissions(index, round);
        let lease = match &previous {
            Some((before, _)) => Some(
                store
                    .acquire_root_lease(lease_request(
                        owner,
                        &exec,
                        exec_root(before.root).id,
                        600_000,
                    ))
                    .await
                    .map_err(|error| failed(index, round, "lease", &error))?,
            ),
            None => None,
        };
        let base = previous
            .as_ref()
            .map(|(before, _)| (round as u64, exec_root(before.root).id));
        let removes: Vec<(EffectKey, NodeId)> = previous
            .iter()
            .flat_map(|(_, before)| before.iter().enumerate())
            .map(|(slot, admission)| (effect(round - 1, slot), admission.id))
            .collect();
        let puts: Vec<(EffectKey, EncodedNode)> = admissions
            .iter()
            .enumerate()
            .map(|(slot, admission)| (effect(round, slot), admission.clone()))
            .collect();
        publish(store, owner, &exec, base, &candidate, &removes, &puts)
            .await
            .map_err(|error| failed(index, round, "publication", &error))?;
        if let (Some(lease), Some((before, _))) = (lease, &previous) {
            let mut leased: Vec<NodeId> = before.fresh.keys().copied().collect();
            leased.push(exec_root(before.root).id);
            let read = store
                .read_pages(&lease, &leased)
                .await
                .map_err(|error| failed(index, round, "leased read", &error))?;
            for page in &read {
                let expected = before
                    .fresh
                    .get(&page.id)
                    .map_or_else(|| exec_root(before.root).bytes, |node| node.bytes.clone());
                ensure!(
                    page.bytes == expected,
                    "owner {index} round {round}: leased page {} read other bytes",
                    page.id
                );
            }
            ensure!(
                read.len() == leased.len(),
                "owner {index} round {round}: a leased read lost pages"
            );
            store
                .release_root_lease(lease.id)
                .await
                .map_err(|error| failed(index, round, "lease release", &error))?;
            retired.insert(before.root);
            retired.insert(exec_root(before.root).id);
        }
        previous = Some((candidate, admissions));
    }
    let mut keeps = BTreeSet::new();
    if let Some((candidate, admissions)) = &previous {
        keeps.extend(candidate.fresh.keys().copied());
        keeps.insert(exec_root(candidate.root).id);
        keeps.extend(admissions.iter().map(|admission| admission.id));
    }
    Ok(Live { keeps, retired })
}

/// One owner commit publishing `candidate` as `exec`'s root over `base`
/// (its revision and root), removing the effect admissions `removes` names
/// and admitting `puts`.
async fn publish(
    store: &dyn PageStore,
    owner: &Owned,
    exec: &ExecKey,
    base: Option<(u64, NodeId)>,
    candidate: &Candidate,
    removes: &[(EffectKey, NodeId)],
    puts: &[(EffectKey, EncodedNode)],
) -> Result<(), DurableError> {
    let manifest = candidate.manifest(base.map(|(_, root)| root), CommitId::mint());
    let summary = lash_core_store::checkpoint_graph::stage::CandidateManifest::summary(&manifest);
    let root = exec_root(candidate.root);
    let mut tx = store.begin(&owner.actor, owner.epoch).await?;
    tx.stage([root.clone()]);
    tx.stage(puts.iter().map(|(_, admission)| admission.clone()));
    for (effect, expected) in removes {
        tx.write(DomainWrite::EffectAdmission(EffectAdmissionWrite::Remove {
            exec: exec.clone(),
            effect: effect.clone(),
            expected: *expected,
        }));
    }
    for (effect, admission) in puts {
        tx.write(DomainWrite::EffectAdmission(EffectAdmissionWrite::Put {
            exec: exec.clone(),
            effect: effect.clone(),
            member: None,
            admission: admission.id,
        }));
    }
    tx.write(DomainWrite::ExecRoot(ExecRootWrite::Put {
        exec: exec.clone(),
        expected: base.map(|(revision, _)| SnapshotRev(revision)),
        expected_root: base.map(|(_, root)| root),
        root: root.id,
        commit_id: summary.commit,
    }));
    store
        .commit_candidate(
            tx,
            CandidatePublication {
                owner: CandidateOwner::ExecRootPut(exec.clone()),
                manifest: Box::new(manifest),
            },
            LABEL,
        )
        .await?;
    Ok(())
}

/// An owner commit of effect admissions alone: `removes` removed, `puts`
/// admitted.
async fn admit(
    store: &dyn PageStore,
    owner: &Owned,
    exec: &ExecKey,
    removes: &[(EffectKey, NodeId)],
    puts: &[(EffectKey, EncodedNode)],
) -> Result<(), DurableError> {
    let mut tx = store.begin(&owner.actor, owner.epoch).await?;
    tx.stage(puts.iter().map(|(_, admission)| admission.clone()));
    for (effect, expected) in removes {
        tx.write(DomainWrite::EffectAdmission(EffectAdmissionWrite::Remove {
            exec: exec.clone(),
            effect: effect.clone(),
            expected: *expected,
        }));
    }
    for (effect, admission) in puts {
        tx.write(DomainWrite::EffectAdmission(EffectAdmissionWrite::Put {
            exec: exec.clone(),
            effect: effect.clone(),
            member: None,
            admission: admission.id,
        }));
    }
    store.commit(tx, LABEL).await?;
    Ok(())
}

fn effect(round: usize, slot: usize) -> EffectKey {
    named(&format!("race {round} {slot}"))
}

fn named(effect: &str) -> EffectKey {
    EffectKey(effect.as_bytes().into())
}

fn failed(owner: usize, round: usize, what: &str, error: &DurableError) -> LawBroken {
    LawBroken(format!(
        "owner {owner} round {round}: the {what} failed: {error}"
    ))
}

/// Bounded retirement steps on `store` until `done`, then one more.
async fn step_until(store: &dyn PageStore, done: &AtomicBool) -> LawOutcome {
    let mut steps = 0_u64;
    loop {
        let last = done.load(Ordering::SeqCst);
        store
            .retire_step(RetirementBudget { nodes: STEP_NODES })
            .await
            .map_err(|error| LawBroken(format!("retirement step {steps} failed: {error}")))?;
        steps += 1;
        if last {
            return Ok(());
        }
        tokio::task::yield_now().await;
    }
}
