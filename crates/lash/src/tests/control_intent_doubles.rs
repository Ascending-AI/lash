//! The facade test catalogs' control-intent ledgers. The deletion fixture
//! keeps one in memory; the others keep none and refuse it, so a session
//! deletion over one fails closed instead of deleting a session whose roots
//! nothing closed.

use std::collections::BTreeMap;

use super::{DeletingStoreFactory, RecordingStoreFactory, ReusableStoreFactory};
use lash_core::SessionId;
use lash_core::store::{
    CONTROL_INTENT_FORMAT, ControlIntent, ControlIntentId, ControlIntentKind, ControlIntentState,
    IntentApplication, decide_intent_acknowledgement, decide_intent_application,
    decide_intent_failure,
};
use lash_sansio::sync::MutexExt as _;

/// The deletion fixture's ledger. It holds no roots, so a close closes none;
/// every write is decided by the deciders a SQL backend runs in its
/// transaction.
#[derive(Debug, Default)]
pub(super) struct InMemoryControlIntents {
    state: std::sync::Mutex<BTreeMap<ControlIntentId, ControlIntent>>,
}

impl InMemoryControlIntents {
    /// The session's kept `CloseSession` intent, else a new one when the
    /// session `exists`, else `None`.
    fn begin_session_close(
        &self,
        session_id: &SessionId,
        exists: bool,
        at_ms: u64,
    ) -> Option<ControlIntent> {
        let mut state = self.state.lock_recover();
        if let Some(intent) = state.values().find(|intent| {
            intent.session_id == *session_id
                && matches!(intent.kind, ControlIntentKind::CloseSession { .. })
        }) {
            return Some(intent.clone());
        }
        if !exists {
            return None;
        }
        let id = ControlIntentId::from_sequence(
            state
                .keys()
                .next_back()
                .map_or(1, |last| last.sequence() + 1),
        );
        let intent = ControlIntent {
            id,
            session_id: session_id.clone(),
            format: CONTROL_INTENT_FORMAT,
            kind: ControlIntentKind::CloseSession { roots: Vec::new() },
            state: ControlIntentState::Pending,
            attempts: 0,
            created_at_ms: at_ms,
            engine: None,
        };
        state.insert(id, intent.clone());
        Some(intent)
    }

    fn rewrite<T>(
        &self,
        id: ControlIntentId,
        write: impl FnOnce(&mut ControlIntent) -> T,
    ) -> Result<T, lash_core::StoreError> {
        let mut state = self.state.lock_recover();
        let intent = state
            .get_mut(&id)
            .ok_or(lash_core::StoreError::ControlIntentUnknown { intent: id })?;
        Ok(write(intent))
    }
}

#[async_trait::async_trait]
impl lash_core::store::ControlIntentStore for ReusableStoreFactory {
    async fn begin_session_close(
        &self,
        _session_id: &SessionId,
        _at_ms: u64,
    ) -> std::result::Result<Option<lash_core::store::ControlIntent>, lash_core::StoreError> {
        Err(lash_core::StoreError::UnsupportedStoreOperation {
            operation: "ControlIntentStore::begin_session_close",
        })
    }

    async fn claim_intent_application(
        &self,
        _id: lash_core::store::ControlIntentId,
        _at_ms: u64,
    ) -> std::result::Result<lash_core::store::IntentApplication, lash_core::StoreError> {
        Err(lash_core::StoreError::UnsupportedStoreOperation {
            operation: "ControlIntentStore::claim_intent_application",
        })
    }

    async fn acknowledge_intent(
        &self,
        _id: lash_core::store::ControlIntentId,
        _at_ms: u64,
    ) -> std::result::Result<(), lash_core::StoreError> {
        Err(lash_core::StoreError::UnsupportedStoreOperation {
            operation: "ControlIntentStore::acknowledge_intent",
        })
    }

    async fn record_intent_failure(
        &self,
        _id: lash_core::store::ControlIntentId,
        _error: &str,
        _retryable: bool,
        _at_ms: u64,
    ) -> std::result::Result<lash_core::store::ControlIntent, lash_core::StoreError> {
        Err(lash_core::StoreError::UnsupportedStoreOperation {
            operation: "ControlIntentStore::record_intent_failure",
        })
    }

