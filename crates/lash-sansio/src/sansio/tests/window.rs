use super::*;

#[test]
fn a_finished_turn_leaves_the_commit_its_records_to_mark_in_place() {
    // A store-read window's history buffer has room past its records, as a
    // grown buffer does: the turn appends there in place.
    let committed = window(64);
    let mut events = lash_protocol_types::AppendVec::with_capacity(128);
    for record in committed.events().iter().cloned() {
        events.push(record);
    }
    let window = TurnWindow::new(
        committed.pin().clone(),
        committed.messages().clone(),
        events,
    );
    let mut machine = TurnMachine::in_window(
        test_config(Arc::new(ProseDriver)),
        window.clone(),
        window.then(vec![user_message("the turn's own input")]),
        vec![conversation_event(user_message("the turn's own input"))],
        0,
    );
    let effects = drain_effects(&mut machine);
    let llm_id = *find_llm_call(&effects).expect("the turn's model call").0;
    machine.handle_response(Response::LlmComplete {
        id: llm_id,
        text_streamed: false,
        result: Ok(LlmResponse::default()),
    });
    let effects = drain_effects(&mut machine);
    // The input was the graph's before the turn started; the protocol's
    // records arrive through the turn's deltas.
    let mut turn_records = vec![conversation_event(user_message("the turn's own input"))];
    turn_records.extend(
        progress_event_delta(&effects)
            .unwrap_or_default()
            .iter()
            .cloned(),
    );
    let (checkpoint_id, _) = find_checkpoint(&effects).expect("the completion checkpoint");
    machine.handle_response(Response::Checkpoint {
        id: checkpoint_id,
        delivery: CheckpointDelivery::default(),
    });
    let effects = drain_effects(&mut machine);
    assert!(find_done(&effects).is_some());
    turn_records.extend(
        done_event_delta(&effects)
            .unwrap_or_default()
            .iter()
            .cloned(),
    );

    // The committing graph's history: the window's records, then the
    // turn's, adopted from the slots the machine wrote.
    assert_eq!(turn_records.len(), 2, "the input and the reply");
    let mut committing = window.events().clone();
    committing.extend_adopting(turn_records, lash_protocol_types::same_history_record);
    let shared = committing.as_slice().as_ptr();
    let reply = committing.len() - 1;
    let SessionHistoryRecord::Conversation(mut marked) = committing[reply].clone() else {
        panic!("the turn's last record is its reply");
    };
    assert_eq!(marked.role, MessageRole::Assistant);
    marked.reply_marker = Some(
        <lash_protocol_types::TurnReply as lash_protocol_types::core_support::TurnReplyCoreSupport>::mint(
            TurnId::from("turn"),
            format!("{}.p0", marked.id),
        ),
    );
    committing.replace_from(reply, [SessionHistoryRecord::Conversation(marked)]);
    assert_eq!(
        committing.as_slice().as_ptr(),
        shared,
        "marking the reply copied the window instead of rewriting its slot"
    );
    assert_eq!(committing.len(), window.events().len() + 2);
}

fn window(count: usize) -> TurnWindow {
    let messages = (0..count)
        .map(|index| {
            let id = format!("w{index}");
            Message {
                parts: vec![Part::text(
                    format!("{id}.p0"),
                    format!("window message {index}: {}", "transcript text ".repeat(12)),
                    None,
                )]
                .into(),
                id,
                role: if index % 2 == 0 {
                    MessageRole::User
                } else {
                    MessageRole::Assistant
                },
                origin: None,
                reply_marker: None,
            }
        })
        .collect::<Vec<_>>();
    let events = messages
        .iter()
        .cloned()
        .map(conversation_event)
        .collect::<Vec<_>>();
    TurnWindow::new(
        TurnWindowPin::new(format!("head-{count}")),
        lash_protocol_types::AppendVec::from(messages),
        lash_protocol_types::AppendVec::from(events),
    )
}

#[test]
fn a_mode_history_policy_reconstructs_the_recorded_request_prefix() {
    let committed = window(3);
    let policy =
        lash_protocol_types::session_model::message::HistoryPresentation::new(|_, part| {
            format!("mode history: {}", part.content())
        });
    let config = || {
        let mut config = test_config(Arc::new(ToolBatchDriver));
        config.projector = Arc::new(ChatContextProjector {
            history: policy.clone(),
        });
        config
    };
    let mut replay = crate::testing::JournalReplay::new(
        config(),
        vec![user_message("next")],
        Default::default(),
        0,
    )
    .with_window(committed.clone());
    let mut machine = replay.start();
    let effects = drain_effects(&mut machine);
    let (_, request) = find_llm_call(&effects).expect("the recorded model request");
    let mut restored = replay
        .resume(&mut machine, &|_| Some(ExecutionEnvironmentSync::default()))
        .expect("the recorded request replays");
    let replayed = drain_effects(&mut restored);
    let (_, replayed) = find_llm_call(&replayed).expect("the redelivered model request");
    assert_eq!(
        serde_json::to_value(replayed).unwrap(),
        serde_json::to_value(request).unwrap()
    );
    assert_eq!(
        serde_json::to_value(restored.messages().as_slice()).unwrap(),
        serde_json::to_value(machine.messages().as_slice()).unwrap()
    );
}
