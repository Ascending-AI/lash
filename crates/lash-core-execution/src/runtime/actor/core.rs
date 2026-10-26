//! The context's core: the owned actor, its fenced transactions, the clock,
//! the cancel token, the replay probe and the due times.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use lash_durable::{
    ActorCommit, ActorKey, ActorTx, CommitLabel, DueSource, Dues, DurableError, DurableInstant,
    DurableProbe, DurableStore, Epoch,
};
use tokio_util::sync::CancellationToken;

#[path = "core_boundary.rs"]
pub(super) mod boundary;
#[path = "core_session.rs"]
mod session;

use crate::AdmittedScope;
use crate::Clock;
use crate::ExecutionScope;
use crate::RuntimeError;
use lash_execution_contracts::backend::Backend;
use lash_vocabulary::identity::TurnId;

/// The one effect context of an owned actor.
///
/// Cloning shares it: every clone commits under the same epoch, notes into
/// the same due times and observes the same cancel token. It is never a
/// grant: a commit under a lost epoch is refused with
/// [`DurableError::OwnershipLost`], and the activation that holds the context
/// then drops it with everything it cached.
///
/// It also carries the admitted scope it executes under (a turn, a process,
/// a runtime operation): [`ActorContext::scoped`] re-scopes a clone.
#[derive(Clone)]
pub struct ActorContext {
    pub(super) first_commit: Option<Arc<boundary::FirstCommit>>,
    pub(super) inner: Arc<Inner>,
    pub(super) scope: Arc<Scope>,
    pub(super) idle_poll: crate::runtime::PollPacing,
}

/// The scope a context executes under, shared by its clones.
pub(super) struct Scope {
    pub(super) admitted: AdmittedScope,
    pub(super) trace_scope: Option<Arc<lash_trace::DurableTraceScope>>,
    pub(super) physical_turn: Option<Arc<TurnId>>,
    pub(super) ordinals: Arc<Ordinals>,
    /// The journal-era scaffolding of [`super::journal`].
    pub(super) journal_guard: Option<Arc<crate::CommandJournalGuard>>,
    pub(super) frontier: Arc<super::journal::DriveFrontier>,
}

/// Per-scope ordinals, shared by a context's clones.
#[derive(Debug, Default)]
pub(super) struct Ordinals {
    keyless_starts: AtomicU32,
    compactions: AtomicU32,
    completions: AtomicU32,
    command_runs: AtomicU32,
    pub(super) effects: std::sync::atomic::AtomicU64,
}

impl Scope {
    fn new(admitted: AdmittedScope) -> Arc<Self> {
        Arc::new(Self {
            admitted,
            trace_scope: None,
            physical_turn: None,
            ordinals: Arc::default(),
            journal_guard: None,
            frontier: Arc::default(),
        })
    }

    pub(super) fn with(&self) -> Self {
        Self {
            admitted: self.admitted.clone(),
            trace_scope: self.trace_scope.clone(),
            physical_turn: self.physical_turn.clone(),
            ordinals: Arc::clone(&self.ordinals),
            journal_guard: self.journal_guard.clone(),
            frontier: Arc::clone(&self.frontier),
        }
    }
}

#[cfg(any(test, feature = "testing"))]
type ModelRetryInterleave = (Arc<tokio::sync::Semaphore>, Arc<tokio::sync::Semaphore>);

#[cfg(any(test, feature = "testing"))]
type PostimageObserver = Arc<dyn Fn(serde_json::Value) + Send + Sync>;

pub(super) struct Inner {
    #[cfg(any(test, feature = "testing"))]
    postimage_observer: std::sync::Mutex<Option<PostimageObserver>>,
    claim_live: AtomicBool,
    #[cfg(any(test, feature = "testing"))]
    cold_after_ack: AtomicBool,
    session_projection: super::claim_cell::ClaimCell<
        Option<super::owner_state::OwnerState<super::owner_state::SessionProjection>>,
    >,
    process_projection: super::claim_cell::ClaimCell<
        Option<super::owner_state::OwnerState<super::process::continuation::Continuation>>,
    >,
    backend: Option<Backend>,

    /// The node's store a claimed context commits through; the backend's
    /// own otherwise.
    durable: Option<Arc<dyn DurableStore>>,
    actor: ActorKey,
    epoch: Epoch,
    /// Independent append operations plan and commit under one owner.
    fresh_updates: tokio::sync::Mutex<()>,
    /// What the claim's activation waits on for mail; a context that owns
    /// no claim polls on the backend's claim interval.
    mail: Option<lash_durable::runner::MailWaker>,
    /// The claiming node's drain switch; a context that owns no claim
    /// never drains.
    drain: Option<lash_durable::runner::Drain>,
    /// The claiming node's lease as its own clock sees it; a context that
    /// owns no claim holds no node lease.
    liveness: Option<lash_durable::runner::Liveness>,
    /// The claiming node's hints, which each acknowledged commit hands its
    /// effects (FIG-7044); a context that owns no claim has none.
    hints: Option<lash_durable::runner::Hints>,
    clock: Arc<dyn Clock>,
    capabilities: lash_vocabulary::claims::NodeCapabilities,
    cancel: CancellationToken,
    probe: Arc<dyn DurableProbe>,
    dues: Dues,
    /// A claimed session's claim obligations, which every commit through
    /// the context derives its `requires` from (S7); none otherwise.
    obligations: Option<super::claim_obligations::SessionObligations>,
    /// The turn drives built under the context, which
    /// [`ActorContext::next_drive`] numbers.
    drives: AtomicU32,
    tool_call_admissions: AtomicU64,
    acknowledged_commits: AtomicU64,
    acknowledged_progress: AtomicU64,
    #[cfg(any(test, feature = "testing"))]
    model_retry_interleave: std::sync::Mutex<Option<ModelRetryInterleave>>,
    model_tasks: std::sync::Mutex<Vec<crate::task::JoinHandle<()>>>,
    model_attempts: std::sync::Mutex<Vec<crate::EffectAttempt>>,
    model_calls: std::sync::Mutex<Vec<lash_durable::domain::model_calls::OwnedModelCall>>,
}

impl std::fmt::Debug for ActorContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ActorContext")
            .field("actor", &self.inner.actor)
            .field("epoch", &self.inner.epoch)
            .field("scope", &self.scope.admitted)
            .finish_non_exhaustive()
    }
}

/// Invalidates an activation's retained state on every exit, including
/// cancellation and unwind, even when a detached body still holds a context.
struct SessionClaimGuard(ActorContext);
impl Drop for SessionClaimGuard {
    fn drop(&mut self) {
        self.0.inner.claim_live.store(false, Ordering::Release);
        self.0.inner.cancel.cancel();
        self.0.inner.session_projection.release();
        self.0.inner.process_projection.release();
        if let Some(obligations) = &self.0.inner.obligations {
            obligations.invalidate_on_exit();
        }
    }
}

