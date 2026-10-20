//! The laws of the node's own page retirement (FIG-6193, `D-RET`): a
//! production runner retires sealed pages with no test hook driving it,
//! holds its claims while its store's retirement is behind, and a node that
//! dies in a retirement batch leaves a frontier the next one finishes
//! exactly.
//!
//! A law serves a production runner over [`Retiring`], which forwards every
//! call to the dialect's store unchanged and records, in order, what the
//! node's retirement task and its claims saw and did: the counted oracles.
//! Only the law can close its step or observation gate, or cut a step.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use lash_core_ids::clock::{Clock, SystemClock};
use lash_core_store::checkpoint_graph::{CommitId, NodeId, NodeKind};
use lash_protocol_types::{ProcessId, SessionId, TurnId};
use tokio::sync::watch;

use super::super::node_wakes::{LawNode, serve_node_on};
use super::super::{LawBroken, LawOutcome, create, ensure, session};
use super::{
    Candidate, GeneratedManifest, Owned, PageTier, Recipe, children_of, exec_root, owned, page,
    publish, simple,
};
use crate::domain::pages::{
    CandidatePublication, CandidateReceipt, PageStore, RetirementBacklog, RetirementBudget,
    RetirementProgress, RootLease, RootLeaseId, RootLeaseRequest, StoredPage,
};
use crate::domain::{self, ExecKey};
use crate::{
    ActorKey, ActorSnapshot, ActorTx, Claimed, CommitLabel, DurableError, DurableInstant,
    DurableReads, DurableSettings, DurableStore, Epoch, FormatSet, HeartbeatOutcome, LeaseSettings,
    MailCommit, MailTx, NodeLease, NodeSpec, Reaped, RetirementSettings,
};

/// The laws' hang guard, never an assertion about latency: a node that
/// does not retire, or never claims, fails here instead of hanging.
const WATCHDOG: Duration = Duration::from_secs(60);

/// What a law node's store saw, in the order observed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Seen {
    /// A backlog read and the holds it answered.
    Backlog(u64),
    /// A retirement step was called.
    Called,
    /// A retirement step committed, with what it did.
    Step(RetirementProgress),
    /// The last batch committed, but its result is behind the observation gate.
    StepPending,
    /// The law's cut fired in a step: the node dies there.
    Cut,
    /// A claim attempt, recorded before entering the store's await.
    ClaimAttempt,
    /// A returned claim and how many actors it took.
    Claim(usize),
    /// A reap: one turn of the node's serve loop.
    Reap,
}

/// Where the law kills a node in its retirement batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BatchCut {
    /// Before the batch's transaction commits: it rolls back.
    BeforeCommit,
    /// After it commits, before the node hears the answer.
    AfterCommit,
}

/// The system clock, counting the node's waits that end at once: a serve
/// loop that sleeps until a deadline already past spins.
#[derive(Debug, Default)]
struct Turns {
    late: AtomicU64,
}

#[async_trait::async_trait]
impl Clock for Turns {
    fn now(&self) -> Instant {
        SystemClock.now()
    }

    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        SystemClock.timestamp_datetime()
    }

    async fn sleep(&self, duration: Duration) {
        SystemClock.sleep(duration).await;
    }

    async fn sleep_until(&self, deadline: Instant) {
        if deadline <= SystemClock.now() {
            self.late.fetch_add(1, Ordering::AcqRel);
        }
        SystemClock.sleep_until(deadline).await;
    }
}

struct Probe {
    seen: Mutex<Vec<Seen>>,
    turns: Arc<Turns>,
    /// Whether retirement steps may run.
    gate: watch::Sender<bool>,
    /// Whether a committed final step may record its result.
    step_observation: watch::Sender<bool>,
    calls: AtomicU32,
    /// The call whose batch the node dies in, and where.
    cut: Option<(u32, BatchCut)>,
    /// A successful claim reached its cut or recorded outcome.
    claim_observed: watch::Sender<bool>,
}

