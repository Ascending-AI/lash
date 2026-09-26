use std::sync::Arc;

use lash::SessionId;
use lash::persistence::{
    AttachmentReclamationPolicy, EmptyRootSetPolicy, FileAttachmentStore, RuntimePersistence,
    SessionStoreCreateRequest, SessionStoreFactory, StoreError, TurnPark, ParkFeedCursor,
    ParkFeedPage, TurnParkQuery, TurnParkTarget, UnsettledTurnCounts, reclaim_unreferenced_attachments,
};

struct DelegatingFactory {
    inner: Arc<dyn SessionStoreFactory>,
}

#[async_trait::async_trait]
impl SessionStoreFactory for DelegatingFactory {
    async fn create_store(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<Arc<dyn RuntimePersistence>, StoreError> {
        self.inner.create_store(request).await
    }

    async fn open_existing_store(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<Option<Arc<dyn RuntimePersistence>>, String> {
        self.inner.open_existing_store(request).await
    }

    async fn open_existing_store_by_id(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<Arc<dyn RuntimePersistence>>, StoreError> {
        self.inner.open_existing_store_by_id(session_id).await
    }

    async fn session_was_deleted(&self, session_id: &SessionId) -> Result<bool, String> {
        self.inner.session_was_deleted(session_id).await
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
        after: ParkFeedCursor,
        limit: std::num::NonZeroUsize,
    ) -> Result<ParkFeedPage<TurnParkTarget>, StoreError> {
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
        through: ParkFeedCursor,
    ) -> Result<(), StoreError> {
        self.inner.compact_turn_park_feed(through).await
    }
}

async fn try_gc(factory: &DelegatingFactory) {
    let backend = FileAttachmentStore::new("attachments");
    let _ = reclaim_unreferenced_attachments(
        factory,
        &backend,
        AttachmentReclamationPolicy {
            grace_period_ms: 0,
            empty_root_set: EmptyRootSetPolicy::Refuse,
        },
    );
}

fn main() {}
