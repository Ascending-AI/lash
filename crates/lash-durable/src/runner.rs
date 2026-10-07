//! A node's runner: it keeps a node's lease, reaps dead nodes, claims actors
//! and runs each claimed actor's activation.
//!
//! The lease renews on a task of its own, one heartbeat at a time, from the
//! moment the node registers: no claim, reap, hand-back or liveness probe
//! the serving loop waits on can delay it, so a saturated pool or a slow
//! claim never makes a healthy node stop itself. Each renewal moves the
//! self-stop deadline to `self_stop_after` past the moment its heartbeat was
//! sent, never past the moment its answer came, so a late answer never
//! extends serving beyond the lease it renewed. When the renewal ends (the
//! lease is lost, or no heartbeat succeeded by the deadline) the serving
//! loop stops at once.
//!
//! The runner owns every activation it starts. When the node stops serving,
//! for any reason, the runner returns and every activation is dropped with
//! it: a node that lost its lease keeps nothing, and a reaped node's actors
//! are already fenced by their bumped epochs.
//!
//! No store call holds the runner: each one the serving loop makes races the
//! host's stop and the renewal's end, and the renewal's own heartbeat races
//! the self-stop deadline, so a node whose heartbeat hangs still stops
//! itself, and drops its activations, before anyone may reap it.
//! Registration and the listener's start share the `startup` budget;
//! the final release shares the `shutdown` budget, and a release that does
//! not answer in time is left to the lease's expiry.
//!
//! All of its time is the injected [`Clock`]'s, so a simulated deployment
//! runs the same loop on virtual time.
//!
//! A runner drains by release (ADR 0106 §1): once its [`Drain`] starts, it
//! marks its node draining and claims nothing more; each activation sees
//! [`Owned::draining`], finishes to its next committed phase and releases
//! its actor `ready` under `drain.release`; when none is left the runner
//! releases its node and returns [`Stopped::Drained`]. Nodes of the next
//! build then claim the released actors they decode.
//!
//! An activation answers how it ended ([`Exit`]). One that stopped while
//! this boot may still own its actor (a read it gave up on, a panic) is
//! [`Exit::Abandoned`]: the runner releases the actor `ready` in one owner
//! transaction fenced by the epoch it was claimed under, so a claim runs it
//! again and no actor is left owned with no activation.
//!
//! With [`Signals`] (PostgreSQL), the runner also listens: it opens its
//! listener before its first claim, so no hint sent after that scan is
//! missed; it rescans after the listener resubscribes; it publishes what
//! its node's commits woke, coalesced, after those commits; and it watches
//! the other boots' liveness locks, reaping a boot whose lock it saw held
//! and then free. None of it is needed for correctness: the claim poll, each
//! owner's mail poll and the lease reap find every piece of work.

use crate::config::{LeaseConfig, LeaseSettings};
use crate::durable_config::DurableConfig;
use crate::error::{DurableError, StoreFailure, StoreFailureKind};
use crate::formats::FormatSet;
use crate::ids::{ActorKey, CommitLabel, Epoch, NodeId};
use crate::port::{
    ActorCommit, ActorState, ClaimCause, Claimed, DurableStore, HeartbeatOutcome, MailCommit,
    NodeLease, NodeSpec, Owner, Woken,
};
use crate::signals::{Signal, SignalFeed, Signals, WakeBatch};
use crate::tx::{ActorTx, Release};
use lash_core_ids::clock::Clock;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;
use tokio::sync::{Notify, watch};
use tokio::task::JoinSet;

/// What a runner runs as.
#[derive(Clone, Debug)]
pub struct RunnerConfig {
    /// The node's stable name.
    pub node: NodeId,
    /// The format sets this build decodes.
    pub decodes: Vec<FormatSet>,
    /// The node-lease timings.
    pub lease: LeaseConfig,
    /// How many actors the node runs at once; claims never take more.
    pub max_active: usize,
    /// The most actors one claim takes.
    pub claim_batch: usize,
}

impl RunnerConfig {
    /// Run as `node`, decoding `decodes`, with `config`'s validated lease,
    /// capacity and claim batch.
    #[must_use]
    pub fn new(node: NodeId, decodes: Vec<FormatSet>, config: &DurableConfig) -> Self {
        let settings = config.settings();
        Self {
            node,
            decodes,
            lease: config.lease(),
            max_active: settings.max_active,
            claim_batch: settings.claim_batch,
        }
    }
}

