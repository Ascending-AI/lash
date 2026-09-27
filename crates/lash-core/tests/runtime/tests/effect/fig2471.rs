use super::*;
use lash_core::facade_support::RuntimeSessionStateFacadeOps;

const SEED: u64 = 0x5_e217;

// Exercise the trait-default turn-control binding over a real journaling host:
// this host forwards its ports to `0` but keeps the trait's own
// `turn_control_binding`, so the binding comes from the default's journaled
// arm rather than from the backend host's override.
struct DefaultBindingHost(Arc<dyn lash_core::EffectHost>);

#[async_trait::async_trait]
impl lash_core::AwaitEventResolver for DefaultBindingHost {
    fn await_event_authority_binding_id(&self) -> Option<String> {
        self.0.await_event_authority_binding_id()
    }

    async fn await_event_key(
        &self,
        scope: &lash_core::ExecutionScope,
        wait: lash_core::AwaitEventWaitIdentity,
    ) -> Result<lash_core::AwaitEventKey, lash_core::RuntimeError> {
        self.0
            .await_event_resolver()
            .await_event_key(scope, wait)
            .await
    }
    async fn resolve_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
        resolution: lash_core::Resolution,
    ) -> Result<lash_core::ResolveOutcome, lash_core::RuntimeError> {
        self.0
            .await_event_resolver()
            .resolve_await_event(key, resolution)
            .await
    }
    async fn peek_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
    ) -> Result<Option<lash_core::Resolution>, lash_core::RuntimeError> {
        self.0.await_event_resolver().peek_await_event(key).await
    }
    async fn await_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
        cancel: CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> Result<lash_core::Resolution, lash_core::RuntimeError> {
        self.0
            .await_event_resolver()
            .await_await_event(key, cancel, deadline)
            .await
    }
}

#[async_trait::async_trait]
impl lash_core::EffectHost for DefaultBindingHost {
    fn turn_control_binding_id(&self) -> String {
        self.0.turn_control_binding_id()
    }

    fn await_event_resolver(&self) -> &dyn lash_core::AwaitEventResolver {
        self
    }

    fn scoped<'run>(
        &'run self,
        scope: lash_core::AdmittedScope,
    ) -> Result<lash_core::ScopedEffectController<'run>, lash_core::RuntimeError> {
        self.0.scoped(scope)
    }

    fn scoped_static(
        &self,
        scope: lash_core::AdmittedScope,
    ) -> Result<Option<lash_core::ScopedEffectController<'static>>, lash_core::RuntimeError> {
        self.0.scoped_static(scope)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn turn_control_default_binding_external_cancel_stops_local_turn() {
    let double = kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let started_tx = Arc::new(Mutex::new(Some(started_tx)));
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |_request| {
            let started_tx = Arc::clone(&started_tx);
            async move {
                if let Some(tx) = started_tx.lock_recover().take() {
                    let _ = tx.send(());
                }
                std::future::pending::<Result<LlmResponse, _>>().await
            }
        })
        .build();
    let mut config = lash_core::facade_support::RuntimeHostConfig::new(
        backend.clone(),
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    );
    config = config.with_effect_host(Arc::new(DefaultBindingHost(backend.effect_host())));
    let driver_store: Arc<dyn lash_core::RuntimePersistence> =
        double_unbound_recording_store(&double).await;
    lash_core::testing::store_fixtures::bind_conformance_session(
        &driver_store,
        &lash_core::SessionId::from("root"),
    )
    .await;
    let driver = lash_core::facade_support::TurnWorkDriver::for_session(
        Arc::clone(&config.control.effect_host),
        "root",
        driver_store,
    );
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(EmptyTools),
        transport,
        EmbeddedRuntimeHost::new(config),
    )
    .await;
    let address = lash_core::facade_support::TurnAddress::new("root", "external-local-cancel");
    let handler = double
        .open_handler(lash_core::AdmittedScope::new(
            runtime
                .export_persistence_state()
                .turn_scope(&address.turn_id),
        ))
        .await
        .expect("open the scope's handler");
    let mut turn = Box::pin(runtime.run_turn_assembled(
        TurnInput::text("wait for host cancellation"),
        CancellationToken::new(),
        handler.scoped(),
    ));
    tokio::select! {
        outcome = turn.as_mut() => panic!("the turn must still be running: {outcome:?}"),
        started = started_rx => started.expect("provider started after the turn gate was minted"),
    }
    let receipt = driver
        .request_cancel(lash_core::facade_support::TurnCancelRequest::new(
            address,
            "host-request",
            None,
        ))
        .await
        .unwrap();
    assert!(matches!(
        receipt.outcome,
        lash_core::facade_support::TurnCancelOutcome::Requested(_)
    ));
    let result = turn.await.unwrap();
    handler.close().await.expect("close the scope's handler");
    assert!(
        matches!(result.outcome, lash_core::facade_support::TurnOutcome::Stopped(lash_core::facade_support::TurnStop::Cancelled { evidence }) if evidence.request_id == "host-request")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn turn_control_default_binding_active_gate_recognizes_host_cancel() {
    let double = kernel_double(SEED + 1, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    use lash_core::EffectHost as _;
    use lash_core::runtime::turn_control::ActiveTurnControl;

    let host = Arc::new(DefaultBindingHost(backend.effect_host()));
    let address = lash_core::facade_support::TurnAddress::new("active-session", "active-turn");
    let handler = double
        .open_handler(lash_core::AdmittedScope::new(address.execution_scope()))
        .await
        .expect("open the scope's handler");
    let scoped = handler.scoped();
    let binding = host.turn_control_binding(&scoped).await.unwrap();
    let resolver = binding.resolver();
    let active = ActiveTurnControl::new(resolver, address.clone())
        .await
        .unwrap();
    // A watch the host accepts stays pending: only a rejected gate answers
    // at once.
    let result = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        active.watch_immediate(host.await_event_resolver()),
    )
    .await;
    assert!(
        !matches!(result, Ok(Err(ref error)) if error.code == lash_core::RuntimeErrorCode::AwaitEventUnknownOrRevoked),
        "host rejected active gate: {result:?}"
    );
    let driver_store: Arc<dyn lash_core::RuntimePersistence> =
        double_unbound_recording_store(&double).await;
    lash_core::testing::store_fixtures::bind_conformance_session(
        &driver_store,
        &lash_core::SessionId::from("active-session"),
    )
    .await;
    lash_core::facade_support::TurnWorkDriver::for_session(
        host.clone(),
        "active-session",
        driver_store,
    )
    .request_cancel(lash_core::facade_support::TurnCancelRequest::new(
        address,
        "host-cancel",
        None,
    ))
    .await
    .unwrap();
    let evidence = active
        .watch_immediate(host.await_event_resolver())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(evidence.request_id, "host-cancel");
    drop(scoped);
    handler.close().await.expect("close the scope's handler");
}
