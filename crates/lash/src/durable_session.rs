//! The **Durable Session**: a session's queue and settled reads, answered from
//! its store.
//!
//! Lash has two session authorities. The *live session*
//! ([`LashSession`](crate::LashSession)) needs a core able to run this session:
//! opening one materialises plugins, restores the tool registry and protocol
//! state, emits `SessionRestored`, and admits the session's processes. The
//! *Durable Session* needs only the session's store, and every operation it
//! owns stays correct while another process holds the Session Execution Lease.
//!
//! Reach one of three ways:
//!
//! * [`SessionBuilder::durable`](crate::SessionBuilder::durable) —
//!   `core.session(id).durable().await` — acquires the store through the
//!   catalog's non-creating seam.
//! * [`SessionBuilder::create`](crate::SessionBuilder::create) —
//!   `core.session(id).create().await` — writes the session's catalog entry
//!   first, then hands back this handle. The only verb that creates.
//! * [`LashSession::durable`](crate::LashSession::durable) — the open
//!   session's own Session Binding, reusing its owner-issued store and ports.
//!
//! Both produce this one type with one body per operation, so a queue mutation
//! behaves identically whichever handle issued it.
//!
//! # Acquisition never creates
//!
//! A Durable Session resolves an *existing* store: the catalog seam is
//! [`SessionStoreFactory::open_existing_store_by_id`](lash_core::SessionStoreFactory::open_existing_store_by_id),
//! never `create_store`. Resolution happens at most once per handle (shared by
//! its clones) and is reused afterwards. Every queue operation therefore
//! requires a session id the store already knows: enqueueing to an id that was
//! never created fails with [`EmbedError::UnknownSession`], and to a deleted
//! one with [`StoreError::SessionDeleted`](lash_core::StoreError::SessionDeleted).
//! Nothing is stored and no driver is woken in either case. Create the session
//! first — `core.session(id).create()`, or `open()` if a runtime is wanted
//! anyway — then enqueue.
//!
//! A catalog that cannot resolve a session by id at all is a different answer
//! from a session that is not there: it surfaces as
//! [`EmbedError::StoreFactory`] carrying the implementor's reason, never as
//! [`EmbedError::UnknownSession`], so a host is not sent looking for a session
//! that exists.
//!
//! The three settled reads are the exception in *reporting*, not in authority:
//! [`exists`](DurableSession::exists), [`was_deleted`](DurableSession::was_deleted)
//! and [`read`](DurableSession::read) exist to answer *about* an id, so an
//! unknown id is their answer (`false`, `false`, `None`), not an error. They
//! write nothing either way.
//!
//! # Observation
//!
//! Queue mutations publish `QueueChanged` through the core's Live Replay
//! publisher at the committed-head revision, best-effort and only after
//! durable success; a publication failure never fails the mutation. There is
//! no separate hub, so cross-process visibility is exactly the property of the
//! configured Live Replay store — in-memory, and therefore process-local, by
//! default.

use crate::support::{
    Arc, EmbedError, QueuedWorkSubstrate, Result, RuntimePersistence, SessionStoreFactory,
    TurnInput,
};
use lash_core::LiveReplayStore;
use lash_core::facade_support::DurableSessionOps;
use lash_core::runtime::{
    PendingTurnInputCancelOutcome, PendingTurnInputCancelReceipt, PendingTurnInputCancelTarget,
    PendingTurnInputRead, PendingTurnInputSuffixCancelOutcome, QueuedWorkBatch, QueuedWorkClaim,
    TurnInputAcceptanceReceipt, TurnInputClaim, TurnInputIngress,
};
use lash_sansio::SessionId;
use tokio::sync::OnceCell;

/// How this handle obtains the session's store.
#[derive(Clone)]
enum DurableAcquisition {
    /// Resolve through the catalog's non-creating seam.
    Catalog(Arc<dyn SessionStoreFactory>),
    /// A host-supplied exact store; existence is still proven before use.
    Exact(Arc<dyn RuntimePersistence>),
    /// An open session's owner-issued store; the open already proved existence.
    Bound(Arc<dyn RuntimePersistence>),
}

/// Store-backed access to one session's durable queue and settled reads.
///
/// See the [module documentation](self) for the two authorities, the
/// non-creating acquisition rule and the observation contract.
#[derive(Clone)]
pub struct DurableSession {
    session_id: SessionId,
    ops: DurableSessionOps,
    acquisition: DurableAcquisition,
    catalog: Option<Arc<dyn SessionStoreFactory>>,
    /// Shared by every clone so concurrent operations acquire the store once.
    store: Arc<OnceCell<Arc<dyn RuntimePersistence>>>,
}

