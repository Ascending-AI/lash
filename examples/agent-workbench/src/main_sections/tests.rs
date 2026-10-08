use super::*;
use lash::TurnId;

#[cfg(test)]
#[path = "tests/support.rs"]
mod support;
use std::future::Future;
pub(crate) use support::*;
#[cfg(test)]
#[path = "tests/harness.rs"]
mod harness;
pub(crate) use harness::*;

#[cfg(test)]
#[path = "tests/after_turn_note.rs"]
mod after_turn_note_tests;
#[cfg(test)]
#[path = "tests/mail_payload.rs"]
mod mail_payload_tests;
#[cfg(test)]
#[path = "tests/multi_session.rs"]
mod multi_session_tests;
#[cfg(test)]
#[path = "tests/product_event_persistence.rs"]
mod product_event_persistence_tests;
#[cfg(test)]
#[path = "tests/projection_suites.rs"]
mod projection_suites_tests;
#[cfg(test)]
#[path = "tests/prompt_sections.rs"]
mod prompt_sections_tests;
#[cfg(test)]
#[path = "tests/recoverable_chat_failures.rs"]
mod recoverable_chat_failures_tests;
#[cfg(test)]
#[path = "tests/recoverable_chat.rs"]
mod recoverable_chat_tests;
#[cfg(test)]
#[cfg(test)]
#[path = "tests/typescript_dialect.rs"]
mod typescript_dialect_tests;

const STACK_BUDGET_BYTES: usize = 2 * 1024 * 1024;

#[test]
fn turn_routing_state_survives_web_process_reconstruction() {
    let temp = tempfile::tempdir().expect("tempdir");
    let session_path = temp.path().join("session-id");
    let turns_path = temp.path().join("active-turns.json");
    let sessions = WorkbenchSessions::persistent(session_path.clone()).expect("session ids");
    let session_id = sessions.current();
    let turns = ActiveTurns::persistent(turns_path.clone()).expect("active turns");
    turns.insert_with_prompt(
        &session_id,
        "durable-stop-turn",
        WorkbenchTurnKind::User,
        Some("actual restored prompt".into()),
        None,
    );
    let original_prompt = turns
        .prompt_for(&session_id, &TurnId::from("durable-stop-turn"))
        .expect("claimed UI prompt");
    drop(sessions);
    drop(turns);
    let recovered_ids = WorkbenchSessions::persistent(session_path).expect("recover ids");
    let recovered_turns = ActiveTurns::persistent(turns_path).expect("recover turns");
    assert_eq!(recovered_ids.current(), session_id);
    assert_eq!(
        recovered_turns
            .for_session(&session_id)
            .map(|active_turn| active_turn.address),
        Some(lash::TurnAddress::new(&session_id, "durable-stop-turn"))
    );
    let recovered_prompt = recovered_turns
        .prompt_for(&session_id, &TurnId::from("durable-stop-turn"))
        .expect("restored prompt");
    assert_eq!(recovered_prompt.text, "actual restored prompt");
    assert_eq!(recovered_prompt.attachment_id, None);
    assert_eq!(recovered_prompt.row_id, original_prompt.row_id);
    assert_eq!(recovered_prompt.at, original_prompt.at);
}

#[cfg(test)]
#[path = "tests/ui_contract.rs"]
mod ui_contract_tests;

#[test]
fn mail_received_account_contract_uses_slugs() {
    const ACCOUNT_SLUG_CONTRACT: &str = "`mail.Received.account` carries the account SLUG, not its display name: use the slug from the account enumeration (for example `work` or `personal`), not a display name such as `Work`, when filtering deliveries.";

    assert!(
        workbench_prompt().contains(ACCOUNT_SLUG_CONTRACT),
        "the workbench prompt must state the mail account slug contract"
    );
}

#[cfg(test)]
#[path = "tests/facade_homes.rs"]
mod facade_homes_tests;

#[test]
fn empty_model_variant_request_clears_selected_variant() {
    let selected_llm_profile = LlmProfileSelection {
        model: "x-ai/grok-build-0.1".to_string(),
        model_variant: Some("medium".to_string()),
    };

    assert_eq!(
        model_variant_for_request(&selected_llm_profile, None),
        Some("medium".to_string())
    );
    assert_eq!(
        model_variant_for_request(&selected_llm_profile, Some(" high ")),
        Some("high".to_string())
    );
    assert_eq!(
        model_variant_for_request(&selected_llm_profile, Some("")),
        None
    );
    assert_eq!(
        model_variant_for_request(&selected_llm_profile, Some("   ")),
        None
    );
}

