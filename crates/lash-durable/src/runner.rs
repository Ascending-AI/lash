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
//! Each registration attempt and the listener's start share the `startup`
//! budget. Transient registration faults retry with the claim backoff until
//! the host stops the node;
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
//! again and no actor is left owned with no activation. The hand-back
//! advances nothing: an activation that never committed counts as a failed
//! one at the next claim, as a crashed one does.
//!
//! With [`NodeWakes`] (PostgreSQL, or a SQLite database file), the runner
//! also listens: it opens its listener before its first claim, so no hint
//! sent after that scan is missed; it rescans after the listener
//! resubscribes; it publishes what
//! its node's commits woke, coalesced, after those commits; and it watches
//! the other boots' liveness locks, reaping a boot whose lock it saw held
//! and then free. A readied unowned actor wakes one node: this one when it
//! has room, or else the live peer its key hashes to among the
//! peers its last liveness probe saw, or else this one anyway, whose claim
//! loop owes the claim until room opens. None of it is needed for
//! correctness: the claim poll, each owner's mail poll and the lease reap
//! find every piece of work.
//!
//! The claim loop claims on evidence of work (FIG-7044): a readiness hint,
//! a committed due time this node's own release handed it
//! ([`Hints::owner_committed`]), or a claim that filled its batch. Without
//! one it reconciles every claim poll with one read-only
//! [`DurableStore::readiness`] read, and claims only when that read finds
//! work due, so a quiet node opens no claim write and a lost hint still
//! costs at most one claim poll. The read's earliest due time also wakes the
//! node when a timer another node committed comes due, one claim backoff
//! after it, so the releasing node, which claims at the due time itself,
//! takes it first while it lives.
//!
//! An owner with nothing to do keeps its actor hot ([`Owned::wait_idle`]),
//! but its slot is room: a claim may take actors in its place and asks it
//! to yield, and its mail poll backs off with its idle time (FIG-6890).

mod hints;
mod lease;

pub use hints::{CommitEffects, Hints, KeyWatch, LogFollowers, WakeAttachment, WakeSink};
use hints::{IdleWait, Running};
pub use lease::NodeRegistration;
use lease::Renewal;

use crate::config::{LeaseConfig, LeaseSettings};
use crate::durable_config::{DurableConfig, RetirementSettings};
use crate::error::{DurableError, StoreFailure, StoreFailureKind};
use crate::formats::FormatSet;
use crate::ids::{ActorKey, CommitLabel, DurableInstant, Epoch, NodeId};
use crate::node_wakes::{NodeWakeEvent, NodeWakeFeed, NodeWakes};
use crate::port::{
    ActorCommit, ClaimCause, Claimed, DurableStore, NodeLease, NodeSpec, Owner, Readiness,
};
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
    /// The claim requirements this build serves.
    pub serves: Vec<String>,
    /// What this build's registrations offer running code: the providers
    /// it registers and the codec versions they read. The node advertises
    /// them beside its package code's capabilities (FIG-6993).
    pub offers: crate::NodeCapabilities,
    /// The node-lease timings.
    pub lease: LeaseConfig,
    /// How many actors the node keeps at once. An owner with nothing to
    /// do is room: a claim past the free slots asks idle owners to yield,
    /// and each releases its actor at once (FIG-6890).
    pub max_active: usize,
    /// Overridable policy for resident session and process owners.
    pub owner_retention: crate::OwnerRetentionSettings,
    /// The most actors one claim takes.
    pub claim_batch: usize,
    /// The node's page retirement and its backpressure.
    pub retirement: RetirementSettings,
}

impl RunnerConfig {
    /// This config, its node serving `serves`.
    #[must_use]
    pub fn with_serves(mut self, serves: Vec<String>) -> Self {
        self.serves = serves;
        self
    }

    /// This config, its node's registrations offering `offers`.
    #[must_use]
    pub fn with_offers(mut self, offers: crate::NodeCapabilities) -> Self {
        self.offers = offers;
        self
    }

