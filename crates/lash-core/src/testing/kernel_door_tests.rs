//! The kernel door from this crate's own unit tests (D1 F6, PR-S2): the
//! Restate server double links another build of this crate, so only types
//! from below it cross, and a turn still runs on an open handler.

use crate::testing::TestTurnDrive as _;
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
        store as Arc<dyn crate::RuntimeStore>,
    )
    .await;
    let session_id = runtime.session_id().to_string();
    let handler = double
        .open_handler(AdmittedScope::turn(session_id.as_str(), "unit-door-turn"))
        .await
        .expect("open the turn's handler");
    let turn = runtime
        .drive_turn(
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

/// The storage twin reads the decorated deployment, while the backend's
/// original store set remains independent of that test layer.
#[tokio::test(flavor = "multi_thread")]
async fn a_faulted_doubles_store_twin_sees_the_fault() {
    struct FaultedCatalogLookup {
        inner: Arc<dyn crate::DeploymentStore>,
    }

    #[async_trait::async_trait]
    impl crate::RuntimeStoreDecorator for FaultedCatalogLookup {
        type Inner = dyn crate::DeploymentStore;

        fn inner(&self) -> &Self::Inner {
            self.inner.as_ref()
        }

        async fn lookup_session(
            &self,
            _session_id: &crate::SessionId,
        ) -> Result<crate::store::SessionLookup, crate::StoreError> {
            Err(crate::StoreError::Backend(
                "injected catalog-lookup failure".to_string(),
            ))
        }
    }

    impl lash_core_execution::DeploymentStoreDecorator for FaultedCatalogLookup {}

    let double = lash_restate_test::backend_with(
        SEED + 1,
        lash_restate_test::ServerConfig::default(),
        |stores| {
            super::runtime_helpers::LayeredStores::over(stores)
                .map_session_store_factory(|inner| {
                    Arc::new(FaultedCatalogLookup { inner }) as Arc<dyn crate::DeploymentStore>
                })
                .into_store_set()
        },
    )
    .await
    .expect("build the double over the faulted store set");
    let session_id = crate::SessionId::from("unit-door-missing");

    let decorated = double.engine_stores().session_store_factory();
    let error = crate::SessionCatalogStore::lookup_session(decorated.as_ref(), &session_id)
        .await
        .expect_err("the decorated catalog faults the lookup");
    assert!(error.to_string().contains("injected catalog-lookup failure"));

    let original = double.stores().session_store_factory();
    assert!(matches!(
        crate::SessionCatalogStore::lookup_session(original.as_ref(), &session_id)
            .await
            .expect("the original catalog answers"),
        crate::store::SessionLookup::Absent
    ));

    let twin = super::double_unbound_recording_store(&double).await;
    let error = crate::SessionCatalogStore::lookup_session(&twin, &session_id)
        .await
        .expect_err("the twin sees the decorated catalog fault");
    assert!(error.to_string().contains("injected catalog-lookup failure"));
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
