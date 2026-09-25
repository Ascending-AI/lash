//! A `SessionStoreFactory` that never states its tombstone answer must not
//! compile.
//!
//! `session_was_deleted` decides whether a resume hands back the caller's
//! conversation or a fresh empty one under a dead id. An inherited "no
//! tombstone here" is a claim the implementor never made, so the method is
//! required: omitting it is a compile error, not a default.

use std::collections::BTreeSet;
use std::sync::Arc;

use lash::SessionId;
use lash::attachments::AttachmentId;
use lash::persistence::{
    AttachmentRootSet, RuntimePersistence, SessionStoreCreateRequest, SessionStoreFactory,
    StoreError, TurnPark, TurnParkFeedCursor, TurnParkFeedPage, TurnParkQuery, UnsettledTurnCounts,
};

struct SilentFactory {
    inner: Arc<dyn SessionStoreFactory>,
}

#[async_trait::async_trait]
impl AttachmentRootSet for SilentFactory {
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
impl SessionStoreFactory for SilentFactory {
    async fn create_store(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<Arc<dyn RuntimePersistence>, StoreError> {
        self.inner.create_store(request).await
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