#[cfg(test)]
#[path = "tests/attachments_usage.rs"]
mod attachments_usage_tests;
#[cfg(test)]
#[cfg(test)]
#[path = "tests/concurrent_send.rs"]
mod concurrent_send_tests;
#[cfg(test)]
#[path = "tests/no_progress_budget.rs"]
mod no_progress_budget_tests;
#[cfg(test)]
#[path = "tests/reference_transport.rs"]
mod reference_transport_tests;
#[cfg(test)]
#[path = "tests/reset_chat.rs"]
mod reset_chat_tests;
#[cfg(test)]
#[path = "tests/session_fence.rs"]
mod session_fence_tests;
#[cfg(test)]
#[path = "tests/store_maintenance.rs"]
mod store_maintenance_tests;
#[cfg(test)]
#[path = "tests/trigger_retention.rs"]
mod trigger_retention_tests;

/// FIG-5045: composer text always uses chat admission and model validation.
#[test]
fn slash_text_uses_chat_admission_and_model_validation() {
    use std::io::Write;
    let node = std::env::var_os("LASH_WORKBENCH_TEST_NODE").unwrap_or_else(|| "node".into());
    let script = r#"
const assert = require('node:assert/strict');
const vm = require('node:vm');
const html = require('node:fs').readFileSync(0, 'utf8');
const submit = html.split('composer.addEventListener("submit", async event => {')[1].split('\n    });')[0];
const sends = [];
let missingModel = false, validations = 0, focuses = 0;
const context = vm.createContext({
  promptInput: {value: ''}, selectedAttachment: {id: 'image'}, lastUserText: '',
  modelEmpty: () => missingModel,
  validateModel: () => validations++, openModelMenu: () => focuses++,
  selectedModelPayload: () => ({model: 'selected-model'}),
  sendInFlight: false,
  sendTurn: async payload => {sends.push({url: '/api/turn', payload}); return {accepted: true};},
  clearAttachment: () => {context.selectedAttachment = null;}, loadSessions: () => {},
});
vm.runInContext('async function submit(event) {' + submit + '\n}', context);
(async () => {
  for (const text of ['/compact', '/help', 'ordinary chat']) {
    context.promptInput.value = text;
    missingModel = true;
    await context.submit({preventDefault() {}});
    assert.equal(sends.length, validations - 1, 'no admission without a model');
    assert.equal(context.promptInput.value, text, 'validation retains the draft');
    assert.equal(focuses, validations);
    missingModel = false;
    context.selectedAttachment = {id: 'image'};
    await context.submit({preventDefault() {}});
    assert.equal(sends.at(-1).url, '/api/turn');
    assert.equal(sends.at(-1).payload.text, text);
    assert.equal(sends.at(-1).payload.attachment_id, 'image');
    assert.equal(sends.at(-1).payload.model, 'selected-model');
    assert.equal(context.promptInput.value, '');
    assert.equal(context.selectedAttachment, null);
  }
  assert.equal(sends.length, 3);
})().catch(error => {console.error(error); process.exitCode = 1;});
"#;
    let mut child = std::process::Command::new(node)
        .arg("-e")
        .arg(script)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("run chat admission law");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(ui::INDEX_HTML.as_bytes())
        .expect("send page");
    let output = child.wait_with_output().expect("chat admission law exits");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(test)]
#[path = "tests/standard_transcript.rs"]
mod standard_transcript_tests;

/// Two inbox lists awaited together in one cell both complete inside a
/// durable workbench turn.
#[tokio::test]
async fn parallel_inbox_lists_complete_in_durable_workbench_turn() {
    let mail_world = mail::MailWorld::new();
    mail_world.add_account("test").expect("add test");
    mail_world.add_account("test2").expect("add test2");
    let workbench = Workbench::builder(replying_provider(
        r#"<typescript>
const boxes = await Promise.all([inbox.test.list({}), inbox.test2.list({})]);
finish(JSON.stringify({ test: boxes[0], test2: boxes[1] }));
</typescript>"#,
    ))
    .mail_world(mail_world)
    .build()
    .await;
    let state = &workbench.state;
    let session = state
        .create_or_open_session(&state.current_session_id(), "test")
        .await
        .expect("open session");
    let output = tokio::time::timeout(
        Duration::from_secs(30),
        session
            .send(lash::TurnInput::text("list both inboxes"))
            .output(),
    )
    .await
    .expect("parallel inbox list turn must not hang")
    .expect("parallel inbox list turn");
    // A workbench chat session finishes with text (FIG-5156): the cell
    // finishes with both listings' JSON.
    let finished = output
        .final_value()
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("the turn finishes with text: {output:?}"));
    assert_eq!(
        serde_json::from_str::<Value>(finished).expect("the finish is JSON text"),
        json!({
            "test": { "account": "test", "messages": [] },
            "test2": { "account": "test2", "messages": [] }
        })
    );
    drop(session);
    workbench.shutdown().await;
}

