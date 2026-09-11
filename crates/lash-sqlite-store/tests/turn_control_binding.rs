use std::sync::Arc;

use lash_core::facade_support::NativeRuntimeEffectController;
use lash_core::runtime::{TurnAddress, TurnCancelOutcome, TurnCancelRequest, TurnWorkDriver};
use lash_core::testing::conformance_support::{ActiveTurnControl, TurnCancelPeekIdentity};
use lash_core::{
    AwaitEventResolver, AwaitEventWaitIdentity, EffectHost, ExecutionScope, RuntimeErrorCode,
    ScopedEffectController, TurnControlBinding,
};
use lash_sqlite_store::SqliteEffectHost;
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
async fn process_scoped_physical_turn_start_gates_are_distinct_and_replayable() {
    let dir = tempfile::tempdir().expect("temporary database directory");
    let path = dir.path().join("physical-turn-peeks.sqlite");
    let host = SqliteEffectHost::open(&path).await.unwrap();
    let process_scope = ExecutionScope::process("process:subagent:physical-turn-peeks");
    let scoped = host.scoped(process_scope.clone()).unwrap();
    let root = TurnAddress::new(
        "session:subagent:physical-turn-peeks",
        "process:subagent:physical-turn-peeks",
    );
    let follow_on = TurnAddress::new(
        &root.session_id,
        "process:subagent:physical-turn-peeks:agent-frame:1",
    );

    for address in [&root, &follow_on, &root, &follow_on] {
        let active = ActiveTurnControl::new(&host, address.clone())
            .await
            .expect("create one physical turn's cancellation control");
        assert_eq!(
            active
                .observe_pending_cancel(&scoped, TurnCancelPeekIdentity::StartGate)
                .await
                .expect("journal one physical turn's start gate"),
            None
        );
    }

    let scope_id = process_scope
        .journal_identity()
        .expect("valid process scope")
        .key()
        .to_string();
    let connection = rusqlite::Connection::open(&path).expect("open journal for inspection");
    let mut statement = connection
        .prepare(
            "SELECT replay_key, envelope_hash, envelope_json \
             FROM runtime_effect_replay WHERE scope_id = ?1 ORDER BY replay_key",
        )
        .expect("prepare journal inspection");
    let rows = statement
        .query_map([scope_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .expect("query physical turn peeks")
        .collect::<Result<Vec<_>, _>>()
        .expect("read physical turn peeks");
    assert_eq!(rows.len(), 2, "same-turn redrive must reuse each row");
    assert_ne!(rows[0].0, rows[1].0, "physical turns need distinct keys");
    assert_ne!(
        rows[0].1, rows[1].1,
        "physical turn envelopes remain distinct"
    );
    let observed_turn_ids = rows
        .iter()
        .map(|(_, _, envelope)| {
            let canonical = serde_json::from_str::<serde_json::Value>(envelope)
                .expect("decode recorded canonical envelope");
            let canonical_json = canonical["json"]
                .as_str()
                .expect("canonical envelope contains source JSON");
            serde_json::from_str::<lash_core::RuntimeEffectEnvelope>(canonical_json)
                .expect("decode canonical peek envelope")
                .invocation
                .attribution
                .turn_id
                .expect("physical turn attribution")
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(observed_turn_ids, [root.turn_id, follow_on.turn_id].into());
}

#[tokio::test]
async fn shared_scope_physical_turn_start_gates_are_distinct_and_replayable() {
    let dir = tempfile::tempdir().expect("temporary database directory");
    let path = dir.path().join("shared-scope-physical-turn-peeks.sqlite");
    let host = SqliteEffectHost::open(&path).await.unwrap();
    let cases = [
        (
            ExecutionScope::turn("shared-turn-session", "shared-turn-root"),
            TurnAddress::new("shared-turn-session", "shared-turn-root"),
            TurnAddress::new("shared-turn-session", "shared-turn-root:agent-frame:1"),
        ),
        (
            ExecutionScope::queue_drain("shared-queue-session", "shared-queue-drain"),
            TurnAddress::new("shared-queue-session", "shared-queue-root"),
            TurnAddress::new("shared-queue-session", "shared-queue-follow-on"),
        ),
        (
            ExecutionScope::runtime_operation("shared-runtime-operation"),
            TurnAddress::new("shared-runtime-session", "shared-runtime-root"),
            TurnAddress::new("shared-runtime-session", "shared-runtime-follow-on"),
        ),
    ];

    for (scope, root, follow_on) in &cases {
        let scoped = host.scoped(scope.clone()).unwrap();
        for address in [root, root, follow_on, follow_on] {
            let active = ActiveTurnControl::new(&host, address.clone())
                .await
                .expect("create one physical turn's cancellation control");
            assert_eq!(
                active
                    .observe_pending_cancel(&scoped, TurnCancelPeekIdentity::StartGate)
                    .await
                    .expect("journal one physical turn's start gate"),
                None
            );
        }
    }

    let connection = rusqlite::Connection::open(&path).expect("open journal for inspection");
    for (scope, root, follow_on) in cases {
        let scope_id = scope
            .journal_identity()
            .expect("valid shared scope")
            .key()
            .to_string();
        let mut statement = connection
            .prepare(
                "SELECT replay_key, envelope_json FROM runtime_effect_replay \
                 WHERE scope_id = ?1 ORDER BY replay_key",
            )
            .expect("prepare shared-scope journal inspection");
        let rows = statement
            .query_map([scope_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .expect("query shared-scope physical turn peeks")
            .collect::<Result<Vec<_>, _>>()
            .expect("read shared-scope physical turn peeks");
        assert_eq!(rows.len(), 2, "same-frame replay must reuse each row");
        assert_ne!(rows[0].0, rows[1].0, "physical turns need distinct keys");
        let observed_turn_ids = rows
            .into_iter()
            .map(|(_, envelope)| {
                let canonical = serde_json::from_str::<serde_json::Value>(&envelope)
                    .expect("decode recorded canonical envelope");
                let canonical_json = canonical["json"]
                    .as_str()
                    .expect("canonical envelope contains source JSON");
                serde_json::from_str::<lash_core::RuntimeEffectEnvelope>(canonical_json)
                    .expect("decode canonical peek envelope")
                    .invocation
                    .attribution
                    .turn_id
                    .expect("physical turn attribution")
            })
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(observed_turn_ids, [root.turn_id, follow_on.turn_id].into());
    }
}