/// Why a runner stopped serving.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stopped {
    /// The host asked it to stop; its actors were released.
    Requested,
    /// Its lease is gone: another node reaped it, or a newer boot of the
    /// same node replaced it.
    LeaseLost,
    /// No heartbeat succeeded for `self_stop_after`, so it stopped before
    /// anyone may reap it.
    Unrenewed,
    /// It drained: every activation released its actor at a committed
    /// phase, and the node released its lease.
    Drained,
}

#[derive(Default)]
struct DrainInner {
    started: AtomicBool,
    notify: Notify,
    /// The actors released under `drain.release`, in commit order.
    released: Mutex<Vec<ActorKey>>,
}

/// A node's drain switch, shared by its runner and every activation it
/// runs. Starting it is one-way: a draining node never claims again.
#[derive(Clone, Default)]
pub struct Drain {
    inner: Arc<DrainInner>,
}

impl Drain {
    /// Start draining.
    pub fn start(&self) {
        self.inner.started.store(true, Ordering::Release);
        self.inner.notify.notify_waiters();
    }

    /// Release `tx`'s actor `ready` under `drain.release`, for a node of
    /// the next build to claim, and record it among the actors this drain
    /// released.
    ///
    /// # Errors
    ///
    /// The store's refusal; nothing is recorded.
    pub async fn release(
        &self,
        store: &dyn DurableStore,
        mut tx: ActorTx,
    ) -> Result<ActorCommit, DurableError> {
        let actor = tx.actor().clone();
        tx.give_up(Release::Ready);
        let commit = store.commit(tx, CommitLabel::DRAIN_RELEASE).await?;
        self.inner
            .released
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(actor);
        Ok(commit)
    }

    /// The actors this drain released, in commit order: each one whose
    /// `drain.release` commit was acknowledged.
    #[must_use]
    pub fn released(&self) -> Vec<ActorKey> {
        self.inner
            .released
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Whether the drain started.
    #[must_use]
    pub fn started(&self) -> bool {
        self.inner.started.load(Ordering::Acquire)
    }

    /// Wait until the drain starts.
    pub async fn wait(&self) {
        loop {
            let notified = self.inner.notify.notified();
            if self.started() {
                return;
            }
            notified.await;
        }
    }
}

impl std::fmt::Debug for Drain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Drain")
            .field("started", &self.started())
            .finish()
    }
}

/// A node's lease as its own clock sees it: held until its self-stop
/// deadline, `self_stop_after` past its last renewal. The runner moves the
/// deadline at each renewal and stops at it. Between the deadline and the
/// runner's next tick the node may already be reaped and its actors owned
/// elsewhere while a reply it waited on still reaches its caller, so an
/// owner reads this before it hands an admitted execution to its body
/// (ADR 0132 §3).
#[derive(Clone)]
pub struct Liveness {
    deadline: Arc<Mutex<Instant>>,
    clock: Arc<dyn Clock>,
}

impl Liveness {
    fn new(clock: Arc<dyn Clock>, deadline: Instant) -> Self {
        Self {
            deadline: Arc::new(Mutex::new(deadline)),
            clock,
        }
    }

    fn renew(&self, deadline: Instant) {
        *self.deadline.lock().unwrap_or_else(PoisonError::into_inner) = deadline;
    }

    /// The lease is gone: the node holds it no longer, whatever its clock.
    fn lose(&self) {
        self.renew(self.clock.now());
    }

    /// Whether the node still holds its lease: its self-stop deadline is
    /// ahead of its clock.
    #[must_use]
    pub fn held(&self) -> bool {
        self.clock.now() < *self.deadline.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl std::fmt::Debug for Liveness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Liveness")
            .field("held", &self.held())
            .finish()
    }
}

/// How an activation ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    /// The actor is not this node's any more: the activation released or
    /// ended it, or lost it to another owner.
    Released,
    /// The activation stopped while this boot may still own the actor: the
    /// runner releases it `ready`, fenced by the epoch it was claimed under,
    /// for a claim to run it again.
    Abandoned,
}

/// What a node does with one actor it claimed.
#[async_trait::async_trait]
pub trait Activation: Send + Sync + 'static {
    /// Run `owned` until the activation releases the actor, ends it, or
    /// loses it, and answer [`Exit::Released`]. A commit refused with
    /// [`DurableError::OwnershipLost`] means the actor is someone else's:
    /// return at once, keeping nothing. An activation that stops before
    /// then answers [`Exit::Abandoned`], and the runner hands the actor
    /// back.
    ///
    /// The runner drops this future, mid-await, when the node stops serving.
    async fn activate(&self, owned: Owned) -> Exit;
}