/// Wait until the next provider call has entered, and answer its index.
async fn next_call(entered: &mut mpsc::UnboundedReceiver<usize>) -> usize {
    tokio::time::timeout(Duration::from_secs(30), entered.recv())
        .await
        .expect("a call reaches the provider")
        .expect("the provider stays alive")
}

/// A turn no user started (a `continue_as` follow-on, which the session's
/// engine runs on its own) renders the connected accounts the host recorded,
/// a build that resumes it after a drain serves the render the turn recorded
/// even though the live accounts changed meanwhile, and a later such turn
/// renders the accounts the host recorded since.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_non_user_turn_records_fresh_accounts_context_and_replays_it() {
    const FOLLOW_ON: &str = "<typescript>\nawait control.continue_as({ task: \"observe the accounts\" });\n</typescript>";
    let requests = Arc::new(Mutex::new(Vec::<lash::provider::LlmRequest>::new()));
    let (entered_tx, mut entered) = mpsc::unbounded_channel::<usize>();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let provider = lash::testing::TestProvider::builder()
        .kind("workbench-harness")
        .complete({
            let requests = Arc::clone(&requests);
            let release = Arc::clone(&release);
            move |request| {
                let call = {
                    let mut seen = requests.lock_recover();
                    seen.push(request);
                    seen.len() - 1
                };
                let entered_tx = entered_tx.clone();
                let release = Arc::clone(&release);
                async move {
                    let _ = entered_tx.send(call);
                    let reply = match call {
                        0 | 3 => FOLLOW_ON.to_string(),
                        1 => {
                            release
                                .acquire()
                                .await
                                .expect("the gate stays open")
                                .forget();
                            "<typescript>\nconst step = 1;\n</typescript>".to_string()
                        }
                        2 => finish_cell("observed"),
                        4 => finish_cell("observed again"),
                        other => panic!("unexpected provider call {other}"),
                    };
                    Ok(text_response(&reply))
                }
            }
        })
        .build()
        .into_handle();
    let mail_world = mail::MailWorld::new();
    let old = Workbench::builder(provider.clone())
        .mail_world(mail_world.clone())
        .build()
        .await;
    let session_id = old.state.current_session_id();
    let _ = add_account(
        State(old.state.clone()),
        Json(AddAccountRequest {
            name: "fresh".to_string(),
        }),
    )
    .await
    .expect("connect an account through the host");
    let recorded = connected_accounts_prompt(&mail_world);
    send_text(&old.state, Some(&session_id), "hand over to a follow-on")
        .await
        .expect("the switching send is admitted");
    assert_eq!(next_call(&mut entered).await, 0);
    assert_eq!(
        next_call(&mut entered).await,
        1,
        "the follow-on's first call is parked"
    );

    // Live data the host never recorded changes while the turn is parked;
    // the old build then hands the session over at its next committed
    // phase, and a new build over the same stores resumes the turn.
    mail_world
        .add_account("unrecorded")
        .expect("change live data before the resume");
    let drain = tokio::spawn({
        let core = old.state.core.clone();
        async move { core.drain().await }
    });
    release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(30), drain)
        .await
        .expect("the old build drains")
        .expect("the drain task")
        .expect("the old build releases its sessions");
    let stores = Arc::clone(&old.stores);
    let new = Workbench::builder(provider)
        .stores(stores)
        .mail_world(mail_world.clone())
        .build()
        .await;
    assert_eq!(
        next_call(&mut entered).await,
        2,
        "the new build resumes the follow-on"
    );

    let _ = delete_account(AxumPath("fresh".to_string()), State(new.state.clone()))
        .await
        .expect("remove an account through the host");
    let _ = add_account(
        State(new.state.clone()),
        Json(AddAccountRequest {
            name: "later".to_string(),
        }),
    )
    .await
    .expect("connect another account through the host");
    let later = connected_accounts_prompt(&mail_world);
    // The follow-on settles before the next send, which would otherwise
    // queue behind it.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while new.state.active_turns.for_session(&session_id).is_some() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the resumed follow-on never settled"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    send_text(&new.state, Some(&session_id), "hand over again")
        .await
        .expect("the second switching send is admitted");
    assert_eq!(next_call(&mut entered).await, 3);
    assert_eq!(
        next_call(&mut entered).await,
        4,
        "the second follow-on calls the model"
    );

    let seen = requests.lock_recover();
    let instructions = |call: usize| seen[call].instructions.clone().unwrap_or_default();
    assert!(
        instructions(1).contains(&recorded),
        "a non-user turn must render the recorded accounts context"
    );
    assert_eq!(
        instructions(1),
        instructions(2),
        "the resumed turn must serve its recorded render after live data changed"
    );
    assert!(
        instructions(4).contains(&later),
        "a later non-user turn must see the host's changed accounts"
    );
}

