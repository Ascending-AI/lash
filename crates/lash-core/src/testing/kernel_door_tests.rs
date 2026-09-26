//! The kernel door from this crate's own unit tests (D1 F6, PR-S2): the
//! Restate server double links another build of this crate, so only types
//! from below it cross, and a turn still runs on an open handler.

use std::sync::Arc;

use super::runtime_helpers::{
    EmptyTools, MockCall, mock_provider, runtime_with_plugins_and_tools_and_host_and_store,
    test_host_config,
};
use crate::facade_support::{TurnFinish, TurnOptions, TurnOutcome};
use crate::{AdmittedScope, LlmOutputPart, LlmResponse, TurnInput};
use tokio_util::sync::CancellationToken;

const SEED: u64 = 0x5_2d10;

/// A turn over an unbound store of the double's store set, in an open
/// handler: `kernel_double` and `double_unbound_recording_store` together.
#[tokio::test(flavor = "multi_thread")]
async fn a_unit_test_turn_runs_in_an_open_handler_on_the_double() {
    let double = super::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let store = Arc::new(super::double_unbound_recording_store(&double).await);
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(vec![MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "Done".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        }]),
        test_host_config(&backend),
        store as Arc<dyn crate::RuntimePersistence>,
    )
    .await;
    let session_id = runtime.session_id().to_string();
    let handler = double
        .open_handler(AdmittedScope::turn(session_id.as_str(), "unit-door-turn"))
        .await
        .expect("open the turn's handler");
    let turn = runtime
        .stream_turn(
            TurnInput::text("hello"),
            TurnOptions::new(CancellationToken::new(), handler.scoped()),
        )
        .await
        .expect("the turn runs in the open handler");
    handler.close().await.expect("close the turn's handler");
    assert!(
        matches!(
            &turn.outcome,
            TurnOutcome::Finished(TurnFinish::AssistantMessage { text }) if text == "Done"
        ),
        "{:?}",
        turn.outcome
    );
}