/// One claimed actor, as its activation holds it.
pub struct Owned {
    store: Arc<dyn DurableStore>,
    clock: Arc<dyn Clock>,
    claimed: Claimed,
    hint: Arc<Notify>,
    poll: std::time::Duration,
    drain: Drain,
    liveness: Liveness,
}

impl Owned {
    /// The actor.
    #[must_use]
    pub fn actor(&self) -> &ActorKey {
        &self.claimed.actor
    }

    /// The epoch the node owns it under.
    #[must_use]
    pub fn epoch(&self) -> Epoch {
        self.claimed.epoch
    }

    /// Why it was claimable.
    #[must_use]
    pub fn cause(&self) -> ClaimCause {
        self.claimed.cause
    }

    /// What the node claimed the actor for: a
    /// [`ClaimPurpose::CancelOnly`](crate::ClaimPurpose::CancelOnly) claim
    /// must not decode the actor's state.
    #[must_use]
    pub fn purpose(&self) -> crate::ClaimPurpose {
        self.claimed.purpose
    }

    /// Whether the node is draining: the activation releases the actor
    /// `ready` at its next committed phase, under `drain.release`, instead
    /// of starting more work.
    #[must_use]
    pub fn draining(&self) -> bool {
        self.drain.started()
    }

    /// The node's drain switch.
    #[must_use]
    pub fn drain(&self) -> &Drain {
        &self.drain
    }

    /// Release the actor `ready` under `drain.release` with `tx`'s writes:
    /// what a draining activation does at a committed phase
    /// ([`Drain::release`]).
    ///
    /// # Errors
    ///
    /// The store's refusal; [`DurableError::OwnershipLost`] once the actor
    /// is someone else's.
    pub async fn drain_release(&self, tx: ActorTx) -> Result<ActorCommit, DurableError> {
        self.drain.release(self.store.as_ref(), tx).await
    }

    /// The node's lease as its own clock sees it.
    #[must_use]
    pub fn liveness(&self) -> &Liveness {
        &self.liveness
    }

    /// Open an owner transaction over the actor at the owned epoch.
    ///
    /// # Errors
    ///
    /// [`DurableError::OwnershipLost`] once the actor is someone else's.
    pub async fn begin(&self) -> Result<ActorTx, DurableError> {
        self.store
            .begin(&self.claimed.actor, self.claimed.epoch)
            .await
    }

    /// Commit `tx` under `label`.
    ///
    /// # Errors
    ///
    /// The store's refusal; [`DurableError::OwnershipLost`] once the actor
    /// is someone else's.
    pub async fn commit(
        &self,
        tx: ActorTx,
        label: CommitLabel,
    ) -> Result<ActorCommit, DurableError> {
        self.store.commit(tx, label).await
    }

    /// Wait until mail may have arrived for the actor: a wake hint, or one
    /// claim-poll interval, whichever comes first. A hint is only a hint;
    /// the next [`Owned::begin`] reads what actually arrived.
    pub async fn wait_for_mail(&self) {
        self.mail_waker().wait().await;
    }

    /// What [`Owned::wait_for_mail`] waits on, for the activation's context
    /// to wait on while it runs: a wake hint, the drain's start, or one
    /// claim-poll interval.
    #[must_use]
    pub fn mail_waker(&self) -> MailWaker {
        MailWaker {
            hint: Arc::clone(&self.hint),
            clock: Arc::clone(&self.clock),
            poll: self.poll,
            drain: self.drain.clone(),
        }
    }

    /// The node's clock.
    #[must_use]
    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    /// The node's store: every owner write and read the activation makes
    /// goes through it.
    #[must_use]
    pub fn store(&self) -> &Arc<dyn DurableStore> {
        &self.store
    }
}

/// Waits until mail may have arrived for one owned actor: a wake hint, or
/// one claim-poll interval, whichever comes first. The per-actor poll is
/// the correctness backstop for a hint that never came (a wake committed on
/// another node); a hint only shortens the wait. The node's drain ends the
/// wait too, so an idle owner releases at once.
#[derive(Clone)]
pub struct MailWaker {
    hint: Arc<Notify>,
    clock: Arc<dyn Clock>,
    poll: std::time::Duration,
    drain: Drain,
}

impl MailWaker {
    /// Wait for a hint, the drain's start or one poll interval.
    pub async fn wait(&self) {
        if self.drain.started() {
            return;
        }
        tokio::select! {
            () = self.hint.notified() => {}
            () = self.drain.wait() => {}
            () = self.clock.sleep(self.poll) => {}
        }
    }

