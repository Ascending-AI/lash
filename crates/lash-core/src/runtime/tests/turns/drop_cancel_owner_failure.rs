use super::*;
use crate::store::{RuntimePersistenceDecorator, TurnInputStore as _};

struct FailSecondCancelRecordStore {
    inner: Arc<RecordingStore>,
    calls: AtomicUsize,
}

impl FailSecondCancelRecordStore {
    fn new(inner: Arc<RecordingStore>) -> Self {
        Self {
            inner,
            calls: AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl RuntimePersistenceDecorator for FailSecondCancelRecordStore {
    fn inner(&self) -> &(dyn crate::RuntimePersistence + '_) {
        self.inner.as_ref()
    }

    async fn record_turn_cancel_request(
        &self,
        request: crate::TurnCancelRequest,
    ) -> Result<crate::TurnCancelRequestRecord, crate::StoreError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if call == 2 {
            return Err(crate::StoreError::Backend(
                "injected finish-time cancellation record failure".to_string(),
            ));
        }
        self.inner.record_turn_cancel_request(request).await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drop_request_survives_owner_failure_before_finish_and_prevents_redelivery() {
    const SESSION_ID: &str = "drop-cancel-owner-failure";
    const TURN_ID: &str = "turn-that-cannot-finish";

    let inner_store = Arc::new(RecordingStore::default());
    let store = Arc::new(FailSecondCancelRecordStore::new(Arc::clone(&inner_store)));
    let runtime_store: Arc<dyn crate::store::RuntimePersistence> = store.clone();
    let (provider_started_tx, provider_started_rx) = tokio::sync::oneshot::channel::<()>();
    let provider_started_tx = Arc::new(Mutex::new(Some(provider_started_tx)));
    let captured_provider_started_tx = Arc::clone(&provider_started_tx);
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |_request| {
            let captured_provider_started_tx = Arc::clone(&captured_provider_started_tx);
            async move {
                if let Some(tx) = captured_provider_started_tx.lock_recover().take() {
                    let _ = tx.send(());
                }
                std::future::pending::<Result<LlmResponse, _>>().await
            }
        })
        .build();
    let mut runtime = TestRuntime::new(transport)
        .tools(Arc::new(EmptyTools))
        .host(test_host_config())
        .store(runtime_store)
        .with_session_id(SESSION_ID)
        .build()
        .await;
    let turn_driver = crate::TurnWorkDriver::for_session(
        Arc::clone(&runtime.host.core.control.effect_host),
        SESSION_ID,
        Arc::clone(&store) as Arc<dyn crate::RuntimePersistence>,
    );
    let effect_loop_ended = Arc::new(AtomicBool::new(false));
    let release_effect_loop = Arc::new(AtomicBool::new(false));
    runtime.set_turn_phase_probe(Arc::new(PauseAfterEffectLoop {
        entered: Arc::clone(&effect_loop_ended),
        release: Arc::clone(&release_effect_loop),
    }));

    let persisted_state = runtime.export_persistence_state();
    let turn_scope = native_scope(persisted_state.turn_scope(TURN_ID));
    let turn_address = crate::TurnAddress::new(SESSION_ID, TURN_ID);
    let turn = crate::task::spawn(async move {
        runtime
            .run_turn_assembled(
                TurnInput::text("cancel before the owner loses its finish commit"),
                CancellationToken::new(),
                turn_scope,
            )
            .await
    });
    provider_started_rx
        .await
        .expect("provider should start after lease acquisition");
    let undelivered = crate::store::TurnInputStore::enqueue_pending_turn_input(
        inner_store.as_ref(),
        crate::PendingTurnInputDraft::new(
            SESSION_ID,
            crate::TurnInputIngress::active_turn(
                TURN_ID,
                crate::TurnInputCheckpointBoundary::AfterWork,
            ),
            crate::TurnInput::text("must be dropped after owner failure"),
        ),
    )
    .await
    .expect("enqueue an undelivered active-turn input");
    let receipt = turn_driver
        .request_cancel(
            crate::TurnCancelRequest::new(
                turn_address.clone(),
                "drop-before-owner-failure",
                Some("test-user".to_string()),
            )
            .with_reason("drop undelivered input")
            .undelivered(crate::TurnCancelDisposition::Drop),
        )
        .await
        .expect("record Drop before the owner reaches finish");
    assert!(matches!(
        receipt.outcome,
        crate::TurnCancelOutcome::Requested(ref evidence)
            if evidence.undelivered == crate::TurnCancelDisposition::Drop
    ));

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !effect_loop_ended.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("turn should seal cancellation before finish-time recording");
    release_effect_loop.store(true, Ordering::SeqCst);

    let error = tokio::time::timeout(std::time::Duration::from_secs(5), turn)
        .await
        .expect("failed owner turn should return")
        .expect("turn task")
        .expect_err("injected owner failure must reject finish-time recording");
    assert_eq!(error.code, crate::RuntimeErrorCode::RuntimeStore);
    assert_eq!(
        store.calls(),
        2,
        "the injected failure must follow the ingress record and precede the turn commit"
    );

    let raw = inner_store.raw_pending_turn_inputs_for_testing();
    let dropped = raw
        .iter()
        .find(|(input_id, ..)| input_id == &undelivered.input_id)
        .expect("the cancelled row remains as durable evidence");
    assert_eq!(dropped.2, crate::TurnInputState::Cancelled);
    assert!(dropped.3.is_none(), "recovery clears the dead turn claim");

    let pending = crate::TurnInputStore::list_pending_turn_inputs(
        inner_store.as_ref(),
        &crate::SessionId::from(SESSION_ID),
    )
    .await
    .expect("list inputs eligible for redelivery after recovery");
    assert!(
        pending
            .iter()
            .all(|input| input.input.input_id != undelivered.input_id),
        "Drop evidence must keep the undelivered input out of every later claim"
    );
    let record = inner_store
        .turn_cancel_request(&turn_address)
        .await
        .expect("read durable cancellation record")
        .expect("Drop request remains recorded");
    let affected = record
        .outcome
        .expect("teardown recovery records its input decision")
        .affected_inputs;
    assert!(affected.iter().any(|input| {
        input.input_id == undelivered.input_id
            && input.disposition == crate::TurnCancelDisposition::Drop
    }));
}