impl ActorContext {
    /// Claim the obligation to invalidate all context clones on activation exit.
    #[must_use]
    pub fn session_claim_guard(&self) -> impl Drop + Send + 'static {
        SessionClaimGuard(self.clone())
    }

    /// Replace every acknowledged projection with a cold reference, for the
    /// LO-POSTIMAGE differential. This changes no durable transition.
    #[cfg(any(test, feature = "testing"))]
    pub fn cold_after_acknowledgement(&self) {
        self.inner.cold_after_ack.store(true, Ordering::Release);
    }

    /// Observe the complete acknowledged projection, after either warm adoption
    /// or the differential's cold reconstruction, before further owner work.
    #[cfg(any(test, feature = "testing"))]
    pub fn observe_session_postimages_for_testing(
        &self,
        observer: impl Fn(serde_json::Value) + Send + Sync + 'static,
    ) {
        use lash_protocol_types::sync::MutexExt as _;
        *self.inner.postimage_observer.lock_recover() = Some(Arc::new(observer));
    }

    /// The context of `actor`, owned at `epoch` over `backend`'s durable
    /// store, executing under `admitted`. `cancel` is cancelled when the
    /// activation must stop: the node stops serving or the actor is lost.
    /// `probe` is where owners report what hidden replay would make them do
    /// (`NoProbe` in production).
    #[must_use]
    pub fn new(
        backend: Backend,
        actor: ActorKey,
        epoch: Epoch,
        admitted: AdmittedScope,
        cancel: CancellationToken,
        probe: Arc<dyn DurableProbe>,
    ) -> Self {
        let clock = backend.clock();
        Self {
            idle_poll: crate::runtime::PollPacing::standard(),
            first_commit: None,
            inner: Arc::new(Inner {
                #[cfg(any(test, feature = "testing"))]
                postimage_observer: std::sync::Mutex::default(),
                claim_live: AtomicBool::new(true),
                #[cfg(any(test, feature = "testing"))]
                cold_after_ack: AtomicBool::new(false),
                session_projection: super::claim_cell::ClaimCell::new(None),
                process_projection: super::claim_cell::ClaimCell::new(None),

                backend: Some(backend),
                durable: None,
                fresh_updates: tokio::sync::Mutex::new(()),
                actor,
                epoch,
                mail: None,
                drain: None,
                liveness: None,
                hints: None,
                clock,
                capabilities: lash_vocabulary::claims::NodeCapabilities::default(),
                cancel,
                probe,
                dues: Dues::new(),
                obligations: None,
                drives: AtomicU32::new(0),
                tool_call_admissions: AtomicU64::new(0),
                acknowledged_commits: AtomicU64::new(0),
                acknowledged_progress: AtomicU64::new(0),
                model_calls: std::sync::Mutex::default(),
                model_attempts: std::sync::Mutex::default(),
                #[cfg(any(test, feature = "testing"))]
                model_retry_interleave: std::sync::Mutex::default(),
                model_tasks: std::sync::Mutex::default(),
            }),
            scope: Scope::new(admitted),
        }
    }

    /// The context of the actor `owned` holds: its epoch, its node's store
    /// and its node's clock, over `backend`'s services. Every activation runs
    /// under the context of its claim.
    #[must_use]
    pub fn claimed(
        backend: Backend,
        owned: &lash_durable::runner::Owned,
        admitted: AdmittedScope,
        cancel: CancellationToken,
        probe: Arc<dyn DurableProbe>,
    ) -> Self {
        Self {
            idle_poll: crate::runtime::PollPacing::standard(),
            first_commit: None,
            inner: Arc::new(Inner {
                #[cfg(any(test, feature = "testing"))]
                postimage_observer: std::sync::Mutex::default(),
                claim_live: AtomicBool::new(true),
                #[cfg(any(test, feature = "testing"))]
                cold_after_ack: AtomicBool::new(false),
                session_projection: super::claim_cell::ClaimCell::new(None),
                process_projection: super::claim_cell::ClaimCell::new(None),

                backend: Some(backend),
                durable: Some(Arc::clone(owned.store())),
                fresh_updates: tokio::sync::Mutex::new(()),
                actor: owned.actor().clone(),
                epoch: owned.epoch(),
                mail: Some(owned.mail_waker()),
                drain: Some(owned.drain().clone()),
                liveness: Some(owned.liveness().clone()),
                hints: Some(owned.hints().clone()),
                clock: Arc::clone(owned.clock()),
                capabilities: owned.capabilities().clone(),
                cancel,
                probe,
                dues: Dues::new(),
                obligations: (owned.actor().kind() == lash_durable::ActorKind::Session)
                    .then(|| {
                        lash_vocabulary::identity::SessionId::try_from(
                            owned.actor().id().to_owned(),
                        )
                        .ok()
                    })
                    .flatten()
                    .map(|session| {
                        super::claim_obligations::SessionObligations::claimed(
                            session,
                            owned.requires(),
                        )
                    }),
                drives: AtomicU32::new(0),
                tool_call_admissions: AtomicU64::new(0),
                acknowledged_commits: AtomicU64::new(0),
                acknowledged_progress: AtomicU64::new(0),
                model_calls: std::sync::Mutex::default(),
                model_attempts: std::sync::Mutex::default(),
                #[cfg(any(test, feature = "testing"))]
                model_retry_interleave: std::sync::Mutex::default(),
                model_tasks: std::sync::Mutex::default(),
            }),
            scope: Scope::new(admitted),
        }
    }

    /// A context with no backend, for a test that never runs an effect:
    /// every commit through it is refused as unavailable.
    #[expect(
        clippy::expect_used,
        reason = "a constant actor id is valid by construction"
    )]
    #[cfg(any(test, feature = "testing"))]
    #[must_use]
    pub fn unavailable() -> Self {
        Self {
            idle_poll: crate::runtime::PollPacing::standard(),
            first_commit: None,
            inner: Arc::new(Inner {
                #[cfg(any(test, feature = "testing"))]
                postimage_observer: std::sync::Mutex::default(),
                claim_live: AtomicBool::new(true),
                #[cfg(any(test, feature = "testing"))]
                cold_after_ack: AtomicBool::new(false),
                session_projection: super::claim_cell::ClaimCell::new(None),
                process_projection: super::claim_cell::ClaimCell::new(None),
                backend: None,

                durable: None,
                fresh_updates: tokio::sync::Mutex::new(()),
                actor: ActorKey::session("unavailable").expect("a constant actor id"),
                epoch: Epoch(0),
                mail: None,
                drain: None,
                liveness: None,
                hints: None,
                clock: Arc::new(crate::SystemClock),
                capabilities: lash_vocabulary::claims::NodeCapabilities::default(),
                cancel: CancellationToken::new(),
                probe: Arc::new(lash_durable::NoProbe),
                dues: Dues::new(),
                obligations: None,
                drives: AtomicU32::new(0),
                tool_call_admissions: AtomicU64::new(0),
                acknowledged_commits: AtomicU64::new(0),
                acknowledged_progress: AtomicU64::new(0),
                model_calls: std::sync::Mutex::default(),
                model_attempts: std::sync::Mutex::default(),
                #[cfg(any(test, feature = "testing"))]
                model_retry_interleave: std::sync::Mutex::default(),
                model_tasks: std::sync::Mutex::default(),
            }),
            scope: Scope::new(AdmittedScope::runtime_operation("unavailable")),
        }
    }

    /// A context over `backend` that owns no actor: the root the journal-era
    /// runtime host scopes its work from; an activation runs under
    /// [`Self::claimed`]. Its reads and `backend()` work; the store's fence
    /// refuses any commit through it, because epoch 0 is never claimed.
    #[expect(
        clippy::expect_used,
        reason = "a constant actor id is valid by construction"
    )]
    #[must_use]
    pub fn detached(backend: Backend) -> Self {
        Self::new(
            backend,
            ActorKey::session("detached").expect("a constant actor id"),
            Epoch(0),
            AdmittedScope::runtime_operation("detached"),
            CancellationToken::new(),
            Arc::new(lash_durable::NoProbe),
        )
    }

    /// The owned actor.
    #[must_use]
    pub fn actor(&self) -> &ActorKey {
        &self.inner.actor
    }

    /// The epoch the actor is owned under.
    #[must_use]
    pub fn epoch(&self) -> Epoch {
        self.inner.epoch
    }

    /// A fresh drive of a turn under this context: the epoch the actor is
    /// owned under and the drive's ordinal among those its claim built. No
    /// other drive of the actor, on this owner or another, is named alike,
    /// so a lane whose ordinals restart with each drive is keyed by it and
    /// never reuses an id another drive published (FIG-6274).
    #[must_use]
    pub fn next_drive(&self) -> String {
        let ordinal = self.inner.drives.fetch_add(1, Ordering::Relaxed);
        format!("{}.{ordinal}", self.inner.epoch.0)
    }

    /// The node's clock now, for live enforcement (local timers for
    /// `expires_at - now`). A durable instant a row stores comes from the
    /// store, inside its transaction, never from here.
    #[must_use]
    pub fn now(&self) -> DurableInstant {
        DurableInstant(i64::try_from(self.inner.clock.timestamp_ms()).unwrap_or(i64::MAX))
    }

    /// Pace waits without a mailbox or backend using `pacing.maximum()` as
    /// the fixed delay. The standard preset uses a 1s idle delay, a historical
    /// value without workload measurements.
    #[must_use]
    pub fn with_idle_pacing(mut self, pacing: crate::runtime::PollPacing) -> Self {
        self.idle_poll = pacing;
        self
    }

    /// The node's clock, for live enforcement: a local timer for
    /// `expires_at - now`.
    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.inner.clock
    }

    /// The claiming node's advertised capabilities, captured from its lease.
    pub fn capabilities(&self) -> &lash_vocabulary::claims::NodeCapabilities {
        &self.inner.capabilities
    }

    /// The owner's unfenced reads of its domain rows (S0): current unless
    /// ownership was lost, which the next commit's fence reports.
    ///
    /// # Errors
    ///
    /// Unavailable for [`Self::unavailable`].
    pub fn durable_reads(&self) -> Result<&dyn lash_durable::DurableReads, DurableError> {
        self.durable()
            .map(|store| &**store as &dyn lash_durable::DurableReads)
    }

    /// The store's clock now: the instant a row stores, such as a deadline
    /// recorded before its work starts.
    ///
    /// # Errors
    ///
    /// The store's refusal.
    pub async fn durable_now(&self) -> Result<DurableInstant, DurableError> {
        self.durable()?.now().await
    }

    /// Wait until mail may have arrived for the actor: its claim's wake
    /// hint, or one claim-poll interval. What arrived shows at the next
    /// fenced read, never here.
    pub async fn wait_for_mail(&self) {
        match &self.inner.mail {
            Some(mail) => {
                mail.wait().await;
            }
            None => {
                let poll = self
                    .inner
                    .backend
                    .as_ref()
                    .map_or(self.idle_poll.maximum(), |backend| {
                        backend.config().lease().settings().claim_poll
                    });
                self.inner.clock.sleep(poll).await;
            }
        }
    }

    /// Whether a session an earlier build left is carried to this build's
    /// formats now (ADR 0106 §2): this build carries an earlier one's
    /// session set, the node is not draining, and every live node that
    /// serves sessions decodes this build's set. Until then a session's
    /// state stays as the build that wrote it reads it.
    ///
    /// # Errors
    ///
    /// The store's.
    pub async fn carries_sessions(&self) -> Result<bool, DurableError> {
        let formats = self.backend().formats();
        if formats.carried_sessions().is_empty() || self.draining() {
            return Ok(false);
        }
        let live = self.durable()?.live_nodes().await?;
        let mut candidates = vec![formats.session().clone()];
        candidates.extend(formats.carried_sessions().iter().cloned());
        Ok(lash_durable::fleet_writable(&candidates, &live) == Some(formats.session()))
    }

    /// Whether the claimed actor, a session, is in the session set of an
    /// earlier build that this build carries forward, and is carried now
    /// ([`Self::carries_sessions`]): such a session with no turn to run is
    /// carried outside a turn (FIG-5787). The actor's row is read
    /// unfenced; only its owner moves its format set, so what it reads is
    /// this owner's own.
    ///
    /// # Errors
    ///
    /// The store's.
    pub async fn holds_carried_session(&self) -> Result<bool, DurableError> {
        let formats = self.backend().formats();
        if formats.carried_sessions().is_empty() {
            return Ok(false);
        }
        let Some(row) = self.durable()?.actor(self.actor()).await? else {
            return Ok(false);
        };
        if !formats.carried_sessions().contains(&row.formats) {
            return Ok(false);
        }
        self.carries_sessions().await
    }

    /// Whether the node this context's claim runs on is draining: the
    /// activation releases the actor at its next committed phase (ADR 0106
    /// §1). False for a context that owns no claim.
    #[must_use]
    pub fn draining(&self) -> bool {
        self.inner
            .drain
            .as_ref()
            .is_some_and(lash_durable::runner::Drain::started)
    }

    /// Release the claimed actor `ready` under `drain.release`, at the
    /// committed phase its rows hold, for a node of the next build; the
    /// node's drain records it among the actors it released.
    ///
    /// # Errors
    ///
    /// The store's refusal: [`DurableError::OwnershipLost`] once the actor
    /// is someone else's; unavailable for a context that owns no claim.
    pub async fn drain_release(&self) -> Result<ActorCommit, DurableError> {
        let drain = self.claimed_drain()?;
        let tx = self.begin().await?;
        let durable = self.durable()?;
        self.with_obligations(tx, CommitLabel::DRAIN_RELEASE, None, None, |tx| {
            drain.release(durable.as_ref(), tx)
        })
        .await
    }

    /// Commit `tx` under `label` with the claimed actor given up as
    /// `release`: a draining activation's phase commit that releases the
    /// actor itself. The node's drain records it among the actors it
    /// released, as [`Self::drain_release`] does.
    ///
    /// # Errors
    ///
    /// The store's refusal: [`DurableError::OwnershipLost`] once the actor
    /// is someone else's, [`DurableError::Domain`] for a refused domain
    /// write; unavailable for a context that owns no claim.
    pub async fn commit_draining(
        &self,
        tx: ActorTx,
        label: CommitLabel,
        release: lash_durable::Release,
    ) -> Result<ActorCommit, DurableError> {
        let drain = self.claimed_drain()?;
        let durable = self.durable()?;
        self.with_obligations(tx, label, None, None, |tx| {
            drain.release_with(durable.as_ref(), tx, label, release)
        })
        .await
    }

    /// Tool calls admitted by acknowledged commits of this activation.
    /// A turn takes a mark beside its loaded row and adds the later delta
    /// at close. Nothing is retained or reconstructed from this projection.
    #[doc(hidden)]
    pub fn tool_call_admissions(&self) -> u64 {
        self.inner.tool_call_admissions.load(Ordering::Relaxed)
    }

    fn with_obligations<'a, F, Fut>(
        &'a self,
        tx: ActorTx,
        label: CommitLabel,
        owing: Option<&'a super::claim_obligations::ExecutionDebts>,
        resolution: Option<
            &'a std::sync::Mutex<Option<lash_core_store::resolutions::ResolutionRecord>>,
        >,
        commit: F,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ActorCommit, DurableError>> + Send + 'a>,
    >
    where
        F: FnOnce(ActorTx) -> Fut + Send + 'a,
        Fut: std::future::Future<Output = Result<ActorCommit, DurableError>> + Send + 'a,
    {
        Box::pin(async move {
            let committed = self
                .commit_owned(tx, label, owing, resolution, commit)
                .await?;
            if let Some(boundary) = &self.first_commit {
                boundary.committed();
            }
            Ok(committed)
        })
    }

    fn commit_owned<'a, F, Fut>(
        &'a self,
        mut tx: ActorTx,
        label: CommitLabel,
        owing: Option<&'a super::claim_obligations::ExecutionDebts>,
        resolution: Option<
            &'a std::sync::Mutex<Option<lash_core_store::resolutions::ResolutionRecord>>,
        >,
        commit: F,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ActorCommit, DurableError>> + Send + 'a>,
    >
    where
        F: FnOnce(ActorTx) -> Fut + Send + 'a,
        Fut: std::future::Future<Output = Result<ActorCommit, DurableError>> + Send + 'a,
    {
        Box::pin(async move {
            // A nested reservation can itself advance this owner. Settle it
            // before claiming the projection, then validate this draft's
            // original revision below; never restamp it after settlement.
            let model_debts = match self.stage_model_calls(&mut tx).await {
                Ok(debts) => debts,
                Err(error) => {
                    self.invalidate_session_projection().await;
                    self.invalidate_process_projection().await;
                    return Err(error);
                }
            };
            let effects = lash_durable::runner::CommitEffects::of(&tx);
            if self.actor().kind() == lash_durable::ActorKind::Process {
                let mut projection = self.inner.process_projection.lock().await?;
                if let Some(owner) = projection.as_mut() {
                    let next = owner.state()?.clone();
                    if let Err(error) = next.prepare(&mut tx) {
                        owner.poison();
                        return Err(error);
                    }
                    tx.retain_process(next.view.clone().ok_or_else(|| {
                        DurableError::ProcessPostimage {
                            actor: self.actor().clone(),
                            message: "process projection has no view".into(),
                        }
                    })?);
                    let appended = tx
                        .domain()
                        .iter()
                        .any(|write| matches!(write, lash_durable::DomainWrite::RunFact(_)));
                    let draft = match owner.draft(tx, next) {
                        Ok(draft) => draft.observed(label, Arc::clone(&self.inner.probe)),
                        Err(error) => {
                            owner.poison();
                            return Err(error);
                        }
                    };
                    let committed = owner
                        .commit(
                            draft,
                            |tx| self.with_obligations_inner(tx, owing, model_debts, commit),
                            |next, receipt| {
                                let mut next = next.acknowledge(receipt, self.actor(), appended)?;
                                if let Some(resolution) = resolution {
                                    let winner = resolution
                                        .lock()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                                        .clone()
                                        .ok_or_else(|| DurableError::ProcessPostimage {
                                            actor: self.actor().clone(),
                                            message: "resolution acknowledgement has no winner"
                                                .into(),
                                        })?;
                                    lash_durable::resolutions::validate_resolution(&winner)?;
                                    next.resolutions.insert(winner.key.clone(), winner);
                                }
                                Ok(next)
                            },
                        )
                        .await?;
                    if !self.inner.claim_live.load(Ordering::Acquire) {
                        owner.poison();
                        return Err(DurableError::OwnershipLost(lash_durable::Fenced {
                            actor: self.actor().clone(),
                            held: self.epoch(),
                            current: None,
                        }));
                    }
                    if committed.state != lash_durable::ActorState::Owned {
                        *projection = None;
                    }
                    drop(projection);
                    if let Some(hints) = &self.inner.hints {
                        hints.owner_committed(&effects, &committed);
                    }
                    return Ok(committed);
                }
            }
            let mut projection = self.inner.session_projection.lock().await?;
            let Some(owner) = projection.as_mut() else {
                // Admission and refusal can commit before a readable head
                // exists. Their empty projection still has the same fenced
                // acknowledgement/adoption boundary as a hydrated owner.
                let mut owner = super::owner_state::OwnerState::loaded(
                    self.actor().clone(),
                    self.epoch(),
                    tx.revision(),
                    (),
                );
                let draft = owner
                    .draft(tx, ())?
                    .observed(label, Arc::clone(&self.inner.probe));
                let committed = owner
                    .commit(
                        draft,
                        |tx| self.with_obligations_inner(tx, owing, model_debts, commit),
                        |(), _| Ok(()),
                    )
                    .await?;
                drop(projection);
                if let Some(hints) = &self.inner.hints {
                    hints.owner_committed(&effects, &committed);
                }
                return Ok(committed);
            };
            let acknowledged = owner.state()?.clone();
            let basis = owner.revision()?;
            let mut next = acknowledged.clone();
            let refused_postimage = match next.prepare(&tx) {
                Ok(()) => false,
                // An ambiguous terminal can already have ended this turn.
                // Let the writer atomically refuse the held answer and all
                // its staged facts, but never adopt an invalid postimage.
                Err(DurableError::Domain(lash_durable::domain::DomainRefusal::TurnNotOpen {
                    ..
                })) => true,
                Err(error) => {
                    owner.poison();
                    return Err(error);
                }
            };
            let postimage = tx.take_session_postimage();
            let prompt = tx.take_prompt_postimage();
            let draft = match owner.draft(tx, next) {
                Ok(draft) => draft.observed(label, Arc::clone(&self.inner.probe)),
                Err(error) => {
                    owner.poison();
                    return Err(error);
                }
            };
            let committed = owner
                .commit(
                    draft,
                    |tx| self.with_obligations_inner(tx, owing, model_debts, commit),
                    |next, receipt| {
                        if refused_postimage {
                            return Err(DurableError::Store(lash_durable::StoreFailure {
                                kind: lash_durable::StoreFailureKind::Corrupt,
                                message: "writer acknowledged a refused turn postimage".into(),
                            }));
                        }
                        next.acknowledge(postimage, receipt, prompt)
                    },
                )
                .await;
            let committed = match committed {
                Ok(committed) => committed,
                Err(error) => {
                    if let Err(reload) = owner
                        .reload(self.recovered_session(acknowledged, basis))
                        .await
                    {
                        tracing::debug!(actor = %self.actor(), error = %reload,
                            "failed session commit still requires cold recovery");
                    }
                    if !self.inner.claim_live.load(Ordering::Acquire) {
                        owner.poison();
                    }
                    return Err(error);
                }
            };
            if !self.inner.claim_live.load(Ordering::Acquire) {
                owner.poison();
                return Err(DurableError::Store(lash_durable::StoreFailure {
                    kind: lash_durable::StoreFailureKind::Unavailable,
                    message: "session claim ended during commit".into(),
                }));
            }
            #[cfg(any(test, feature = "testing"))]
            if committed.state == lash_durable::ActorState::Owned
                && self.inner.cold_after_ack.load(Ordering::Acquire)
            {
                let mut adopted = owner.state()?.clone();
                let id = adopted.head.session_id.clone();
                owner.poison();
                let catalog = self.backend().session_store_factory();
                let runtime: Arc<dyn lash_core_store::store::RuntimeStore> = catalog;
                let failed = |error: lash_core_store::store::StoreError| {
                    DurableError::Store(lash_durable::StoreFailure {
                        kind: lash_durable::StoreFailureKind::Corrupt,
                        message: error.to_string(),
                    })
                };
                let store = lash_core_store::store::SessionStore::new(runtime, id.clone())
                    .map_err(failed)?;
                let (mut loaded, turn) =
                    lash_core_ids::operational_metrics::sql::classify_owner_reads(
                        lash_core_ids::operational_metrics::sql::OwnerReadClass::Recovery,
                        async {
                            let loaded = lash_core_store::store::load_session_window_state(
                                &store,
                                lash_core_store::store::WindowSelector::Current,
                            )
                            .await
                            .map_err(failed)?
                            .ok_or_else(|| {
                                failed(lash_core_store::store::StoreError::SessionNotFound {
                                    session_id: id.clone(),
                                })
                            })?;
                            let turn = self.durable_reads()?.turn(&id).await?;
                            self.durable()?
                                .begin_retained(self.actor(), self.epoch(), committed.revision, 0)
                                .await?;
                            Ok::<_, DurableError>((loaded, turn))
                        },
                    )
                    .await?;
                // Cancellation is an external frontier: it can advance while
                // an acknowledgement is delayed. Compare owner-controlled
                // state under the same freshly read cancellation overlay.
                if let Some(held) = adopted.turn.as_mut() {
                    held.cancel = turn.as_ref().and_then(|turn| turn.cancel.clone());
                }
                // Same-deployment oracle: every acknowledged active postimage
                // must equal a cold read, with identities and values intact.
                if serde_json::to_value(adopted.head.to_snapshot()).map_err(|error| {
                    failed(lash_core_store::store::StoreError::StoredDataCorrupt {
                        record_kind: "owner postimage",
                        message: error.to_string(),
                    })
                })? != serde_json::to_value(loaded.state.to_snapshot()).map_err(|error| {
                    failed(lash_core_store::store::StoreError::StoredDataCorrupt {
                        record_kind: "cold postimage",
                        message: error.to_string(),
                    })
                })? || adopted.head.head_revision != loaded.state.head_revision
                    || adopted.head.config_revision != loaded.state.config_revision
                    || adopted.turn != turn
                {
                    return Err(DurableError::Store(lash_durable::StoreFailure {
                        kind: lash_durable::StoreFailureKind::Corrupt,
                        message: format!(
                            "acknowledged postimage differs from cold read at {} revision {}: warm turn {:?}, cold turn {:?}",
                            label.as_str(),
                            committed.revision.0,
                            adopted.turn,
                            turn
                        ),
                    }));
                }
                loaded.state.session_graph.detach_owner_prompt_rendering();
                *owner = super::owner_state::OwnerState::loaded(
                    self.actor().clone(),
                    self.epoch(),
                    committed.revision,
                    super::owner_state::SessionProjection {
                        prompt: Default::default(),
                        composition: None,
                        environment: None,
                        phase: None,
                        fleet: store.fleet_format(),
                        head: Arc::new(loaded.state),
                        turn,
                    },
                );
            }
            #[cfg(any(test, feature = "testing"))]
            {
                use lash_protocol_types::sync::MutexExt as _;
                let observer = self.inner.postimage_observer.lock_recover().clone();
                if let Some(observer) = observer
                    && let Ok(state) = owner.state()
                {
                    // Observe the acknowledged cut, before a cold read's later
                    // external cancellation frontier. Keep its actual receipt.
                    let mut acknowledged = state.clone();
                    if let Some(turn) = acknowledged.turn.as_mut() {
                        turn.cancel = committed.turn_cancel.clone();
                    }
                    observer(serde_json::json!({
                        "label":label.as_str(), "revision":committed.revision.0,
                        "head_revision":acknowledged.head.head_revision,
                        "config_revision":acknowledged.head.config_revision,
                        "head":acknowledged.head.to_snapshot(),
                        "turn":acknowledged.turn.as_ref().map(active_turn_postimage),
                    }));
                }
            }
            if committed.state != lash_durable::ActorState::Owned {
                *projection = None;
            }
            drop(projection);
            if let Some(hints) = &self.inner.hints {
                hints.owner_committed(&effects, &committed);
            }
            Ok(committed)
        })
    }

    async fn with_obligations_inner<F, Fut>(
        &self,
        tx: ActorTx,
        owing: Option<&super::claim_obligations::ExecutionDebts>,
        model_debts: model_calls::ModelCallDebts,
        commit: F,
    ) -> Result<ActorCommit, DurableError>
    where
        F: FnOnce(ActorTx) -> Fut,
        Fut: std::future::Future<Output = Result<ActorCommit, DurableError>>,
    {
        let calls = tx
            .domain()
            .iter()
            .filter_map(|write| match write {
                lash_durable::DomainWrite::Turn(
                    lash_durable::domain::TurnWrite::AddToolCalls { calls, .. },
                ) => Some(*calls),
                _ => None,
            })
            .sum::<u64>();
        // Preparing an environment installs its cleanup owner before package
        // holds, but publishes no head or phase. Retrying work that failed
        // before publication can prepare again without advancing the run.
        let preparation_only = tx.release().is_none()
            && tx.ack().is_none()
            && tx.graph().is_empty()
            && tx.root_leases().is_empty()
            && tx.released_root_leases().is_empty()
            && tx.formats().is_none()
            && tx.requires().is_none()
            && !tx.domain().is_empty()
            && tx.domain().iter().all(|write| {
                matches!(
                    write,
                    lash_durable::DomainWrite::Environment(
                        lash_durable::environment::EnvironmentWrite::Prepare { .. }
                    )
                )
            });
        let committed = self.commit_with_obligations(tx, owing, commit).await?;
        self.inner
            .acknowledged_commits
            .fetch_add(1, Ordering::Relaxed);
        if !preparation_only {
            self.inner
                .acknowledged_progress
                .fetch_add(1, Ordering::Relaxed);
        }
        self.ack_model_calls(model_debts);
        self.inner
            .tool_call_admissions
            .fetch_add(calls, Ordering::Relaxed);
        Ok(committed)
    }

    /// Commit `tx` through `commit`; a claimed session's
    /// commit carries the claim requirements its records leave it owing
    /// (S7), and its commits are serialized. `owing` states what a root
    /// `tx` publishes owes, as its writer staged it.
    async fn commit_with_obligations<F, Fut>(
        &self,
        tx: ActorTx,
        owing: Option<&super::claim_obligations::ExecutionDebts>,
        commit: F,
    ) -> Result<ActorCommit, DurableError>
    where
        F: FnOnce(ActorTx) -> Fut,
        Fut: std::future::Future<Output = Result<ActorCommit, DurableError>>,
    {
        let Some(obligations) = self.inner.obligations.as_ref() else {
            return commit(tx).await;
        };
        let graph_reads = self.backend().config().settings().graph_reads;
        let limits = graph_reads.execution();
        let phase_limits = graph_reads.owner();
        let reads = self.durable_reads()?;
        let heads = self.backend().session_store_factory();
        let staged = super::claim_obligations::Staged {
            epoch: self.inner.epoch,
            owing,
            limits,
            phase_limits,
        };
        // Boxed: the ledger's read and commit would otherwise inline into
        // every caller's future.
        Box::pin(obligations.commit(
            |session, scope| {
                let heads = Arc::clone(&heads);
                async move {
                    use super::claim_obligations::{ReadScope, RecordedWork};
                    match scope {
                        ReadScope::Turn => {
                            RecordedWork::read_turn(reads, &session, limits, phase_limits).await
                        }
                        ReadScope::Head => RecordedWork::read_head(heads.as_ref(), &session).await,
                    }
                }
            },
            staged,
            tx,
            commit,
        ))
        .await
    }

    fn claimed_drain(&self) -> Result<&lash_durable::runner::Drain, DurableError> {
        self.inner.drain.as_ref().ok_or_else(|| {
            DurableError::Store(lash_durable::StoreFailure {
                kind: lash_durable::StoreFailureKind::Unavailable,
                message: "a context that owns no claim has no drain".to_owned(),
            })
        })
    }

    /// Whether the claiming node still holds its lease by its own clock:
    /// its self-stop deadline is ahead. Past it the node may already be
    /// reaped and the actor owned elsewhere, even though a commit it sent
    /// before was acknowledged, so an owner starts no body once this is
    /// false (ADR 0132 §3). A context that owns no claim holds no node
    /// lease and answers true.
    #[must_use]
    pub fn lease_held(&self) -> bool {
        self.inner
            .liveness
            .as_ref()
            .is_none_or(lash_durable::runner::Liveness::held)
    }

    /// Cancelled when the activation must stop.
    #[must_use]
    pub fn cancel(&self) -> &CancellationToken {
        &self.inner.cancel
    }

    /// Where owners report what hidden replay would make them do.
    #[must_use]
    pub fn probe(&self) -> &dyn DurableProbe {
        &*self.inner.probe
    }

    pub(crate) fn retain_model_attempt(&self, attempt: crate::EffectAttempt) {
        use lash_protocol_types::sync::MutexExt as _;
        self.inner.model_attempts.lock_recover().push(attempt);
    }

    pub(crate) fn retain_owned_model_task(&self, task: crate::task::JoinHandle<()>) {
        use lash_protocol_types::sync::MutexExt as _;
        self.inner.model_tasks.lock_recover().push(task);
    }

    /// Retain a command's sealed call until the command's existing commit.
    pub fn record_owned_model_call(&self, call: lash_durable::domain::model_calls::OwnedModelCall) {
        use lash_protocol_types::sync::MutexExt as _;
        let mut calls = self.inner.model_calls.lock_recover();
        if !calls.iter().any(|stored| stored.key == call.key) {
            calls.push(call);
        }
    }

    /// Acknowledged progress of this activation, used to bound immediate
    /// conflict recovery. Reads and idle passes do not reset that budget.
    #[doc(hidden)]
    pub fn acknowledged_commits(&self) -> u64 {
        self.inner.acknowledged_commits.load(Ordering::Relaxed)
    }

    /// Acknowledged forward progress for the session's failed-pass budget.
    /// Environment preparations alone persist cleanup authority, not a new
    /// head or phase, and cannot reset that budget. Publications and commits
    /// carrying other work still advance it.
    #[doc(hidden)]
    pub fn acknowledged_progress(&self) -> u64 {
        self.inner.acknowledged_progress.load(Ordering::Relaxed)
    }

    /// Discard claim work derived from a refused attempt before reloading.
    #[doc(hidden)]
    pub async fn discard_conflicted_attempt(&self) {
        self.invalidate_session_projection().await;
        self.invalidate_process_projection().await;
    }

    /// Open a fenced owner transaction over the actor.
    ///
    /// # Errors
    ///
    /// [`DurableError::OwnershipLost`] once the actor is someone else's; a
    /// store failure; unavailable for [`Self::unavailable`].
    pub async fn begin(&self) -> Result<ActorTx, DurableError> {
        if self.actor().kind() == lash_durable::ActorKind::Process {
            return self.open_process_projection().await.map(|(tx, _)| tx);
        }
        let revision = self
            .inner
            .session_projection
            .lock()
            .await?
            .as_ref()
            .map(super::owner_state::OwnerState::revision)
            .transpose()?;
        let mut tx = match revision {
            Some(expected) => {
                self.probe().owner_read(
                    self.actor(),
                    self.epoch(),
                    lash_durable::OwnerRead::ExternalFrontierReads,
                    1,
                );
                self.durable()?
                    .begin_retained(
                        self.actor(),
                        self.epoch(),
                        expected,
                        self.backend()
                            .config()
                            .settings()
                            .graph_reads
                            .owner()
                            .total_bytes,
                    )
                    .await
            }
            None => self.durable()?.begin(self.actor(), self.epoch()).await,
        }?;
        if let Some(boundary) = &self.first_commit {
            boundary.stage(&mut tx)?;
        }
        Ok(tx)
    }

    /// Plan and commit an independent append under one synchronization owner.
    /// The closure adds facts that do not depend on an earlier mutable view;
    /// it must not copy a prepared actor draft. No external callback or effect
    /// body runs while this guard is held. Dropping the future releases its
    /// place in the queue.
    ///
    /// # Errors
    ///
    /// The open or commit refusal, including a typed revision conflict from
    /// any writer outside this queue. A draft is never re-fenced or retried.
    #[doc(hidden)]
    pub async fn commit_fresh(
        &self,
        label: CommitLabel,
        write: impl FnOnce(&mut ActorTx),
    ) -> Result<ActorCommit, DurableError> {
        let _guard = self.inner.fresh_updates.lock().await;
        let mut tx = self.begin().await?;
        write(&mut tx);
        self.commit(tx, label).await
    }

    /// Commit `tx` under `label`.
    ///
    /// # Errors
    ///
    /// The store's refusal: [`DurableError::OwnershipLost`] once the actor is
    /// someone else's, [`DurableError::Domain`] for a refused domain write.
    pub async fn commit(
        &self,
        tx: ActorTx,
        label: CommitLabel,
    ) -> Result<ActorCommit, DurableError> {
        let durable = self.durable()?;
        self.with_obligations(tx, label, None, None, |tx| durable.commit(tx, label))
            .await
    }

    /// Commit `tx` under `label`, which publishes an execution's root that
    /// owes `owing`: a claimed session's requirements take the root's debts
    /// as its writer states them rather than decoding the root again. A
    /// cell executor's hook, not host surface.
    ///
    /// # Errors
    ///
    /// As [`Self::commit`].
    #[doc(hidden)]
    pub async fn commit_owing(
        &self,
        tx: ActorTx,
        label: CommitLabel,
        owing: &super::claim_obligations::ExecutionDebts,
    ) -> Result<ActorCommit, DurableError> {
        let durable = self.durable()?;
        self.with_obligations(tx, label, Some(owing), None, |tx| durable.commit(tx, label))
            .await
    }

    /// Commit `tx` with `candidate` under `label`, owing `owing`: the
    /// candidate's heap pages, its owner write and the transaction's own
    /// rows in one fenced transaction (S6), through this context's store's
    /// page publication.
    ///
    /// # Errors
    ///
    /// The store's refusal; nothing was written. [`DurableError::AckLost`]
    /// leaves the publication ambiguous: the caller resolves it against
    /// the owner's row.
    pub async fn commit_candidate_owing(
        &self,
        tx: ActorTx,
        candidate: lash_durable::domain::pages::CandidatePublication,
        label: CommitLabel,
        owing: Option<&super::claim_obligations::ExecutionDebts>,
    ) -> Result<ActorCommit, DurableError> {
        let pages = self.pages()?;
        self.with_obligations(tx, label, owing, None, |tx| async move {
            pages
                .commit_candidate(tx, candidate, label)
                .await
                .map(|receipt| receipt.actor)
        })
        .await
    }

    /// [`Self::commit_candidate_owing`] owing nothing, with the claimed
    /// actor given up as `release`: a draining activation's phase commit
    /// that publishes a candidate and releases the actor itself, as
    /// [`Self::commit_draining`] does.
    ///
    /// # Errors
    ///
    /// As [`Self::commit_candidate_owing`]; unavailable for a context that
    /// owns no claim.
    pub async fn commit_candidate_draining(
        &self,
        tx: ActorTx,
        candidate: lash_durable::domain::pages::CandidatePublication,
        label: CommitLabel,
        release: lash_durable::Release,
    ) -> Result<ActorCommit, DurableError> {
        let drain = self.claimed_drain()?;
        let pages = self.pages()?;
        self.with_obligations(tx, label, None, None, |tx| async move {
            drain
                .release_candidate_with(pages.as_ref(), tx, candidate, label, release)
                .await
        })
        .await
    }

    /// This context's store's heap page publication (S6).
    ///
    /// # Errors
    ///
    /// Unavailable for a store that holds no heap pages.
    pub fn pages(&self) -> Result<Arc<dyn lash_durable::domain::pages::PageStore>, DurableError> {
        self.durable()?.pages().ok_or_else(|| {
            DurableError::Store(lash_durable::StoreFailure {
                kind: lash_durable::StoreFailureKind::Unavailable,
                message: "the store publishes no heap pages".to_owned(),
            })
        })
    }

    /// Commit the mailbox transaction `tx` under `label` through this
    /// context's store (its node's, under a claim), then hint every actor it
    /// woke, as [`Backend::commit_mail`] does.
    ///
    /// # Errors
    ///
    /// The store's refusal; nothing was written. Unavailable for
    /// [`Self::unavailable`].
    pub async fn commit_mail(
        &self,
        tx: lash_durable::MailTx,
        label: CommitLabel,
    ) -> Result<lash_durable::MailCommit, DurableError> {
        let commit = self.durable()?.commit_mail(tx, label).await?;
        if let Some(backend) = &self.inner.backend {
            backend.hint_woken(&commit);
        }
        Ok(commit)
    }

    /// Note that `source` is due at `at`. A release as `waiting` records the
    /// earliest due time of every source.
    pub fn note_due(&self, source: DueSource, at: DurableInstant) {
        self.inner.dues.note(source, at);
    }

    /// Forget `source`'s due time: what it waited for happened.
    pub fn clear_due(&self, source: DueSource) {
        self.inner.dues.clear(source);
    }

    /// The earliest due time noted: what a release as `waiting` records.
    #[must_use]
    pub fn next_due(&self) -> Option<DurableInstant> {
        self.inner.dues.next()
    }

    /// The backend the actor runs over.
    ///
    /// # Panics
    ///
    /// On [`Self::unavailable`], which has none.
    #[expect(
        clippy::expect_used,
        reason = "only the testing context is unavailable; its panic is documented"
    )]
    #[must_use]
    pub fn backend(&self) -> &Backend {
        self.inner
            .backend
            .as_ref()
            .expect("an unavailable ActorContext has no backend")
    }

    /// This context executing under `admitted` instead, with fresh
    /// per-scope ordinals: the same actor, epoch, due times and cancel token.
    ///
    /// # Errors
    ///
    /// The scope's validation refusal.
    pub fn scoped(&self, admitted: AdmittedScope) -> Result<Self, RuntimeError> {
        admitted.scope().validate()?;
        Ok(Self {
            idle_poll: self.idle_poll,
            first_commit: self.first_commit.clone(),
            inner: Arc::clone(&self.inner),
            scope: Scope::new(admitted),
        })
    }

    /// This context bound to another admitted scope within the same work:
    /// its frontier and trace scope carry over.
    ///
    /// A rescope changes the scope, never the admitted process: the only
    /// process target it accepts is the process this context was already
    /// admitted for.
    ///
    /// # Errors
    ///
    /// `ExecutionScopeAdmissionRefused` for another process, or the scope's
    /// validation refusal.
    pub fn rescope(&self, admitted: AdmittedScope) -> Result<Self, RuntimeError> {
        if let Some(target) = admitted.process_id()
            && self.scope.admitted.process_id() != Some(target)
        {
            return Err(RuntimeError::new(
                crate::RuntimeErrorCode::ExecutionScopeAdmissionRefused,
                format!(
                    "cannot rescope {existing} onto process {target}: a context carries its admission and is never rebound",
                    existing = self.scope.admitted.scope().id(),
                ),
            ));
        }
        admitted.scope().validate()?;
        let mut next = Scope::new(admitted);
        let fresh = Arc::get_mut(&mut next).map(|scope| {
            scope.frontier = Arc::clone(&self.scope.frontier);
            scope.trace_scope = self.scope.trace_scope.clone();
        });
        debug_assert!(fresh.is_some());
        Ok(Self {
            idle_poll: self.idle_poll,
            first_commit: self.first_commit.clone(),
            inner: Arc::clone(&self.inner),
            scope: next,
        })
    }

    /// The admitted scope it executes under.
    #[must_use]
    pub fn admitted_scope(&self) -> &AdmittedScope {
        &self.scope.admitted
    }

    /// The execution scope it executes under.
    #[must_use]
    pub fn execution_scope(&self) -> &ExecutionScope {
        self.scope.admitted.scope()
    }

    /// The scope's id.
    #[must_use]
    pub fn scope_id(&self) -> &str {
        self.scope.admitted.scope().id()
    }

    /// The turn of a turn scope.
    #[must_use]
    pub fn turn_id(&self) -> Option<&TurnId> {
        self.scope.admitted.scope().turn_id()
    }

    /// The process a process scope is.
    #[must_use]
    pub fn admitted_process(&self) -> Option<&lash_vocabulary::identity::ProcessId> {
        self.scope.admitted.process_id()
    }

    /// This context with `scope` as the trace scope its work descends from.
    #[must_use]
    pub fn with_trace_scope(&self, scope: lash_trace::DurableTraceScope) -> Self {
        let mut next = self.scope.with();
        next.trace_scope = Some(Arc::new(scope));
        Self {
            idle_poll: self.idle_poll,
            first_commit: self.first_commit.clone(),
            inner: Arc::clone(&self.inner),
            scope: Arc::new(next),
        }
    }

    /// The trace scope its work descends from.
    #[must_use]
    pub fn trace_scope(&self) -> Option<&lash_trace::DurableTraceScope> {
        self.scope.trace_scope.as_deref()
    }

    /// This context bound to the physical turn of its admitted Run: waits
    /// built from it race that turn's cancellation gate.
    #[must_use]
    pub fn for_physical_turn(&self, turn: TurnId) -> Self {
        let mut next = self.scope.with();
        next.physical_turn = Some(Arc::new(turn));
        Self {
            idle_poll: self.idle_poll,
            first_commit: self.first_commit.clone(),
            inner: Arc::clone(&self.inner),
            scope: Arc::new(next),
        }
    }

    /// The turn-cancel wait for a wait built directly against this scope:
    /// always observing.
    pub(crate) fn turn_cancel_wait(
        &self,
        cancellation: CancellationToken,
    ) -> crate::TurnCancelWait {
        crate::TurnCancelWait::observing(cancellation, self.turn_cancel_scope())
    }

    /// The scope whose turn-cancel gate its waits race: the bound physical
    /// turn of a turn scope, otherwise the admitted scope.
    fn turn_cancel_scope(&self) -> ExecutionScope {
        match (
            self.scope.physical_turn.as_ref(),
            self.scope.admitted.scope(),
        ) {
            (Some(turn), ExecutionScope::Turn { session_id, .. }) => {
                ExecutionScope::turn(session_id.clone(), TurnId::clone(turn))
            }
            (_, scope) => scope.clone(),
        }
    }

    /// The start key of the next keyless host start under this scope: the
    /// admitted scope and the start's ordinal among its keyless starts
    /// (ADR 0107).
    pub(crate) fn next_keyless_start_key(&self) -> crate::StartKey {
        let ordinal = self
            .scope
            .ordinals
            .keyless_starts
            .fetch_add(1, Ordering::SeqCst);
        crate::StartKeyDerivation::LASH_START_PATHS
            .for_keyless_host(self.scope.admitted.scope(), ordinal)
    }

    /// The ordinal of the next administrative compaction under this scope.
    pub fn next_compaction_ordinal(&self) -> u32 {
        self.scope
            .ordinals
            .compactions
            .fetch_add(1, Ordering::SeqCst)
    }

    /// The ordinal of the next unkeyed direct completion under this scope:
    /// its identity within the scope, in program order, so a redrive of the
    /// scope names each call as its first execution did.
    pub fn next_completion_ordinal(&self) -> u32 {
        self.scope
            .ordinals
            .completions
            .fetch_add(1, Ordering::SeqCst)
    }

    /// The ordinal of the next read of the session's command lane under
    /// this scope.
    pub fn next_command_run_ordinal(&self) -> u32 {
        self.scope
            .ordinals
            .command_runs
            .fetch_add(1, Ordering::SeqCst)
    }

    fn durable(&self) -> Result<&Arc<dyn DurableStore>, DurableError> {
        if let Some(store) = &self.inner.durable {
            return Ok(store);
        }
        self.inner
            .backend
            .as_ref()
            .map(Backend::durable)
            .ok_or_else(|| {
                DurableError::Store(lash_durable::StoreFailure {
                    kind: lash_durable::StoreFailureKind::Unavailable,
                    message: "this ActorContext has no durable store".to_owned(),
                })
            })
    }
}

