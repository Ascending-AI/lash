use super::*;
use lash_core::store::{RuntimePersistenceDecorator, TurnInputStore as _};

struct FailCancelClosureAuthorizationStore {
    inner: Arc<RecordingStore>,
    calls: AtomicUsize,
}

impl FailCancelClosureAuthorizationStore {
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
impl RuntimePersistenceDecorator for FailCancelClosureAuthorizationStore {
    fn inner(&self) -> &(dyn lash_core::RuntimePersistence + '_) {
        self.inner.as_ref()
    }

    async fn authorize_turn_cancel_closure(
        &self,
        lease: &lash_core::SessionExecutionLeaseAuthority,
        authorization: &lash_core::TurnCancelClosureAuthorization,
    ) -> Result<lash_core::TurnCancelClosureAuthorizationOutcome, lash_core::StoreError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(lash_core::StoreError::Backend(
                "injected finish-time cancellation authorization failure".to_string(),
            ));
        }
        lash_core::store::TurnCancelStore::authorize_turn_cancel_closure(
            self.inner.as_ref(),
            lease,
            authorization,
        )
        .await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drop_request_survives_owner_failure_before_finish_and_prevents_redelivery() {
    let backend = memory_backend().await;
    const SESSION_ID: &str = "drop-cancel-owner-failure";
    const TURN_ID: &str = "turn-that-cannot-finish";

    let inner_store = unbound_recording_store(&backend).await;
    let store = Arc::new(FailCancelClosureAuthorizationStore::new(Arc::clone(
        &inner_store,
    )));
    let runtime_store: Arc<dyn lash_core::store::RuntimePersistence> = store.clone();
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
    let mut runtime = TestRuntime::new(&backend, transport)
        .tools(Arc::new(EmptyTools))
        .host(test_host_config(&backend))
        .store(runtime_store)
        .with_session_id(SESSION_ID)
        .build()
        .await;
    let turn_driver = lash_core::facade_support::TurnWorkDriver::for_session(
        Arc::clone(&runtime.host.core.control.effect_host),
        SESSION_ID,
        Arc::clone(&store) as Arc<dyn lash_core::RuntimePersistence>,
    );
    let effect_loop_ended = Arc::new(AtomicBool::new(false));
    let release_effect_loop = Arc::new(AtomicBool::new(false));
    runtime.set_turn_phase_probe(Arc::new(PauseAfterEffectLoop {
        entered: Arc::clone(&effect_loop_ended),
        release: Arc::clone(&release_effect_loop),
    }));

    let persisted_state = runtime.export_persistence_state();
    let turn_scope = backend_admitted_scope(
        &backend,
        lash_core::AdmittedScope::unpinned(persisted_state.turn_scope(TURN_ID))
            .expect("turn scope"),
    );
    let turn_address = lash_core::facade_support::TurnAddress::new(SESSION_ID, TURN_ID);
    let mut turn = lash_core::task::spawn(async move {
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
    let undelivered = lash_core::store::TurnInputStore::enqueue_pending_turn_input(
        inner_store.as_ref(),
        lash_core::PendingTurnInputDraft::new(
            SESSION_ID,
            lash_core::TurnInputIngress::active_turn(
                TURN_ID,
                lash_core::TurnInputCheckpointBoundary::AfterWork,
            ),
            lash_core::TurnInput::text("must be dropped after owner failure"),
        ),
    )
    .await
    .expect("enqueue an undelivered active-turn input");
    let receipt = turn_driver
        .request_cancel(
            lash_core::facade_support::TurnCancelRequest::new(
                turn_address.clone(),
                "drop-before-owner-failure",
                Some("test-user".to_string()),
            )
            .with_reason("drop undelivered input")
            .undelivered(lash_core::TurnCancelDisposition::Drop),
        )
        .await
        .expect("record Drop before the owner reaches finish");
    assert!(matches!(
        receipt.outcome,
        lash_core::facade_support::TurnCancelOutcome::Requested(ref evidence)
            if evidence.undelivered == lash_core::TurnCancelDisposition::Drop
    ));

    tokio::select! {
        result = &mut turn => panic!("owner ended before the finish-time failure seam: {result:?}"),
        result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !effect_loop_ended.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        }) => result.expect("turn should seal cancellation before finish-time authorization"),
    }
    release_effect_loop.store(true, Ordering::SeqCst);

    let error = tokio::time::timeout(std::time::Duration::from_secs(5), turn)
        .await
        .expect("failed owner turn should return")
        .expect("turn task")
        .expect_err("injected owner failure must reject finish-time authorization");
    assert_eq!(error.code, lash_core::RuntimeErrorCode::StoreCommitFailed);
    assert!(
        error
            .message
            .contains("injected finish-time cancellation authorization failure")
    );
    assert_eq!(
        store.calls(),
        2,
        "finish authorization fails once, then teardown authorizes the durable Drop repair"
    );

    let pending = lash_core::TurnInputStore::list_pending_turn_inputs(
        inner_store.as_ref(),
        &lash_core::SessionId::from(SESSION_ID),
    )
    .await
    .expect("list inputs eligible for redelivery after recovery");
    assert!(
        pending
            .iter()
            .all(|input| input.input.input_id != undelivered.input_id),
        "Drop evidence must keep the undelivered input out of every later claim"
    );
    let record =
        lash_core::store::TurnCancelStore::turn_cancel_request(inner_store.as_ref(), &turn_address)
            .await
            .expect("read durable cancellation record")
            .expect("Drop request remains recorded");
    let affected = record
        .outcome
        .expect("teardown recovery records its input decision")
        .affected_inputs;
    assert!(affected.iter().any(|input| {
        input.input_id == undelivered.input_id
            && input.disposition == lash_core::TurnCancelDisposition::Drop
    }));
}