    /// Run as `node`, decoding `decodes`, with `config`'s validated lease,
    /// capacity and claim batch.
    #[must_use]
    pub fn new(node: NodeId, decodes: Vec<FormatSet>, config: &DurableConfig) -> Self {
        let settings = config.settings();
        Self {
            node,
            decodes,
            serves: Vec::new(),
            offers: crate::NodeCapabilities::default(),
            lease: config.lease(),
            max_active: settings.max_active,
            owner_retention: settings.owner_retention,
            claim_batch: settings.claim_batch,
            retirement: settings.retirement,
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
    /// The actors released under `drain.release` or with a phase commit,
    /// in commit order.
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
        tx: ActorTx,
    ) -> Result<ActorCommit, DurableError> {
        self.release_with(store, tx, CommitLabel::DRAIN_RELEASE, Release::Ready)
            .await
    }

    /// Commit `tx` under `label` with its actor given up as `release`: a
    /// draining activation's phase commit that also releases the actor, so
    /// no `drain.release` write follows it. Recorded among the actors this
    /// drain released, as [`Self::release`] is.
    ///
    /// # Errors
    ///
    /// The store's refusal; nothing is recorded.
    pub async fn release_with(
        &self,
        store: &dyn DurableStore,
        mut tx: ActorTx,
        label: CommitLabel,
        release: Release,
    ) -> Result<ActorCommit, DurableError> {
        let actor = tx.actor().clone();
        tx.give_up(release);
        let commit = store.commit(tx, label).await?;
        self.inner
            .released
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(actor);
        Ok(commit)
    }

    /// [`Self::release_with`] for a commit that publishes `candidate`
    /// through `pages` (S6): its heap pages, its owner write and `tx`'s own
    /// rows in one fenced transaction that also gives up the actor.
    ///
    /// # Errors
    ///
    /// The store's refusal; nothing is recorded.
    pub async fn release_candidate_with(
        &self,
        pages: &dyn crate::domain::pages::PageStore,
        mut tx: ActorTx,
        candidate: crate::domain::pages::CandidatePublication,
        label: CommitLabel,
        release: Release,
    ) -> Result<ActorCommit, DurableError> {
        let actor = tx.actor().clone();
        tx.give_up(release);
        let commit = pages.commit_candidate(tx, candidate, label).await?.actor;
        self.inner
            .released
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(actor);
        Ok(commit)
    }

    /// The actors this drain released, in commit order: each one whose
    /// releasing commit was acknowledged.
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

    /// Machine and registered execution capabilities this activation offers.
    /// The runner unions these with its configured registrations before it
    /// registers its node lease. Claims use that same advertised record.
    ///
    /// # Errors
    ///
    /// The activation cannot construct its capability record.
    async fn capabilities(&self) -> Result<crate::NodeCapabilities, DurableError> {
        Ok(crate::NodeCapabilities::default())
    }
}

/// One claimed actor, as its activation holds it.
pub struct Owned {
    store: Arc<dyn DurableStore>,
    clock: Arc<dyn Clock>,
    claimed: Claimed,
    capabilities: crate::NodeCapabilities,
    claimed_on: Wake,
    hint: Arc<Notify>,
    hints: Hints,
    poll: std::time::Duration,
    idle: std::time::Duration,
    drain: Drain,
    liveness: Liveness,
    acknowledged_commits: std::sync::atomic::AtomicU64,
    turn_profile: Option<lash_core_ids::operational_metrics::turn::Context>,
}

impl Owned {
    /// Measurements of the physical claim that gave this owner its actor.
    #[doc(hidden)]
    pub fn turn_profile(&self) -> Option<&lash_core_ids::operational_metrics::turn::Context> {
        self.turn_profile.as_ref()
    }

    /// Successful writes acknowledged through this grant. Idle reads do not
    /// reset an activation's immediate conflict-retry allowance.
    #[doc(hidden)]
    pub fn acknowledged_commits(&self) -> u64 {
        self.acknowledged_commits
            .load(std::sync::atomic::Ordering::Relaxed)
    }

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

    /// What woke the claim that took it: a hint, or the claim poll.
    #[must_use]
    pub fn claimed_on(&self) -> Wake {
        self.claimed_on
    }