    async fn load_intent(
        &self,
        _id: lash_core::store::ControlIntentId,
    ) -> std::result::Result<Option<lash_core::store::ControlIntent>, lash_core::StoreError> {
        Err(lash_core::StoreError::UnsupportedStoreOperation {
            operation: "ControlIntentStore::load_intent",
        })
    }
}

#[async_trait::async_trait]
impl lash_core::store::ControlIntentStore for RecordingStoreFactory {
    async fn begin_session_close(
        &self,
        _session_id: &SessionId,
        _at_ms: u64,
    ) -> std::result::Result<Option<lash_core::store::ControlIntent>, lash_core::StoreError> {
        Err(lash_core::StoreError::UnsupportedStoreOperation {
            operation: "ControlIntentStore::begin_session_close",
        })
    }

    async fn claim_intent_application(
        &self,
        _id: lash_core::store::ControlIntentId,
        _at_ms: u64,
    ) -> std::result::Result<lash_core::store::IntentApplication, lash_core::StoreError> {
        Err(lash_core::StoreError::UnsupportedStoreOperation {
            operation: "ControlIntentStore::claim_intent_application",
        })
    }

    async fn acknowledge_intent(
        &self,
        _id: lash_core::store::ControlIntentId,
        _at_ms: u64,
    ) -> std::result::Result<(), lash_core::StoreError> {
        Err(lash_core::StoreError::UnsupportedStoreOperation {
            operation: "ControlIntentStore::acknowledge_intent",
        })
    }

    async fn record_intent_failure(
        &self,
        _id: lash_core::store::ControlIntentId,
        _error: &str,
        _retryable: bool,
        _at_ms: u64,
    ) -> std::result::Result<lash_core::store::ControlIntent, lash_core::StoreError> {
        Err(lash_core::StoreError::UnsupportedStoreOperation {
            operation: "ControlIntentStore::record_intent_failure",
        })
    }

    async fn load_intent(
        &self,
        _id: lash_core::store::ControlIntentId,
    ) -> std::result::Result<Option<lash_core::store::ControlIntent>, lash_core::StoreError> {
        Err(lash_core::StoreError::UnsupportedStoreOperation {
            operation: "ControlIntentStore::load_intent",
        })
    }
}

#[async_trait::async_trait]
impl lash_core::store::ControlIntentStore for DeletingStoreFactory {
    async fn begin_session_close(
        &self,
        session_id: &SessionId,
        at_ms: u64,
    ) -> std::result::Result<Option<lash_core::store::ControlIntent>, lash_core::StoreError> {
        let exists = self.stores.lock_recover().contains_key(session_id);
        Ok(self.intents.begin_session_close(session_id, exists, at_ms))
    }

    async fn claim_intent_application(
        &self,
        id: lash_core::store::ControlIntentId,
        at_ms: u64,
    ) -> std::result::Result<lash_core::store::IntentApplication, lash_core::StoreError> {
        self.intents.rewrite(id, |intent| {
            // It keeps no parks: its intents are session closes.
            let application = decide_intent_application(intent.clone(), None, at_ms);
            if let IntentApplication::Apply(applied) = &application {
                *intent = applied.clone();
            }
            application
        })
    }

    async fn acknowledge_intent(
        &self,
        id: lash_core::store::ControlIntentId,
        at_ms: u64,
    ) -> std::result::Result<(), lash_core::StoreError> {
        self.intents.rewrite(id, |intent| {
            if let Some(state) = decide_intent_acknowledgement(&intent.state, at_ms) {
                intent.state = state;
            }
        })
    }

    async fn record_intent_failure(
        &self,
        id: lash_core::store::ControlIntentId,
        error: &str,
        retryable: bool,
        _at_ms: u64,
    ) -> std::result::Result<lash_core::store::ControlIntent, lash_core::StoreError> {
        self.intents.rewrite(id, |intent| {
            if let Some(state) = decide_intent_failure(&intent.state, error, retryable) {
                intent.state = state;
            }
            intent.clone()
        })
    }

    async fn load_intent(
        &self,
        id: lash_core::store::ControlIntentId,
    ) -> std::result::Result<Option<lash_core::store::ControlIntent>, lash_core::StoreError> {
        Ok(self.intents.state.lock_recover().get(&id).cloned())
    }
}