impl Probe {
    fn see(&self, seen: Seen) {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(seen);
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Every step that committed, its cut one's included.
    fn steps(&self) -> Vec<RetirementProgress> {
        self.seen()
            .into_iter()
            .filter_map(|seen| match seen {
                Seen::Step(progress) => Some(progress),
                _ => None,
            })
            .collect()
    }

    /// Wait until what the node's store saw satisfies `reached`.
    async fn until(&self, what: &str, reached: impl Fn(&[Seen]) -> bool + Sync) -> LawOutcome {
        within(what, || async { Ok(reached(&self.seen())) }).await
    }
}

/// Wait on `reached`, yielding between reads, within the hang guard.
async fn within<F, Fut>(what: &str, mut reached: F) -> LawOutcome
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<bool, LawBroken>>,
{
    let deadline = tokio::time::Instant::now() + WATCHDOG;
    while !reached().await? {
        ensure!(
            tokio::time::Instant::now() < deadline,
            "{what}: not reached within the {WATCHDOG:?} hang guard"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Ok(())
}

/// The dialect's store as a law node runs on it: every call forwarded
/// unchanged, retirement and claims recorded, and retirement steps behind
/// the law's gate and cut.
#[derive(Clone)]
struct Retiring {
    inner: Arc<dyn PageStore>,
    probe: Arc<Probe>,
    /// Park after a successful claim, before recording its outcome.
    cut_claim: bool,
    /// Force the final commit to precede observation of its result.
    pause_drained_step: bool,
}

impl Retiring {
    /// Over `tier`'s store, with steps gated by `open` and dying at `cut`.
    fn over(
        tier: &dyn PageTier,
        open: bool,
        cut: Option<(u32, BatchCut)>,
    ) -> Result<Self, LawBroken> {
        let inner = tier
            .pages()
            .pages()
            .ok_or_else(|| LawBroken("a page tier's store publishes pages".to_owned()))?;
        Ok(Self {
            inner,
            probe: Arc::new(Probe {
                seen: Mutex::new(Vec::new()),
                turns: Arc::default(),
                gate: watch::Sender::new(open),
                step_observation: watch::Sender::new(true),
                calls: AtomicU32::new(0),
                cut,
                claim_observed: watch::Sender::new(false),
            }),
            cut_claim: false,
            pause_drained_step: false,
        })
    }

    /// Stop a healthy node only after its retirement task observed the
    /// drained frontier. That task records each step before reading its next
    /// backlog; an independent reader can see the final commit too early.
    async fn finish(&self, node: LawNode) -> LawOutcome {
        self.probe.step_observation.send_replace(true);
        self.probe
            .until(
                "the retirement task observes its drained frontier",
                |seen| seen.contains(&Seen::Backlog(0)),
            )
            .await?;
        node.kill().await;
        Ok(())
    }

    /// Serve production's runner as node `name` over this store.
    fn serve(&self, name: &str, retirement: RetirementSettings) -> LawNode {
        serve_node_on(
            Arc::new(self.clone()),
            None,
            name,
            DurableSettings {
                // Claims and reaps turn the serve loop every 50 ms.
                lease: LeaseSettings {
                    claim_poll: Duration::from_millis(50),
                    claim_backoff: Duration::from_millis(10),
                    reap_every: Duration::from_millis(50),
                    ..LeaseSettings::default()
                },
                retirement,
                ..DurableSettings::default()
            },
            self.probe.turns.clone(),
        )
    }
}

/// A retired tree: what its replaced root held, and what of it a later
/// root still reaches.
struct Retired {
    /// Its pages and the execution root over them.
    nodes: BTreeSet<NodeId>,
    survivors: BTreeSet<NodeId>,
}

impl Retired {
    /// How many nodes its retirement deletes.
    fn unowned(&self) -> u64 {
        (self.nodes.len() - self.survivors.len()) as u64
    }

    /// Exactly the survivors are stored: no page leaked, none freed while
    /// reached.
    async fn reclaimed(&self, tier: &dyn PageTier) -> LawOutcome {
        let stored = tier.stored().await;
        for node in &self.nodes {
            ensure!(
                stored.contains(node) == self.survivors.contains(node),
                "{node} is {} after retirement",
                if stored.contains(node) {
                    "stored though no root reaches it"
                } else {
                    "gone though a root reaches it"
                }
            );
        }
        Ok(())
    }
}

/// Publishes a `leaves`-leaf tree under `owner`'s execution `cell`, then
/// replaces it with a root that keeps its first index: the old root becomes
/// a retirement hold.
async fn retired_tree(
    tier: &dyn PageTier,
    owner: &Owned,
    cell: &str,
    leaves: u32,
) -> Result<Retired, LawBroken> {
    let exec: ExecKey = owner.exec(cell);
    let big = GeneratedManifest::new(cell, leaves, 50, CommitId::mint());
    let mut nodes = big.nodes();
    let kept = big.build(Recipe::Index(0));
    let first_exec = exec_root(big.summary.root).id;
    nodes.insert(first_exec);
    publish(tier, owner, &simple(&exec, None), Box::new(big)).await?;
    let mut fresh = std::collections::BTreeMap::new();
    let root = page(
        &mut fresh,
        NodeKind::CHECKPOINT_ROOT,
        &format!("{cell} replacement"),
        &[kept.id],
    );
    let replacement = Candidate { root, fresh };
    publish(
        tier,
        owner,
        &simple(&exec, Some((1, first_exec))),
        Box::new(replacement.manifest(Some(first_exec), CommitId::mint())),
    )
    .await?;
    let mut survivors: BTreeSet<NodeId> = children_of(&kept).into_iter().collect();
    survivors.insert(kept.id);
    Ok(Retired { nodes, survivors })
}

async fn holds(tier: &dyn PageTier) -> Result<u64, LawBroken> {
    Ok(tier.pages().retirement_backlog().await?.holds)
}

/// Every step kept its batch, and together they deleted exactly `unowned`
/// nodes: none twice. Returns the batches that did work.
fn counted(steps: &[RetirementProgress], batch: u64, unowned: u64) -> Result<u64, LawBroken> {
    for step in steps {
        ensure!(
            step.nodes_deleted <= batch,
            "a batch of {batch} deleted {} nodes",
            step.nodes_deleted
        );
    }
    let deleted: u64 = steps.iter().map(|step| step.nodes_deleted).sum();
    ensure!(
        deleted == unowned,
        "the batches deleted {deleted} nodes, not the {unowned} no root reaches"
    );
    let batches = steps
        .iter()
        .filter(|step| **step != RetirementProgress::default())
        .count() as u64;
    ensure!(
        batches >= unowned.div_ceil(batch),
        "{batches} batches of {batch} deleted {unowned} nodes"
    );
    Ok(batches)
}

/// D-RET: a serving node retires sealed pages on its own. Nothing but the
/// node's retirement task runs a step: the replaced tree's holds drain to
/// none in bounded batches, exactly what no root reaches is deleted, once,
/// and the page a later root still reaches is kept.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_node_retires_sealed_pages_on_its_own(tier: &dyn PageTier) -> LawOutcome {
    const BATCH: u64 = 64;
    let owner = owned(tier, "pages-driver").await?;
    let retired = retired_tree(tier, &owner, "driver", 2_000).await?;
    ensure!(holds(tier).await? > 0, "the replaced root left no hold");
    let store = Retiring::over(tier, true, None)?;
    let node = store.serve(
        "pages-driver-retirer",
        RetirementSettings {
            every: Duration::from_millis(5),
            batch_nodes: BATCH,
            steps_per_sweep: 4,
            backlog_limit: RetirementSettings::MAX_BACKLOG_LIMIT,
        },
    );
    store.finish(node).await?;
    retired.reclaimed(tier).await?;
    let batches = counted(&store.probe.steps(), BATCH, retired.unowned())?;
    eprintln!(
        "driver: retirement_batches={batches} deleted={} survivors={} batch_nodes={BATCH}",
        retired.unowned(),
        retired.survivors.len()
    );
    Ok(())
}

/// D-RET backpressure: a node whose store's retirement backlog is above its
/// limit claims nothing, from its boot on, while its retirement cannot
/// catch up; its serve loop keeps turning on its timers without spinning.
/// Once retirement brings the backlog under the limit it claims the ready
/// actor, and no claim attempt comes before a backlog read under the limit.
/// Both a returned claim and a kill after its commit but before its outcome
/// is recorded retain the attempt that acquired the actor.
///
/// # Errors
///
/// The first rule broken.
pub async fn backpressure_holds_claims_until_retirement_catches_up(
    tier: &dyn PageTier,
) -> LawOutcome {
    for cut_claim in [false, true] {
        backpressure_case(tier, cut_claim).await?;
    }
    Ok(())
}

async fn backpressure_case(tier: &dyn PageTier, cut_claim: bool) -> LawOutcome {
    const LIMIT: u64 = 1;
    let case = if cut_claim { "cut" } else { "returned" };
    let owner = owned(tier, &format!("pages-pressure-{case}")).await?;
    for cell in ["pressure-a", "pressure-b", "pressure-c"] {
        retired_tree(tier, &owner, cell, 300).await?;
    }
    let before = holds(tier).await?;
    ensure!(before > LIMIT, "{before} holds are not behind {LIMIT}");
    let work = session(&format!("pages-pressure-work-{case}"))?;
    create(tier.pages(), std::slice::from_ref(&work)).await?;
    // Retirement stays behind until the law opens the gate.
    let mut store = Retiring::over(tier, false, None)?;
    // Force the cancellation window: ownership is durable, but the wrapper
    // cannot record the returned claim before the law kills the node.
    store.cut_claim = cut_claim;
    let name = &format!("pages-pressure-claimer-{case}");
    let node = store.serve(
        name,
        RetirementSettings {
            every: Duration::from_millis(5),
            batch_nodes: 16,
            steps_per_sweep: 4,
            backlog_limit: LIMIT,
        },
    );
    let probe = &store.probe;
    probe
        .until("the node's retirement starts a step", |seen| {
            seen.contains(&Seen::Called)
        })
        .await?;
    // Five turns of the serve loop after retirement fell behind: each would
    // have claimed the ready actor.
    let called = probe.seen().len();
    let late = probe.turns.late.load(Ordering::Acquire);
    probe
        .until("the serve loop turns while held back", |seen| {
            seen[called..]
                .iter()
                .filter(|seen| **seen == Seen::Reap)
                .count()
                >= 5
        })
        .await?;
    let held = probe.seen();
    let late = probe.turns.late.load(Ordering::Acquire) - late;
    let turns = held[called..]
        .iter()
        .filter(|seen| **seen == Seen::Reap)
        .count() as u64;
    // A reap can outlast the claim backoff once per turn; a spinning loop
    // wakes at once on every turn.
    ensure!(
        late <= turns + 2,
        "the held node's serve loop spun: {late} waits ended at once across {turns} reaps"
    );
    ensure!(
        !held.contains(&Seen::ClaimAttempt),
        "the node claimed while its retirement was behind: {held:?}"
    );
    ensure!(
        tier.pages()
            .actor(&work)
            .await?
            .and_then(|actor| actor.owner)
            .is_none(),
        "the ready actor was taken while retirement was behind"
    );
    probe.gate.send_replace(true);
    within("the node claims once retirement catches up", || async {
        Ok(tier
            .pages()
            .actor(&work)
            .await?
            .and_then(|actor| actor.owner)
            .is_some_and(|owner| owner.node.as_str() == name))
    })
    .await?;
    let mut observed = probe.claim_observed.subscribe();
    tokio::time::timeout(WATCHDOG, observed.wait_for(|observed| *observed))
        .await
        .map_err(|_| LawBroken("the successful claim was never observed".to_owned()))?
        .map_err(|_| LawBroken("the claim probe closed before observing it".to_owned()))?;
    node.kill().await;
    let seen = probe.seen();
    if cut_claim {
        ensure!(
            !seen.iter().any(|seen| matches!(seen, Seen::Claim(_))),
            "the claim outcome escaped the cancellation cut: {seen:?}"
        );
    }
    let first_claim = seen
        .iter()
        .position(|seen| *seen == Seen::ClaimAttempt)
        .ok_or_else(|| LawBroken("the owner took the actor without a claim".to_owned()))?;
    let caught_up = seen
        .iter()
        .position(|seen| matches!(seen, Seen::Backlog(holds) if *holds <= LIMIT))
        .ok_or_else(|| LawBroken("no backlog read fell under the limit".to_owned()))?;
    ensure!(
        caught_up < first_claim,
        "the node claimed before a backlog read under {LIMIT}: {seen:?}"
    );
    let batches = seen[..first_claim]
        .iter()
        .filter(|seen| matches!(seen, Seen::Step(progress) if *progress != RetirementProgress::default()))
        .count();
    ensure!(batches > 0, "no retirement batch ran before the claim");
    eprintln!(
        "backpressure {case}: holds_before={before} limit={LIMIT} retirement_batches_before_claim={batches} held_reaps={turns} late_waits={late}"
    );
    Ok(())
}

/// D-RET crash: a node that dies inside a retirement batch, before or after
/// its transaction commits, leaves holds another node resumes from. In the
/// end exactly what no root reaches is gone, the batches deleted each such
/// page once, and the page a later root reaches is stored.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_crash_in_a_retirement_batch_resumes_without_leak_or_double_free(
    tier: &mut dyn PageTier,
) -> LawOutcome {
    const BATCH: u64 = 64;
    const CUT_AT: u32 = 3;
    let settings = RetirementSettings {
        every: Duration::from_millis(5),
        batch_nodes: BATCH,
        steps_per_sweep: 4,
        backlog_limit: RetirementSettings::MAX_BACKLOG_LIMIT,
    };
    for (round, cut) in [
        ("before", BatchCut::BeforeCommit),
        ("after", BatchCut::AfterCommit),
    ] {
        let owner = owned(&*tier, &format!("pages-crash-{round}")).await?;
        let retired = retired_tree(&*tier, &owner, &format!("crash-{round}"), 2_000).await?;
        let dying = Retiring::over(&*tier, true, Some((CUT_AT, cut)))?;
        let node = dying.serve(&format!("pages-crash-{round}-a"), settings);
        dying
            .probe
            .until("the node reaches its cut batch", |seen| {
                seen.contains(&Seen::Cut)
            })
            .await?;
        // The process dies in the batch; another opens the store again.
        node.kill().await;
        tier.reopen().await;
        let committed = dying.probe.steps();
        let expected = CUT_AT - u32::from(cut == BatchCut::BeforeCommit);
        ensure!(
            committed.len() == expected as usize,
            "{round}: {} batches committed before the cut, not {expected}",
            committed.len()
        );
        ensure!(
            holds(&*tier).await? > 0,
            "{round}: the frontier did not survive the crash"
        );
        let mut resuming = Retiring::over(&*tier, true, None)?;
        resuming.pause_drained_step = true;
        resuming.probe.step_observation.send_replace(false);
        let node = resuming.serve(&format!("pages-crash-{round}-b"), settings);
        within("the next node retires every hold", || async {
            Ok(holds(&*tier).await? == 0)
        })
        .await?;
        resuming
            .probe
            .until("the final commit precedes its observation", |seen| {
                seen.contains(&Seen::StepPending)
            })
            .await?;
        let observed_deleted: u64 = committed
            .iter()
            .chain(resuming.probe.steps().iter())
            .map(|step| step.nodes_deleted)
            .sum();
        ensure!(
            observed_deleted < retired.unowned(),
            "the forced commit-before-observation window lost no evidence"
        );
        eprintln!(
            "crash {round}: holds=0 before_observation={observed_deleted} expected={}",
            retired.unowned()
        );
        resuming.finish(node).await?;
        retired.reclaimed(&*tier).await?;
        let steps: Vec<RetirementProgress> = committed
            .into_iter()
            .chain(resuming.probe.steps())
            .collect();
        let batches = counted(&steps, BATCH, retired.unowned())?;
        eprintln!(
            "crash {round}: retirement_batches={batches} before_cut={expected} deleted={}",
            retired.unowned()
        );
    }
    Ok(())
}

#[async_trait::async_trait]
impl PageStore for Retiring {
    async fn acquire_root_lease(
        &self,
        request: RootLeaseRequest,
    ) -> Result<RootLease, DurableError> {
        self.inner.acquire_root_lease(request).await
    }

