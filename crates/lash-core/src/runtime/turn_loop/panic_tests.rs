use super::*;
use crate::runtime::tests::helpers::{MockCall, TestRuntime, mock_provider, named_turn_scope};

struct PanicBeforeCommit;

impl RuntimeTurnPhaseProbe for PanicBeforeCommit {
    fn begin(&self, phase: RuntimeTurnPhase) {
        if phase == RuntimeTurnPhase::EffectLoop {
            panic!("contained lease witness");
        }
    }

    fn end(&self, _phase: RuntimeTurnPhase) {}
}

async fn runtime(store: Arc<InMemorySessionStore>) -> LashRuntime {
    TestRuntime::new(mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: "done".to_string(),
                response_meta: None,
            }],
            ..LlmResponse::default()
        }),
    }]))
    .with_session_id("lease-panic-witness")
    .store(store)
    .build()
    .await
}

#[tokio::test]
async fn contained_panic_releases_lease_before_immediate_successor() {
    Box::pin(assert_immediate_successor(false)).await;
}

#[tokio::test]
async fn contained_panic_waits_for_lease_release_acknowledgement() {
    Box::pin(assert_immediate_successor(true)).await;
}

async fn assert_immediate_successor(gate_release: bool) {
    let store = Arc::new(InMemorySessionStore::new());
    let mut first = runtime(Arc::clone(&store)).await;
    let mut successor = runtime(Arc::clone(&store)).await;
    let session_id = first.state.session_id.clone();
    first.set_turn_phase_probe(Arc::new(PanicBeforeCommit));
    let gate = gate_release.then(|| store.gate_session_execution_lease_release());
    let task = crate::task::spawn(async move {
        first
            .stream_turn_with_agent_frames(
                TurnInput::text("panic"),
                TurnOptions::new(
                    CancellationToken::new(),
                    named_turn_scope(&session_id, &TurnId::from("panic")),
                ),
            )
            .await
    });
    if let Some(gate) = &gate {
        gate.wait_entered().await;
        // Force the losing schedule: an already surfaced panic gets no help
        // from this test releasing its detached cleanup. Its successor must
        // expose SessionExecutionLaneBusy below. An awaited release keeps the
        // turn task pending until we acknowledge the backend operation.
        if !task.is_finished() {
            gate.admit_one();
        }
    }
    let failure = task.await.expect_err("turn task must panic");
    assert!(failure.is_panic());
    assert_eq!(
        failure.into_panic().downcast_ref::<&str>(),
        Some(&"contained lease witness"),
        "cleanup preserves the original panic payload"
    );
    let lease = successor
        .claim_session_execution_lease()
        .await
        .expect("immediate successor admitted without SessionExecutionLaneBusy")
        .expect("store-backed successor holds a lease");
    if let Some(gate) = gate {
        gate.admit_one();
    }
    lease.release_if_live().await.expect("release successor");
}

#[tokio::test]
async fn happy_turn_keeps_atomic_lease_release_without_extra_call() {
    let store = Arc::new(InMemorySessionStore::new());
    let mut successor = runtime(Arc::clone(&store)).await;
    let mut runtime = runtime(Arc::clone(&store)).await;
    let session_id = runtime.state.session_id.clone();
    runtime
        .stream_turn_with_agent_frames(
            TurnInput::text("complete"),
            TurnOptions::new(
                CancellationToken::new(),
                named_turn_scope(&session_id, &TurnId::from("happy")),
            ),
        )
        .await
        .expect("happy turn completes");
    assert_eq!(
        store.session_execution_lease_release_attempt_count(),
        0,
        "atomic commit releases the lease; settlement and Drop add no release call"
    );
    let lease = successor
        .claim_session_execution_lease()
        .await
        .expect("lane free before return")
        .expect("successor admitted");
    lease.release_if_live().await.expect("release successor");
}
