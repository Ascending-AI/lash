//! A turn checkpoint is bounded machine state plus content digests
//! (FIG-5171): its transcript is stored content-addressed beside it, so the
//! body does not grow with the transcript, and a restored machine checkpoints
//! to the same bytes. A turn started from a committed window (FIG-5206)
//! stores only what it added to the window, so its content does not grow
//! with the session's history either.

use super::*;

fn history(count: usize) -> Vec<Message> {
    (0..count)
        .map(|index| {
            user_message(&format!(
                "message {index}: {}",
                "transcript text ".repeat(12)
            ))
        })
        .collect()
}

/// A machine over `count` history messages, waiting on its first model
/// call, with that call's effect id.
fn llm_pending(count: usize) -> (TurnMachine, EffectId) {
    let mut machine = TurnMachine::new(
        test_config(Arc::new(ToolBatchDriver)),
        history(count),
        crate::AppendVec::new(),
        0,
    );
    let effects = drain_effects(&mut machine);
    let llm_id = *find_llm_call(&effects).expect("first model call").0;
    (machine, llm_id)
}

fn waiting_on_llm(count: usize) -> TurnMachine {
    llm_pending(count).0
}

/// The same machine, answered and waiting on its tool batch.
fn waiting_on_tools(count: usize) -> TurnMachine {
    let (mut machine, llm_id) = llm_pending(count);
    machine.handle_response(Response::LlmComplete {
        id: llm_id,
        text_streamed: false,
        result: Ok(LlmResponse::default()),
    });
    let effects = drain_effects(&mut machine);
    assert!(
        matches!(effects.last(), Some(Effect::ToolCalls { .. })),
        "{effects:?}"
    );
    machine
}

fn encode(saved: &SavedTurn) -> (Vec<u8>, Vec<u8>) {
    (
        serde_json::to_vec(&saved.checkpoint).expect("checkpoint body"),
        serde_json::to_vec(&saved.content).expect("checkpoint content"),
    )
}

/// The checkpoint as the earlier encoding wrote it: the transcript and a
/// pending model request inline in the body.
fn inline_encoding_bytes(saved: &SavedTurn) -> usize {
    let mut body = serde_json::to_value(&saved.checkpoint).expect("checkpoint body");
    for (field, head) in [
        ("messages", &body["messages"]["rest"]),
        ("prompt_messages", &body["prompt_messages"]["rest"]),
        ("events", &body["events"]),
    ]
    .map(|(field, head)| (field, head.clone()))
    {
        let head: super::checkpoint_content::CheckpointContentRef =
            serde_json::from_value(head).expect("content digest");
        let items: Vec<serde_json::Value> = saved.content.sequence(&head).expect("stored sequence");
        body[field] = serde_json::Value::Array(items);
    }
    let work = &mut body["state"]["Waiting"]["work"];
    if work["kind"] == "llm" {
        let head: super::checkpoint_content::CheckpointContentRef =
            serde_json::from_value(work["request"].clone()).expect("request digest");
        let request: serde_json::Value = saved.content.value(&head).expect("stored request");
        *work =
            serde_json::json!({"Llm": {"request": request, "driver_state": work["driver_state"]}});
    }
    serde_json::to_vec(&body).expect("inline body").len()
}

#[test]
fn checkpoint_restore_checkpoint_is_byte_identical() {
    for count in [1, 10, 50] {
        for (state, machine) in [
            ("llm", waiting_on_llm(count)),
            ("tools", waiting_on_tools(count)),
        ] {
            let saved = machine.checkpoint();
            let (body, content) = encode(&saved);
            let decoded: SavedTurn =
                serde_json::from_slice(&serde_json::to_vec(&saved).expect("saved turn"))
                    .expect("decoded saved turn");
            let restored = TurnMachine::restore_from_checkpoint(
                test_config(Arc::new(ToolBatchDriver)),
                decoded,
                None,
            )
            .expect("supported checkpoint");
            assert_eq!(
                encode(&restored.checkpoint()),
                (body, content),
                "{count} messages, waiting on {state}"
            );
        }
    }
}

#[test]
fn checkpoint_body_size_is_independent_of_message_count() {
    let mut rows = Vec::new();
    for (state, build) in [
        ("llm", waiting_on_llm as fn(usize) -> TurnMachine),
        ("tools", waiting_on_tools),
    ] {
        let mut body_sizes = Vec::new();
        for count in [1, 10, 50] {
            let saved = build(count).checkpoint();
            let (body, content) = encode(&saved);
            rows.push(format!(
                "waiting on {state}, {count:>2} messages: before {} bytes inline; after {} body + {} content bytes in {} blobs",
                inline_encoding_bytes(&saved),
                body.len(),
                content.len(),
                saved.content.len(),
            ));
            body_sizes.push(body.len());
        }
        assert!(
            body_sizes.windows(2).all(|pair| pair[0] == pair[1]),
            "waiting on {state}: body sizes {body_sizes:?} grew with the transcript"
        );
    }
    eprintln!("{}", rows.join("\n"));
}

