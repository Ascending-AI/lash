use std::sync::{Arc, Mutex};

use axum::body::to_bytes;
use lash::LashCore;
use lash::direct::LlmOutputPart;
use lash::provider::LlmResponse;

use super::*;
use crate::db::AppDb;

#[tokio::test]
async fn message_route_streams_session_observations_with_mock_provider() {
    let temp = tempfile::tempdir().expect("tempdir");
    let data_dir = temp.path();
    let provider = lash::testing::TestProvider::builder()
        .kind("agent-service-route-mock")
        .complete(|_request| async {
            let text = r#"<typescript>
finish("done through route");
</typescript>"#;
            Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: text.to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            })
        })
        .build()
        .into_handle();
    let double = crate::state::test_support::test_double().await;
    let backend = double.lash_backend();
    let factory = crate::rlm_factory(&backend);
    let core = LashCore::rlm_builder(backend, factory)
        .serve_test_llm_profile(
            provider,
            lash::LlmProfileMetadata::builder("mock-model")
                .context_window_tokens(200_000)
                .build()
                .expect("model spec"),
        )
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "agent-service-test",
            "test",
        ))
        .expect("core");
    let db = Arc::new(Mutex::new(
        AppDb::open(&data_dir.join("app.db")).expect("app db"),
    ));
    let state = AppStateData::new(
        core,
        Arc::clone(&db),
        "mock-model".to_string(),
        None,
        double.connection(),
    );
    let chat = state
        .with_db(|db| db.create_chat("route replay", "mock-model", None))
        .await
        .expect("create chat");
    // Boxed: the handler's future is large enough that holding it inline
    // in a test frame trips `clippy::large_futures` under the `restate`
    // feature, where this target is only ever built.
    let response = Box::pin(send_message(
        State(state.clone()),
        AxumPath(chat.id.clone()),
        test_remote_headers(),
        Json(SendMessageRequest {
            text: "exercise live replay".to_string(),
            board: crate::board::default_board(),
            model: None,
            model_variant: Default::default(),
        }),
    ))
    .await
    .expect("send message");
    let accept: Negotiation = serde_json::from_str(
        response
            .headers()
            .get("x-lash-protocol-accept")
            .expect("protocol Accept response header")
            .to_str()
            .expect("protocol Accept header text"),
    )
    .expect("protocol Accept JSON");
    assert_eq!(
        Negotiated::from_accept(REMOTE_PROTOCOL, &accept)
            .expect("valid protocol Accept")
            .selected(),
        lash::remote::REMOTE_PROTOCOL_VERSION
    );
    let turn_id = TurnId::fixture(
        response
            .headers()
            .get("x-lash-turn-id")
            .expect("turn id response header")
            .to_str()
            .expect("turn id header text"),
    );
    let input_id = lash::InputId::parse(
        response
            .headers()
            .get("x-lash-input-id")
            .expect("accepted input id")
            .to_str()
            .expect("input id header"),
    )
    .expect("input identity");
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("response body");
    let lines = std::str::from_utf8(&body)
        .expect("utf8")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("json line"))
        .collect::<Vec<_>>();

    assert!(
        lines
            .iter()
            .any(|line| line.get("type").and_then(serde_json::Value::as_str)
                == Some("replay_cursor")),
        "stream should expose an opaque live replay cursor: {lines:#?}"
    );
    assert!(
        lines
            .iter()
            .all(|line| { line.get("type").and_then(serde_json::Value::as_str) != Some("event") }),
        "stream should not expose legacy direct turn events: {lines:#?}"
    );
    assert!(
        lines.iter().any(|line| {
            line.get("type").and_then(serde_json::Value::as_str) == Some("observation")
                && line
                    .pointer("/event/type")
                    .and_then(serde_json::Value::as_str)
                    == Some("turn_activity")
                && line
                    .pointer("/event/activity/type")
                    .and_then(serde_json::Value::as_str)
                    == Some("final_value")
        }),
        "stream should contain remote observation turn activity: {lines:#?}"
    );
    assert!(
        lines.iter().any(|line| {
            line.get("type").and_then(serde_json::Value::as_str) == Some("message")
                && line
                    .pointer("/message/role")
                    .and_then(serde_json::Value::as_str)
                    == Some("assistant")
                && line
                    .pointer("/message/text")
                    .and_then(serde_json::Value::as_str)
                    == Some("done through route")
        }),
        "stream should include the persisted assistant message: {lines:#?}"
    );
    let durable = state
        .core()
        .session(SessionId::parse(&chat.id).expect("session id"))
        .durable()
        .await
        .expect("durable session");
    let recovered = durable
        .attach(input_id)
        .outcome()
        .await
        .expect("follow accepted input");
    assert_eq!(recovered.run(), Some(&turn_id));
    let cancelled = cancel_turn(
        State(state),
        AxumPath((chat.id, turn_id)),
        Json(CancelTurnRequest {
            request_id: Some("route-test-stop".to_string()),
            reason: Some("test completed turn".to_string()),
        }),
    )
    .await
    .expect("cancel endpoint");
    assert!(matches!(
        cancelled.0.outcome,
        CancelTurnOutcome::AlreadySettled
    ));
}

/// A queued input cancels by withdrawal: no run ever applies it.
#[tokio::test]
async fn cancel_turn_withdraws_a_still_queued_input() {
    let temp = tempfile::tempdir().expect("tempdir");
    let data_dir = temp.path();
    // The first turn's provider never answers, so its run stays in flight
    // and the second input waits queued behind it.
    let provider = lash::testing::TestProvider::builder()
        .kind("agent-service-cancel-test")
        .complete(|_request| async {
            std::future::pending::<Result<LlmResponse, lash::provider::LlmTransportError>>().await
        })
        .build()
        .into_handle();
    let double = crate::state::test_support::test_double().await;
    let core = crate::state::test_support::test_core_with_provider(&double, provider).await;
    let state = crate::state::test_support::test_state(
        &double,
        &core,
        AppDb::open(&data_dir.join("app.db")).expect("app db"),
    );
    let chat = state
        .with_db(|db| db.create_chat("cancel queued", "mock-model", None))
        .await
        .expect("create chat");
    let session = state
        .open_session(&chat.id, crate::state::test_support::mock_llm_profile())
        .await
        .expect("open session");
    let running_turn = TurnId::prefixed("agent-service-turn:", uuid::Uuid::new_v4());
    let _running = session
        .send(TurnInput::text("run forever"))
        .id(running_turn)
        .require_finish()
        .expect("legal turn shape")
        .await
        .expect("first input accepted");
    let queued_turn = TurnId::prefixed("agent-service-turn:", uuid::Uuid::new_v4());
    let _queued = session
        .send(TurnInput::text("still queued"))
        .id(queued_turn.clone())
        .require_finish()
        .expect("legal turn shape")
        .await
        .expect("queued input accepted");

    let cancelled = cancel_turn(
        State(state),
        AxumPath((chat.id, queued_turn)),
        Json(CancelTurnRequest {
            request_id: Some("route-test-queued-stop".to_string()),
            reason: Some("test queued input".to_string()),
        }),
    )
    .await
    .expect("cancel endpoint");
    assert!(
        matches!(cancelled.0.outcome, CancelTurnOutcome::Withdrawn),
        "a queued input cancels by withdrawal: {:?}",
        cancelled.0.outcome
    );
}
