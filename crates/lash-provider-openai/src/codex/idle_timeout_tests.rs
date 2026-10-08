use super::*;

#[tokio::test]
async fn codex_websocket_first_response_frame_emits_establishment_marker_first() {
    let ws = spawn_scripted_websocket(vec![ScriptedWsAction::Complete {
        response_id: "response",
        message_id: "message",
        text: "done",
    }])
    .await;
    let mut provider = websocket_test_provider(
        CodexTransport::Websocket,
        "http://127.0.0.1:9/unused".to_string(),
        ws.url.clone(),
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let event_sink = Arc::clone(&events);
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    req.stream_events = Some(lash_core::llm::types::LlmEventSender::new(move |event| {
        event_sink.lock_recover().push(event);
    }));

    provider
        .complete(
            req,
            &lash_core::provider::NoSlotDeliveries,
            &lash_core::provider::LiveCallHorizon::fixture(),
        )
        .await
        .expect("websocket response");

    assert!(matches!(
        events.lock_recover().first(),
        Some(lash_core::llm::types::LlmStreamEvent::Evidence(evidence))
            if evidence.response_started
    ));
}

#[tokio::test]
async fn codex_websocket_idle_before_response_start_emits_no_stream_events() {
    let idle_ready = Arc::new(Notify::new());
    let idle = ScriptedWsAction::IdleBeforeStart {
        ready: idle_ready.clone(),
    };
    let ws = spawn_scripted_websocket(vec![idle]).await;
    let mut provider = websocket_test_provider_with_chunk_timeout(
        CodexTransport::Websocket,
        "http://127.0.0.1:9/unused".to_string(),
        ws.url.clone(),
        Some(SCRIPTED_WEBSOCKET_IDLE_TIMEOUT_MS),
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let event_sink = Arc::clone(&events);
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    req.stream_events = Some(lash_core::llm::types::LlmEventSender::new(move |event| {
        event_sink.lock_recover().push(event);
    }));

    let completion = tokio::spawn(async move {
        provider
            .complete(
                req,
                &lash_core::provider::NoSlotDeliveries,
                &lash_core::provider::LiveCallHorizon::fixture(),
            )
            .await
    });
    advance_scripted_websocket_idle_timeout(&idle_ready).await;
    let error = tokio::time::timeout(Duration::from_secs(5), completion)
        .await
        .expect("idle WebSocket completion must not hang")
        .expect("join idle WebSocket completion")
        .expect_err("idle before response start must time out");

    assert_eq!(
        error.code.as_ref().map(|code| code.to_string()),
        Some("lash:websocket_idle_timeout".to_string())
    );
    assert!(
        events.lock_recover().is_empty(),
        "no stream event may commit before the first response frame"
    );
}

#[tokio::test]
async fn codex_scripted_websocket_idle_before_start_falls_back_to_sse() {
    let idle_ready = Arc::new(Notify::new());
    let idle = ScriptedWsAction::IdleBeforeStart {
        ready: idle_ready.clone(),
    };
    let ws = spawn_scripted_websocket(vec![idle]).await;
    let http = spawn_http_sse("resp_http", "msg_http", "fallback").await;
    let mut provider = websocket_test_provider_with_chunk_timeout(
        CodexTransport::Auto,
        http.url.clone(),
        ws.url.clone(),
        Some(SCRIPTED_WEBSOCKET_IDLE_TIMEOUT_MS),
    );

    let request = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    let completion = tokio::spawn(async move {
        provider
            .complete(
                request,
                &lash_core::provider::NoSlotDeliveries,
                &lash_core::provider::LiveCallHorizon::fixture(),
            )
            .await
    });
    advance_scripted_websocket_idle_timeout(&idle_ready).await;
    let response = tokio::time::timeout(Duration::from_secs(5), completion)
        .await
        .expect("idle WebSocket fallback must not hang")
        .expect("join idle WebSocket fallback")
        .expect("sse fallback response");

    assert_eq!(response.full_text(), "fallback");
    assert_eq!(ws.captured().len(), 1);
    assert_eq!(http.captured_len(), 1);
}

#[tokio::test]
async fn codex_scripted_websocket_idle_after_output_is_terminal_error() {
    let ws = spawn_scripted_websocket(vec![ScriptedWsAction::IdleAfterStart {
        message_id: "msg_1",
        text: "partial",
    }])
    .await;
    let http = spawn_http_sse("resp_http", "msg_http", "fallback").await;
    let mut provider = websocket_test_provider_with_chunk_timeout(
        CodexTransport::Auto,
        http.url.clone(),
        ws.url.clone(),
        Some(SCRIPTED_WEBSOCKET_IDLE_TIMEOUT_MS),
    );

    let err = provider
        .complete(
            request(vec![LlmMessage::text(LlmRole::User, "hello")]),
            &lash_core::provider::NoSlotDeliveries,
            &lash_core::provider::LiveCallHorizon::fixture(),
        )
        .await
        .expect_err("idle after output");

    assert_eq!(
        err.code.as_ref().map(|code| code.to_string()),
        Some("lash:websocket_idle_timeout".to_string())
    );
    assert_eq!(http.captured_len(), 0);
    assert_eq!(ws.captured().len(), 1);
}

#[tokio::test]
async fn codex_websocket_regular_frames_cannot_outlive_request_deadline() {
    // Non-terminal frames arrive well inside every idle/chunk window, so the
    // per-frame deadline never lapses. Only the absolute request deadline may
    // end the stream.
    let ws = spawn_scripted_websocket(vec![ScriptedWsAction::Heartbeat {
        interval: Duration::from_millis(10),
    }])
    .await;
    let mut provider = websocket_test_provider_with_timeouts(
        CodexTransport::Websocket,
        "http://127.0.0.1:9/unused".to_string(),
        ws.url.clone(),
        300,
        Some(60),
    );
    let started = std::time::Instant::now();

    let err = tokio::time::timeout(
        Duration::from_secs(5),
        provider.complete(
            request(vec![LlmMessage::text(LlmRole::User, "hello")]),
            &lash_core::provider::NoSlotDeliveries,
            &lash_core::provider::LiveCallHorizon::fixture(),
        ),
    )
    .await
    .expect("regular frames must not let the stream outlive the request deadline")
    .expect_err("a nonterminal heartbeat stream must end at the request deadline");

    assert_eq!(
        err.code.as_ref().map(|code| code.to_string()),
        Some("lash:timeout".to_string())
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the request deadline, not the outer test timeout, ended the stream"
    );
    assert_eq!(ws.captured().len(), 1);
}