#[test]
fn restore_refuses_missing_or_corrupt_checkpoint_content() {
    let saved = waiting_on_llm(10).checkpoint();
    let (missing, _) = saved.content.iter().next().expect("stored content");
    let missing = missing.clone();
    let mut without = TurnCheckpointContent::default();
    let mut corrupt = TurnCheckpointContent::default();
    for (digest, bytes) in saved.content.iter() {
        if *digest != missing {
            without
                .insert_stored(digest.clone(), bytes.to_vec())
                .expect("verified content");
        }
        assert!(matches!(
            corrupt.insert_stored(digest.clone(), b"[]".to_vec()),
            Err(TurnCheckpointRestoreError::CorruptContent { .. })
        ));
    }
    let Err(error) = TurnMachine::restore_from_checkpoint(
        test_config(Arc::new(ToolBatchDriver)),
        SavedTurn {
            checkpoint: saved.checkpoint,
            content: without,
        },
        None,
    ) else {
        panic!("missing content is refused");
    };
    assert_eq!(
        error,
        TurnCheckpointRestoreError::MissingContent {
            content: missing.as_str().to_string()
        }
    );
}

/// A committed window of `count` messages.
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
        crate::AppendVec::from(messages),
        crate::AppendVec::from(events),
    )
}

/// A turn over `window` with one input of its own, waiting on its first
/// model call, with that call.
fn windowed_llm_pending(window: &TurnWindow) -> (TurnMachine, EffectId, LlmRequest) {
    let mut machine = TurnMachine::in_window(
        test_config(Arc::new(ToolBatchDriver)),
        window.clone(),
        window.then(vec![user_message("the turn's own input")]),
        Vec::new(),
        0,
    );
    let effects = drain_effects(&mut machine);
    let (id, request) = find_llm_call(&effects).expect("first model call");
    (machine, *id, request.clone())
}

/// The same turn, answered and waiting on its tool batch.
fn windowed_waiting_on_tools(window: &TurnWindow) -> TurnMachine {
    let (mut machine, llm_id, _) = windowed_llm_pending(window);
    machine.handle_response(Response::LlmComplete {
        id: llm_id,
        text_streamed: false,
        result: Ok(LlmResponse::default()),
    });
    let effects = drain_effects(&mut machine);
    assert!(
        matches!(effects.last(), Some(Effect::ToolCalls { .. })),
        "{effects:?}"
    );
    machine
}

fn restored(saved: &SavedTurn, window: Option<TurnWindow>) -> TurnMachine {
    let decoded: SavedTurn =
        serde_json::from_slice(&serde_json::to_vec(saved).expect("saved turn"))
            .expect("decoded saved turn");
    TurnMachine::restore_from_checkpoint(test_config(Arc::new(ToolBatchDriver)), decoded, window)
        .expect("the window the checkpoint names")
}

#[test]
fn a_windowed_checkpoint_holds_only_the_turn_and_restores_over_its_window() {
    for (state, waiting) in [
        (
            "llm",
            (|window| windowed_llm_pending(window).0) as fn(&TurnWindow) -> TurnMachine,
        ),
        ("tools", windowed_waiting_on_tools),
    ] {
        let mut contents = Vec::new();
        for count in [0, 10, 300] {
            let window = window(count);
            let machine = waiting(&window);
            let saved = machine.checkpoint();
            assert_eq!(
                saved.checkpoint.window_pin(),
                Some(window.pin()),
                "waiting on {state}"
            );
            contents.push((count, serde_json::to_vec(&saved.content).expect("content")));

            let mut restored = restored(&saved, Some(window.clone()));
            assert_eq!(
                encode(&restored.checkpoint()),
                encode(&saved),
                "{count} window messages, waiting on {state}"
            );
            assert_eq!(
                serde_json::to_value(restored.messages().as_slice()).expect("messages"),
                serde_json::to_value(machine.messages().as_slice()).expect("messages"),
                "{count} window messages, waiting on {state}"
            );
            assert_eq!(
                serde_json::to_value(restored.events().as_slice()).expect("events"),
                serde_json::to_value(machine.events().as_slice()).expect("events"),
                "{count} window messages, waiting on {state}"
            );
            if state == "llm" {
                let (_, _, pinned) = windowed_llm_pending(&window);
                let effects = drain_effects(&mut restored);
                let (_, redelivered) = find_llm_call(&effects).expect("re-delivered model call");
                assert_eq!(
                    serde_json::to_vec(redelivered).expect("request"),
                    serde_json::to_vec(&pinned).expect("request"),
                    "{count} window messages: the re-delivered request is the pinned one"
                );
            }
        }
        assert!(
            contents.windows(2).all(|pair| pair[0].1 == pair[1].1),
            "waiting on {state}: the content grew with the window: {:?}",
            contents
                .iter()
                .map(|(count, content)| (*count, content.len()))
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn a_windowed_checkpoint_refuses_any_window_but_its_own() {
    let window = window(10);
    let saved = windowed_llm_pending(&window).0.checkpoint();
    let decoded = || -> SavedTurn {
        serde_json::from_slice(&serde_json::to_vec(&saved).expect("saved turn"))
            .expect("decoded saved turn")
    };
    let other_pin = TurnWindow::new(
        TurnWindowPin::new("head-other".to_owned()),
        window.messages().clone(),
        window.events().clone(),
    );
    let mut shorter = window.messages().clone();
    shorter.truncate(9);
    let shorter = TurnWindow::new(window.pin().clone(), shorter, window.events().clone());
    for (case, handed) in [
        ("no window", None),
        ("another pin", Some(other_pin)),
        ("fewer messages", Some(shorter)),
    ] {
        let Err(error) = TurnMachine::restore_from_checkpoint(
            test_config(Arc::new(ToolBatchDriver)),
            decoded(),
            handed,
        ) else {
            panic!("{case}: a restore over another window is refused");
        };
        assert!(
            matches!(error, TurnCheckpointRestoreError::WindowMismatch { .. }),
            "{case}: {error:?}"
        );
    }
}
