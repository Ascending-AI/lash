//! The **Durable Session**: a session's queue and settled reads, answered from
//! its store.
//!
//! Lash has two session authorities. The *live session*
//! ([`LashSession`](crate::LashSession)) needs a core able to run this session:
//! opening one materialises plugins, restores the tool registry and protocol
//! state, emits `SessionRestored`, and admits the session's processes. The
//! *Durable Session* needs only the session's store, and every operation it
//! owns stays correct while another process runs the session live.
//!
//! Reach one of three ways:
//!
//! * [`SessionBuilder::durable`](crate::SessionBuilder::durable) —
//!   `core.session(id).durable().await` — acquires the store through the
//!   catalog's non-creating seam.
//! * [`SessionBuilder::create`](crate::SessionBuilder::create) —
//!   `core.session(id).create(creation).await` — writes the session's catalog
//!   entry and initial config head first, then hands back this handle. The
//!   only verb that creates.
//! * [`LashSession::durable`](crate::LashSession::durable) — the open
//!   session's own Session Binding, reusing its owner-issued store and ports.
//!
//! Both produce this one type with one body per operation, so a queue mutation
//! behaves identically whichever handle issued it.
//!
//! # Acquisition never creates
//!
//! A Durable Session resolves an *existing* session: the catalog seam is
//! [`SessionCatalogStore::lookup_session`](lash_core::store::SessionCatalogStore::lookup_session),
//! never `admit_session`. Resolution happens at most once per handle (shared by
//! its clones) and is reused afterwards. Every queue operation therefore
//! requires a session id the store already knows: sending to an id that was
//! never created fails with [`EmbedError::UnknownSession`], and to a deleted
//! one with [`StoreError::SessionDeleted`](lash_core::StoreError::SessionDeleted).
//! Nothing is stored and no driver is woken in either case. Create the session
//! first — `core.session(id).create(creation)` — then send.
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

use crate::core::ResolvedQueuedWork;
use crate::support::{Arc, DeploymentStore, EffectHost, EmbedError, Result, TurnInput};
use lash_core::LiveReplayStore;
use lash_core::facade_support::DurableSessionOps;
use lash_core::runtime::{
    PendingTurnInputCancelOutcome, PendingTurnInputCancelReceipt, PendingTurnInputCancelTarget,
    PendingTurnInputRead, PendingTurnInputSuffixCancelOutcome, QueuedWorkBatch,
};
use lash_sansio::SessionId;
use tokio::sync::OnceCell;

/// How this handle obtains the session's store.
#[derive(Clone)]
enum DurableAcquisition {
    /// Resolve an existing session through the catalog's non-creating seam.
    Catalog,
    /// An open or just-created session's store; the open or the creation
    /// already proved existence.
    Bound(lash_core::store::SessionStore),
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
    catalog: Arc<dyn DeploymentStore>,
    /// Shared by every clone so concurrent operations acquire the store once.
    store: Arc<OnceCell<lash_core::store::SessionStore>>,
    /// What a [`send`](Self::send) needs beyond the queue: the engine a
    /// handle waits on, the effect host its terminal reads and cancels go
    /// through, and the live replay its events come from.
    work: Arc<ResolvedQueuedWork>,
    effect_host: Arc<dyn EffectHost>,
    live_replay_store: Arc<dyn LiveReplayStore>,
    /// The resolver a [`send`](Self::send) judges a spec's route against
    /// before the input is accepted (FIG-3877).
    provider_resolver: Arc<dyn lash_core::provider::RuntimeProviderResolver>,
}

impl DurableSession {
    pub(crate) fn from_catalog(
        session_id: SessionId,
        catalog: Arc<dyn DeploymentStore>,
        work: Arc<ResolvedQueuedWork>,
        ingress: lash_core::drive::IngressRelay,
        effect_host: Arc<dyn EffectHost>,
        live_replay_store: Arc<dyn LiveReplayStore>,
        provider_resolver: Arc<dyn lash_core::provider::RuntimeProviderResolver>,
    ) -> Self {
        Self {
            ops: DurableSessionOps::new(
                session_id.clone(),
                ingress,
                Arc::clone(&live_replay_store),
            ),
            acquisition: DurableAcquisition::Catalog,
            catalog,
            store: Arc::new(OnceCell::new()),
            session_id,
            work,
            effect_host,
            live_replay_store,
            provider_resolver,
        }
    }

