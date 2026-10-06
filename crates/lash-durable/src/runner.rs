//! A node's runner: the one loop that keeps a node's lease, reaps dead
//! nodes, claims actors and runs each claimed actor's activation.
//!
//! The runner owns every activation it starts. When the node stops serving,
//! for any reason, the runner returns and every activation is dropped with
//! it: a node that lost its lease keeps nothing, and a reaped node's actors
//! are already fenced by their bumped epochs.
//!
//! All of its time is the injected [`Clock`]'s, so a simulated deployment
//! runs the same loop on virtual time.
//!
//! With [`Signals`] (PostgreSQL), the runner also listens: it opens its
//! listener before its first claim, so no hint sent after that scan is
//! missed; it rescans after the listener resubscribes; it publishes what
//! its node's commits woke, coalesced, after those commits; and it watches
//! the other boots' liveness locks, reaping a boot whose lock it saw held
//! and then free. None of it is needed for correctness: the claim poll, each
//! owner's mail poll and the lease reap find every piece of work.

use crate::config::LeaseConfig;
use crate::durable_config::DurableConfig;
use crate::error::DurableError;
use crate::ids::{ActorKey, CommitLabel, Epoch, FormatSet, NodeId};
use crate::port::{
    ActorCommit, ActorState, ClaimCause, Claimed, DurableStore, HeartbeatOutcome, MailCommit,
    NodeLease, NodeSpec, Owner, Woken,
};
use crate::signals::{Signal, SignalFeed, Signals, WakeBatch};
use crate::tx::ActorTx;
use lash_core_ids::clock::Clock;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use tokio::sync::Notify;
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
}

/// What a node does with one actor it claimed.
#[async_trait::async_trait]
pub trait Activation: Send + Sync + 'static {
    /// Run `owned` until the activation releases the actor, ends it, or
    /// loses it. A commit refused with [`DurableError::OwnershipLost`] means
    /// the actor is someone else's: return at once, keeping nothing.
    ///
    /// The runner drops this future, mid-await, when the node stops serving.
    async fn activate(&self, owned: Owned);
}