    /// Whether the node is draining.
    #[must_use]
    pub fn draining(&self) -> bool {
        self.drain.started()
    }
}

impl std::fmt::Debug for MailWaker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MailWaker")
            .field("poll", &self.poll)
            .finish_non_exhaustive()
    }
}

#[derive(Default)]
struct HintsInner {
    running: Mutex<HashMap<ActorKey, Arc<Notify>>>,
    claim: Notify,
    /// This node's boot, once registered: the owner a local hint serves.
    boot: Mutex<Option<Owner>>,
    /// Whether woken actors owned elsewhere, or readied, are published.
    publishing: AtomicBool,
    /// What the next flush publishes.
    pending: Mutex<WakeBatch>,
    /// Rings the publisher.
    flush: Notify,
}

/// A wake's delivery: a mailbox writer on this node hands the runner what
/// its commit woke, after the commit. An actor this boot runs is hinted in
/// process and nothing is published; a readied unowned actor rings this
/// node's claim loop and, with [`Signals`], every other node's; an actor
/// another node owns is published to that node alone. Publishes coalesce
/// until the publisher's next flush.
#[derive(Clone, Default)]
pub struct Hints {
    inner: Arc<HintsInner>,
}

impl Hints {
    /// Deliver everything `commit` woke.
    pub fn woke(&self, commit: &MailCommit) {
        for woken in &commit.woken {
            self.wake(woken);
        }
    }

    /// Deliver one woken actor.
    pub fn wake(&self, woken: &Woken) {
        match &woken.owner {
            Some(owner) if self.is_mine(owner) => {
                if !self.hint_running(&woken.actor) {
                    self.inner.claim.notify_one();
                }
            }
            Some(owner) => self.queue(|batch| {
                batch
                    .owned
                    .entry(owner.node.clone())
                    .or_default()
                    .insert(woken.actor.clone());
            }),
            None if woken.state == ActorState::Ready => {
                self.inner.claim.notify_one();
                self.queue(|batch| batch.ready = true);
            }
            None => {}
        }
    }

    fn is_mine(&self, owner: &Owner) -> bool {
        self.inner
            .boot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            == Some(owner)
    }

    fn hint_running(&self, actor: &ActorKey) -> bool {
        let running = self
            .inner
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(actor)
            .cloned();
        running.is_some_and(|hint| {
            hint.notify_one();
            true
        })
    }

    fn hint_all_running(&self) {
        for hint in self
            .inner
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
        {
            hint.notify_one();
        }
    }

    fn queue(&self, add: impl FnOnce(&mut WakeBatch)) {
        if !self.inner.publishing.load(Ordering::Acquire) {
            return;
        }
        add(&mut self
            .inner
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner));
        self.inner.flush.notify_one();
    }

    fn take_pending(&self) -> WakeBatch {
        std::mem::take(
            &mut *self
                .inner
                .pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        )
    }

    fn start(&self, actor: ActorKey) -> Arc<Notify> {
        let hint = Arc::new(Notify::new());
        self.inner
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(actor, Arc::clone(&hint));
        hint
    }

    fn end(&self, actor: &ActorKey) {
        self.inner
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(actor);
    }

    fn running(&self, actor: &ActorKey) -> bool {
        self.inner
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains_key(actor)
    }
}

/// One node's serving loop.
pub struct Runner {
    store: Arc<dyn DurableStore>,
    clock: Arc<dyn Clock>,
    config: RunnerConfig,
    activation: Arc<dyn Activation>,
    hints: Hints,
    signals: Option<Arc<dyn Signals>>,
    drain: Drain,
    liveness: Liveness,
}

/// Publish the node's coalesced wakes, one flush at a time: whatever is
/// woken while a publish is in flight rides the next one.
async fn publish(hints: Hints, signals: Arc<dyn Signals>) {
    loop {
        hints.inner.flush.notified().await;
        let batch = hints.take_pending();
        if !batch.is_empty() {
            // A hint that fails to send costs latency only: the polls find
            // the work.
            let _ = signals.publish(&batch).await;
        }
    }
}

/// The feed's next signal, or never without one.
async fn next_signal(feed: &mut Option<Box<dyn SignalFeed>>) -> Signal {
    match feed {
        Some(feed) => feed.next().await,
        None => std::future::pending().await,
    }
}

