//! Chat termination on the workbench: a reply in prose ends a turn, and a
//! turn's reply renders once, whatever committed it.

use super::*;
use lash::SessionId;

/// The agent replies a page shows for `snapshot`: one per turn. A committed
/// reply stands for its turn; a live product reply shows only for a turn
/// that has not committed one.
fn shown_replies(snapshot: &StateReadSnapshot) -> Vec<String> {
    let mut turns = BTreeSet::new();
    let mut replies = Vec::new();
    for row in snapshot
        .transcript
        .iter()
        .filter(|row| row.suppressed.is_none() && row.provenance.is_turn_reply)
    {
        if let Some(turn_id) = &row.provenance.turn_id {
            turns.insert(turn_id.clone());
        }
        replies.push(row.content.text.clone());
    }
    for event in &snapshot.state.product_events.events {
        if let StreamItem::Message { message } = &event.item
            && let Some(ChatMessageProvenance::TurnOutput { turn_id }) = &message.provenance
            && turns.insert(turn_id.clone())
        {
            replies.push(message.text.clone());
        }
    }
    replies
}

/// The replies and the collapsed reasoning rows the settled page shows.
async fn settled_rows(state: &AppState) -> (Vec<String>, Vec<String>) {
    let snapshot = read_state(state, None)
        .await
        .expect("read the settled state");
    let reasoning = snapshot
        .transcript
        .iter()
        .filter(|row| !row.content.reasoning.is_empty())
        .map(|row| row.content.reasoning.join("\n"))
        .collect();
    (shown_replies(&snapshot), reasoning)
}

/// The settled report of `turn_id`, read lease-free.
async fn run_report(
    state: &AppState,
    session_id: &SessionId,
    turn_id: &TurnId,
) -> lash::TurnReport {
    state
        .core
        .session(session_id.clone())
        .durable()
        .await
        .expect("bind the durable session")
        .run(turn_id.clone().into())
        .output()
        .await
        .expect("the run's settled output")
        .result
}

/// The committed turn-reply rows of `turn_id`.
async fn committed_replies(
    state: &AppState,
    session_id: &SessionId,
    turn_id: &TurnId,
) -> Vec<String> {
    state
        .open_session(session_id, "test")
        .await
        .expect("open the session")
        .read_view()
        .transcript()
        .expect("a valid committed history")
        .visible()
        .filter(|row| {
            row.provenance.is_turn_reply && row.provenance.turn_id.as_ref() == Some(turn_id)
        })
        .map(|entry| crate::ChatRow::of(entry).content.text)
        .collect()
}

/// A reasoning part, then `text`.
fn reasoned_response(reasoning: &str, text: &str) -> lash::provider::LlmResponse {
    let mut response = text_response(text);
    response.parts.insert(
        0,
        lash::direct::LlmOutputPart::Reasoning {
            text: reasoning.to_string(),
            replay: None,
        },
    );
    response
}

/// A provider whose call `n` answers `respond(n)`, counting its calls.
fn counted_provider(
    respond: impl Fn(usize) -> lash::provider::LlmResponse + Send + Sync + 'static,
) -> (ProviderHandle, Arc<std::sync::atomic::AtomicUsize>) {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let provider = lash::testing::TestProvider::builder()
        .kind("workbench-harness")
        .complete({
            let calls = Arc::clone(&calls);
            move |_| {
                let response = respond(calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst));
                async move { Ok(response) }
            }
        })
        .build()
        .into_handle();
    (provider, calls)
}

/// Chat refuses a raw record as a finish, accepts answer text, and still
/// lets prose end the next turn: the session records a text finish schema
/// with prose ending a turn, session-wide (FIG-5156).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_finish_requires_text_session_wide_and_prose_still_ends_a_turn() {
    use std::sync::atomic::Ordering;

    let (provider, calls) = counted_provider(|call| {
        text_response(match call {
            0 => "<typescript>await control.finish({temperature: 15});</typescript>",
            1 => "<typescript>await control.finish(\"It is 15 °C.\");</typescript>",
            _ => "A prose follow-up.",
        })
    });
    let workbench = Workbench::builder(provider).build().await;
    let state = &workbench.state;
    run_turn(state, "weather?").await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "a record finish asks again"
    );
    run_turn(state, "and now?").await;
    assert_eq!(calls.load(Ordering::SeqCst), 3);

    let session = state
        .open_session(&state.current_session_id(), "test")
        .await
        .expect("open the chat");
    let recorded: lash::rlm::RlmRecordedConfig = session
        .read_view()
        .protocol_turn_options()
        .decode()
        .expect("the recorded RLM config");
    drop(session);
    let termination = recorded
        .termination
        .expect("chat states its termination session-wide");
    assert!(termination.prose_ends_turn());
    assert_eq!(
        recorded
            .finish_schema
            .as_ref()
            .expect("a text finish schema")
            .as_value(),
        &json!({"type": "string"})
    );
    let (replies, _) = settled_rows(state).await;
    assert_eq!(replies, vec!["It is 15 °C.", "A prose follow-up."]);
    workbench.shutdown().await;
}

