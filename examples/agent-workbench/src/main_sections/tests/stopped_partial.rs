//! The stopped-partial panel's routes (ADR 0114 §5.4, §6.11): discard is an
//! ordinary send and leaves lash's backtrack default, continue in context
//! sends the helper's ordinary input through `send()`, and both read the
//! partial durably, as a page does after a reconnect.

use super::*;

const STREAMED: &str = "The answer is fort";

fn sealed(items: Vec<lash::PartialItem>) -> lash::StoppedPartial {
    lash::StoppedPartial::seal(
        lash::StoppedPartialId {
            session_id: SessionId::from("workbench-session"),
            root: TurnId::from("stopped"),
            turn_id: TurnId::from("stopped"),
            base: lash::CaptureBase(0),
            sealed_through: 4,
        },
        lash::StopReason::UserCancel,
        false,
        lash::CaptureCoverage::Complete,
        items,
    )
    .expect("seal a partial")
}

/// The id the capture reducer mints: `{invocation}/{epoch}/{kind}/{key}`.
fn item_id(kind: &str, key: &str) -> lash::PartialItemId {
    lash::PartialItemId(format!("llm/1/{kind}/{key}"))
}

fn text_item() -> lash::PartialItem {
    lash::PartialItem::Text {
        id: item_id("text", "b0"),
        state: lash::CutState::Interrupted,
        text: STREAMED.to_string(),
    }
}

fn reasoning_item() -> lash::PartialItem {
    lash::PartialItem::Reasoning {
        id: item_id("reasoning", "r0"),
        state: lash::CutState::Complete,
        summary: "weighing forty".to_string(),
    }
}

fn texts(input: &[lash::InputItem]) -> Vec<&str> {
    input
        .iter()
        .filter_map(|item| match item {
            lash::InputItem::Text { text } => Some(text.as_str()),
            lash::InputItem::Attachment { .. } => None,
        })
        .collect()
}

#[test]
fn the_default_selection_includes_text_and_leaves_reasoning_to_the_user() {
    let partial = sealed(vec![text_item(), reasoning_item()]);
    let reasoning = item_id("reasoning", "r0");
    assert_eq!(
        default_choices(&partial),
        BTreeMap::from([(item_id("text", "b0").0, lash::ItemChoice::Include)])
    );
    match preview(&partial, &BTreeMap::new(), "") {
        ResubmissionPreview::Refused { error } => assert_eq!(
            error,
            ResubmissionRefusal {
                code: "selection_incomplete",
                message: lash::ResubmissionError::SelectionIncomplete {
                    items: vec![reasoning.clone()],
                }
                .to_string(),
                items: vec![reasoning.0.clone()],
            }
        ),
        other => panic!("reasoning needs a choice: {other:?}"),
    }
    let choices = BTreeMap::from([(reasoning.0.clone(), lash::ItemChoice::Quote)]);
    match preview(&partial, &choices, "  go on  ") {
        ResubmissionPreview::Ready { input, omissions } => {
            let texts = texts(&input);
            assert_eq!(texts.len(), 2);
            assert!(texts[0].starts_with(lash::RESUBMISSION_PREAMBLE));
            assert!(texts[0].contains(STREAMED));
            assert!(texts[0].contains("quoted_reasoning"));
            assert_eq!(texts[1], "go on");
            assert!(omissions.omitted.is_empty());
        }
        other => panic!("a quoted reasoning item renders: {other:?}"),
    }
}

#[test]
fn omissions_are_reported_and_nothing_to_send_is_refused() {
    let partial = sealed(vec![text_item()]);
    let text = item_id("text", "b0");
    let omit = BTreeMap::from([(text.0.clone(), lash::ItemChoice::Omit)]);
    match preview(&partial, &omit, "") {
        ResubmissionPreview::Refused { error } => assert_eq!(error.code, "empty"),
        other => panic!("nothing to send: {other:?}"),
    }
    match preview(&partial, &omit, "only this") {
        ResubmissionPreview::Ready { input, omissions } => {
            assert_eq!(texts(&input), vec!["only this"]);
            assert_eq!(omissions.omitted.len(), 1);
            assert_eq!(omissions.omitted[0].item, text);
        }
        other => panic!("the follow-up alone is sent: {other:?}"),
    }
    let include_reasoning =
        BTreeMap::from([(item_id("text", "elsewhere").0, lash::ItemChoice::Include)]);
    match preview(&partial, &include_reasoning, "") {
        ResubmissionPreview::Refused { error } => assert_eq!(error.code, "unknown_item"),
        other => panic!("a foreign item is refused: {other:?}"),
    }
}