impl Runner {
    #[must_use]
    pub fn new(
        store: Arc<dyn DurableStore>,
        clock: Arc<dyn Clock>,
        config: RunnerConfig,
        activation: Arc<dyn Activation>,
    ) -> Self {
        let liveness = Liveness::new(Arc::clone(&clock), clock.now());
        Self {
            store,
            clock,
            config,
            activation,
            hints: Hints::default(),
            signals: None,
            drain: Drain::default(),
            liveness,
        }
    }

    /// This runner draining by release once `drain` starts.
    #[must_use]
    pub fn with_drain(mut self, drain: Drain) -> Self {
        self.drain = drain;
        self
    }

    /// This runner taking its wake hints from `hints`, which this node's
    /// mailbox writers already hold.
    #[must_use]
    pub fn with_hints(mut self, hints: Hints) -> Self {
        hints
            .inner
            .publishing
            .store(self.signals.is_some(), Ordering::Release);
        self.hints = hints;
        self
    }

    /// Listen, publish and watch liveness through `signals`.
    #[must_use]
    pub fn with_signals(mut self, signals: Arc<dyn Signals>) -> Self {
        self.hints.inner.publishing.store(true, Ordering::Release);
        self.signals = Some(signals);
        self
    }

    /// The runner's hint handle, for this node's mailbox writers.
    #[must_use]
    pub fn hints(&self) -> Hints {
        self.hints.clone()
    }