    async fn renew_root_lease(&self, lease: &RootLease) -> Result<RootLease, DurableError> {
        self.inner.renew_root_lease(lease).await
    }

    async fn release_root_lease(&self, lease: RootLeaseId) -> Result<(), DurableError> {
        self.inner.release_root_lease(lease).await
    }

    async fn read_pages(
        &self,
        lease: &RootLease,
        pages: &[NodeId],
    ) -> Result<Vec<StoredPage>, DurableError> {
        self.inner.read_pages(lease, pages).await
    }

    async fn commit_candidate(
        &self,
        tx: ActorTx,
        candidate: CandidatePublication,
        label: CommitLabel,
    ) -> Result<CandidateReceipt, DurableError> {
        self.inner.commit_candidate(tx, candidate, label).await
    }

    async fn candidate_state(
        &self,
        exec: &ExecKey,
    ) -> Result<Option<(NodeId, CommitId)>, DurableError> {
        self.inner.candidate_state(exec).await
    }

    async fn retire_step(
        &self,
        budget: RetirementBudget,
    ) -> Result<RetirementProgress, DurableError> {
        let call = self.probe.calls.fetch_add(1, Ordering::AcqRel) + 1;
        self.probe.see(Seen::Called);
        let mut open = self.probe.gate.subscribe();
        // The probe owns the sender, so the gate never closes for good.
        let _ = open.wait_for(|open| *open).await;
        let cut = self
            .probe
            .cut
            .and_then(|(at, cut)| (at == call).then_some(cut));
        if cut == Some(BatchCut::BeforeCommit) {
            self.probe.see(Seen::Cut);
            return std::future::pending().await;
        }
        let progress = self.inner.retire_step(budget).await;
        if let Ok(progress) = &progress {
            if self.pause_drained_step
                && *progress != RetirementProgress::default()
                && self.inner.retirement_backlog().await?.holds == 0
            {
                self.probe.see(Seen::StepPending);
                let mut observation = self.probe.step_observation.subscribe();
                let _ = observation.wait_for(|open| *open).await;
            }
            self.probe.see(Seen::Step(*progress));
        }
        if cut == Some(BatchCut::AfterCommit) {
            self.probe.see(Seen::Cut);
            return std::future::pending().await;
        }
        progress
    }