/// The `/api/observations` stream of `session_id` from `cursor` (the
/// session's snapshot cursor when `None`), as the page reads it.
async fn observation_stream(
    state: &AppState,
    session_id: &lash::SessionId,
    cursor: Option<String>,
) -> mpsc::Receiver<ObservationStreamItem> {
    let response = session_observations_with_shutdown(
        State(state.clone()),
        Query(EventsQuery {
            cursor,
            session_id: Some(session_id.clone()),
        }),
        remote_hello_headers(),
        None,
    )
    .await
    .expect("open the observation route");
    let mut body = response.into_body().into_data_stream();
    let (tx, rx) = mpsc::channel(256);
    tokio::spawn(async move {
        let mut buffer = Vec::new();
        while let Some(Ok(bytes)) = body.next().await {
            buffer.extend_from_slice(&bytes);
            while let Some(end) = buffer.iter().position(|byte| *byte == b'\n') {
                let line = buffer.drain(..=end).collect::<Vec<_>>();
                if line.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                let item = serde_json::from_slice::<ObservationStreamItem>(&line)
                    .expect("an observation stream item");
                if tx.send(item).await.is_err() {
                    return;
                }
            }
        }
    });
    rx
}

async fn next_observation(rx: &mut mpsc::Receiver<ObservationStreamItem>) -> ObservationStreamItem {
    tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("timed out waiting for stream item")
        .expect("stream item")
}

/// The page's observation stream hands it a replay cursor, the turn's
/// activity, and a queued input's typed application evidence.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn event_stream_forwards_session_observation_live_replay() {
    let workbench = Workbench::replying(
        "<typescript>\nfinish(\"observed through live replay\");\n</typescript>",
    )
    .await;
    let state = &workbench.state;
    let session_id = state.current_session_id();
    let session = state
        .create_or_open_session(&session_id, "test")
        .await
        .expect("open session");
    let mut rx = observation_stream(state, &session_id, None).await;

    session
        .send(lash::TurnInput::text("exercise observation stream"))
        .output()
        .await
        .expect("turn");

    let mut saw_cursor = false;
    let mut saw_final_value_observation = false;
    for _ in 0..64 {
        match next_observation(&mut rx).await {
            ObservationStreamItem::Cursor { cursor } => {
                assert!(!cursor.is_empty(), "cursor should be opaque but non-empty");
                saw_cursor = true;
            }
            ObservationStreamItem::Observation { event } => {
                let value = serde_json::to_value(&event).expect("remote event json");
                if value.pointer("/type").and_then(Value::as_str) == Some("turn_activity")
                    && value.pointer("/activity/type").and_then(Value::as_str)
                        == Some("final_value")
                {
                    saw_final_value_observation = true;
                }
            }
            ObservationStreamItem::ReplayGap { .. }
            | ObservationStreamItem::TerminalReplacement { .. }
            | ObservationStreamItem::ResidentReplacement { .. } => {}
        }
        if saw_cursor && saw_final_value_observation {
            break;
        }
    }
    assert!(saw_cursor, "stream should expose a replay cursor");
    assert!(
        saw_final_value_observation,
        "stream should expose turn activity through session observation"
    );

    let handle = session
        .send(lash::TurnInput::text("queued workbench input"))
        .id(lash::TurnId::parse("workbench-queued-input").expect("nonblank host identity"))
        .await
        .expect("admit queued workbench input");
    let admission = handle.receipt().clone();
    handle.output().await.expect("queued workbench turn");
    let mut typed_application = None;
    for _ in 0..64 {
        let ObservationStreamItem::Observation { event } = next_observation(&mut rx).await else {
            continue;
        };
        let value = serde_json::to_value(&event).expect("remote event json");
        if value.pointer("/type").and_then(Value::as_str) == Some("turn_activity")
            && value.pointer("/activity/type").and_then(Value::as_str) == Some("turn_input_applied")
        {
            assert!(
                value.pointer("/activity/kind").is_none(),
                "workbench must consume a typed event, not an untyped diagnostic: {value}"
            );
            typed_application = value.pointer("/activity/applications/0").cloned();
            break;
        }
    }
    let typed_application = typed_application.expect("typed application observation");
    assert_eq!(
        typed_application.get("input_id").and_then(Value::as_str),
        Some(admission.input_id.as_str())
    );
    assert_eq!(
        typed_application.get("source_key").and_then(Value::as_str),
        Some("workbench-queued-input")
    );
    assert_eq!(
        typed_application.get("turn_id").and_then(Value::as_str),
        Some("workbench-queued-input")
    );
    let durable = session
        .durable()
        .remote_turn_input_applications()
        .await
        .expect("durable workbench applications");
    let settled = durable
        .iter()
        .find(|application| application.input_id == admission.input_id)
        .expect("the queued admission settles as durable application evidence");
    assert_eq!(settled.turn_id.as_str(), "workbench-queued-input");
    assert!(
        session
            .durable()
            .pending_turn_inputs()
            .await
            .expect("pending input snapshot")
            .is_empty(),
        "application evidence must remain available without a pending-input snapshot"
    );
    assert!(ui::TIMELINE_JS.contains("turn_input_applied"));
    for page in [ui::INDEX_HTML, ui::TIMELINE_JS] {
        assert!(!page.contains("queued_input_accepted"));
        assert!(!page.contains("runtime_diagnostic"));
    }
}

