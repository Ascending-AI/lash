use super::*;

/// FIG-3206: the live product stream must carry the operator's send exactly
/// once, all the way through settlement.
///
/// The workbench publishes a UI-owned `workbench-user:{turn_id}` row when it
/// admits a send, and at settlement it republishes every committed
/// `m_ingress_*` message so a page that joined mid-turn sees the durable copy.
/// The turn's opening input is one of those commits, and the browser
/// deduplicates by message id alone, so the republish appended a second copy of
/// the operator's own words above the reply until the next `/api/state`
/// rebuilt the transcript. A mid-turn injected input has no UI row standing in
/// for it and must still arrive on the live stream exactly once.
#[tokio::test]
async fn the_live_stream_carries_one_user_row_per_input_through_settlement() {
    let data_dir = tempfile::tempdir().expect("fig3206 live stream tempdir");
    let (provider_entered_tx, mut provider_entered_rx) = mpsc::unbounded_channel();
    let provider_release = Arc::new(tokio::sync::Notify::new());
    let provider_release_for_completion = Arc::clone(&provider_release);
    let response_index = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let response_index_for_completion = Arc::clone(&response_index);
    let provider = lash::testing::TestProvider::builder()
        .kind("fig3206-live-stream-user-rows")
        .complete(move |_| {
            let provider_entered_tx = provider_entered_tx.clone();
            let provider_release = Arc::clone(&provider_release_for_completion);
            let response_index = Arc::clone(&response_index_for_completion);
            async move {
                let call = response_index.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let _ = provider_entered_tx.send(call);
                if call == 0 {
                    provider_release.notified().await;
                }
                Ok(match call {
                    0 => text_response(
                        "<typescript>\nprint(\"work before the boundary\");\n</typescript>",
                    ),
                    _ => text_response("<typescript>\nfinish(\"settled answer\");\n</typescript>"),
                })
            }
        })
        .build()
        .into_handle();
    let state = recoverable_chat_test_state_with_provider(data_dir.path(), 16, provider).await;
    let session_id = state.current_session_id();
    let sent_text = "the send the operator typed";
    let injected_text = "the input injected mid-turn";

    let Json(accepted) = send_turn(
        State(state.clone()),
        Query(SessionQuery::default()),
        Json(TurnRequest {
            text: sent_text.to_string(),
            model: Some("test-model".to_string()),
            model_variant: None,
            attachment_id: None,
        }),
    )
    .await
    .expect("send turn through the production handler");
    let turn_id = started_turn_id(&accepted);
    // The session's engine runs the turn; the send's follower settles it.
    let turn = tokio::spawn({
        let state = state.clone();
        let session_id = session_id.clone();
        let turn_id = turn_id.clone();
        async move {
            wait_for_turn_released(&state, &session_id, &turn_id, Duration::from_secs(30)).await;
        }
    });

    assert_eq!(
        provider_entered_rx.recv().await,
        Some(0),
        "the first provider call must be blocked so the injection lands mid-turn"
    );
    let Json(injected) = enqueue_turn_input(
        State(state.clone()),
        Query(SessionQuery::default()),
        Json(TurnInputRequest {
            text: injected_text.to_string(),
            ingress: TurnInputIngressRequest::ActiveTurn,
        }),
    )
    .await
    .expect("inject an input into the running turn");
    let injected_committed_id = format!("m_ingress_{}", injected.input_id);

    provider_release.notify_waiters();
    turn.await.expect("the submitted turn task");

    // What the browser holds after settlement, without a snapshot rebuild: the
    // product-event log is the live stream.
    let live_rows = product_user_rows(&state, &session_id);
    assert_eq!(
        live_rows,
        vec![
            (
                workbench_turn_user_message_id(&turn_id),
                sent_text.to_string()
            ),
            (injected_committed_id.clone(), injected_text.to_string()),
        ],
        "the live stream must carry the send once, on the UI-owned row, and the \
         injected input once, on its committed row"
    );
    assert!(
        live_rows
            .iter()
            .all(|(id, _)| !(id.starts_with("m_ingress_") && id != &injected_committed_id)),
        "the settlement republish must not add the opening input's committed \
         copy beside the UI-owned row it duplicates: {live_rows:?}"
    );

    // The snapshot the next `/api/state` builds agrees, so the live page never
    // had to be corrected by a rebuild.
    let Json(settled) = Box::pin(app_state(
        State(state.clone()),
        Query(SessionQuery::default()),
    ))
    .await
    .expect("materialize the settled snapshot");
    assert_eq!(
        user_rows(&settled)
            .into_iter()
            .map(|(_, text)| text)
            .collect::<Vec<_>>(),
        vec![sent_text.to_string(), injected_text.to_string()],
        "the snapshot projection and the live stream must agree"
    );
}