    async fn retirement_backlog(&self) -> Result<RetirementBacklog, DurableError> {
        let backlog = self.inner.retirement_backlog().await;
        if let Ok(backlog) = &backlog {
            self.probe.see(Seen::Backlog(backlog.holds));
        }
        backlog
    }
}

#[async_trait::async_trait]
impl DurableStore for Retiring {
    async fn now(&self) -> Result<DurableInstant, DurableError> {
        self.inner.now().await
    }

    async fn await_owner_commit(&self, actor: &ActorKey) -> Result<(), DurableError> {
        self.inner.await_owner_commit(actor).await
    }

    fn pages(&self) -> Option<Arc<dyn PageStore>> {
        Some(Arc::new(self.clone()))
    }

    async fn register_node(&self, spec: &NodeSpec) -> Result<NodeLease, DurableError> {
        self.inner.register_node(spec).await
    }

    async fn heartbeat(&self, node: &NodeLease) -> Result<HeartbeatOutcome, DurableError> {
        self.inner.heartbeat(node).await
    }

    async fn reap(&self, reaper: &NodeLease) -> Result<Vec<Reaped>, DurableError> {
        let reaped = self.inner.reap(reaper).await;
        self.probe.see(Seen::Reap);
        reaped
    }

    async fn release_node(&self, node: &NodeLease) -> Result<Vec<ActorKey>, DurableError> {
        self.inner.release_node(node).await
    }

