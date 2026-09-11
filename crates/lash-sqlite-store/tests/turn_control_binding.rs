use std::sync::Arc;

use lash_core::SessionStoreFactory;
use lash_core::facade_support::{NativeEffectHost, NativeRuntimeEffectController};
use lash_core::runtime::{TurnAddress, TurnCancelOutcome, TurnCancelRequest, TurnWorkDriver};
use lash_core::{
    AwaitEventResolver, AwaitEventWaitIdentity, EffectHost, RuntimeErrorCode,
    ScopedEffectController, TurnControlBinding,
};
use lash_sqlite_store::SqliteEffectHost;
use lash_sqlite_store::SqliteSessionStoreFactory;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn turn_control_local_gate_is_owned_and_awaited_by_sqlite_host() {
    let dir = tempfile::tempdir().expect("temporary database directory");
    let host = Arc::new(
        SqliteEffectHost::open(&dir.path().join("effects.sqlite"))
            .await
            .unwrap(),
    );
    let controller = NativeRuntimeEffectController::default();
    let address = TurnAddress::new("local-session", "local-turn");
    let scoped = ScopedEffectController::borrowed(&controller, address.execution_scope()).unwrap();
    let binding = host.turn_control_binding(&scoped).await.unwrap();
    let resolver = match &binding {
        TurnControlBinding::HostOwned { resolver, .. }
        | TurnControlBinding::RunScoped { resolver, .. } => *resolver,
    };
    // ActiveTurnControl is crate-private. Exercise its exact cancel-gate identity
    // through the public resolver seam, without adding a testing-only facade.
    let cancel_key = resolver
        .await_event_key(
            &address.execution_scope(),
            AwaitEventWaitIdentity::TurnCancelGate,
        )
        .await
        .unwrap();
    assert_eq!(
        host.peek_await_event(&cancel_key)
            .await
            .expect("host must recognize the turn cancel gate"),
        None
    );
    let stop_wait = CancellationToken::new();
    stop_wait.cancel();
    let result = host.await_await_event(&cancel_key, stop_wait, None).await;
    assert!(
        !matches!(result, Err(ref error) if error.code == RuntimeErrorCode::AwaitEventUnknownOrRevoked),
        "host wait rejected its turn gate: {result:?}"
    );
    assert!(matches!(binding, TurnControlBinding::HostOwned { .. }));

    // An external request must resolve the gate the Local turn actually watches.
    let store: Arc<dyn lash_core::RuntimePersistence> =
        Arc::new(lash_core::runtime::InMemorySessionStore::new());
    lash_core::testing::store_fixtures::bind_conformance_session(&store, &address.session_id).await;
    let receipt = TurnWorkDriver::for_session(host.clone(), address.session_id.clone(), store)
        .request_cancel(TurnCancelRequest::new(address, "external-cancel", None))
        .await
        .unwrap();
    assert!(matches!(receipt.outcome, TurnCancelOutcome::Requested(_)));
    let terminal = resolver
        .peek_await_event(&cancel_key)
        .await
        .unwrap()
        .expect("external cancellation reaches the Local turn gate");
    assert_eq!(
        host.await_await_event(&cancel_key, CancellationToken::new(), None)
            .await
            .unwrap(),
        terminal
    );
}

#[tokio::test]
async fn turn_control_durable_journaled_binding_remains_run_scoped() {
    let dir = tempfile::tempdir().expect("temporary database directory");
    let host = SqliteEffectHost::open(&dir.path().join("effects.sqlite"))
        .await
        .unwrap();
    let address = TurnAddress::new("durable-session", "durable-turn");
    let scoped = host.scoped(address.execution_scope()).unwrap();
    let binding = host.turn_control_binding(&scoped).await.unwrap();
    match binding {
        TurnControlBinding::RunScoped {
            resolver,
            durable_cancel_after_llm,
        } => {
            assert!(durable_cancel_after_llm);
            let key = resolver
                .await_event_key(
                    &address.execution_scope(),
                    AwaitEventWaitIdentity::TurnCancelGate,
                )
                .await
                .unwrap();
            assert_eq!(host.peek_await_event(&key).await.unwrap(), None);
        }
        TurnControlBinding::HostOwned { .. } => {
            panic!("durable controller must own its journaled turn control")
        }
    }
}

#[tokio::test]
async fn native_turn_control_reopens_through_durable_core_authority() {
    let dir = tempfile::tempdir().expect("temporary durable-core directory");
    let factory = SqliteSessionStoreFactory::new(dir.path());
    let address = TurnAddress::new("native-reopen-session", "native-reopen-turn");
    let create = lash_core::SessionStoreCreateRequest {
        pending_observer_intents: Vec::new(),
        session_id: address.session_id.clone(),
        relation: lash_core::SessionRelation::Root,
        policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
    };
    let first_store = factory.create_store(&create).await.expect("create store");
    let first = TurnWorkDriver::for_session(
        Arc::new(NativeEffectHost::default()),
        address.session_id.clone(),
        first_store,
    )
    .request_cancel(TurnCancelRequest::new(
        address.clone(),
        "native-persistent-cancel",
        None,
    ))
    .await
    .expect("resolve persistent Native cancellation");
    assert!(matches!(first.outcome, TurnCancelOutcome::Requested(_)));

    let reopened = factory
        .open_existing_store_by_id(&address.session_id)
        .await
        .expect("reopen catalog")
        .expect("session exists");
    let repeated = TurnWorkDriver::for_session(
        Arc::new(NativeEffectHost::default()),
        address.session_id.clone(),
        reopened,
    )
    .request_cancel(TurnCancelRequest::new(
        address,
        "native-persistent-cancel",
        None,
    ))
    .await
    .expect("observe cancellation after fresh Native host reopen");
    assert!(matches!(
        repeated.outcome,
        TurnCancelOutcome::AlreadyRequested(ref evidence)
            if evidence.request_id == "native-persistent-cancel"
    ));
}