    /// What the node claimed the actor for: a
    /// [`ClaimPurpose::CancelOnly`](crate::ClaimPurpose::CancelOnly) claim
    /// must not decode the actor's state.
    #[must_use]
    pub fn purpose(&self) -> crate::ClaimPurpose {
        self.claimed.purpose
    }

    /// The claim requirements the actor carried when it was claimed.
    #[must_use]
    pub fn requires(&self) -> &[String] {
        &self.claimed.requires
    }

    /// Capabilities of the node lease that claimed this activation.
    #[must_use]
    pub fn capabilities(&self) -> &crate::NodeCapabilities {
        &self.capabilities
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
        let effects = CommitEffects::of(&tx);
        let commit = Box::pin(self.drain.release(self.store.as_ref(), tx)).await?;
        self.hints.owner_committed(&effects, &commit);
        Ok(commit)
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
    pub fn commit(
        &self,
        tx: ActorTx,
        label: CommitLabel,
    ) -> impl Future<Output = Result<ActorCommit, DurableError>> + Send + '_ {
        // The store's commit takes `tx` now, so the answer carries only the
        // commit's boxed future and its effects.
        let effects = CommitEffects::of(&tx);
        let committing = self.store.commit(tx, label);
        async move {
            let commit = committing.await?;
            self.acknowledged_commits
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.hints.owner_committed(&effects, &commit);
            Ok(commit)
        }
    }

    /// The node's hints: an owner commit made through another path than
    /// [`Owned::commit`] hands its [`CommitEffects`] to
    /// [`Hints::owner_committed`] once acknowledged.
    #[must_use]
    pub fn hints(&self) -> &Hints {
        &self.hints
    }

    /// Wait until mail may have arrived for the actor: a wake hint, or one
    /// claim-poll interval, whichever comes first, and answer which. A hint
    /// is only a hint; the next [`Owned::begin`] reads what actually arrived.
    pub async fn wait_for_mail(&self) -> Wake {
        self.mail_waker().wait().await
    }

    /// Report that work won a race with idle eviction.
    pub fn busy(&self) {
        self.hints.busy(self.actor());
    }

