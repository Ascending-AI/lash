//! Continuation rejection recovery and the no-output retry rule.
use super::*;

#[tokio::test]
async fn codex_code_only_continuation_rejections_recover_before_output() {
    for (code, handshakes) in [
        ("previous_response_not_found", 1),
        ("websocket_connection_limit_reached", 2),
    ] {
        let ws = spawn_scripted_websocket(vec![
            ScriptedWsAction::Complete {
                response_id: "resp_1",
                message_id: "msg_1",
                text: "answer",
            },
            ScriptedWsAction::RecordedFrames {
                frames: vec![json!({"type": "error", "error": {"code": code}}).to_string()],
                close_after_frames: false,
            },
            ScriptedWsAction::Complete {
                response_id: "resp_2",
                message_id: "msg_2",
                text: "recovered",
            },
        ])
        .await;
        let mut provider = websocket_test_provider(
            CodexTransport::WebsocketCached,
            "http://127.0.0.1:9/unused".to_string(),
            ws.url.clone(),
        );

        provider
            .complete(
                request(vec![LlmMessage::text(LlmRole::User, "hello")]),
                &lash_core::provider::NoSlotDeliveries,
                &lash_core::provider::LiveCallHorizon::fixture(),
            )
            .await
            .expect("first response");
        let second = request(vec![
            LlmMessage::text(LlmRole::User, "hello"),
            assistant_message_with_meta(&provider.route_identity("gpt-5.4"), "msg_1", "answer"),
            LlmMessage::text(LlmRole::User, "next"),
        ]);
        let full_body = provider.build_request_body(&second, true).unwrap();
        let response = provider
            .complete(
                second,
                &lash_core::provider::NoSlotDeliveries,
                &lash_core::provider::LiveCallHorizon::fixture(),
            )
            .await
            .expect("stale retry response");

        assert_eq!(response.full_text(), "recovered");
        assert!(
            response
                .http_summary
                .as_deref()
                .unwrap_or_default()
                .contains("retry_after_stale=true")
        );
        let captured = ws.captured();
        assert_eq!(captured.len(), 3);
        assert_eq!(captured[1]["previous_response_id"], "resp_1");
        assert!(captured[2].get("previous_response_id").is_none());
        assert_eq!(captured[2]["input"], full_body["input"]);
        assert_eq!(
            ws.handshakes().len(),
            handshakes,
            "only the connection-limit rejection rotates a live connection"
        );
    }
}

#[tokio::test]
async fn continuation_rejections_never_retry_after_output() {
    for code in [
        "previous_response_not_found",
        "websocket_connection_limit_reached",
    ] {
        let ws = spawn_scripted_websocket(vec![
        ScriptedWsAction::Complete {
            response_id: "resp_1",
            message_id: "msg_1",
            text: "answer",
        },
        ScriptedWsAction::RecordedFrames {
            frames: vec![
                json!({"type": "response.output_text.delta", "item_id": "msg_partial", "delta": "partial"}).to_string(),
                json!({"type": "error", "error": {"code": code}}).to_string(),
            ],
            close_after_frames: false,
        },
        ScriptedWsAction::Complete {
            response_id: "resp_2",
            message_id: "msg_2",
            text: "recovered",
        },
    ])
    .await;
        let mut provider = websocket_test_provider(
            CodexTransport::WebsocketCached,
            "http://127.0.0.1:9/unused".to_string(),
            ws.url.clone(),
        );

        provider
            .complete(
                request(vec![LlmMessage::text(LlmRole::User, "hello")]),
                &lash_core::provider::NoSlotDeliveries,
                &lash_core::provider::LiveCallHorizon::fixture(),
            )
            .await
            .expect("first response");
        let second = request(vec![
            LlmMessage::text(LlmRole::User, "hello"),
            assistant_message_with_meta(&provider.route_identity("gpt-5.4"), "msg_1", "answer"),
            LlmMessage::text(LlmRole::User, "next"),
        ]);
        let response = provider
            .complete(
                second,
                &lash_core::provider::NoSlotDeliveries,
                &lash_core::provider::LiveCallHorizon::fixture(),
            )
            .await
            .expect_err("output forbids an internal continuation retry");
        assert!(response.output_started);
        assert_eq!(
            response
                .partial_response
                .expect("partial response")
                .full_text(),
            "partial"
        );
        assert_eq!(ws.captured().len(), 2);
        assert_eq!(ws.handshakes().len(), 1);
    }
}

#[tokio::test]
async fn connection_limit_rejection_rotates_even_without_cached_context() {
    let ws = spawn_scripted_websocket(vec![
        ScriptedWsAction::RecordedFrames {
            frames: vec![
                json!({"type": "error", "code": "websocket_connection_limit_reached"}).to_string(),
            ],
            close_after_frames: false,
        },
        ScriptedWsAction::Complete {
            response_id: "resp_fresh",
            message_id: "msg_fresh",
            text: "recovered",
        },
    ])
    .await;
    let mut provider = websocket_test_provider(
        CodexTransport::Websocket,
        "http://127.0.0.1:9/unused".into(),
        ws.url.clone(),
    );
    let response = provider
        .complete(
            request(vec![LlmMessage::text(LlmRole::User, "hello")]),
            &lash_core::provider::NoSlotDeliveries,
            &lash_core::provider::LiveCallHorizon::fixture(),
        )
        .await
        .expect("connection-limit retry");
    assert_eq!(response.full_text(), "recovered");
    assert_eq!(ws.handshakes().len(), 2);
    let captured = ws.captured();
    assert_eq!(captured.len(), 2);
    assert_eq!(
        captured[0], captured[1],
        "retry sends the same full context"
    );
}