#[test]
fn the_continue_request_is_strict() {
    let request: ContinueInContextRequest = serde_json::from_value(json!({
        "choices": { "llm/1/reasoning/r0": "quote" },
        "text": "go on",
        "preview": true,
    }))
    .expect("decode continue request");
    assert!(request.preview);
    assert_eq!(
        request.choices.get("llm/1/reasoning/r0"),
        Some(&lash::ItemChoice::Quote)
    );
    assert!(serde_json::from_value::<ContinueInContextRequest>(json!({ "resend": true })).is_err());
    assert!(
        serde_json::from_value::<ContinueInContextRequest>(json!({
            "choices": { "item": "rewrite" }
        }))
        .is_err()
    );
}

/// The discard handler, as the page's script defines it.
fn discard_handler() -> &'static str {
    let html = crate::ui::INDEX_HTML;
    let start = html
        .find("stoppedPartialDiscard.addEventListener(\"click\"")
        .expect("the discard control has a handler");
    let end = html[start..].find("});").expect("the handler ends") + start;
    &html[start..end]
}

#[test]
fn the_panel_reads_and_continues_through_its_routes_and_discard_sends_nothing() {
    let html = crate::ui::INDEX_HTML;
    assert!(html.contains("id=\"stoppedPartialPanel\""));
    assert!(html.contains("resubmit as new input"));
    assert!(html.contains("\"/api/turns/\" + encodeURIComponent(turnId) + \"/stopped-partial\""));
    assert!(html.contains("stoppedPartialPath(shown.turnId, \"/continue\")"));
    assert!(html.contains("globalThis.refreshStoppedPartial?.(event.turn_id)"));
    assert!(html.contains("globalThis.syncStoppedPartialFromSnapshot?.(state)"));
    let discard = discard_handler();
    assert!(!discard.contains("fetch("), "discard asks lash for nothing");
    assert!(!discard.contains("postStoppedPartialContinue"));
}