    /// Wait idle until mail, a slot request, or the idle deadline. Mail polls
    /// back off with idle time, while the host's eviction deadline stays exact.
    pub async fn wait_idle(&self, since: Instant) -> Wake {
        let Some(idle) = IdleWait::enter(&self.hints, &self.claimed.actor, since) else {
            return Wake::Yield;
        };
        let waker = MailWaker {
            poll: idle_poll(self.poll, self.clock.now().saturating_duration_since(since)).min(
                self.idle
                    .saturating_sub(self.clock.now().saturating_duration_since(since)),
            ),
            ..self.mail_waker()
        };
        let woke = waker.wait().await;
        idle.leave(woke)
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

/// How many claim polls an idle owner's mail poll backs off to.
const IDLE_POLL_CEILING: u32 = 16;

/// How long an owner idle for `idle` waits for mail before it reads its
/// mailbox again: as long as it has been idle, from one claim poll `poll`
/// up to [`IDLE_POLL_CEILING`] of them. A hint ends the wait at once
/// whatever its length; the poll is the backstop for one that never came.
fn idle_poll(poll: std::time::Duration, idle: std::time::Duration) -> std::time::Duration {
    idle.clamp(poll, poll.saturating_mul(IDLE_POLL_CEILING))
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
    /// Wait for a hint, the drain's start or one poll interval, and answer
    /// which came.
    pub async fn wait(&self) -> Wake {
        if self.drain.started() {
            return Wake::Drain;
        }
        tokio::select! {
            () = self.hint.notified() => Wake::Hint,
            () = self.drain.wait() => Wake::Drain,
            () = self.clock.sleep(self.poll) => Wake::Poll,
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

/// What ended a wait for work: a wake hint, the node's drain, or the poll
/// interval running out with no hint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wake {
    /// A wake hint: a commit on this node, or a node wake, named the actor.
    Hint,
    /// The node's drain started.
    Drain,
    /// One poll interval passed with no hint: the backstop for a hint that
    /// never came. A claim on it follows a reconciliation that found work
    /// due.
    Poll,
    /// A due time a release on this node committed passed: the claim takes
    /// the actors whose timers are due (FIG-7044).
    Deadline,
    /// A claim took the idle owner's slot ([`Owned::wait_idle`]): the
    /// owner releases its actor if it still has nothing to do.
    Yield,
}

/// One node's serving loop.
pub struct Runner {
    turn_profiles: bool,
    store: Arc<dyn DurableStore>,
    clock: Arc<dyn Clock>,
    config: RunnerConfig,
    activation: Arc<dyn Activation>,
    hints: Hints,
    node_wakes: Option<Arc<dyn NodeWakes>>,
    drain: Drain,
    liveness: Liveness,
    registration: NodeRegistration,
}

/// Publish the node's coalesced wakes, one flush at a time: whatever is
/// woken while a publish is in flight rides the next one.
async fn publish(hints: Hints, node_wakes: Arc<dyn NodeWakes>) {
    loop {
        hints.inner.flush.notified().await;
        let batch = hints.take_pending();
        if !batch.is_empty() {
            // A hint that fails to send costs latency only: the polls find
            // the work.
            let _ = node_wakes.committed(&batch).await;
        }
    }
}

/// The feed's next wake event, or never without one.
async fn next_node_wake(feed: &mut Option<Box<dyn NodeWakeFeed>>) -> NodeWakeEvent {
    match feed {
        Some(feed) => feed.next().await,
        None => std::future::pending().await,
    }
}

impl Runner {
    /// Enable physical claim profiles for a host that observes its turns.
    #[must_use]
    pub fn with_turn_profiles(mut self, enabled: bool) -> Self {
        self.turn_profiles = enabled;
        self
    }

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
            node_wakes: None,
            drain: Drain::default(),
            liveness,
            registration: NodeRegistration::default(),
            turn_profiles: false,
        }
    }

    /// Publish this runner's successful registration through `registration`.
    pub fn with_registration(mut self, registration: NodeRegistration) -> Self {
        self.registration = registration;
        self
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
            .store(self.node_wakes.is_some(), Ordering::Release);
        self.hints = hints;
        self
    }

    /// Listen, publish and watch liveness through `node_wakes`.
    #[must_use]
    pub fn with_node_wakes(mut self, node_wakes: Arc<dyn NodeWakes>) -> Self {
        self.hints.inner.publishing.store(true, Ordering::Release);
        self.node_wakes = Some(node_wakes);
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
    /// A non-transient refusal of the node's registration, the listener's
    /// refusal or startup timeout, or the store's refusal of the final release.
    pub async fn run(self, stop: impl Future<Output = ()> + Send) -> Result<Stopped, DurableError> {
        let settings = self.config.lease.settings();
        let retention = self.config.owner_retention;
        let capacity = self
            .config
            .max_active
            .min(retention.max_resident_owners.get());
        tokio::pin!(stop);
        let mut capabilities = tokio::select! {
            biased;
            () = &mut stop => return Ok(Stopped::Requested),
            capabilities = self.activation.capabilities() => capabilities?,
        };
        capabilities.extend(&self.config.offers);
        let spec = NodeSpec {
            node: self.config.node.clone(),
            decodes: self.config.decodes.clone(),
            serves: self.config.serves.clone(),
            capabilities,
            ttl_millis: self.config.lease.ttl_millis(),
        };
        let mut register_delay = settings.claim_backoff;
        let (lease, registering) = loop {
            // A failed attempt may have registered an unacknowledged boot.
            // The next registration replaces it; only the successful
            // attempt's send time starts this runner's local lease clock.
            let registering = self.clock.now();
            let registered = tokio::select! {
                biased;
                () = &mut stop => return Ok(Stopped::Requested),
                answer = self.within(registering + settings.startup, self.store.register_node(&spec)) => {
                    answer.unwrap_or_else(|| Err(not_started("registration", settings)))
                }
            };
            match registered {
                Ok(lease) => break (lease, registering),
                Err(DurableError::Store(StoreFailure {
                    kind: StoreFailureKind::Unavailable | StoreFailureKind::Contended,
                    ..
                })) => {}
                Err(error) => return Err(error),
            }
            tokio::select! {
                biased;
                () = &mut stop => return Ok(Stopped::Requested),
                () = self.clock.sleep_until(self.clock.now() + register_delay) => {}
            }
            register_delay = register_delay.saturating_mul(2).min(settings.claim_poll);
        };
        self.registration.registered(lease.clone(), registering);
        // The stored lease cannot start before this send, so the local
        // deadline is never later than the stored one.
        let started_by = registering + settings.startup;
        self.hints.serve_as(lease.owner.clone());
        self.liveness.renew(registering + settings.self_stop_after);
        // Renewal runs before the listener opens, so a slow listener never
        // ages the lease. Dropping the set on any return cancels it.
        let (ended_tx, ended) = watch::channel(None);
        let mut renewal: JoinSet<()> = JoinSet::new();
        // Page retirement runs beside the renewal and ends with it.
        let behind = match self.store.pages() {
            Some(pages) => {
                let behind = crate::retirement::Behind::unread();
                renewal.spawn(crate::retirement::retire(
                    pages,
                    Arc::clone(&self.clock),
                    self.config.retirement,
                    behind.clone(),
                ));
                behind
            }
            None => crate::retirement::Behind::never(),
        };
        let mut caught_up = behind.watch();
        // Whether the last claim was held back for retirement: the loop then
        // claims as soon as retirement catches up.
        let mut deferred = false;
        renewal.spawn(
            Renewal {
                store: Arc::clone(&self.store),
                clock: Arc::clone(&self.clock),
                lease: lease.clone(),
                settings,
                liveness: self.liveness.clone(),
                registration: self.registration.clone(),
                registering,
                ended: ended_tx,
            }
            .run(),
        );
        // The listener is in place before the first claim scans.
        let mut feed = match &self.node_wakes {
            Some(node_wakes) => match self.within(started_by, node_wakes.listen(&lease)).await {
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
        if let Some(node_wakes) = &self.node_wakes {
            publisher.spawn(publish(self.hints.clone(), Arc::clone(node_wakes)));
        }
        let mut active: JoinSet<Exit> = JoinSet::new();
        // The actor and epoch each running activation was claimed with, and
        // the claims of activations that ended abandoning their actors,
        // until the runner has handed them back.
        let mut claims: HashMap<tokio::task::Id, (ActorKey, Epoch)> = HashMap::new();
        let mut abandoned: Vec<(ActorKey, Epoch)> = Vec::new();
        let start = self.clock.now();
        let mut next_reap = start + settings.reap_every;
        // A claim evidence asked for: a hint, a filled batch, a hand-back.
        let mut next_claim: Option<Instant> = None;
        // The next reconciliation: a readiness read, then a claim only when
        // it finds work due. The first runs at once.
        let mut next_reconcile = start;
        // The store's time less this node's wall clock, as the last
        // readiness read saw it: how committed due times map onto the clock.
        let mut skew = 0_i64;
        // The first probe runs at once, so the node knows its live peers
        // before its first readied actor.
        let mut next_watch = start;
        // Each other boot whose liveness lock a probe saw held, with the
        // listener session it was seen under.
        let mut seen_held: HashMap<Owner, u64> = HashMap::new();
        // Claimed actors await resident admission outside any owner. A pending
        // claim holds no projection and never makes the resident count exceed N.
        let mut pending = std::collections::VecDeque::new();
        let mut adopt = false;
        // Whether the store records this node draining: the drain switch
        // alone already stops this runner's claims.
        let mut marked_draining = false;
        let stopped = 'serve: loop {
            while !self.drain.started() && active.len() < capacity {
                let Some((claimed, woke, profile)) = pending.pop_front() else {
                    break;
                };
                self.activate(
                    &mut active,
                    &mut claims,
                    (claimed, profile),
                    &lease.capabilities,
                    woke,
                    retention.idle,
                );
            }
            if !pending.is_empty() {
                self.hints.make_room(capacity.saturating_sub(pending.len()));
            }
            // A hand-back that failed is retried at the next turn of the
            // loop; one the fence refused finds the actor already gone.
            for (actor, epoch) in std::mem::take(&mut abandoned) {
                match self
                    .bounded(&mut stop, &ended, self.hand_back(&actor, epoch))
                    .await
                {
                    Ok(Ok(())) => next_claim = Some(self.clock.now()),
                    Ok(Err(DurableError::OwnershipLost(_))) => {}
                    Ok(Err(_)) => abandoned.push((actor, epoch)),
                    Err(stopped) => break 'serve stopped,
                }
            }
            if self.drain.started() {
                abandoned.extend(
                    pending
                        .drain(..)
                        .map(|(claim, _, _)| (claim.actor, claim.epoch)),
                );
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
            // A node behind on retirement takes no new actors.
            let held_back = behind.get();
            self.hints.offer_room(if self.drain.started() || held_back {
                0
            } else {
                self.hints.room(capacity)
            });
            // A draining node claims nothing, so nothing it would claim
            // wakes it.
            let claiming = !self.drain.started() && pending.is_empty();
            let deadline = self
                .hints
                .next_deadline()
                .filter(|_| claiming)
                .map(|due| self.on_clock(due, skew));
            let mut next = next_reap;
            if claiming {
                next = next.min(next_reconcile);
            }
            if let Some(at) = next_claim
                .filter(|_| claiming)
                .into_iter()
                .chain(deadline)
                .min()
            {
                next = next.min(at);
            }
            // What woke this turn of the loop: a claim it makes is on a hint,
            // or on the claim poll.
            let woke;
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
                event = next_node_wake(&mut feed) => {
                    woke = Wake::Hint;
                    match event {
                    NodeWakeEvent::Ready => next_claim = Some(self.clock.now()),
                    NodeWakeEvent::Owned(actors) => {
                        self.hints.changed_all_of(&actors);
                        for actor in &actors {
                            if !self.hints.hint_running(actor) {
                                next_claim = Some(self.clock.now());
                            }
                        }
                    }
                    NodeWakeEvent::Appended(actors) => {
                        self.hints.changed_all_of(&actors);
                        if let Some(followers) = self.hints.followers() {
                            followers.appended(&actors);
                        }
                        continue;
                    }
                    NodeWakeEvent::PollStore => {
                        self.hints.everything_changed();
                        self.hints.hint_all_running();
                        next_claim = Some(self.clock.now());
                    }
                    NodeWakeEvent::Resubscribed => {
                        seen_held.clear();
                        self.hints.everything_changed();
                        self.hints.hint_all_running();
                        if let Some(followers) = self.hints.followers() {
                            followers.resubscribed();
                        }
                        next_claim = Some(self.clock.now());
                    }
                    }
                }
                () = self.hints.inner.claim.notified() => {
                    woke = Wake::Hint;
                    next_claim = Some(self.clock.now());
                }
                // A new earliest due time: the next turn sleeps until it.
                () = self.hints.inner.deadline.notified() => continue,
                Ok(()) = caught_up.changed(), if deferred => {
                    woke = Wake::Poll;
                    next_claim = Some(self.clock.now());
                }
                () = self.clock.sleep_until(next) => woke = Wake::Poll,
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
            if let (Some(node_wakes), Some(session)) =
                (&self.node_wakes, feed.as_ref().map(|feed| feed.session()))
                && now >= next_watch
            {
                next_watch = now + settings.claim_poll;
                let probed = match self.bounded(&mut stop, &ended, node_wakes.liveness()).await {
                    Ok(probed) => probed,
                    Err(stopped) => break stopped,
                };
                let released = match probed {
                    // An observation that spans a lost listener session is
                    // stale: the outage may have dropped every boot's lock.
                    Ok(boots) if feed.as_ref().map(|feed| feed.session()) == Some(session) => {
                        self.hints.see_peers(live_peers(&lease, &boots));
                        released_boots(&lease, boots, session, &mut seen_held)
                    }
                    _ => Vec::new(),
                };
                for boot in released {
                    let reap = node_wakes.reap_released(&lease, &boot);
                    match self.bounded(&mut stop, &ended, reap).await {
                        Ok(Ok(reaped)) if !reaped.is_empty() => next_claim = Some(now),
                        Ok(Err(DurableError::NodeLeaseLost { .. })) => {
                            break 'serve Stopped::LeaseLost;
                        }
                        Ok(Ok(_) | Err(_)) => {}
                        Err(stopped) => break 'serve stopped,
                    }
                }
            }
            if self.drain.started() {
                continue;
            }
            // Why this turn claims, if it does: evidence first, then a due
            // time, then a reconciliation that found work.
            if !pending.is_empty() {
                // Capacity waits are woken by drop/yield events. Retaining an
                // already-due claim timer here would spin if the victim got busy.
                next_claim = None;
                continue;
            }
            let deadline_due = deadline.is_some_and(|at| now >= at);
            let cause = if next_claim.is_some_and(|at| now >= at) {
                Some(woke)
            } else if deadline_due || now >= next_reconcile {
                if !deadline_due {
                    next_reconcile = now + settings.claim_poll;
                }
                if deadline_due {
                    // A release acknowledgement may arrive after a claim
                    // already took its timer. Forget the due hints being
                    // checked before the read: a newer hint delivered during
                    // that await must survive for the next turn.
                    self.hints.claimed_through(self.store_time(skew));
                }
                if adopt {
                    Some(Wake::Poll)
                } else {
                    let reason = if deadline_due {
                        Wake::Deadline
                    } else {
                        Wake::Poll
                    };
                    match self
                        .bounded(&mut stop, &ended, self.store.readiness())
                        .await
                    {
                        Ok(Ok(readiness)) => {
                            skew = readiness_skew(&readiness, self.clock.timestamp_ms());
                            // A timer another node committed: look again
                            // one backoff after it is due, once its releasing
                            // node had its turn.
                            if let Some(due) = readiness.next_due {
                                let at = self.on_clock(due, skew) + settings.claim_backoff;
                                next_reconcile = next_reconcile.min(at);
                            }
                            readiness.due.then_some(reason)
                        }
                        // An unanswered read claims, as the poll did.
                        Ok(Err(_)) => Some(reason),
                        Err(stopped) => break stopped,
                    }
                }
            } else {
                None
            };
            let Some(woke) = cause else {
                continue;
            };
            next_claim = None;
            // Whatever is due by now, this claim, the claim it owes or the
            // claim retirement's catching up rings takes.
            self.hints.claimed_through(self.store_time(skew));
            // Behind as of now, not as of the turn's start: no new actors
            // until retirement catches up.
            deferred = *caught_up.borrow_and_update();
            if deferred {
                continue;
            }
            // The room this claim may fill; an adoption takes what this boot
            // already owns, whatever the room.
            let mut room = 0;
            let profile = self
                .turn_profiles
                .then(lash_core_ids::operational_metrics::turn::Context::default);
            let claimed = if adopt {
                match lash_core_ids::operational_metrics::turn::claim(
                    profile.as_ref(),
                    self.bounded(&mut stop, &ended, self.store.owned(&lease)),
                )
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
                // Idle owners' slots are room: the claim asks them to
                // yield below. With no room the claim is owed, and the
                // next room that opens rings it.
                room = self.hints.claim_room(capacity).min(self.config.claim_batch);
                if room == 0 {
                    continue;
                }
                match lash_core_ids::operational_metrics::turn::claim(
                    profile.as_ref(),
                    self.bounded(&mut stop, &ended, self.store.claim(&lease, room)),
                )
                .await
                {
                    Ok(claimed) => claimed,
                    Err(stopped) => break stopped,
                }
            };
            match claimed {
                Ok(claimed) => {
                    adopt = false;
                    // A claim that filled its room may have left readied
                    // actors behind: it claims again after a backoff, or
                    // owes the claim to the next room that opens. One that
                    // took less found all there was.
                    if room > 0 && claimed.len() >= room {
                        self.hints.owe_claim();
                        next_claim = Some(now + settings.claim_backoff);
                    }
                    pending.extend(claimed.into_iter().enumerate().map(|(index, claimed)| {
                        (
                            claimed,
                            woke,
                            profile.as_ref().map(|p| p.claimed_actor(index == 0)),
                        )
                    }));
                }
                Err(DurableError::NodeLeaseLost { .. }) => break Stopped::LeaseLost,
                // A claim whose reply is lost may have taken actors: adopt
                // whatever this boot owns before claiming more.
                Err(_) => adopt = true,
            }
        };
        if stopped == Stopped::LeaseLost {
            self.liveness.lose();
        }
        // Capture the serving decision before waiting for activation cancellation,
        // listener teardown or any shutdown/reporting work.
        self.registration.stopped(self.clock.now());
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

    /// When this node's clock reaches store time `due`, given the store's
    /// `skew` from its wall clock: a millisecond late rather than early, so
    /// a claim at that moment finds the due time passed.
    fn on_clock(&self, due: DurableInstant, skew: i64) -> Instant {
        let ahead = due
            .0
            .saturating_sub(self.store_time(skew).0)
            .saturating_add(1);
        self.clock.now() + std::time::Duration::from_millis(u64::try_from(ahead).unwrap_or(0))
    }

    /// The store's time now, as this node's wall clock and `skew` put it.
    fn store_time(&self, skew: i64) -> DurableInstant {
        DurableInstant(
            i64::try_from(self.clock.timestamp_ms())
                .unwrap_or(i64::MAX)
                .saturating_add(skew),
        )
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
        let commit = if self.drain.started() {
            let effects = CommitEffects::of(&tx);
            (effects, self.drain.release(self.store.as_ref(), tx).await?)
        } else {
            tx.give_up(Release::Ready);
            let effects = CommitEffects::of(&tx);
            (
                effects,
                self.store.commit(tx, CommitLabel::DRAIN_RELEASE).await?,
            )
        };
        // A draining node's hand-back reaches a peer through its hint.
        self.hints.owner_committed(&commit.0, &commit.1);
        Ok(())
    }

    fn activate(
        &self,
        active: &mut JoinSet<Exit>,
        claims: &mut HashMap<tokio::task::Id, (ActorKey, Epoch)>,
        profiled: (
            Claimed,
            Option<lash_core_ids::operational_metrics::turn::Context>,
        ),
        capabilities: &crate::NodeCapabilities,
        claimed_on: Wake,
        idle: std::time::Duration,
    ) {
        let (claimed, turn_profile) = profiled;
        let actor = claimed.actor.clone();
        let claim = (claimed.actor.clone(), claimed.epoch);
        let owned = Owned {
            store: Arc::clone(&self.store),
            clock: Arc::clone(&self.clock),
            hint: self.hints.start(actor.clone()),
            hints: self.hints.clone(),
            claimed,
            capabilities: capabilities.clone(),
            claimed_on,
            poll: self.config.lease.settings().claim_poll,
            idle,
            drain: self.drain.clone(),
            liveness: self.liveness.clone(),
            acknowledged_commits: std::sync::atomic::AtomicU64::new(0),
            turn_profile,
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

/// The store's time less the wall clock's `local` milliseconds, as
/// `readiness` read it.
fn readiness_skew(readiness: &Readiness, local: u64) -> i64 {
    readiness
        .now
        .0
        .saturating_sub(i64::try_from(local).unwrap_or(i64::MAX))
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
        code: None,
        kind: StoreFailureKind::Unavailable,
        message: format!(
            "the node's {what} was not in place within startup ({:?})",
            settings.startup
        ),
    })
}

/// The other nodes whose listeners hold their boots' locks in `boots`,
/// sorted and once each.
fn live_peers(lease: &NodeLease, boots: &[crate::node_wakes::BootLiveness]) -> Vec<NodeId> {
    let mut peers: Vec<NodeId> = boots
        .iter()
        .filter(|liveness| liveness.held && liveness.boot.node != lease.owner.node)
        .map(|liveness| liveness.boot.node.clone())
        .collect();
    peers.sort();
    peers.dedup();
    peers
}

/// Fold one liveness probe into `seen_held`, answering each boot this node
/// saw holding its lock under listener session `session` and now sees free:
/// a boot that died. A boot seen free without having been seen held is
/// starting up or never listens, so its lease alone decides it.
fn released_boots(
    lease: &NodeLease,
    boots: Vec<crate::node_wakes::BootLiveness>,
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