    /// Serve until `stop` completes or the node loses its lease.
    ///
    /// # Errors
    ///
    /// The store's refusal of the node's registration or listener, either
    /// one not in place within `startup` of the registration attempt, or
    /// the store's refusal of the final release.
    pub async fn run(self, stop: impl Future<Output = ()> + Send) -> Result<Stopped, DurableError> {
        let settings = self.config.lease.settings();
        // The lease's clock starts when the registration is sent: the
        // stored lease cannot start earlier, so the local deadline is never
        // later than the stored one.
        let registering = self.clock.now();
        let started_by = registering + settings.startup;
        let spec = NodeSpec {
            node: self.config.node.clone(),
            decodes: self.config.decodes.clone(),
            ttl_millis: self.config.lease.ttl_millis(),
        };
        let lease = self
            .within(started_by, self.store.register_node(&spec))
            .await
            .unwrap_or_else(|| Err(not_started("registration", settings)))?;
        *self
            .hints
            .inner
            .boot
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(lease.owner.clone());
        self.liveness.renew(registering + settings.self_stop_after);
        // Renewal runs before the listener opens, so a slow listener never
        // ages the lease. Dropping the set on any return cancels it.
        let (ended_tx, ended) = watch::channel(None);
        let mut renewal: JoinSet<()> = JoinSet::new();
        renewal.spawn(renew(
            Arc::clone(&self.store),
            Arc::clone(&self.clock),
            lease.clone(),
            settings,
            self.liveness.clone(),
            registering,
            ended_tx,
        ));
        // The listener is in place before the first claim scans.
        let mut feed = match &self.signals {
            Some(signals) => match self.within(started_by, signals.listen(&lease)).await {
                Some(Ok(feed)) => Some(feed),
                failed => {
                    renewal.abort_all();
                    while renewal.join_next().await.is_some() {}
                    let _ = self.release(&lease, settings).await;
                    return Err(match failed {
                        Some(Err(error)) => error,
                        _ => not_started("listener", settings),
                    });
                }
            },
            None => None,
        };
        let mut publisher: JoinSet<()> = JoinSet::new();
        if let Some(signals) = &self.signals {
            publisher.spawn(publish(self.hints.clone(), Arc::clone(signals)));
        }
        let mut active: JoinSet<Exit> = JoinSet::new();
        // The actor and epoch each running activation was claimed with, and
        // the claims of activations that ended abandoning their actors,
        // until the runner has handed them back.
        let mut claims: HashMap<tokio::task::Id, (ActorKey, Epoch)> = HashMap::new();
        let mut abandoned: Vec<(ActorKey, Epoch)> = Vec::new();
        let start = self.clock.now();
        let mut next_reap = start + settings.reap_every;
        let mut next_claim = start;
        let mut claim_delay = settings.claim_backoff;
        let mut next_watch = start + settings.claim_poll;
        // Each other boot whose liveness lock a probe saw held, with the
        // listener session it was seen under.
        let mut seen_held: HashMap<Owner, u64> = HashMap::new();
        let mut adopt = false;
        // Whether the store records this node draining: the drain switch
        // alone already stops this runner's claims.
        let mut marked_draining = false;
        tokio::pin!(stop);
        let stopped = 'serve: loop {
            // A hand-back that failed is retried at the next turn of the
            // loop; one the fence refused finds the actor already gone.
            for (actor, epoch) in std::mem::take(&mut abandoned) {
                match self
                    .bounded(&mut stop, &ended, self.hand_back(&actor, epoch))
                    .await
                {
                    Ok(Ok(())) => {
                        next_claim = self.clock.now();
                        claim_delay = settings.claim_backoff;
                    }
                    Ok(Err(DurableError::OwnershipLost(_))) => {}
                    Ok(Err(_)) => abandoned.push((actor, epoch)),
                    Err(stopped) => break 'serve stopped,
                }
            }
            if self.drain.started() {
                if !marked_draining {
                    let mark = self.store.mark_draining(&lease);
                    match self.bounded(&mut stop, &ended, mark).await {
                        Ok(Ok(())) => {
                            marked_draining = true;
                            // Idle owners release at once.
                            self.hints.hint_all_running();
                        }
                        Ok(Err(DurableError::NodeLeaseLost { .. })) => break Stopped::LeaseLost,
                        // Retried at the next turn of the loop.
                        Ok(Err(_)) => {}
                        Err(stopped) => break stopped,
                    }
                }
                if marked_draining && active.is_empty() && abandoned.is_empty() {
                    break Stopped::Drained;
                }
            }
            let mut next = next_reap.min(next_claim);
            if feed.is_some() {
                next = next.min(next_watch);
            }
            tokio::select! {
                biased;
                () = &mut stop => break Stopped::Requested,
                stopped = renewal_ended(&ended) => break stopped,
                Some(joined) = active.join_next_with_id(), if !active.is_empty() => {
                    // A panicked activation stopped holding its actor too.
                    let (task, exit) = match joined {
                        Ok((task, exit)) => (task, exit),
                        Err(error) => (error.id(), Exit::Abandoned),
                    };
                    if let Some(claim) = claims.remove(&task)
                        && exit == Exit::Abandoned
                    {
                        abandoned.push(claim);
                    }
                    continue;
                }
                () = self.drain.wait(), if !self.drain.started() => continue,
                signal = next_signal(&mut feed) => match signal {
                    Signal::Ready => {
                        next_claim = self.clock.now();
                        claim_delay = settings.claim_backoff;
                    }
                    Signal::Owned(actors) => {
                        for actor in &actors {
                            if !self.hints.hint_running(actor) {
                                next_claim = self.clock.now();
                            }
                        }
                    }
                    Signal::Resubscribed => {
                        seen_held.clear();
                        self.hints.hint_all_running();
                        next_claim = self.clock.now();
                        claim_delay = settings.claim_backoff;
                    }
                },
                () = self.hints.inner.claim.notified() => {
                    next_claim = self.clock.now();
                    claim_delay = settings.claim_backoff;
                }
                () = self.clock.sleep_until(next) => {}
            }
            let now = self.clock.now();
            if now >= next_reap {
                next_reap = now + settings.reap_every;
                match self
                    .bounded(&mut stop, &ended, self.store.reap(&lease))
                    .await
                {
                    Ok(Err(DurableError::NodeLeaseLost { .. })) => break Stopped::LeaseLost,
                    Ok(_) => {}
                    Err(stopped) => break stopped,
                }
            }
            if let (Some(signals), Some(session)) =
                (&self.signals, feed.as_ref().map(|feed| feed.session()))
                && now >= next_watch
            {
                next_watch = now + settings.claim_poll;
                let probed = match self.bounded(&mut stop, &ended, signals.liveness()).await {
                    Ok(probed) => probed,
                    Err(stopped) => break stopped,
                };
                let released = match probed {
                    // An observation that spans a lost listener session is
                    // stale: the outage may have dropped every boot's lock.
                    Ok(boots) if feed.as_ref().map(|feed| feed.session()) == Some(session) => {
                        released_boots(&lease, boots, session, &mut seen_held)
                    }
                    _ => Vec::new(),
                };
                for boot in released {
                    let reap = signals.reap_released(&lease, &boot);
                    match self.bounded(&mut stop, &ended, reap).await {
                        Ok(Ok(reaped)) if !reaped.is_empty() => next_claim = now,
                        Ok(Err(DurableError::NodeLeaseLost { .. })) => {
                            break 'serve Stopped::LeaseLost;
                        }
                        Ok(Ok(_) | Err(_)) => {}
                        Err(stopped) => break 'serve stopped,
                    }
                }
            }
            if now >= next_claim && !self.drain.started() {
                next_claim = now + settings.claim_poll;
                let claimed = if adopt {
                    match self
                        .bounded(&mut stop, &ended, self.store.owned(&lease))
                        .await
                    {
                        Ok(owned) => owned.map(|owned| {
                            owned
                                .into_iter()
                                .filter(|claimed| !self.hints.running(&claimed.actor))
                                .collect()
                        }),
                        Err(stopped) => break stopped,
                    }
                } else {
                    let room = self
                        .config
                        .max_active
                        .saturating_sub(active.len())
                        .min(self.config.claim_batch);
                    if room == 0 {
                        continue;
                    }
                    match self
                        .bounded(&mut stop, &ended, self.store.claim(&lease, room))
                        .await
                    {
                        Ok(claimed) => claimed,
                        Err(stopped) => break stopped,
                    }
                };
                match claimed {
                    Ok(claimed) => {
                        // Claim again soon after work, and back off while
                        // claims come back empty.
                        claim_delay = if claimed.is_empty() {
                            claim_delay.saturating_mul(2).min(settings.claim_poll)
                        } else {
                            settings.claim_backoff
                        };
                        next_claim = now + claim_delay;
                        adopt = false;
                        for claimed in claimed {
                            self.activate(&mut active, &mut claims, claimed);
                        }
                    }
                    Err(DurableError::NodeLeaseLost { .. }) => break Stopped::LeaseLost,
                    // A claim whose reply is lost may have taken actors:
                    // adopt whatever this boot owns before claiming more.
                    Err(_) => adopt = true,
                }
            }
        };
        if stopped == Stopped::LeaseLost {
            self.liveness.lose();
        }
        active.abort_all();
        while active.join_next().await.is_some() {}
        renewal.abort_all();
        while renewal.join_next().await.is_some() {}
        publisher.abort_all();
        let released = if matches!(stopped, Stopped::Requested | Stopped::Drained) {
            self.release(&lease, settings).await
        } else {
            Ok(())
        };
        // Dropping the listener ends its session, which frees the boot's
        // liveness lock; nothing waits on it.
        drop(feed);
        released.map(|()| stopped)
    }

    /// Release the node's lease within `shutdown`. A release that does not
    /// answer in time is left to the lease's expiry.
    async fn release(
        &self,
        lease: &NodeLease,
        settings: LeaseSettings,
    ) -> Result<(), DurableError> {
        let by = self.clock.now() + settings.shutdown;
        match self.within(by, self.store.release_node(lease)).await {
            Some(released) => released.map(drop),
            None => Ok(()),
        }
    }

    /// `call`'s answer, unless `deadline` comes first.
    async fn within<T>(&self, deadline: Instant, call: impl Future<Output = T>) -> Option<T> {
        tokio::select! {
            biased;
            () = self.clock.sleep_until(deadline) => None,
            answer = call => Some(answer),
        }
    }

    /// `call`'s answer, unless the host's `stop` or the end of the node's
    /// renewal comes first: then why the node stops instead. A call that
    /// hangs (a held lock, a dead connection's TCP wait, a pool's acquire
    /// timeout) never holds the node past either. An answer that comes
    /// once the node no longer holds its lease is not acted on, even before
    /// the renewal reports its end.
    async fn bounded<T>(
        &self,
        stop: &mut (impl Future<Output = ()> + Unpin),
        ended: &watch::Receiver<Option<Stopped>>,
        call: impl Future<Output = T>,
    ) -> Result<T, Stopped> {
        let answer = tokio::select! {
            biased;
            () = stop => return Err(Stopped::Requested),
            stopped = renewal_ended(ended) => return Err(stopped),
            answer = call => answer,
        };
        if self.liveness.held() {
            Ok(answer)
        } else {
            Err(ended.borrow().clone().unwrap_or(Stopped::Unrenewed))
        }
    }

    /// Release `actor` `ready` under `drain.release`, fenced by `epoch`, the
    /// epoch its abandoned activation was claimed under: an actor that
    /// activation released or ended, or another node claimed, is left as it
    /// is ([`DurableError::OwnershipLost`]). A draining node records it among
    /// the drain's releases.
    async fn hand_back(&self, actor: &ActorKey, epoch: Epoch) -> Result<(), DurableError> {
        let mut tx = self.store.begin(actor, epoch).await?;
        if self.drain.started() {
            self.drain.release(self.store.as_ref(), tx).await?;
        } else {
            tx.give_up(Release::Ready);
            self.store.commit(tx, CommitLabel::DRAIN_RELEASE).await?;
        }
        Ok(())
    }

    fn activate(
        &self,
        active: &mut JoinSet<Exit>,
        claims: &mut HashMap<tokio::task::Id, (ActorKey, Epoch)>,
        claimed: Claimed,
    ) {
        let actor = claimed.actor.clone();
        let claim = (claimed.actor.clone(), claimed.epoch);
        let owned = Owned {
            store: Arc::clone(&self.store),
            clock: Arc::clone(&self.clock),
            hint: self.hints.start(actor.clone()),
            claimed,
            poll: self.config.lease.settings().claim_poll,
            drain: self.drain.clone(),
            liveness: self.liveness.clone(),
        };
        let activation = Arc::clone(&self.activation);
        let running = Running {
            hints: self.hints.clone(),
            actor,
        };
        let task = active.spawn(async move {
            let _running = running;
            activation.activate(owned).await
        });
        claims.insert(task.id(), claim);
    }
}