/// A cursor the live replay has trimmed past is a typed `replay_gap` naming
/// the requested cursor, the latest recoverable one, and the session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn event_stream_forwards_session_observation_replay_gap() {
    let workbench = Workbench::builder(replying_provider(
        "<typescript>\nfinish(\"gap source\");\n</typescript>",
    ))
    .live_replay(Arc::new(lash::observe::InMemoryLiveReplayStore::new(
        lash::observe::InMemoryLiveReplayStoreConfig {
            max_events_per_session: 1,
            ..lash::observe::InMemoryLiveReplayStoreConfig::default()
        },
    )))
    .build()
    .await;
    let state = &workbench.state;
    let session_id = state.current_session_id();
    let session = state
        .create_or_open_session(&session_id, "test")
        .await
        .expect("open session");
    let requested_cursor = session
        .observe()
        .snapshot()
        .await
        .expect("durable snapshot")
        .cursor
        .to_string();
    session
        .send(lash::TurnInput::text("trim cursor"))
        .output()
        .await
        .expect("turn");

    let mut rx = observation_stream(state, &session_id, Some(requested_cursor.clone())).await;
    let mut saw_gap = false;
    for _ in 0..8 {
        if let ObservationStreamItem::ReplayGap { observation, gap } =
            next_observation(&mut rx).await
        {
            assert_eq!(gap.body.requested_cursor, requested_cursor);
            assert!(
                !gap.body.latest_cursor.is_empty(),
                "gap should include the latest recoverable cursor"
            );
            assert_eq!(observation.body.cursor, gap.body.latest_cursor);
            assert_eq!(observation.body.session_id, session_id.as_str());
            assert_eq!(
                gap.body.reason,
                lash::remote::observations::RemoteLiveReplayGapReason::Trimmed
            );
            saw_gap = true;
            break;
        }
    }
    assert!(saw_gap, "trimmed cursor should emit replay_gap");
}

/// A healthy shell produces no `replay_gap` between reconnects: the cursor
/// `/api/state` hands the page attaches clean, again after a re-snapshot,
/// and a cursor from a replaced process gaps exactly once and converges on
/// the recovery cursor it hands back (FIG-3162).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn state_snapshot_cursor_attaches_to_the_live_incarnation_without_a_gap() {
    let workbench =
        Workbench::replying("<typescript>\nfinish(\"snapshot cursor\");\n</typescript>").await;
    let state = &workbench.state;
    let session_id = state.current_session_id();
    let session = state
        .create_or_open_session(&session_id, "test")
        .await
        .expect("open session");
    session
        .send(lash::TurnInput::text("fill the live replay buffer"))
        .output()
        .await
        .expect("turn");

    for read in ["snapshot", "re-snapshot"] {
        let snapshot = read_state_projection(state, &session_id)
            .await
            .expect("read state projection");
        assert!(
            matches!(
                session
                    .observe()
                    .subscribe_from_cursor(&snapshot.cursor)
                    .await
                    .expect("attach at the snapshot cursor"),
                lash::observe::SessionObservationSubscription::Subscribed(_)
            ),
            "the {read} cursor from a healthy shell must attach without a replay gap"
        );
    }

    let dead_incarnation = lash::observe::InMemoryLiveReplayStore::default();
    let stale_cursor = lash::observe::LiveReplayStore::current_cursor(
        &dead_incarnation,
        &session_id,
        lash::observe::SessionRevision(0),
    );
    let recovery_cursor = match session
        .observe()
        .subscribe_from_cursor(&stale_cursor)
        .await
        .expect("attach at the dead incarnation's cursor")
    {
        lash::observe::SessionObservationSubscription::Gap { observation, gap } => {
            assert_eq!(
                gap.reason,
                lash::observe::LiveReplayGapReason::Unavailable,
                "a cursor from a replaced process is unavailable, not trimmed"
            );
            observation.cursor
        }
        lash::observe::SessionObservationSubscription::Subscribed(_) => {
            panic!("a cursor from a replaced process must gap")
        }
    };
    assert!(
        matches!(
            session
                .observe()
                .subscribe_from_cursor(&recovery_cursor)
                .await
                .expect("attach at the recovery cursor"),
            lash::observe::SessionObservationSubscription::Subscribed(_)
        ),
        "the recovery cursor must converge, so one real outage costs exactly one gap"
    );
}