impl DurableSession {
    pub(crate) fn from_catalog(
        session_id: SessionId,
        catalog: Arc<dyn SessionStoreFactory>,
        queued: Arc<dyn QueuedWorkSubstrate>,
        live_replay_store: Arc<dyn LiveReplayStore>,
    ) -> Self {
        Self {
            ops: DurableSessionOps::new(session_id.clone(), queued, live_replay_store),
            acquisition: DurableAcquisition::Catalog(Arc::clone(&catalog)),
            catalog: Some(catalog),
            store: Arc::new(OnceCell::new()),
            session_id,
        }
    }

    pub(crate) fn from_exact_store(
        session_id: SessionId,
        store: Arc<dyn RuntimePersistence>,
        queued: Arc<dyn QueuedWorkSubstrate>,
        live_replay_store: Arc<dyn LiveReplayStore>,
        catalog: Option<Arc<dyn SessionStoreFactory>>,
    ) -> Self {
        Self {
            ops: DurableSessionOps::new(session_id.clone(), queued, live_replay_store),
            acquisition: DurableAcquisition::Exact(store),
            catalog,
            store: Arc::new(OnceCell::new()),
            session_id,
        }
    }

    /// Build the handle an open session exposes, from its Session Binding.
    ///
    /// The binding's store and ports are reused as-is: an exact binding never
    /// manufactures a catalog, so catalog-only reads stay optional here with
    /// the same typed error a root opened with an explicit store already
    /// returns.
    pub(crate) fn from_binding(
        session_id: SessionId,
        store: Arc<dyn RuntimePersistence>,
        queued: Arc<dyn QueuedWorkSubstrate>,
        live_replay_store: Arc<dyn LiveReplayStore>,
        catalog: Option<Arc<dyn SessionStoreFactory>>,
    ) -> Self {
        Self {
            ops: DurableSessionOps::new(session_id.clone(), queued, live_replay_store),
            acquisition: DurableAcquisition::Bound(store),
            catalog,
            store: Arc::new(OnceCell::new()),
            session_id,
        }
    }

    /// The session this handle is bound to.
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// The acquired store, resolving it once on first use.
    async fn store(&self) -> Result<&Arc<dyn RuntimePersistence>> {
        self.store.get_or_try_init(|| self.acquire()).await
    }

    async fn acquire(&self) -> Result<Arc<dyn RuntimePersistence>> {
        let resolved = match &self.acquisition {
            DurableAcquisition::Bound(store) => return Ok(Arc::clone(store)),
            DurableAcquisition::Exact(store) => store
                .load_session_meta()
                .await
                .map_err(EmbedError::Store)?
                .map(|_| Arc::clone(store)),
            // The seam's two negative answers are kept apart: `Err` is a
            // catalog that cannot resolve by id, and surfaces as
            // `StoreFactory` naming that capability; only `Ok(None)` below
            // becomes "no such session". The method is required on the trait
            // precisely so an implementor cannot inherit the second answer
            // while meaning the first.
            DurableAcquisition::Catalog(catalog) => catalog
                .open_existing_store_by_id(&self.session_id)
                .await
                .map_err(|message| EmbedError::StoreFactory {
                    session_id: self.session_id.clone(),
                    message,
                })?,
        };
        match resolved {
            Some(store) => Ok(store),
            None => Err(self.absent_session_error().await),
        }
    }

    /// Distinguish "never created" from "used and deleted" for a caller whose
    /// acquisition found no store.
    async fn absent_session_error(&self) -> EmbedError {
        if let Some(catalog) = self.catalog.as_ref() {
            match catalog.session_was_deleted(&self.session_id).await {
                Ok(true) => {
                    return EmbedError::Store(lash_core::StoreError::SessionDeleted {
                        session_id: self.session_id.clone(),
                    });
                }
                Ok(false) => {}
                Err(message) => {
                    return EmbedError::StoreFactory {
                        session_id: self.session_id.clone(),
                        message,
                    };
                }
            }
        }
        EmbedError::UnknownSession {
            session_id: self.session_id.clone(),
        }
    }

    /// Resolve the store without turning absence into an error. Used only by
    /// the settled reads, whose job is to report absence.
    async fn store_if_present(&self) -> Result<Option<&Arc<dyn RuntimePersistence>>> {
        match self.store().await {
            Ok(store) => Ok(Some(store)),
            Err(EmbedError::UnknownSession { .. })
            | Err(EmbedError::Store(lash_core::StoreError::SessionDeleted { .. })) => Ok(None),
            Err(err) => Err(err),
        }
    }