/// One claimed actor, as its activation holds it.
pub struct Owned {
    store: Arc<dyn DurableStore>,
    clock: Arc<dyn Clock>,
    claimed: Claimed,
    hint: Arc<Notify>,
    poll: std::time::Duration,
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
    /// to wait on while it runs: a wake hint, or one claim-poll interval.
    #[must_use]
    pub fn mail_waker(&self) -> MailWaker {
        MailWaker {
            hint: Arc::clone(&self.hint),
            clock: Arc::clone(&self.clock),
            poll: self.poll,
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
/// another node); a hint only shortens the wait.
#[derive(Clone)]
pub struct MailWaker {
    hint: Arc<Notify>,
    clock: Arc<dyn Clock>,
    poll: std::time::Duration,
}

impl MailWaker {
    /// Wait for a hint or one poll interval.
    pub async fn wait(&self) {
        tokio::select! {
            () = self.hint.notified() => {}
            () = self.clock.sleep(self.poll) => {}
        }
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
        Self {
            store,
            clock,
            config,
            activation,
            hints: Hints::default(),
            signals: None,
        }
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
    /// The store's refusal of the node's registration.
    pub async fn run(self, stop: impl Future<Output = ()> + Send) -> Result<Stopped, DurableError> {
        let settings = self.config.lease.settings();
        let lease = self
            .store
            .register_node(&NodeSpec {
                node: self.config.node.clone(),
                decodes: self.config.decodes.clone(),
                ttl_millis: self.config.lease.ttl_millis(),
            })
            .await?;
        *self
            .hints
            .inner
            .boot
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(lease.owner.clone());
        // The listener is in place before the first claim scans.
        let mut feed = match &self.signals {
            Some(signals) => match signals.listen(&lease).await {
                Ok(feed) => Some(feed),
                Err(error) => {
                    let _ = self.store.release_node(&lease).await;
                    return Err(error);
                }
            },
            None => None,
        };
        let mut publisher: JoinSet<()> = JoinSet::new();
        if let Some(signals) = &self.signals {
            publisher.spawn(publish(self.hints.clone(), Arc::clone(signals)));
        }
        let mut active: JoinSet<()> = JoinSet::new();
        let start = self.clock.now();
        let mut last_renewed = start;
        let mut next_heartbeat = start + settings.heartbeat_every;
        let mut next_reap = start + settings.reap_every;
        let mut next_claim = start;
        let mut claim_delay = settings.claim_backoff;
        let mut next_watch = start + settings.claim_poll;
        // Each other boot whose liveness lock a probe saw held, with the
        // listener session it was seen under.
        let mut seen_held: HashMap<Owner, u64> = HashMap::new();
        let mut adopt = false;
        tokio::pin!(stop);
        let stopped = 'serve: loop {
            let mut next = next_heartbeat.min(next_reap).min(next_claim);
            if feed.is_some() {
                next = next.min(next_watch);
            }
            tokio::select! {
                biased;
                () = &mut stop => {
                    active.abort_all();
                    while active.join_next().await.is_some() {}
                    self.store.release_node(&lease).await?;
                    break Stopped::Requested;
                }
                Some(_) = active.join_next(), if !active.is_empty() => continue,
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
            if now >= next_heartbeat {
                next_heartbeat = now + settings.heartbeat_every;
                match self.store.heartbeat(&lease).await {
                    Ok(HeartbeatOutcome::Renewed { .. }) => last_renewed = now,
                    Ok(HeartbeatOutcome::Reaped) | Err(DurableError::NodeLeaseLost { .. }) => {
                        break Stopped::LeaseLost;
                    }
                    Err(_) => {}
                }
            }
            if self.clock.now().saturating_duration_since(last_renewed) >= settings.self_stop_after
            {
                break Stopped::Unrenewed;
            }
            if now >= next_reap {
                next_reap = now + settings.reap_every;
                if let Err(DurableError::NodeLeaseLost { .. }) = self.store.reap(&lease).await {
                    break Stopped::LeaseLost;
                }
            }
            if let (Some(signals), Some(session)) =
                (&self.signals, feed.as_ref().map(|feed| feed.session()))
                && now >= next_watch
            {
                next_watch = now + settings.claim_poll;
                let probed = signals.liveness().await;
                let released = match probed {
                    // An observation that spans a lost listener session is
                    // stale: the outage may have dropped every boot's lock.
                    Ok(boots) if feed.as_ref().map(|feed| feed.session()) == Some(session) => {
                        released_boots(&lease, boots, session, &mut seen_held)
                    }
                    _ => Vec::new(),
                };
                for boot in released {
                    match signals.reap_released(&lease, &boot).await {
                        Ok(reaped) if !reaped.is_empty() => next_claim = now,
                        Err(DurableError::NodeLeaseLost { .. }) => {
                            break 'serve Stopped::LeaseLost;
                        }
                        Ok(_) | Err(_) => {}
                    }
                }
            }
            if now >= next_claim {
                next_claim = now + settings.claim_poll;
                let claimed = if adopt {
                    self.store.owned(&lease).await.map(|owned| {
                        owned
                            .into_iter()
                            .filter(|claimed| !self.hints.running(&claimed.actor))
                            .collect()
                    })
                } else {
                    let room = self
                        .config
                        .max_active
                        .saturating_sub(active.len())
                        .min(self.config.claim_batch);
                    if room == 0 {
                        continue;
                    }
                    self.store.claim(&lease, room).await
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
                            self.activate(&mut active, claimed);
                        }
                    }
                    Err(DurableError::NodeLeaseLost { .. }) => break Stopped::LeaseLost,
                    // A claim whose reply is lost may have taken actors:
                    // adopt whatever this boot owns before claiming more.
                    Err(_) => adopt = true,
                }
            }
        };
        active.abort_all();
        while active.join_next().await.is_some() {}
        publisher.abort_all();
        drop(feed);
        Ok(stopped)
    }

    fn activate(&self, active: &mut JoinSet<()>, claimed: Claimed) {
        let actor = claimed.actor.clone();
        let owned = Owned {
            store: Arc::clone(&self.store),
            clock: Arc::clone(&self.clock),
            hint: self.hints.start(actor.clone()),
            claimed,
            poll: self.config.lease.settings().claim_poll,
        };
        let activation = Arc::clone(&self.activation);
        let running = Running {
            hints: self.hints.clone(),
            actor,
        };
        active.spawn(async move {
            let _running = running;
            activation.activate(owned).await;
        });
    }
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