/// Renew `lease` until it is lost or unrenewed, one heartbeat at a time,
/// and report why the renewal ended through `ended`. The self-stop deadline
/// starts at `registering + self_stop_after` and each renewal moves it to
/// `self_stop_after` past the moment its heartbeat was sent; a heartbeat
/// still unanswered at the deadline is dropped and the node stops.
async fn renew(
    store: Arc<dyn DurableStore>,
    clock: Arc<dyn Clock>,
    lease: NodeLease,
    settings: LeaseSettings,
    liveness: Liveness,
    registering: Instant,
    ended: watch::Sender<Option<Stopped>>,
) {
    let mut deadline = registering + settings.self_stop_after;
    let mut next = registering + settings.heartbeat_every;
    let stopped = loop {
        tokio::select! {
            biased;
            () = clock.sleep_until(deadline) => break Stopped::Unrenewed,
            () = clock.sleep_until(next) => {}
        }
        let sent = clock.now();
        next = sent + settings.heartbeat_every;
        tokio::select! {
            biased;
            () = clock.sleep_until(deadline) => break Stopped::Unrenewed,
            beat = store.heartbeat(&lease) => match beat {
                Ok(HeartbeatOutcome::Renewed { .. }) => {
                    deadline = deadline.max(sent + settings.self_stop_after);
                    liveness.renew(deadline);
                }
                Ok(HeartbeatOutcome::Reaped) | Err(DurableError::NodeLeaseLost { .. }) => {
                    break Stopped::LeaseLost;
                }
                // Retried at the next heartbeat.
                Err(_) => {}
            },
        }
    };
    if stopped == Stopped::LeaseLost {
        liveness.lose();
    }
    ended.send_replace(Some(stopped));
}