    async fn acquire_idle_for_testing(
        &self,
        node: &NodeLease,
        actor: &ActorKey,
        epoch: Epoch,
    ) -> Result<Epoch, DurableError> {
        self.inner
            .acquire_idle_for_testing(node, actor, epoch)
            .await
    }

    async fn claim(&self, node: &NodeLease, limit: usize) -> Result<Vec<Claimed>, DurableError> {
        // A committed claim can become visible before this await returns.
        // Cancellation must not erase the attempt that acquired its owner.
        self.probe.see(Seen::ClaimAttempt);
        let claimed = self.inner.claim(node, limit).await;
        if let Ok(claimed) = &claimed {
            if self.cut_claim && !claimed.is_empty() {
                self.probe.claim_observed.send_replace(true);
                return std::future::pending().await;
            }
            self.probe.see(Seen::Claim(claimed.len()));
            if !claimed.is_empty() {
                self.probe.claim_observed.send_replace(true);
            }
        }
        claimed
    }

    async fn mark_draining(&self, node: &NodeLease) -> Result<(), DurableError> {
        self.inner.mark_draining(node).await
    }

    async fn live_nodes(&self) -> Result<Vec<crate::NodeCapability>, DurableError> {
        self.inner.live_nodes().await
    }

    async fn install_packages(
        &self,
        artifacts: &[crate::packages::ArtifactInstall],
        sets: &[crate::packages::SetInstall],
    ) -> Result<(), DurableError> {
        self.inner.install_packages(artifacts, sets).await
    }

