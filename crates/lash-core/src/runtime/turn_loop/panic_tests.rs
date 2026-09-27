use super::*;
use crate::runtime::tests::helpers::{
    MockCall, RecordingStore, TestRuntime, mock_provider, recording_session_store,
};

const SESSION_ID: &str = "lease-panic-witness";

struct PanicBeforeCommit;

impl RuntimeTurnPhaseProbe for PanicBeforeCommit {
    fn begin(&self, phase: RuntimeTurnPhase) {
        if phase == RuntimeTurnPhase::EffectLoop {
            panic!("contained lease witness");
        }
    }

    fn end(&self, _phase: RuntimeTurnPhase) {}
}

async fn runtime(backend: &crate::Backend, store: Arc<RecordingStore>) -> LashRuntime {
    TestRuntime::new(
        backend,
        mock_provider(vec![MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "done".to_string(),
                    response_meta: None,
                }],
                ..LlmResponse::default()
            }),
        }]),
    )
    .with_session_id(SESSION_ID)
    .store(store)
    .build()
    .await
}

#[tokio::test]
async fn contained_panic_releases_lease_before_immediate_successor() {
    Box::pin(assert_immediate_successor()).await;
}

async fn assert_immediate_successor() {
    let double =
        crate::testing::kernel_double(0x3861_0201, lash_restate_test::ServerConfig::default())
            .await;
    let backend = double.lash_backend();
    let store = recording_session_store(&backend, SESSION_ID).await;
    let mut first = runtime(&backend, Arc::clone(&store)).await;
    let mut successor = runtime(&backend, Arc::clone(&store)).await;
    let session_id = first.state.session_id.clone();
    first.set_turn_phase_probe(Arc::new(PanicBeforeCommit));
    let task = crate::task::spawn(async move {
        let handler = double
            .open_handler(crate::AdmittedScope::turn(&session_id, "panic"))
            .await
            .expect("open the panic turn handler");
        first
            .stream_turn_with_agent_frames(
                TurnInput::text("panic"),
                TurnOptions::new(CancellationToken::new(), handler.scoped()),
            )
            .await
    });
    let failure = task.await.expect_err("turn task must panic");
    assert!(failure.is_panic());
    assert_eq!(
        failure.into_panic().downcast_ref::<&str>(),
        Some(&"contained lease witness"),
        "cleanup preserves the original panic payload"
    );
    let lease = successor
        .claim_drive_authority()
        .await
        .expect("immediate successor admitted without SessionExecutionLaneBusy")
        .expect("store-backed successor holds a lease");
    lease.release_if_live().await.expect("release successor");
}

#[tokio::test]
async fn happy_turn_keeps_atomic_lease_release_without_extra_call() {
    let double =
        crate::testing::kernel_double(0x3861_0202, lash_restate_test::ServerConfig::default())
            .await;
    let backend = double.lash_backend();
    let store = recording_session_store(&backend, SESSION_ID).await;
    let mut successor = runtime(&backend, Arc::clone(&store)).await;
    let mut runtime = runtime(&backend, Arc::clone(&store)).await;
    let handler = double
        .open_handler(crate::AdmittedScope::turn(
            &runtime.state.session_id,
            "happy",
        ))
        .await
        .expect("open the happy turn handler");
    runtime
        .stream_turn_with_agent_frames(
            TurnInput::text("complete"),
            TurnOptions::new(CancellationToken::new(), handler.scoped()),
        )
        .await
        .expect("happy turn completes");
    handler.close().await.expect("close the happy turn handler");
    let lease = successor
        .claim_drive_authority()
        .await
        .expect("lane free before return")
        .expect("successor admitted");
    lease.release_if_live().await.expect("release successor");
}