/// Why the node's renewal ended, once it has. A renewal task that ended
/// without saying (it panicked) renews no more: the node is unrenewed.
async fn renewal_ended(ended: &watch::Receiver<Option<Stopped>>) -> Stopped {
    let mut ended = ended.clone();
    match ended.wait_for(Option::is_some).await {
        Ok(stopped) => stopped.clone().unwrap_or(Stopped::Unrenewed),
        Err(_) => Stopped::Unrenewed,
    }
}

/// A node that did not start within `startup`: whether its `what` took
/// effect is unknown, and an unrenewed lease expires on its own.
fn not_started(what: &str, settings: LeaseSettings) -> DurableError {
    DurableError::Store(StoreFailure {
        kind: StoreFailureKind::Unavailable,
        message: format!(
            "the node's {what} was not in place within startup ({:?})",
            settings.startup
        ),
    })
}

/// Fold one liveness probe into `seen_held`, answering each boot this node
/// saw holding its lock under listener session `session` and now sees free:
/// a boot that died. A boot seen free without having been seen held is
/// starting up or never listens, so its lease alone decides it.
fn released_boots(
    lease: &NodeLease,
    boots: Vec<crate::signals::BootLiveness>,
    session: u64,
    seen_held: &mut HashMap<Owner, u64>,
) -> Vec<Owner> {
    let mut registered = HashSet::new();
    let mut released = Vec::new();
    for liveness in boots {
        if liveness.boot == lease.owner {
            continue;
        }
        registered.insert(liveness.boot.clone());
        if liveness.held {
            seen_held.insert(liveness.boot, session);
        } else if seen_held.remove(&liveness.boot) == Some(session) {
            released.push(liveness.boot);
        }
    }
    seen_held.retain(|boot, _| registered.contains(boot));
    released
}

/// Ends an activation's hint registration however its task ends: returned,
/// panicked or aborted.
struct Running {
    hints: Hints,
    actor: ActorKey,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.hints.end(&self.actor);
    }
}