/// The Abort route asks the engine to cancel the session's running turn
/// as a first-party user request, attaches the cancelled terminal carrying
/// that same evidence, and leaves the turn's one `Done` to its run's
/// follower; a later request finds the completion already won.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turn_cancel_route_requests_first_party_turn_cancellation() {
    let mut gate = GatedProvider::new();
    let workbench = Workbench::builder(gate.provider.clone()).build().await;
    let state = &workbench.state;
    let session_id = state.current_session_id();
    let mut events = state.event_tx.subscribe(&session_id);
    let accepted = send_text(state, None, "a turn to abort")
        .await
        .expect("the send is admitted");
    let turn_id = started_turn_id(&accepted);
    gate.next_call().await;

    let (status, Json(cancelled)) = Box::pin(cancel_turn(
        State(state.clone()),
        Query(TurnCancelQuery::default()),
    ))
    .await
    .expect("cancel turn");
    assert_eq!(status, StatusCode::OK);
    assert!(cancelled.accepted);
    match cancelled.cancellations.as_slice() {
        [
            TurnCancelReceipt::TerminalAttached {
                address,
                cancellation: RecordedTurnCancellation::Requested(requested),
                terminal:
                    lash::TurnTerminal::Committed {
                        stop: Some(lash::TurnStop::Cancelled { evidence }),
                        ..
                    },
            },
        ] => {
            assert_eq!(address.turn_id, turn_id);
            assert_eq!(evidence, requested);
            assert_eq!(evidence.origin.as_deref(), Some("user"));
            assert_eq!(evidence.reason.as_deref(), Some("workbench Abort control"));
            assert_eq!(evidence.mode, lash::TurnCancelMode::Immediate);
        }
        other => panic!("the abort must attach the cancelled terminal: {other:?}"),
    }
    // The abort released the route's claim; the run's follower publishes
    // the turn's Done once it settles the cancelled run.
    let is_turn_done = |event: &ProductEvent| matches!(&event.item, StreamItem::Done { turn_id: Some(done), .. } if *done == turn_id);
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let event = events.recv().await.expect("the session's product events");
            if is_turn_done(&event) {
                break;
            }
        }
    })
    .await
    .expect("the follower publishes the cancelled turn's Done");
    let mut later = Vec::new();
    while let Ok(event) = events.try_recv() {
        later.push(event);
    }
    assert!(
        !later.iter().any(is_turn_done),
        "the turn's one Done is its follower's, never the route's: {later:?}"
    );

    let session = state
        .open_session(&session_id, "test")
        .await
        .expect("open the session");
    let duplicate = state
        .core
        .turn_work_driver()
        .request_cancel(lash::TurnCancelRequest::new(
            session.turn_address(turn_id.clone()),
            "duplicate",
            Some("test-host".to_string()),
        ))
        .await
        .expect("read cancellation gate");
    // A won completion records nothing: the receipt carries no record.
    assert!(matches!(
        duplicate.outcome,
        lash::TurnCancelOutcome::CompletionWonRace
    ));
}