/// A provider whose first call streams [`STREAMED`] and hangs until the turn
/// is stopped. Every later call records its request's text and finishes.
fn stop_mid_stream_provider() -> (
    lash::provider::ProviderHandle,
    mpsc::UnboundedReceiver<()>,
    Arc<Mutex<Vec<String>>>,
) {
    let (streamed_tx, streamed_rx) = mpsc::unbounded_channel();
    let later = Arc::new(Mutex::new(Vec::new()));
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let provider = lash::testing::TestProvider::builder()
        .kind("workbench-stopped-partial")
        .requires_streaming(true)
        .complete({
            let later = Arc::clone(&later);
            move |request| {
                let call = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let streamed_tx = streamed_tx.clone();
                let later = Arc::clone(&later);
                async move {
                    if call == 0 {
                        let stream = request.stream_events.expect("stream events");
                        let block = lash::direct::StreamBlockIdentity::new("text:0", 0);
                        stream.send(lash::direct::LlmStreamEvent::TextBlockStart {
                            block: block.clone(),
                        });
                        stream.send(lash::direct::LlmStreamEvent::Delta {
                            block,
                            text: STREAMED.to_string(),
                        });
                        let _ = streamed_tx.send(());
                        std::future::pending::<()>().await;
                        unreachable!("the stop drops the provider call");
                    }
                    let context = request
                        .messages
                        .iter()
                        .flat_map(|message| message.blocks.iter())
                        .filter_map(|block| match block {
                            lash::provider::LlmContentBlock::Text { text, .. } => {
                                Some(text.to_string())
                            }
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    later.lock_recover().push(context);
                    Ok(crate::tests::text_response(
                        "<typescript>\nfinish(\"ok\");\n</typescript>",
                    ))
                }
            }
        })
        .build()
        .into_handle();
    (provider, streamed_rx, later)
}

async fn send(state: &AppState, text: &str) -> TurnId {
    let Json(accepted) = send_turn(
        State(state.clone()),
        Query(SessionQuery::default()),
        Json(TurnRequest {
            text: text.to_string(),
            model: Some("test-model".to_string()),
            model_variant: None,
            attachment_id: None,
        }),
    )
    .await
    .expect("send admitted");
    crate::tests::started_turn_id(&accepted)
}

async fn read(state: &AppState, turn_id: &TurnId) -> StoppedPartialResponse {
    let Json(read) = read_stopped_partial(
        AxumPath(turn_id.to_string()),
        State(state.clone()),
        Query(SessionQuery::default()),
    )
    .await
    .expect("read the stopped partial");
    read
}

fn history_has_assistant_quote(session: &lash::LashSession) -> bool {
    session.read_view().messages().iter().any(|message| {
        lash::message_role(message) == "assistant" && lash::message_text(message).contains(STREAMED)
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn discard_backtracks_and_continue_in_context_sends_only_through_send() {
    let (provider, mut streamed, later) = stop_mid_stream_provider();
    let double = crate::tests::test_double_backend(0x0433).await;
    let state = crate::tests::queued_send_test_state(&double, provider).await;
    let session_id = state.current_session_id();

    // A turn streams prose, and the user aborts it.
    let stopped = send(&state, "what is the answer?").await;
    tokio::time::timeout(Duration::from_secs(10), streamed.recv())
        .await
        .expect("the provider streamed")
        .expect("stream signal");
    let (_, Json(cancelled)) = cancel_turn(
        State(state.clone()),
        Query(TurnCancelQuery {
            session: SessionQuery::default(),
            mode: WorkbenchTurnCancelMode::Abort,
        }),
    )
    .await
    .expect("abort the turn");
    assert!(cancelled.accepted);
    crate::tests::wait_for_turn_released(&state, &session_id, &stopped, Duration::from_secs(20))
        .await;

    // The panel's read is durable: it answers the same after the page lost
    // its live stream, which is all a reconnect changes.
    let AvailableStoppedPartial {
        partial,
        summary,
        preview,
        ..
    } = match read(&state, &stopped).await {
        StoppedPartialResponse::Available(available) => *available,
        other => panic!("the aborted turn has a partial, read {other:?}"),
    };
    assert_eq!(summary, partial.summary());
    match partial.items.as_slice() {
        [lash::PartialItem::Text { state, text, .. }] => {
            assert_eq!(*state, lash::CutState::Interrupted);
            assert_eq!(text, STREAMED);
        }
        items => panic!("one interrupted text item, got {items:?}"),
    }
    assert!(matches!(preview, ResubmissionPreview::Ready { .. }));
    let session = state
        .core
        .session(session_id.clone())
        .open()
        .await
        .expect("open the session");
    assert!(!history_has_assistant_quote(&session));

    // Discard: the page makes no request, and the next message is an
    // ordinary send whose context is lash's backtrack.
    let ordinary = send(&state, "never mind").await;
    crate::tests::wait_for_turn_released(&state, &session_id, &ordinary, Duration::from_secs(20))
        .await;
    {
        let requests = later.lock_recover();
        let context = requests.last().expect("the ordinary turn asked the model");
        assert!(context.contains("never mind"));
        assert!(!context.contains(STREAMED), "discard leaked: {context}");
    }
    assert!(matches!(
        read(&state, &ordinary).await,
        StoppedPartialResponse::NotStopped
    ));

    // Continue in context, read again from the store: the route renders the
    // helper's input and sends it through `send()`.
    let response = continue_stopped_partial(
        AxumPath(stopped.to_string()),
        State(state.clone()),
        Query(SessionQuery::default()),
        Json(ContinueInContextRequest {
            text: "please finish".to_string(),
            ..ContinueInContextRequest::default()
        }),
    )
    .await
    .expect("continue in context");
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read the continue response");
    let sent: Value = serde_json::from_slice(&body).expect("decode the continue response");
    assert_eq!(sent["status"], "sent");
    let continued = TurnId::from(sent["turn_id"].as_str().expect("the continued turn"));
    crate::tests::wait_for_turn_released(&state, &session_id, &continued, Duration::from_secs(20))
        .await;
    {
        let requests = later.lock_recover();
        let context = requests.last().expect("the continued turn asked the model");
        assert!(context.contains(lash::RESUBMISSION_PREAMBLE));
        assert!(context.contains(STREAMED));
        assert!(context.contains("please finish"));
    }
    let session = state
        .core
        .session(session_id.clone())
        .open()
        .await
        .expect("reopen the session");
    assert!(
        !history_has_assistant_quote(&session),
        "the stopped prose is in history only as the user's input"
    );
    assert!(
        crate::tests::product_user_rows(&state, &session_id)
            .iter()
            .any(
                |(id, text)| *id == workbench_turn_user_message_id(&continued)
                    && text.contains(STREAMED)
                    && text.ends_with("please finish")
            )
    );
}