    async fn package_catalog(&self) -> Result<crate::PackageCatalog, DurableError> {
        self.inner.package_catalog().await
    }

    async fn package_surface(
        &self,
        digest: &crate::PackageSetDigest,
    ) -> Result<Option<Vec<u8>>, DurableError> {
        self.inner.package_surface(digest).await
    }

    async fn package_paths(
        &self,
        functions: &[crate::packages::FunctionId],
    ) -> Result<
        std::collections::BTreeMap<crate::packages::FunctionId, std::collections::BTreeSet<String>>,
        DurableError,
    > {
        self.inner.package_paths(functions).await
    }

    async fn internal_operations(
        &self,
        operations: &[crate::packages::EffectName],
    ) -> Result<
        std::collections::BTreeMap<crate::packages::EffectName, std::collections::BTreeSet<String>>,
        DurableError,
    > {
        self.inner.internal_operations(operations).await
    }

    async fn package_labels(
        &self,
        artifacts: &[crate::ArtifactDigest],
    ) -> Result<std::collections::BTreeMap<crate::ArtifactDigest, Vec<String>>, DurableError> {
        self.inner.package_labels(artifacts).await
    }

    async fn owned(&self, node: &NodeLease) -> Result<Vec<Claimed>, DurableError> {
        self.inner.owned(node).await
    }