/// Every installed inbox account, whatever its name, resolves as a tool the
/// session's recorded catalog carries, membership edits that catalog, and a
/// turn sends through each account's installed plugin.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inbox_authority_resolves_for_any_account_name() {
    let mail_world = mail::MailWorld::new();
    mail_world.add_account("test").expect("add test");
    mail_world
        .add_account("live")
        .expect("add second installed account");
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let provider = lash::testing::TestProvider::builder()
        .kind("workbench-harness")
        .complete(move |_| {
            let account = match calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                0 => "test",
                1 => "live",
                call => panic!("unexpected workbench provider call {call}"),
            };
            async move {
                Ok(text_response(&format!(
                    "<typescript>\nconst result = await inbox.{account}.send({{ title: \"Hi\", text: \"Yo\" }});\nfinish(result.id);\n</typescript>"
                )))
            }
        })
        .build()
        .into_handle();
    let workbench = Workbench::builder(provider)
        .mail_world(mail_world.clone())
        .build()
        .await;
    let state = &workbench.state;
    let session = state
        .create_or_open_session(&state.current_session_id(), "test")
        .await
        .expect("open session");

    let commands = session.admin().commands();
    let receipt = commands
        .refresh_tool_catalog("inbox catalog inspection", "inbox-catalog")
        .await
        .expect("submit catalog publication");
    assert!(matches!(
        commands.settle(receipt).await.expect("publish catalog"),
        lash::SessionCommandSettlement::Durable(_)
    ));
    let session_tools = session.admin().tools();
    let active_send = || async {
        session_tools
            .active_manifests()
            .await
            .expect("read active session manifests")
            .into_iter()
            .find(|manifest| manifest.name == "inbox__test__send")
    };
    let send_manifest = active_send()
        .await
        .expect("the session catalog composes the workbench plugin's inbox tool");
    let compact = send_manifest
        .compact_contract
        .as_ref()
        .expect("the recorded manifest carries its compact contract");
    assert!(
        compact.signature.contains("title"),
        "the recorded contract names the runtime input schema: {}",
        compact.signature
    );
    session_tools
        .set_membership(send_manifest.id.clone(), false)
        .await
        .expect("remove send from this session catalog");
    assert!(
        active_send().await.is_none(),
        "a non-member tool leaves the session's recorded catalog"
    );
    session_tools
        .set_membership(send_manifest.id, true)
        .await
        .expect("restore send to this session catalog");
    assert!(
        active_send().await.is_some(),
        "membership restores the tool to the session's recorded catalog"
    );

    for account in ["test", "live"] {
        let output = tokio::time::timeout(
            Duration::from_secs(20),
            session
                .send(lash::TurnInput::text("send through the plugin provider"))
                .output(),
        )
        .await
        .expect("the account turn completes")
        .expect("turn executes the account through its installed plugin revision");
        assert_eq!(output.final_value(), Some(&json!(format!("{account}-1"))));
        assert_eq!(
            mail_world
                .inbox(account)
                .expect("installed account inbox")
                .len(),
            1
        );
    }
}

fn press(button: ButtonChoice) -> Json<ButtonEventRequest> {
    Json(ButtonEventRequest {
        button,
        model: None,
        model_variant: None,
    })
}

/// The `(occurrence_id, started_process_ids)` of every press `trace` saw.
fn traced_presses(trace: &RecordingTrace) -> Vec<(String, Vec<lash::ProcessId>)> {
    trace
        .custom("button_trigger.trigger_occurrence")
        .into_iter()
        .map(|(_, payload)| {
            (
                payload["occurrence_id"]
                    .as_str()
                    .expect("the occurrence id")
                    .to_string(),
                serde_json::from_value(payload["started_process_ids"].clone())
                    .expect("the started process ids"),
            )
        })
        .collect()
}

/// A press selects the model it names and emits one button occurrence for
/// its session, carrying the button, whose delivery starts the registered
/// trigger's work.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_button_press_emits_its_occurrence_under_the_selected_model() {
    let trace = Arc::new(RecordingTrace::default());
    let workbench = Workbench::builder(replying_provider(
        reset_chat_tests::BUTTON_TRIGGER_REGISTRATION,
    ))
    .trace_sink(Arc::clone(&trace) as Arc<dyn TraceSink>)
    .build()
    .await;
    let state = &workbench.state;
    let session_id = state.current_session_id();
    reset_chat_tests::register_button_trigger(state).await;

    let Json(accepted) = button_trigger(
        State(state.clone()),
        Query(SessionQuery::default()),
        Json(ButtonEventRequest {
            button: ButtonChoice::Blue,
            model: Some("button-model".to_string()),
            model_variant: Some("high".to_string()),
        }),
    )
    .await
    .expect("button command");
    assert!(accepted.accepted);
    let selected_llm_profile = state.selected_llm_profile();
    assert_eq!(selected_llm_profile.model, "button-model");
    assert_eq!(selected_llm_profile.model_variant.as_deref(), Some("high"));

    let emitted = trace.custom("trigger.emit");
    let [(emitted_session, emitted)] = emitted.as_slice() else {
        panic!("one press is one emission: {emitted:?}");
    };
    assert_eq!(emitted_session.as_ref(), Some(&session_id));
    assert_eq!(emitted["source_type"], json!(BUTTON_TRIGGER_SOURCE_TYPE));
    assert_eq!(emitted["payload"]["button"], json!("Blue"));
    let presses = traced_presses(&trace);
    let [(_, started)] = presses.as_slice() else {
        panic!("one press is one occurrence: {presses:?}");
    };
    assert_eq!(started.len(), 1, "the delivery starts the trigger's work");
}