    fn catalog(&self, operation: &'static str) -> Result<&Arc<dyn SessionStoreFactory>> {
        self.catalog
            .as_ref()
            .ok_or(EmbedError::SessionCatalogUnavailable { operation })
    }

    /// Creates a builder for durably enqueueing turn input.
    ///
    /// The session id must already be known to the store; see the
    /// [module documentation](self).
    pub fn enqueue(&self, input: TurnInput) -> EnqueueTurnBuilder {
        EnqueueTurnBuilder {
            durable: self.clone(),
            input,
            id: None,
            ingress: TurnInputIngress::NextTurn,
        }
    }

    /// Returns every open turn input and its factual read-time claim status.
    ///
    /// A held input remains present with the exact expiry of the matching live
    /// session-execution lease. That status does not prove the holder is alive;
    /// resubmitting while it is held creates another admission unless the host
    /// reuses the same source key.
    pub async fn pending_turn_inputs(&self) -> Result<Vec<PendingTurnInputRead>> {
        let store = self.store().await?;
        Ok(self.ops.pending_turn_inputs(store).await?)
    }

    /// Read settled canonical input applications from durable turn commits.
    ///
    /// This is the reconciliation surface for hosts whose live observation
    /// cursor fell outside the bounded replay window.
    pub async fn turn_input_applications(&self) -> Result<Vec<lash_core::TurnInputApplication>> {
        let store = self.store().await?;
        Ok(self.ops.turn_input_applications(store).await?)
    }

    /// Versioned wire form of [`turn_input_applications`](Self::turn_input_applications).
    pub async fn remote_turn_input_applications(
        &self,
    ) -> Result<Vec<lash_remote_protocol::RemoteTurnInputApplication>> {
        Ok(self
            .turn_input_applications()
            .await?
            .iter()
            .map(Into::into)
            .collect())
    }

    /// Return all pending durable queued-work batches for this session.
    ///
    /// This is an admin/introspection view for non-user queued work such as
    /// process wakes and session commands. User-visible model input is stored
    /// separately as pending turn input and is exposed by
    /// [`pending_turn_inputs`](Self::pending_turn_inputs).
    pub async fn queued_work(&self) -> Result<Vec<QueuedWorkBatch>> {
        let store = self.store().await?;
        Ok(self.ops.queued_work(store).await?)
    }

    /// Cancels pending turn input.
    pub async fn cancel_pending_turn_input(
        &self,
        input_id: &lash_core::InputId,
    ) -> Result<PendingTurnInputCancelOutcome> {
        let store = self.store().await?;
        Ok(self.ops.cancel_pending_turn_input(store, input_id).await?)
    }

    /// Atomically cancel a set of pending user inputs by runtime input id or
    /// app source key.
    ///
    /// This is the app reconciliation path for explicit selections such as
    /// "remove these pending drafts". Returned outcomes distinguish newly
    /// cancelled input from input that was already claimed, completed,
    /// cancelled, or missing.
    pub async fn cancel_pending_turn_inputs(
        &self,
        targets: impl IntoIterator<Item = PendingTurnInputCancelTarget>,
    ) -> Result<Vec<PendingTurnInputCancelReceipt>> {
        let targets = targets.into_iter().collect::<Vec<_>>();
        let store = self.store().await?;
        Ok(self.ops.cancel_pending_turn_inputs(store, &targets).await?)
    }

    /// Atomically cancel the same-session pending-input suffix from `anchor`.
    ///
    /// Apps that let users edit previously submitted product messages should
    /// map the edited message to the stored pending-input `input_id` or
    /// `source_key`, call this method, and only restore/edit drafts that return
    /// [`PendingTurnInputCancelOutcome::Cancelled`]. Claimed or completed
    /// inputs have already crossed the runtime boundary and should be treated
    /// as reconciliation state, not local editable drafts.
    pub async fn cancel_pending_turn_input_suffix(
        &self,
        anchor: PendingTurnInputCancelTarget,
    ) -> Result<PendingTurnInputSuffixCancelOutcome> {
        let store = self.store().await?;
        Ok(self
            .ops
            .cancel_pending_turn_input_suffix(store, &anchor)
            .await?)
    }

    /// Cancels queued work batch.
    pub async fn cancel_queued_work_batch(
        &self,
        batch_id: &lash_core::BatchId,
    ) -> Result<Option<QueuedWorkBatch>> {
        let store = self.store().await?;
        Ok(self.ops.cancel_queued_work_batch(store, batch_id).await?)
    }

