//! The facade test catalogs keep no control-intent ledger: each refuses it,
//! so a session deletion over one fails closed instead of deleting a
//! session whose roots nothing closed.

use super::{DeletingStoreFactory, RecordingStoreFactory, ReusableStoreFactory};
use lash_core::SessionId;

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