/// FIG-5036: a press is one row, published with the occurrence it emitted,
/// once however often it is published, stamped when the press happened and
/// naming the processes it started so the page folds the woken work into it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_button_press_is_one_row_published_by_its_occurrence() {
    let trace = Arc::new(RecordingTrace::default());
    let workbench = Workbench::builder(replying_provider(
        reset_chat_tests::BUTTON_TRIGGER_REGISTRATION,
    ))
    .trace_sink(Arc::clone(&trace) as Arc<dyn TraceSink>)
    .build()
    .await;
    let state = &workbench.state;
    let session_id = state.current_session_id();
    reset_chat_tests::register_button_trigger(state).await;

    let Json(accepted) = button_trigger(
        State(state.clone()),
        Query(SessionQuery::default()),
        press(ButtonChoice::Red),
    )
    .await
    .expect("button command");
    assert!(accepted.accepted);
    let presses = traced_presses(&trace);
    let [(occurrence_id, started)] = presses.as_slice() else {
        panic!("one press is one occurrence: {presses:?}");
    };
    let rows = state
        .messages_snapshot()
        .into_iter()
        .filter(|message| message.role == "event")
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 1, "the press is one row: {rows:?}");
    assert_eq!(rows[0].id, *occurrence_id);
    assert_eq!(rows[0].text, "red pressed");
    assert!(matches!(
        &rows[0].provenance,
        Some(ChatMessageProvenance::TriggerOccurrence { occurrence_id: named, process_ids })
            if named == occurrence_id && process_ids == started
    ));

    let report = lash::triggers::TriggerEmitReport {
        occurrence_id: "trigger:workbench-button-trigger:press-1".to_string(),
        deliveries: vec![lash::triggers::TriggerDeliveryEmitReceipt {
            occurrence_id: "trigger:workbench-button-trigger:press-1".to_string(),
            subscription_id: "trigger-subscription:watch".to_string(),
            outcome: lash::triggers::TriggerDeliveryEmitOutcome::Started {
                process_id: lash::ProcessId::fixture("p_watch"),
            },
        }],
    };
    for _ in 0..2 {
        state.push_trigger_occurrence_for_session(
            &session_id,
            "red pressed",
            &report,
            "2026-06-02T12:00:00Z",
        );
    }
    let republished = state
        .messages_snapshot()
        .into_iter()
        .filter(|message| message.id == report.occurrence_id)
        .collect::<Vec<_>>();
    assert_eq!(
        republished.len(),
        1,
        "one occurrence is one row: {republished:?}"
    );
    assert_eq!(
        republished[0].at, "2026-06-02T12:00:00Z",
        "the row is stamped when the press happened, not when it was published"
    );
    assert!(matches!(
        &republished[0].provenance,
        Some(ChatMessageProvenance::TriggerOccurrence { occurrence_id, process_ids })
            if *occurrence_id == report.occurrence_id
                && *process_ids == vec![lash::ProcessId::fixture("p_watch")]
    ));
}

/// A trigger one build registered fires from the button route of a later
/// build over the same stores: the subscription, its compiled artifacts and
/// the process registry are read back from them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persisted_trigger_route_fires_after_reopening_the_core() {
    let old = Workbench::replying(reset_chat_tests::BUTTON_TRIGGER_REGISTRATION).await;
    let session_id = old.state.current_session_id();
    reset_chat_tests::register_button_trigger(&old.state).await;
    tokio::time::timeout(Duration::from_secs(30), old.state.core.drain())
        .await
        .expect("the old build drains")
        .expect("the old build releases its sessions");

    let trace = Arc::new(RecordingTrace::default());
    let new = Workbench::builder(silent_provider())
        .stores(Arc::clone(&old.stores))
        .trace_sink(Arc::clone(&trace) as Arc<dyn TraceSink>)
        .build()
        .await;
    let Json(accepted) = button_trigger(
        State(new.state.clone()),
        Query(SessionQuery {
            session_id: Some(session_id),
        }),
        press(ButtonChoice::Blue),
    )
    .await
    .expect("press the button on the reopened core");
    assert!(accepted.accepted);
    let presses = traced_presses(&trace);
    let [(_, started)] = presses.as_slice() else {
        panic!("one press is one occurrence: {presses:?}");
    };
    assert_eq!(started.len(), 1, "the persisted trigger delivers");
    tokio::time::timeout(
        Duration::from_secs(30),
        new.state.core.processes().await_output(&started[0]),
    )
    .await
    .expect("the trigger's process finishes in time")
    .expect("trigger process should finish");
}
