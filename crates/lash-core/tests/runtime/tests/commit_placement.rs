use super::*;
use lash_core::testing::TestTurnExecution as _;

const SEED: u64 = 0x5_c0aa;

/// A layer whose controllers own commit backpressure over the Restate server double.
struct EngineOwnedCommitLayer;

impl lash_core::testing::EffectLayer for EngineOwnedCommitLayer {
    fn owns_commit_backpressure(&self, _inner: &dyn lash_core::RuntimeEffectController) -> bool {
        true
    }
}

/// The host an engine-owned controller is lent through.
fn engine_commit_host(backend: &lash_core::Backend) -> Arc<dyn lash_core::EffectHost> {
    effect::layered_effect_host(backend, Arc::new(EngineOwnedCommitLayer))
}

#[tokio::test]
async fn durable_journaled_engine_commits_bypass_local_admission() {
    let double = kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let session_id = SessionId::from("engine-commit-placement");
    let store = double_unbound_recording_store(&double).await;
    let transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: "committed".into(),
                response_meta: None,
            }],
            ..LlmResponse::default()
        }),
    }]);
    let host = EmbeddedRuntimeHost::new(
        test_runtime_host_config(&backend).with_effect_host(engine_commit_host(&backend)),
    );
    let mut runtime = TestRuntime::new(&backend, transport)
        .host(host)
        .store(store.clone())
        .with_session_id(&session_id)
        .build()
        .await;
    let _ = lash_core::runtime::commit_admission::take_product_commit_admission_observations(
        &session_id,
    );
    let handler = double
        .open_handler(AdmittedScope::turn(&session_id, "placement-turn"))
        .await
        .expect("open the scope's handler");
    let scope = lash_core::testing::LayeredEffectHost::layer_scoped(
        handler.scoped(),
        Arc::new(EngineOwnedCommitLayer),
    )
    .expect("layer the handler's scope");
    runtime
        .execute_turn(
            TurnInput::text("commit"),
            lash_core::facade_support::TurnOptions::new(CancellationToken::new(), scope),
        )
        .await
        .expect("commit real turn");
    handler.close().await.expect("close the scope's handler");
    let observations =
        lash_core::runtime::commit_admission::take_product_commit_admission_observations(
            &session_id,
        );
    assert_eq!(observations.len(), 0, "turn commit coordinator entries");
}

/// Passes every operation through untouched.
struct PassThrough;

impl lash_core::testing::EffectLayer for PassThrough {}

#[tokio::test]
async fn commit_admission_ownership_survives_controller_wrappers() {
    let backend = sqlite_recording_backend().await;
    let hosts: [(Arc<dyn lash_core::EffectHost>, bool); 2] = [
        (engine_commit_host(&backend), true),
        (backend.effect_host(), false),
    ];
    for (inner, expected) in hosts {
        let host = lash_core::testing::LayeredEffectHost::new(inner, Arc::new(PassThrough));
        let admitted = lash_core::AdmittedScope::turn("ownership", "turn");
        let scoped = lash_core::EffectHost::scoped_static(&host, admitted.clone())
            .unwrap()
            .expect("both inner hosts lend owned controllers");
        assert_eq!(scoped.controller().owns_commit_backpressure(), expected);
        let (proxy, _requests) =
            lash_core::runtime::effect::EffectTaskController::scoped(scoped.controller(), admitted)
                .unwrap();
        assert_eq!(proxy.controller().owns_commit_backpressure(), expected);
    }
}
