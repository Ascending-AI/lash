//! A turn checkpoint is bounded machine state plus content digests
//! (FIG-5171): its transcript is stored content-addressed beside it, so the
//! body does not grow with the transcript, and a restored machine checkpoints
//! to the same bytes.

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
    for field in ["messages", "prompt_messages", "events"] {
        let head: super::checkpoint_content::CheckpointContentRef =
            serde_json::from_value(body[field].clone()).expect("content digest");
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
            // The synthetic-message counter starts at the history length:
            // a counter, not transcript, so the law compares bodies with it
            // fixed.
            let mut fixed = saved.checkpoint.clone();
            fixed.next_synthetic_message_id = 0;
            body_sizes.push(serde_json::to_vec(&fixed).expect("checkpoint body").len());
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
