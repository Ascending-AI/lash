//! What the page shows across `continue_as` frame switches. A switch commits
//! with its follow-on mailed to the session as the next run (ADR 0101 §3),
//! which the session's engine starts on its own; the transcript walks the
//! retained ancestry across frames (ADR 0129).

use super::*;

/// Read the page until `done` holds for its snapshot, and answer it.
async fn await_snapshot(
    state: &AppState,
    what: &str,
    done: impl Fn(&StateReadSnapshot) -> bool,
) -> StateReadSnapshot {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let snapshot = read_state(state, None).await.expect("read the state");
            if done(&snapshot) {
                return snapshot;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the page never showed {what}"))
}

/// Whether `snapshot` renders a committed reply `text`.
fn shows_reply(snapshot: &StateReadSnapshot, text: &str) -> bool {
    snapshot
        .transcript
        .iter()
        .any(|row| row.provenance.is_turn_reply && row.content.text == text)
}

/// The text of every rendered user row.
fn user_rows(snapshot: &StateReadSnapshot) -> Vec<String> {
    snapshot
        .transcript
        .iter()
        .filter(|row| row.suppressed.is_none() && row.kind == crate::ChatRowKind::User)
        .map(|row| row.content.text.clone())
        .collect()
}

fn continue_as_cell(task: &str, seed: &str) -> String {
    format!(
        "await control.continueAs({{ task: {}, seed: {{ marker: {} }} }});",
        serde_json::to_string(task).expect("a task encodes"),
        serde_json::to_string(seed).expect("a seed encodes"),
    )
}

/// Two switches in a row, then an ordinary send in the final frame: the page
/// shows each real send, each frame's task and the final frame's answers, in
/// order and once each; no seed reaches a row; and the ordinary send carries
/// its runtime-stamped turn input.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_continue_as_switches_keep_real_sends_and_show_the_current_follow_task() {
    let workbench = Workbench::builder(scripted_cells_provider(vec![
        continue_as_cell("enter the middle follow frame", "hidden-middle-seed"),
        continue_as_cell("enter the final follow frame", "hidden-final-seed"),
        finish_cell("third frame answer"),
        finish_cell("ordinary follow-frame answer"),
    ]))
    .build()
    .await;
    let state = &workbench.state;
    let session_id = state.current_session_id();
    let initial_prompt = "switch through three frames";
    run_turn(state, initial_prompt).await;
    await_snapshot(state, "the third frame's answer", |snapshot| {
        shows_reply(snapshot, "third frame answer")
    })
    .await;
    let ordinary_prompt = "ordinary send inside the final follow frame";
    let ordinary_turn_id = run_turn(state, ordinary_prompt).await;
    let projected = await_snapshot(state, "the ordinary send's answer", |snapshot| {
        shows_reply(snapshot, "ordinary follow-frame answer")
    })
    .await;

    let session = state
        .open_session(&session_id, "test")
        .await
        .expect("open the final frame");
    assert!(
        session.read_view().messages().iter().any(|message| {
            matches!(
                message.origin.as_ref(),
                Some(lash::messages::MessageOrigin::TurnInput { turn_id, .. })
                    if *turn_id == ordinary_turn_id
            ) && message_text(message) == ordinary_prompt
        }),
        "the follow-frame send carries its runtime-stamped turn input"
    );
    drop(session);

    let canonical = projected
        .transcript
        .iter()
        .filter_map(|row| chat_message_from_row(row).expect("project a canonical row"))
        .collect::<Vec<_>>();
    let expected_rows = vec![
        ("user", initial_prompt),
        ("user", "enter the middle follow frame"),
        ("user", "enter the final follow frame"),
        ("assistant", "third frame answer"),
        ("user", ordinary_prompt),
        ("assistant", "ordinary follow-frame answer"),
    ];
    assert_eq!(
        canonical
            .iter()
            .map(|message| (message.role.as_str(), message.text.as_str()))
            .collect::<Vec<_>>(),
        expected_rows
    );
    assert_eq!(
        canonical
            .iter()
            .map(|message| &message.id)
            .collect::<BTreeSet<_>>()
            .len(),
        expected_rows.len(),
        "every row is shown once"
    );
    let reply_turns = canonical
        .iter()
        .filter(|message| message.role == "assistant")
        .filter_map(|message| {
            message
                .provenance
                .as_ref()
                .map(ChatMessageProvenance::turn_id)
        })
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(reply_turns.len(), 2, "each reply names its turn");
    assert_eq!(reply_turns[1], ordinary_turn_id);
    assert!(
        canonical
            .iter()
            .all(|message| !message.text.contains("hidden-middle-seed")
                && !message.text.contains("hidden-final-seed")),
        "a seed is protocol state, never a row"
    );
    workbench.shutdown().await;
}

/// The workbench need not win a race to keep what the operator sent. A
/// rebuild that sees a settled turn before the runtime's committed copy of
/// its input is readable must not retire the submitted row: `continue_as`
/// retires the frame that copy lives in, and a retired row would leave
/// nothing on screen. Here every rebuild loses that race, and every
/// submitted send is still shown, once, after the switch (FIG-3143).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frame_switch_keeps_sends_the_workbench_never_saw_commit() {
    let data_dir = tempfile::tempdir().expect("product events tempdir");
    let mut cells = (0..4)
        .map(|index| finish_cell(&format!("answer before switch {index}")))
        .collect::<Vec<_>>();
    cells.push(continue_as_cell(
        "carry on in the next frame",
        "protocol-only",
    ));
    cells.push(finish_cell("answer in the follow frame"));
    let workbench = Workbench::builder(scripted_cells_provider(cells))
        .event_tx(
            SessionEventRegistry::persistent(data_dir.path().join("product-events.json"), 16)
                .expect("open the persistent product event registry"),
        )
        .build()
        .await;
    let state = &workbench.state;
    let session_id = state.current_session_id();
    // A rebuild in the race window: the turns are settled and the workbench
    // has observed none of their committed inputs.
    let lose_the_race = || {
        state.event_tx.reconcile_settled(
            &session_id,
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
        );
    };
    for index in 0..4 {
        run_turn(state, &format!("submitted prompt before switch {index}")).await;
        lose_the_race();
    }
    run_turn(state, "switch frames without an observed commit").await;
    await_snapshot(state, "the follow frame's answer", |snapshot| {
        shows_reply(snapshot, "answer in the follow frame")
    })
    .await;
    lose_the_race();

    let boundary = read_state(state, None)
        .await
        .expect("read after the switch");
    let expected = (0..4)
        .map(|index| format!("submitted prompt before switch {index}"))
        .chain([
            "switch frames without an observed commit".to_string(),
            "carry on in the next frame".to_string(),
        ])
        .collect::<Vec<_>>();
    assert_eq!(user_rows(&boundary), expected);
    assert_eq!(
        boundary
            .transcript
            .iter()
            .filter(|row| { row.suppressed.is_none() && row.kind == crate::ChatRowKind::User })
            .map(|row| serde_json::to_string(&row.row_id).expect("encode the row id"))
            .collect::<BTreeSet<_>>()
            .len(),
        expected.len(),
        "each send is shown once"
    );
    workbench.shutdown().await;
}