/// A bare-prose reply ends the turn as its assistant message and leaves one
/// committed reply, which the settled page renders once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactive_bare_prose_termination_leaves_one_committed_agent_reply() {
    const REPLY: &str = "bare prose answer";
    let workbench = Workbench::replying(REPLY).await;
    let state = &workbench.state;
    let session_id = state.current_session_id();
    let turn_id = run_turn(state, "answer in prose").await;

    let report = run_report(state, &session_id, &turn_id).await;
    assert!(
        matches!(
            &report.outcome,
            lash::TurnOutcome::Finished(lash::TurnFinish::AssistantMessage { text })
                if text == REPLY
        ),
        "a bare prose reply ends the turn as its message: {:?}",
        report.outcome
    );
    assert_eq!(
        committed_replies(state, &session_id, &turn_id).await,
        vec![REPLY.to_string()],
        "one committed reply for the turn"
    );
    let (replies, _) = settled_rows(state).await;
    assert_eq!(replies, vec![REPLY.to_string()], "the page renders it once");
    workbench.shutdown().await;
}

/// The same termination with reasoning attached: the RLM protocol commits
/// the reasoned answer itself and the runtime mints no copy of its own, so
/// that message is the reply's only durable copy. It renders once, and the
/// reasoning keeps its own collapsed row (FIG-1406).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bare_prose_reply_with_reasoning_renders_its_committed_prose_once() {
    const REPLY: &str = "FIG-1406 reasoned prose answer";
    const REASONING: &str = "FIG-1406 private deliberation";
    let (provider, _) = counted_provider(|_| reasoned_response(REASONING, REPLY));
    let workbench = Workbench::builder(provider).build().await;
    let state = &workbench.state;
    let session_id = state.current_session_id();
    let turn_id = run_turn(state, "answer in prose, thinking first").await;

    let report = run_report(state, &session_id, &turn_id).await;
    assert!(
        matches!(
            &report.outcome,
            lash::TurnOutcome::Finished(lash::TurnFinish::AssistantMessage { text })
                if text == REPLY
        ),
        "{:?}",
        report.outcome
    );
    let committed = state
        .open_session(&session_id, "test")
        .await
        .expect("open the session")
        .read_view()
        .messages()
        .iter()
        .filter(|message| {
            message_role(message) == "assistant" && message_text(message).contains(REPLY)
        })
        .count();
    assert_eq!(committed, 1, "the reasoned reply commits once");
    let (replies, reasoning) = settled_rows(state).await;
    assert_eq!(replies, vec![REPLY.to_string()]);
    assert!(
        !replies.iter().any(|reply| reply.contains(REASONING)),
        "reasoning stays out of the chat rows: {replies:?}"
    );
    assert_eq!(reasoning, vec![REASONING.to_string()]);
    workbench.shutdown().await;
}

/// The protocol prose a turn commits before its answer stays out of the
/// chat: one turn projects one agent row, its reply, and each iteration
/// keeps its own collapsed reasoning row (FIG-1406, FIG-984).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mid_turn_protocol_prose_stays_out_of_the_chat_rows() {
    const MID_TURN_PROSE: &str = "FIG-1406 mid-turn thinking out loud";
    const FINAL_REPLY: &str = "FIG-1406 answer after a code step";
    let (provider, _) = counted_provider(|call| {
        let text = match call {
            0 => format!("{MID_TURN_PROSE}\n<typescript>\nprint(\"step\");\n</typescript>"),
            _ => FINAL_REPLY.to_string(),
        };
        reasoned_response(&format!("FIG-1406 reasoning {call}"), &text)
    });
    let workbench = Workbench::builder(provider).build().await;
    let state = &workbench.state;
    run_turn(state, "take a step, then answer").await;
    let (replies, reasoning) = settled_rows(state).await;
    assert_eq!(
        replies,
        vec![FINAL_REPLY.to_string()],
        "one turn projects exactly one agent row: its reply"
    );
    assert_eq!(
        reasoning,
        vec![
            "FIG-1406 reasoning 0".to_string(),
            "FIG-1406 reasoning 1".to_string(),
        ],
        "each iteration keeps its own collapsed reasoning row"
    );
    workbench.shutdown().await;
}

/// A host restart re-follows every claimed turn and settles it again
/// (`resume_turn_followers`). The reply that settlement publishes is named by
/// its turn, so the second settlement republishes the same row rather than
/// a second reply (FIG-5086).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resettled_turn_publishes_its_reply_once() {
    let trace = Arc::new(RecordingTrace::default());
    let workbench = Workbench::builder(replying_provider("the one reply"))
        .trace_sink(Arc::clone(&trace) as Arc<dyn TraceSink>)
        .build()
        .await;
    let state = &workbench.state;
    let session_id = state.current_session_id();
    let turn_id = run_turn(state, "answer once").await;

    // The claim a restarted host finds in its ledger for a settled turn. A
    // restarted host has no follower on the run: a follower still ending
    // would hold the run, and the resumed one would leave the claim to it.
    wait_for_run_let_go(state, &session_id, &turn_id).await;
    state.track_turn(&session_id, &turn_id);
    crate::turns::resume_turn_followers(state).await;
    wait_for_turn_released(state, &session_id, &turn_id).await;
    assert_eq!(
        trace.custom("user_turn.completed").len(),
        2,
        "the turn settled twice: once followed, once re-followed"
    );

    let replies = state
        .messages_snapshot()
        .into_iter()
        .filter(|message| {
            matches!(
                &message.provenance,
                Some(ChatMessageProvenance::TurnOutput { turn_id: owner }) if *owner == turn_id
            )
        })
        .count();
    assert_eq!(replies, 1, "a re-settled turn publishes its reply once");
    workbench.shutdown().await;
}
