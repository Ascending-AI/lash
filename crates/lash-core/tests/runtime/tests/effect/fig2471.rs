use super::*;
use lash_core::facade_support::RuntimeSessionStateFacadeOps;

// Exercise the trait default with a host registry distinct from the borrowed
// Local controller, as durable deployment hosts do for foreground native runs.
#[derive(Default)]
struct DefaultBindingHost(lash_core::facade_support::NativeEffectHost);

#[async_trait::async_trait]
impl lash_core::AwaitEventResolver for DefaultBindingHost {
    async fn await_event_key(
        &self,
        scope: &lash_core::ExecutionScope,
        wait: lash_core::AwaitEventWaitIdentity,
    ) -> Result<lash_core::AwaitEventKey, lash_core::RuntimeError> {
        self.0.await_event_key(scope, wait).await
    }
    async fn resolve_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
        resolution: lash_core::Resolution,
    ) -> Result<lash_core::ResolveOutcome, lash_core::RuntimeError> {
        self.0.resolve_await_event(key, resolution).await
    }
    async fn peek_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
    ) -> Result<Option<lash_core::Resolution>, lash_core::RuntimeError> {
        self.0.peek_await_event(key).await
    }
    async fn await_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
        cancel: CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> Result<lash_core::Resolution, lash_core::RuntimeError> {
        self.0.await_await_event(key, cancel, deadline).await
    }
}

#[async_trait::async_trait]
impl lash_core::EffectHost for DefaultBindingHost {
    fn turn_control_binding_id(&self) -> String {
        "fig2471-default-binding-host".to_string()
    }

    fn await_event_resolver(&self) -> &dyn lash_core::AwaitEventResolver {
        self
    }

    fn scoped<'run>(
        &'run self,
        scope: lash_core::ExecutionScope,
    ) -> Result<lash_core::ScopedEffectController<'run>, lash_core::RuntimeError> {
        self.0.scoped(scope)
    }
}

#[tokio::test]
async fn turn_control_default_binding_external_cancel_stops_local_turn() {
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
    let mut config = lash_core::facade_support::RuntimeHostConfig::in_memory(
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    );
    config.control.effect_host = Arc::new(DefaultBindingHost::default());
    let driver_store: Arc<dyn lash_core::RuntimePersistence> = Arc::new(RecordingStore::default());
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
    let scope = native_scope(
        runtime
            .export_persistence_state()
            .turn_scope(&address.turn_id),
    );
    let turn = lash_core::task::spawn(async move {
        runtime
            .run_turn_assembled(
                TurnInput::text("wait for host cancellation"),
                CancellationToken::new(),
                scope,
            )
            .await
    });
    started_rx
        .await
        .expect("provider started after the turn gate was minted");
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
    let result = turn.await.unwrap().unwrap();
    assert!(
        matches!(result.outcome, lash_core::facade_support::TurnOutcome::Stopped(lash_core::facade_support::TurnStop::Cancelled { evidence }) if evidence.request_id == "host-request")
    );
}

#[tokio::test]
async fn turn_control_default_binding_active_gate_recognizes_host_cancel() {
    use lash_core::EffectHost as _;
    use lash_core::runtime::turn_control::ActiveTurnControl;

    let host = Arc::new(DefaultBindingHost::default());
    let controller = NativeRuntimeEffectController::default();
    let address = lash_core::facade_support::TurnAddress::new("active-session", "active-turn");
    let scoped =
        lash_core::ScopedEffectController::borrowed(&controller, address.execution_scope())
            .unwrap();
    let binding = host.turn_control_binding(&scoped).await.unwrap();
    let resolver = match binding {
        lash_core::TurnControlBinding::HostOwned { resolver, .. }
        | lash_core::TurnControlBinding::RunScoped { resolver, .. } => resolver,
    };
    let active = ActiveTurnControl::new(resolver, address.clone())
        .await
        .unwrap();
    let token = CancellationToken::new();
    token.cancel();
    let result = active.await_cancel(host.as_ref(), token).await;
    assert!(
        !matches!(result, Err(ref error) if error.code == lash_core::RuntimeErrorCode::AwaitEventUnknownOrRevoked),
        "host rejected active gate: {result:?}"
    );
    let driver_store: Arc<dyn lash_core::RuntimePersistence> = Arc::new(RecordingStore::default());
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
        .await_cancel(host.as_ref(), CancellationToken::new())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(evidence.request_id, "host-cancel");
}