    async fn begin(&self, actor: &ActorKey, epoch: Epoch) -> Result<ActorTx, DurableError> {
        self.inner.begin(actor, epoch).await
    }

    async fn open_process(
        &self,
        actor: &ActorKey,
        epoch: Epoch,
    ) -> Result<(ActorTx, crate::domain::ProcessView), DurableError> {
        self.inner.open_process(actor, epoch).await
    }

    async fn commit(
        &self,
        tx: ActorTx,
        label: CommitLabel,
    ) -> Result<crate::ActorCommit, DurableError> {
        self.inner.commit(tx, label).await
    }

    async fn commit_mail(
        &self,
        tx: MailTx,
        label: CommitLabel,
    ) -> Result<MailCommit, DurableError> {
        self.inner.commit_mail(tx, label).await
    }

    async fn claim_facts(
        &self,
        node: Option<&crate::Owner>,
        actor: &ActorKey,
    ) -> Result<crate::ClaimFacts, DurableError> {
        self.inner.claim_facts(node, actor).await
    }

    async fn actor(&self, actor: &ActorKey) -> Result<Option<ActorSnapshot>, DurableError> {
        self.inner.actor(actor).await
    }

    async fn actors_in(
        &self,
        formats: &FormatSet,
        after: Option<&ActorKey>,
        limit: usize,
    ) -> Result<Vec<ActorKey>, DurableError> {
        self.inner.actors_in(formats, after, limit).await
    }
}

#[async_trait::async_trait]
impl DurableReads for Retiring {
    async fn model_call_pins(
        &self,
        session: &lash_protocol_types::SessionId,
        owner: &str,
    ) -> Result<Vec<crate::domain::ModelCallPinRow>, DurableError> {
        self.inner.model_call_pins(session, owner).await
    }

    async fn model_calls(
        &self,
        session: &lash_protocol_types::SessionId,
        owner: &str,
    ) -> Result<Vec<lash_core_store::model_calls::ModelCallRecord>, DurableError> {
        self.inner.model_calls(session, owner).await
    }

    async fn turn(&self, session: &SessionId) -> Result<Option<domain::TurnRow>, DurableError> {
        self.inner.turn(session).await
    }

