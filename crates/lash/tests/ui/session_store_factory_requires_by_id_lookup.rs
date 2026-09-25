//! A `SessionStoreFactory` that never states its by-id answer must not compile.
//!
//! `open_existing_store_by_id` is the non-creating seam a Durable Session
//! acquires through (ADR 0097), and its two negative answers mean opposite
//! things: `Ok(None)` is "no such session", `Err` is "this catalog cannot
//! resolve a session by id". An inherited `Ok(None)` would report every
//! existing session as missing, so the method is required.

use std::collections::BTreeSet;
use std::sync::Arc;

use lash::SessionId;
use lash::attachments::AttachmentId;
use lash::persistence::{
    AttachmentRootSet, RuntimePersistence, SessionStoreCreateRequest, SessionStoreFactory,
    StoreError, TurnPark, TurnParkFeedCursor, TurnParkFeedPage, TurnParkQuery, UnsettledTurnCounts,
};

struct SilentByIdFactory {
    inner: Arc<dyn SessionStoreFactory>,
}

#[async_trait::async_trait]
impl AttachmentRootSet for SilentByIdFactory {
    async fn live_attachment_refs(
        &self,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<BTreeSet<AttachmentId>, StoreError> {
        self.inner
            .live_attachment_refs(intent_grace_cutoff_epoch_ms)
            .await
    }

    async fn has_live_attachment_ref(
        &self,
        id: &AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, StoreError> {
        self.inner
            .has_live_attachment_ref(id, intent_grace_cutoff_epoch_ms)
            .await
    }
}

#[async_trait::async_trait]
impl SessionStoreFactory for SilentByIdFactory {
    async fn create_store(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<Arc<dyn RuntimePersistence>, StoreError> {
        self.inner.create_store(request).await
    }

    async fn session_was_deleted(&self, session_id: &SessionId) -> Result<bool, String> {
        SessionStoreFactory::session_was_deleted(self.inner.as_ref(), session_id).await
    }

    async fn delete_session(
        &self,
        session_id: &SessionId,
    ) -> lash::persistence::MaintenanceResult<lash::persistence::SessionBlobReclaimReport> {
        self.inner.delete_session(session_id).await
    }

    // A decorator forwards the deployment turn count to the catalog it wraps.
    async fn count_unsettled_turns(&self) -> Result<UnsettledTurnCounts, StoreError> {
        self.inner.count_unsettled_turns().await
    }

    async fn list_turn_parks(
        &self,
        query: &TurnParkQuery,
    ) -> Result<Vec<TurnPark>, StoreError> {
        self.inner.list_turn_parks(query).await
    }

    async fn turn_park_feed(
        &self,
        after: TurnParkFeedCursor,
        limit: std::num::NonZeroUsize,
    ) -> Result<TurnParkFeedPage, StoreError> {
        self.inner.turn_park_feed(after, limit).await
    }

    async fn root_terminal(
        &self,
        session_id: &lash::SessionId,
        root: &lash::TurnId,
    ) -> std::result::Result<Option<lash::persistence::RootTerminal>, StoreError> {
        self.inner.root_terminal(session_id, root).await
    }

    async fn list_open_control_intents(
        &self,
        after: Option<lash::persistence::ControlIntentId>,
        limit: std::num::NonZeroUsize,
    ) -> std::result::Result<Vec<lash::persistence::ControlIntent>, StoreError> {
        self.inner.list_open_control_intents(after, limit).await
    }

    async fn compact_turn_park_feed(
        &self,
        through: TurnParkFeedCursor,
    ) -> Result<(), StoreError> {
        self.inner.compact_turn_park_feed(through).await
    }
}

fn main() {}