    /// Release a held queued-work claim without completing it, returning its
    /// batches to the pending queue immediately.
    ///
    /// Token-authorised release, not interference with a live claimant: the
    /// store matches the claim's `claim_id` *and* `claim_token`, so a
    /// non-holder is refused. A host stopping an external queued-work driver
    /// mid-claim calls this with the claims that driver still holds so the work
    /// becomes claimable again at once instead of waiting out the claim's lease
    /// TTL.
    pub async fn abandon_queued_work_claim(&self, claim: &QueuedWorkClaim) -> Result<()> {
        let store = self.store().await?;
        Ok(self.ops.abandon_queued_work_claim(store, claim).await?)
    }

    /// Release a held pending-turn-input claim without completing it, returning
    /// its inputs to the pending queue immediately. The turn-input counterpart
    /// of [`abandon_queued_work_claim`](Self::abandon_queued_work_claim), with
    /// the same `claim_id`/`claim_token` authorisation.
    pub async fn abandon_turn_input_claim(&self, claim: &TurnInputClaim) -> Result<()> {
        let store = self.store().await?;
        Ok(self.ops.abandon_turn_input_claim(store, claim).await?)
    }

    /// Read the canonical settled view of this durable session without opening
    /// a live runtime, acquiring its execution lease, or exposing mutations.
    ///
    /// This is the inspection path for exporters, debuggers, and administrative
    /// tooling that must coexist with a live writer. `Ok(None)` means the
    /// catalog has no readable committed state for this id; unsupported
    /// backends return
    /// [`StoreError::UnsupportedStoreOperation`](lash_core::StoreError::UnsupportedStoreOperation).
    pub async fn read(&self) -> Result<Option<crate::persistence::SessionReadView>> {
        self.catalog("read")?
            .read_session(&self.session_id)
            .await
            .map_err(EmbedError::Store)
    }

    /// Report whether this session still has durable live session metadata.
    ///
    /// A cheap existence read: it does not create, hydrate, or open the
    /// session. A permanently deleted session returns `false`; callers that try
    /// to recreate the id still receive the store's typed deletion error.
    pub async fn exists(&self) -> Result<bool> {
        let Some(store) = self.store_if_present().await? else {
            return Ok(false);
        };
        Ok(self.ops.session_exists(store).await?)
    }

    /// Report whether the durable single-use tombstone for this session exists.
    ///
    /// A `false` result means only "no tombstone"; it is not evidence that the
    /// session is live. Tombstones are monotonic: once this returns `true`, the
    /// session id cannot become live again. Compose this read with
    /// [`exists`](Self::exists) when deciding live/retired/unknown disposition.
    pub async fn was_deleted(&self) -> Result<bool> {
        self.catalog("was_deleted")?
            .session_was_deleted(&self.session_id)
            .await
            .map_err(|message| EmbedError::StoreFactory {
                session_id: self.session_id.clone(),
                message,
            })
    }
}

/// Builder for configuring enqueue turn.
///
/// The builder owns its [`DurableSession`] (a cheap shared handle), so
/// `session.durable().enqueue(input).send().await` reads as one expression.
pub struct EnqueueTurnBuilder {
    durable: DurableSession,
    input: TurnInput,
    id: Option<String>,
    ingress: TurnInputIngress,
}

impl EnqueueTurnBuilder {
    /// Sets the idempotency identifier for the enqueued input.
    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    /// Sets how the enqueued input enters the turn pipeline.
    pub fn ingress(mut self, ingress: TurnInputIngress) -> Self {
        self.ingress = ingress;
        self
    }

    /// Persist the input and return stable durable-acceptance identity.
    ///
    /// For retryable host requests, supply [`id`](Self::id) again after an
    /// ambiguous transport failure; its source key is the idempotency identity.
    /// Mutable queue lifecycle state is available from
    /// [`DurableSession::pending_turn_inputs`].
    ///
    /// The session must already exist: an unknown or deleted id is refused
    /// without storing anything or waking any driver.
    pub async fn send(self) -> Result<TurnInputAcceptanceReceipt> {
        let source_key = self.id.map(|id| format!("host:{id}"));
        let store = self.durable.store().await?;
        let enqueued = self
            .durable
            .ops
            .enqueue_turn_input(store, self.input, self.ingress, source_key)
            .await?;
        Ok(TurnInputAcceptanceReceipt::from(&enqueued))
    }
}

impl std::future::IntoFuture for EnqueueTurnBuilder {
    type Output = Result<TurnInputAcceptanceReceipt>;
    type IntoFuture = std::pin::Pin<Box<dyn std::future::Future<Output = Self::Output>>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(self.send())
    }
}