    async fn turn_namespaces(
        &self,
        session: &SessionId,
        run: &TurnId,
    ) -> Result<Vec<domain::TurnNamespace>, DurableError> {
        self.inner.turn_namespaces(session, run).await
    }

    async fn turn_end(
        &self,
        session: &SessionId,
        run: &TurnId,
    ) -> Result<Option<domain::TurnEnd>, DurableError> {
        self.inner.turn_end(session, run).await
    }

    async fn run_facts(&self, owner: &domain::OwnerKey) -> Result<domain::RunFacts, DurableError> {
        self.inner.run_facts(owner).await
    }

    async fn run_facts_of(
        &self,
        owner: &domain::OwnerKey,
        run: domain::RunSeq,
    ) -> Result<domain::RunFacts, DurableError> {
        self.inner.run_facts_of(owner, run).await
    }
    async fn run_facts_within(
        &self,
        owner: &crate::domain::OwnerKey,
        run: Option<crate::domain::RunSeq>,
        max_rows: usize,
        limits: crate::domain::checkpoint_graph::DecodeLimits,
    ) -> Result<crate::domain::RunFacts, DurableError> {
        self.inner
            .run_facts_within(owner, run, max_rows, limits)
            .await
    }

    async fn exec_root(
        &self,
        exec: &domain::ExecKey,
    ) -> Result<Option<domain::ExecRootRow>, DurableError> {
        self.inner.exec_root(exec).await
    }

    async fn cell_roots(
        &self,
        session: &SessionId,
        run: &TurnId,
    ) -> Result<Vec<domain::ExecRootRow>, DurableError> {
        self.inner.cell_roots(session, run).await
    }
    async fn cell_roots_within(
        &self,
        session: &lash_protocol_types::SessionId,
        run: &lash_protocol_types::TurnId,
        max_rows: usize,
    ) -> Result<Vec<crate::domain::ExecRootRow>, DurableError> {
        self.inner.cell_roots_within(session, run, max_rows).await
    }

    async fn hydrate(
        &self,
        owner: domain::RootOwner,
        expected: Option<domain::checkpoint_graph::NodeId>,
        limits: domain::checkpoint_graph::DecodeLimits,
    ) -> Result<domain::HydratedGraph, DurableError> {
        self.inner.hydrate(owner, expected, limits).await
    }

    async fn pending_waits(&self, owner: &ActorKey) -> Result<Vec<domain::WaitRow>, DurableError> {
        self.inner.pending_waits(owner).await
    }

    async fn wait(&self, id: &domain::WaitId) -> Result<Option<domain::WaitRow>, DurableError> {
        self.inner.wait(id).await
    }

    async fn process(
        &self,
        process: &ProcessId,
    ) -> Result<Option<domain::ProcessActorRow>, DurableError> {
        self.inner.process(process).await
    }

    async fn live_until_descendants(
        &self,
        scope: &domain::ScopeKey,
        limit: usize,
    ) -> Result<Vec<ProcessId>, DurableError> {
        self.inner.live_until_descendants(scope, limit).await
    }

    async fn until_children(
        &self,
        scope: &domain::ScopeKey,
        after: Option<&ProcessId>,
        limit: usize,
    ) -> Result<Vec<ProcessId>, DurableError> {
        self.inner.until_children(scope, after, limit).await
    }

    async fn session_close(
        &self,
        session: &SessionId,
    ) -> Result<Option<domain::SessionCloseRow>, DurableError> {
        self.inner.session_close(session).await
    }

    async fn ending_scopes(
        &self,
        session: &SessionId,
    ) -> Result<Vec<domain::ScopeKey>, DurableError> {
        self.inner.ending_scopes(session).await
    }

    async fn session_mailbox(
        &self,
        session: &SessionId,
    ) -> Result<domain::SessionMailbox, DurableError> {
        self.inner.session_mailbox(session).await
    }

    async fn prompt_snapshot(
        &self,
        call: &domain::PromptCallKey,
    ) -> Result<Option<domain::PromptSnapshotRow>, DurableError> {
        self.inner.prompt_snapshot(call).await
    }

    async fn prompt_texts(
        &self,
        hashes: &[String],
    ) -> Result<Vec<domain::PromptText>, DurableError> {
        self.inner.prompt_texts(hashes).await
    }
}