/// The unbound twins open through the double's engine store set, so a
/// `backend_with` decorator on its session-store factory is the twin's
/// answer too: a faulted double's twin sees the fault, where a read of the
/// pre-decoration set does not.
#[tokio::test(flavor = "multi_thread")]
async fn a_faulted_doubles_unbound_twin_sees_the_fault() {
    struct FaultedUnboundOpen {
        inner: Arc<dyn crate::SessionStoreFactory>,
    }

    #[async_trait::async_trait]
    impl crate::AttachmentRootSet for FaultedUnboundOpen {
        async fn live_attachment_refs(
            &self,
            intent_grace_cutoff_epoch_ms: u64,
        ) -> Result<std::collections::BTreeSet<crate::AttachmentId>, crate::StoreError> {
            self.inner
                .live_attachment_refs(intent_grace_cutoff_epoch_ms)
                .await
        }

        async fn has_live_attachment_ref(
            &self,
            id: &crate::AttachmentId,
            intent_grace_cutoff_epoch_ms: u64,
        ) -> Result<bool, crate::StoreError> {
            self.inner
                .has_live_attachment_ref(id, intent_grace_cutoff_epoch_ms)
                .await
        }
    }

    #[async_trait::async_trait]
    impl crate::SessionStoreFactory for FaultedUnboundOpen {
        async fn create_store(
            &self,
            request: &crate::SessionStoreCreateRequest,
        ) -> Result<Arc<dyn crate::RuntimePersistence>, crate::StoreError> {
            self.inner.create_store(request).await
        }

        async fn count_unsettled_turns(
            &self,
        ) -> Result<crate::store::UnsettledTurnCounts, crate::StoreError> {
            self.inner.count_unsettled_turns().await
        }

        async fn list_turn_parks(
            &self,
            query: &crate::store::TurnParkQuery,
        ) -> Result<Vec<crate::store::TurnPark>, crate::StoreError> {
            self.inner.list_turn_parks(query).await
        }

        async fn turn_park_feed(
            &self,
            after: crate::store::ParkFeedCursor,
            limit: std::num::NonZeroUsize,
        ) -> Result<crate::store::ParkFeedPage<crate::store::TurnParkTarget>, crate::StoreError>
        {
            self.inner.turn_park_feed(after, limit).await
        }

        async fn root_terminal(
            &self,
            session_id: &crate::SessionId,
            root: &crate::TurnId,
        ) -> Result<Option<crate::store::RootTerminal>, crate::StoreError> {
            self.inner.root_terminal(session_id, root).await
        }

        async fn list_open_control_intents(
            &self,
            after: Option<crate::store::ControlIntentId>,
            limit: std::num::NonZeroUsize,
        ) -> Result<Vec<crate::store::ControlIntent>, crate::StoreError> {
            self.inner.list_open_control_intents(after, limit).await
        }

        async fn compact_turn_park_feed(
            &self,
            through: crate::store::ParkFeedCursor,
        ) -> Result<(), crate::StoreError> {
            self.inner.compact_turn_park_feed(through).await
        }

        async fn open_existing_store_by_id(
            &self,
            session_id: &crate::SessionId,
        ) -> Result<Option<Arc<dyn crate::RuntimePersistence>>, crate::StoreError> {
            self.inner.open_existing_store_by_id(session_id).await
        }

        async fn session_was_deleted(&self, session_id: &crate::SessionId) -> Result<bool, String> {
            self.inner.session_was_deleted(session_id).await
        }

        async fn delete_session(
            &self,
            session_id: &crate::SessionId,
        ) -> crate::store::MaintenanceResult<crate::store::SessionBlobReclaimReport> {
            self.inner.delete_session(session_id).await
        }

        async fn open_unbound_store(
            &self,
        ) -> Result<Arc<dyn crate::RuntimePersistence>, crate::StoreError> {
            Err(crate::StoreError::Backend(
                "injected unbound-open failure".to_string(),
            ))
        }
    }

    #[async_trait::async_trait]
    impl crate::store::ControlIntentStore for FaultedUnboundOpen {
        async fn begin_session_close(
            &self,
            session_id: &crate::SessionId,
            at_ms: u64,
        ) -> Result<Option<crate::store::ControlIntent>, crate::StoreError> {
            self.inner.begin_session_close(session_id, at_ms).await
        }

        async fn claim_intent_application(
            &self,
            id: crate::store::ControlIntentId,
        ) -> Result<crate::store::IntentApplication, crate::StoreError> {
            self.inner.claim_intent_application(id).await
        }

        async fn acknowledge_intent(
            &self,
            id: crate::store::ControlIntentId,
            at_ms: u64,
        ) -> Result<(), crate::StoreError> {
            self.inner.acknowledge_intent(id, at_ms).await
        }

        async fn record_intent_failure(
            &self,
            id: crate::store::ControlIntentId,
            error: &str,
            retryable: bool,
            at_ms: u64,
        ) -> Result<crate::store::ControlIntent, crate::StoreError> {
            self.inner
                .record_intent_failure(id, error, retryable, at_ms)
                .await
        }

        async fn load_intent(
            &self,
            id: crate::store::ControlIntentId,
        ) -> Result<Option<crate::store::ControlIntent>, crate::StoreError> {
            self.inner.load_intent(id).await
        }
    }

    let double = lash_restate_test::backend_with(
        SEED + 1,
        lash_restate_test::ServerConfig::default(),
        |stores| {
            super::runtime_helpers::LayeredStores::over(stores)
                .map_session_store_factory(|inner| {
                    Arc::new(FaultedUnboundOpen { inner }) as Arc<dyn crate::SessionStoreFactory>
                })
                .into_store_set()
        },
    )
    .await
    .expect("build the double over the faulted store set");

    // The decorated set — the one the engine runs over — faults the open.
    let error = crate::SessionStoreFactory::open_unbound_store(
        double.engine_stores().session_store_factory().as_ref(),
    )
    .await
    .err()
    .expect("the decorated set's factory faults the unbound open");
    assert!(
        error.to_string().contains("injected unbound-open failure"),
        "{error}"
    );
    // `stores()` is the pre-decoration set: its open sees no fault.
    double
        .stores()
        .open_store()
        .await
        .expect("the pre-decoration store set still opens unbound stores");
    // And the twin reads the decorated set: its open meets the fault.
    let failed = crate::task::spawn({
        let double = double.clone();
        async move {
            let _ = super::double_unbound_recording_store(&double).await;
        }
    })
    .await
    .expect_err("the twin's unbound open reports the injected fault");
    assert!(failed.is_panic(), "the twin's expect aborts on the fault");
}

/// The storage-only twins: a store set and a backend over it that reaches
/// its store ports and runs no effect.
#[tokio::test]
async fn the_storage_only_twins_reach_store_ports_and_run_no_effect() {
    let stores = super::memory_store_set().await;
    let backend = super::memory_store_backend().await;
    assert_ne!(
        backend.binding_identity(),
        crate::StoreSet::binding_identity(stores.as_ref()).clone(),
        "each call opens a fresh store set"
    );
    assert_eq!(
        backend.binding_identity(),
        backend.stores().binding_identity().clone(),
        "the backend's stores are the store set it was built over"
    );
    assert_eq!(
        backend.effect_host().turn_control_binding_id(),
        "conformance-recording-effect-host"
    );
}