#[path = "core_process.rs"]
mod process_projection;

#[path = "model_calls.rs"]
mod model_calls;

#[cfg(any(test, feature = "testing"))]
fn active_turn_postimage(turn: &lash_durable::domain::TurnRow) -> serde_json::Value {
    use lash_durable::domain::{ModelCall, UnfinishedPhase};
    let phase = match &turn.phase {
        UnfinishedPhase::Admitted => serde_json::json!({"kind":"admitted"}),
        UnfinishedPhase::Tools { run, checkpoint } => {
            serde_json::json!({"kind":"tools", "run":run.0,"checkpoint_ref":checkpoint.to_hex()})
        }
        UnfinishedPhase::Model {
            pin,
            checkpoint,
            call,
        } => serde_json::json!({
            "kind":"model", "call":pin.call,"attempt":pin.attempt,
            "request_ref":pin.request_ref,"deadline":pin.deadline.0,"stream_from":pin.stream_from,
            "checkpoint_ref":checkpoint.to_hex(),
            "answer_ref":match call { ModelCall::Requested => None, ModelCall::Answered {receipt} => Some(receipt.to_hex()) },
        }),
    };
    serde_json::json!({"session":turn.session,"run":turn.run,
    "admission":turn.admission,"phase":phase,
    "working_ref":turn.working.map(|node|node.to_hex()),
    "iteration":turn.iteration,"model_calls":turn.model_calls,
    "tool_calls":turn.tool_calls,"turn_deadline":turn.turn_deadline.map(|deadline|deadline.0),
    "written_epoch":turn.written_epoch.0,"cancel":turn.cancel.as_ref().map(|cancel| serde_json::json!({
        "session":cancel.session, "run":cancel.run, "request_id":cancel.request_id,
        "origin":cancel.origin, "reason":cancel.reason,
        "undelivered":cancel.undelivered, "mode":cancel.mode,
    }))})
}

#[cfg(test)]
#[path = "core_release_tests.rs"]
mod release_tests;
