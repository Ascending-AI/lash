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
    clock: Arc<dyn Clock>,
    cancel: CancellationToken,
    probe: Arc<dyn DurableProbe>,
    dues: Dues,
    run_records: crate::trace::RunRecordObserver,
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
            inner: Arc::new(Inner {
                backend: Some(backend),
                durable: None,
                actor,
                epoch,
                clock,
                cancel,
                probe,
                dues: Dues::new(),
                run_records: crate::trace::RunRecordObserver::default(),
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
            inner: Arc::new(Inner {
                backend: Some(backend),
                durable: Some(Arc::clone(owned.store())),
                actor: owned.actor().clone(),
                epoch: owned.epoch(),
                clock: Arc::clone(owned.clock()),
                cancel,
                probe,
                dues: Dues::new(),
                run_records: crate::trace::RunRecordObserver::default(),
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
            inner: Arc::new(Inner {
                backend: None,
                durable: None,
                actor: ActorKey::session("unavailable").expect("a constant actor id"),
                epoch: Epoch(0),
                clock: Arc::new(crate::SystemClock),
                cancel: CancellationToken::new(),
                probe: Arc::new(lash_durable::NoProbe),
                dues: Dues::new(),
                run_records: crate::trace::RunRecordObserver::default(),
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

    /// The node's clock, for live enforcement: a local timer for
    /// `expires_at - now`.
    #[must_use]
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

    /// The scope-bound observer of the Run records this context's tool
    /// rounds record, for their traces.
    #[must_use]
    pub fn run_record_observer(&self) -> &crate::trace::RunRecordObserver {
        &self.inner.run_records
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