    /// The binding's store and ports are reused as-is, beside the catalog the
    /// store came from.
    #[allow(
        clippy::too_many_arguments,
        reason = "a binding-derived Durable Session reuses each of the binding's ports as-is"
    )]
    pub(crate) fn from_binding(
        session_id: SessionId,
        store: lash_core::store::SessionStore,
        work: Arc<ResolvedQueuedWork>,
        ingress: lash_core::drive::IngressRelay,
        effect_host: Arc<dyn EffectHost>,
        live_replay_store: Arc<dyn LiveReplayStore>,
        catalog: Arc<dyn DeploymentStore>,
        provider_resolver: Arc<dyn lash_core::provider::RuntimeProviderResolver>,
    ) -> Self {
        Self {
            ops: DurableSessionOps::new(
                session_id.clone(),
                ingress,
                Arc::clone(&live_replay_store),
            ),
            acquisition: DurableAcquisition::Bound(store),
            catalog,
            store: Arc::new(OnceCell::new()),
            session_id,
            work,
            effect_host,
            live_replay_store,
            provider_resolver,
        }
    }

    /// The session's store, its queue operations and its send ports: what a
    /// send handle bound to this Durable Session reads and writes through.
    pub(crate) async fn send_parts(&self) -> Result<crate::send::SendParts> {
        Ok(crate::send::SendParts {
            session_id: self.session_id.clone(),
            store: self.store().await?.clone(),
            ops: self.ops.clone(),
            work: self.work.clone(),
            effect_host: Arc::clone(&self.effect_host),
            live_replay_store: Arc::clone(&self.live_replay_store),
            provider_resolver: Arc::clone(&self.provider_resolver),
        })
    }

    /// The live replay a handle's events come from.
    pub(crate) fn live_replay_store(&self) -> &Arc<dyn LiveReplayStore> {
        &self.live_replay_store
    }

    /// The session this handle is bound to.
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// The acquired store, resolving it once on first use.
    async fn store(&self) -> Result<&lash_core::store::SessionStore> {
        self.store.get_or_try_init(|| self.acquire()).await
    }

    async fn acquire(&self) -> Result<lash_core::store::SessionStore> {
        if let DurableAcquisition::Bound(store) = &self.acquisition {
            return Ok(store.clone());
        }
        // `lookup_session` keeps its answers apart: `Err` is a catalog that
        // could not answer, and surfaces as `StoreFactory`; `Deleted` and
        // `Absent` are answers (ADR 0112 §1.1).
        let lookup = self
            .catalog
            .lookup_session(&self.session_id)
            .await
            .map_err(|error| EmbedError::StoreFactory {
                session_id: self.session_id.clone(),
                message: error.to_string(),
            })?;
        match lookup {
            lash_core::store::SessionLookup::Live(_) => {
                let runtime: Arc<dyn lash_core::store::RuntimeStore> = self.catalog.clone();
                Ok(lash_core::store::SessionStore::new(
                    runtime,
                    self.session_id.clone(),
                )?)
            }
            lash_core::store::SessionLookup::Deleted => {
                Err(EmbedError::Store(lash_core::StoreError::SessionDeleted {
                    session_id: self.session_id.clone(),
                }))
            }
            lash_core::store::SessionLookup::Absent => Err(EmbedError::UnknownSession {
                session_id: self.session_id.clone(),
            }),
        }
    }

    /// Used only by the settled reads, whose job is to report absence.
    async fn store_if_present(&self) -> Result<Option<&lash_core::store::SessionStore>> {
        match self.store().await {
            Ok(store) => Ok(Some(store)),
            Err(EmbedError::UnknownSession { .. })
            | Err(EmbedError::Store(lash_core::StoreError::SessionDeleted { .. })) => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// Accept `input` durably and ask the engine to drive the session; the
    /// same acceptance as [`LashSession::send`](crate::LashSession::send), with
    /// no resident runtime to refresh when the handle answers.
    pub fn send(&self, input: TurnInput) -> crate::SendBuilder {
        crate::SendBuilder::new(crate::send::SendTarget::Durable(self.clone()), input)
    }

    /// Accept `inputs` durably as one request under one shared spec; the
    /// same acceptance as
    /// [`LashSession::send_batch`](crate::LashSession::send_batch).
    pub fn send_batch<I>(&self, inputs: impl IntoIterator<Item = I>) -> crate::SendBatchBuilder
    where
        I: Into<crate::BatchInput>,
    {
        crate::SendBatchBuilder::new(
            crate::send::SendTarget::Durable(self.clone()),
            inputs.into_iter().map(Into::into).collect(),
        )
    }

    /// Re-attach to an input accepted earlier, by its input id.
    pub fn attach(&self, input_id: lash_core::InputId) -> crate::SendHandle {
        crate::send::attach(crate::send::SendTarget::Durable(self.clone()), input_id)
    }

    /// Re-attach to the input a send accepted under host id `id`, with
    /// nothing but the id: see [`LashSession::attach_id`](crate::LashSession::attach_id).
    pub fn attach_id(&self, id: impl Into<lash_core::TurnId>) -> crate::SendHandle {
        crate::send::attach_id(crate::send::SendTarget::Durable(self.clone()), id.into())
    }

    /// Re-await a logical root by id.
    pub fn root(&self, root: impl Into<lash_core::TurnId>) -> crate::RootHandle {
        crate::send::root(crate::send::SendTarget::Durable(self.clone()), root.into())
    }

    /// Withdraw a queued input, or cooperatively cancel a running root
    /// (ADR 0039).
    pub fn cancel(&self, target: crate::CancelTarget) -> crate::CancelBuilder {
        crate::CancelBuilder::new(crate::send::SendTarget::Durable(self.clone()), target)
    }

    /// A held input reports the sealed drive epoch under which its admission was
    /// taken. That status does not prove the holder is alive; resubmitting
    /// while it is held creates another admission unless the host reuses the
    /// same source key.
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
    /// cancelled input from input that was already admitted, completed,
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

    /// Returns the session's admitted root that has no terminal evidence yet,
    /// with the head its admission recorded: the one root the next drive
    /// resumes before admitting anything else.
    pub async fn unfinished_root(&self) -> Result<Option<lash_core::store::UnfinishedRoot>> {
        Ok(self.store().await?.unfinished_root().await?)
    }

    /// Cancels queued work batch.
    pub async fn cancel_queued_work_batch(
        &self,
        batch_id: &lash_core::BatchId,
    ) -> Result<Option<QueuedWorkBatch>> {
        let store = self.store().await?;
        Ok(self.ops.cancel_queued_work_batch(store, batch_id).await?)
    }

    /// Read the canonical settled view of this durable session's current
    /// frame without opening a live runtime or exposing mutations.
    ///
    /// This is the inspection path for exporters, debuggers, and administrative
    /// tooling that must coexist with a live writer. The view holds the
    /// current frame only (ADR 0112 §9); earlier frames are paged through
    /// [`history`](Self::history), and failure evidence through
    /// [`failure_evidence`](Self::failure_evidence). `Ok(None)` means the
    /// catalog has no readable committed state for this id.
    pub async fn read(&self) -> Result<Option<crate::persistence::SessionReadView>> {
        let Some(store) = self.store_if_present().await? else {
            return Ok(None);
        };
        lash_core::store::load_session_read_view(store)
            .await
            .map_err(EmbedError::Store)
    }

    /// One page of this session's history, descending by generation from
    /// `anchor`, across frame and fork boundaries (ADR 0112 §6).
    ///
    /// Both budget limits are required. Continue with the page's `next`
    /// cursor through [`HistoryAnchor::Cursor`](lash_core::store::HistoryAnchor::Cursor);
    /// a page with a `next` always holds at least one node.
    pub async fn history(
        &self,
        anchor: lash_core::store::HistoryAnchor,
        budget: lash_core::store::HistoryBudget,
    ) -> Result<lash_core::store::HistoryPage> {
        Ok(self.store().await?.load_ancestors(anchor, budget).await?)
    }

    /// One page of this session's turn failure evidence, ordered by commit
    /// time (ADR 0112 §8). `next` is `Some` only when more exists.
    pub async fn failure_evidence(
        &self,
        after: Option<&lash_core::store::FailureEvidenceCursor>,
        limit: std::num::NonZeroU32,
    ) -> Result<lash_core::store::FailureEvidencePage> {
        Ok(self
            .store()
            .await?
            .load_failure_evidence_page(after, limit)
            .await?)
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
        self.catalog
            .lookup_session(&self.session_id)
            .await
            .map(|lookup| matches!(lookup, lash_core::store::SessionLookup::Deleted))
            .map_err(|error| EmbedError::StoreFactory {
                session_id: self.session_id.clone(),
                message: error.to_string(),
            })
    }
}
