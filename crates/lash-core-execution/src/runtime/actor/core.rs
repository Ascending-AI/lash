//! The context's core: the owned actor, its fenced transactions, the clock,
//! the cancel token, the replay probe and the due times.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use lash_durable::{
    ActorCommit, ActorKey, ActorTx, CommitLabel, DueSource, Dues, DurableError, DurableInstant,
    DurableProbe, DurableStore, Epoch,
};
use tokio_util::sync::CancellationToken;

use crate::{AdmittedScope, Backend, Clock, ExecutionScope, RuntimeError, TurnId};

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

pub(super) struct Inner {
    backend: Option<Backend>,
    /// The node's store a claimed context commits through; the backend's
    /// own otherwise.
    durable: Option<Arc<dyn DurableStore>>,
    actor: ActorKey,
    epoch: Epoch,
    /// What the claim's activation waits on for mail; a context that owns
    /// no claim polls on the backend's claim interval.
    mail: Option<lash_durable::runner::MailWaker>,
    /// The claiming node's drain switch; a context that owns no claim
    /// never drains.
    drain: Option<lash_durable::runner::Drain>,
    /// The claiming node's lease as its own clock sees it; a context that
    /// owns no claim holds no node lease.
    liveness: Option<lash_durable::runner::Liveness>,
    clock: Arc<dyn Clock>,
    cancel: CancellationToken,
    probe: Arc<dyn DurableProbe>,
    dues: Dues,
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

impl ActorContext {
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
            inner: Arc::new(Inner {
                backend: Some(backend),
                durable: None,
                actor,
                epoch,
                mail: None,
                drain: None,
                liveness: None,
                clock,
                cancel,
                probe,
                dues: Dues::new(),
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
            inner: Arc::new(Inner {
                backend: Some(backend),
                durable: Some(Arc::clone(owned.store())),
                actor: owned.actor().clone(),
                epoch: owned.epoch(),
                mail: Some(owned.mail_waker()),
                drain: Some(owned.drain().clone()),
                liveness: Some(owned.liveness().clone()),
                clock: Arc::clone(owned.clock()),
                cancel,
                probe,
                dues: Dues::new(),
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
            inner: Arc::new(Inner {
                backend: None,
                durable: None,
                actor: ActorKey::session("unavailable").expect("a constant actor id"),
                epoch: Epoch(0),
                mail: None,
                drain: None,
                liveness: None,
                clock: Arc::new(crate::SystemClock),
                cancel: CancellationToken::new(),
                probe: Arc::new(lash_durable::NoProbe),
                dues: Dues::new(),
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
            Some(mail) => mail.wait().await,
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
        let live = self.durable()?.live_decodes().await?;
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
        let Some(drain) = &self.inner.drain else {
            return Err(DurableError::Store(lash_durable::StoreFailure {
                kind: lash_durable::StoreFailureKind::Unavailable,
                message: "a context that owns no claim has no drain".to_owned(),
            }));
        };
        let tx = self.begin().await?;
        drain.release(self.durable()?.as_ref(), tx).await
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

    /// Open a fenced owner transaction over the actor.
    ///
    /// # Errors
    ///
    /// [`DurableError::OwnershipLost`] once the actor is someone else's; a
    /// store failure; unavailable for [`Self::unavailable`].
    pub async fn begin(&self) -> Result<ActorTx, DurableError> {
        self.durable()?
            .begin(&self.inner.actor, self.inner.epoch)
            .await
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
        self.durable()?.commit(tx, label).await
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

    /// The backend's projection providers (ADR 0132 §9); none on
    /// [`Self::unavailable`], which has no backend.
    #[must_use]
    pub fn projection_providers(&self) -> Option<&Arc<dyn super::projection::ProjectionProviders>> {
        self.inner
            .backend
            .as_ref()
            .map(Backend::projection_providers)
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
    pub fn admitted_process(&self) -> Option<&crate::ProcessId> {
        self.scope.admitted.process_id()
    }

    /// This context with `scope` as the trace scope its work descends from.
    #[must_use]
    pub fn with_trace_scope(&self, scope: lash_trace::DurableTraceScope) -> Self {
        let mut next = self.scope.with();
        next.trace_scope = Some(Arc::new(scope));
        Self {
            idle_poll: self.idle_poll,
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
