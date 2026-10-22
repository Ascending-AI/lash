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
        lash_protocol_types::AppendVec::new(),
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
    let items = |head: &serde_json::Value| -> Vec<serde_json::Value> {
        let head: super::checkpoint_content::CheckpointContentRef =
            serde_json::from_value(head.clone()).expect("content digest");
        saved.content.sequence(&head).expect("stored sequence").0
    };
    let messages = items(&body["messages"]["rest"]);
    let mut prompt = items(&body["prompt_messages"]["rest"]);
    let shared_from = body["prompt_messages"]["messages_from"]
        .as_u64()
        .expect("the prompt's shared messages") as usize;
    let from_window = body["messages"]["from_window"]
        .as_u64()
        .expect("the window's messages") as usize;
    prompt.extend(
        messages
            .iter()
            .skip(shared_from.saturating_sub(from_window))
            .cloned(),
    );
    let events = items(&body["events"]);
    body["messages"] = serde_json::Value::Array(messages);
    body["prompt_messages"] = serde_json::Value::Array(prompt);
    body["events"] = serde_json::Value::Array(events);
    let work = &mut body["state"]["Waiting"]["work"];
    if work["kind"] == "llm" {
        let head: super::checkpoint_content::CheckpointContentRef =
            serde_json::from_value(work["request"].clone()).expect("request digest");
        let mut request: serde_json::Value = saved.content.value(&head).expect("stored request");
        request["messages"] = serde_json::Value::Array(items(&work["messages"]));
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

/// FIG-6910: a turn's records and the resident graph that commits them are
/// one buffer, the window's, which every turn appends to in place; the
/// commit then marks the reply among those records. Once the machine has
/// finished, no live handle but the committing one sees past the window, so
/// the mark rewrites the reply's slot where it stands instead of copying
/// the window into a buffer of its own.
#[test]
fn a_finished_turn_leaves_the_commit_its_records_to_mark_in_place() {
    // A store-read window's history buffer has room past its records, as a
    // grown buffer does: the turn appends there in place.
    let committed = window(64);
    let mut events = lash_protocol_types::AppendVec::with_capacity(128);
    for record in committed.events().iter().cloned() {
        events
            .push(record)
            .expect("source fixture edit is admitted");
    }
    let window = TurnWindow::new(
        committed.pin().clone(),
        committed.messages().clone(),
        events,
    );
    let mut machine = TurnMachine::in_window(
        test_config(Arc::new(ProseDriver)),
        window.clone(),
        window
            .then(vec![user_message("the turn's own input")])
            .unwrap(),
        vec![conversation_event(user_message("the turn's own input"))],
        0,
    )
    .unwrap();
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
    committing
        .extend_adopting(turn_records, lash_protocol_types::same_history_record)
        .unwrap();
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
    committing
        .replace_from(reply, [SessionHistoryRecord::Conversation(marked)])
        .expect("source fixture edit is admitted");
    assert_eq!(
        committing.as_slice().as_ptr(),
        shared,
        "marking the reply copied the window instead of rewriting its slot"
    );
    assert_eq!(committing.len(), window.events().len() + 2);
}

/// A committed window of `count` messages.
fn window(count: usize) -> TurnWindow {
    let messages = (0..count)
        .map(|index| {
            let id = format!("w{index}");
            Message {
                parts: std::sync::Arc::new(
                    (vec![Part::text(
                        format!("{id}.p0"),
                        format!("window message {index}: {}", "transcript text ".repeat(12)),
                        None,
                    )])
                    .into(),
                ),
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

/// A turn over `window` with one input of its own, waiting on its first
/// model call, with that call.
fn windowed_llm_pending(window: &TurnWindow) -> (TurnMachine, EffectId, LlmRequest) {
    let mut machine = TurnMachine::in_window(
        test_config(Arc::new(ToolBatchDriver)),
        window.clone(),
        window
            .then(vec![user_message("the turn's own input")])
            .unwrap(),
        Vec::new(),
        0,
    )
    .unwrap();
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
                    serde_json::to_vec(&redelivered).expect("request"),
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

/// What each finish call of [`FinishingDriver`] carries.
const FINISH_VALUE: &str = "the finish value a superseded candidate keeps";

/// Settles a finish call on every model reply and hands its candidate to
/// BeforeCompletion.
struct FinishingDriver;

impl ProtocolDriverHandle for FinishingDriver {
    fn prepare_protocol_iteration(&self, ctx: DriverContextView<'_>) -> Vec<DriverAction> {
        vec![DriverAction::Start(PendingWork::Llm {
            request: ctx
                .project_llm_request_retained(false)
                .expect("fixture history projects"),
            driver_state: None,
        })]
    }

    fn handle_llm_success(
        &self,
        ctx: DriverContextView<'_>,
        _request: Arc<LlmRequest>,
        _driver_state: Option<serde_json::Value>,
        _llm_response: LlmResponse,
        _calls: &crate::ResponseToolCalls,
        _text_streamed: bool,
    ) -> Vec<DriverAction> {
        let iteration = ctx.protocol_iteration();
        vec![DriverAction::Start(PendingWork::Checkpoint {
            checkpoint: CheckpointKind::BeforeCompletion,
            on_empty: CheckpointResumeAction::Complete {
                candidate: Box::new(lash_protocol_types::CompletionCandidate::new(
                    iteration,
                    lash_protocol_types::ToolCallId::fixture(&format!("finish-{iteration}")),
                    "finish",
                    lash_protocol_types::TurnControl::Finish {
                        value: lash_protocol_types::ToolValue::String(FINISH_VALUE.repeat(64)),
                        value_schema: None,
                    },
                )),
                superseded: Vec::new(),
            },
        })]
    }

    fn handle_tool_results(
        &self,
        _ctx: DriverContextView<'_>,
        _completed: Vec<CompletedToolCall>,
    ) -> Vec<DriverAction> {
        Vec::new()
    }

    fn handle_exec_result(
        &self,
        _ctx: DriverContextView<'_>,
        _driver_state: serde_json::Value,
        _result: Result<lash_protocol_types::ExecResponse, lash_protocol_types::ExecCodeFailure>,
    ) -> Vec<DriverAction> {
        Vec::new()
    }
}

/// FIG-5831: a decided completion candidate keeps its control, which grows
/// with what the call carried, and a turn decides one at every
/// BeforeCompletion that input supersedes. The checkpoint names its decided
/// history by digest, as it names the turn's records, so the body holds no
/// decided candidate however many the turn superseded, and a restored
/// machine holds the same history in the same order.
#[test]
fn decided_completion_candidates_are_checkpoint_content_not_body() {
    let mut machine = TurnMachine::new(
        test_config(Arc::new(FinishingDriver)),
        vec![user_message("finish")],
        lash_protocol_types::AppendVec::new(),
        0,
    );
    let mut effects = drain_effects(&mut machine);
    for _ in 0..3 {
        let llm_id = *find_llm_call(&effects).expect("the model call").0;
        machine.handle_response(Response::LlmComplete {
            id: llm_id,
            text_streamed: false,
            result: Ok(LlmResponse::default()),
        });
        let (id, kind) =
            find_checkpoint(&drain_effects(&mut machine)).expect("the completion checkpoint");
        assert_eq!(kind, CheckpointKind::BeforeCompletion);
        machine.handle_response(Response::Checkpoint {
            id,
            delivery: CheckpointDelivery {
                committed_user_messages: vec![user_message("go on")],
            },
        });
        effects = drain_effects(&mut machine);
    }
    let decided = machine.decided_completions().to_vec();
    assert_eq!(
        decided.len(),
        3,
        "each BeforeCompletion decided its candidate"
    );

    let saved = machine.checkpoint();
    let body = serde_json::to_string(&saved.checkpoint).expect("checkpoint body");
    assert!(
        !body.contains(FINISH_VALUE),
        "the body holds a decided candidate's control: {} bytes",
        body.len()
    );
    let restored = TurnMachine::restore_from_checkpoint(
        test_config(Arc::new(FinishingDriver)),
        roundtrip_checkpoint(saved),
        None,
    )
    .expect("the checkpoint restores");
    assert_eq!(restored.decided_completions(), decided.as_slice());
}

/// What a host stored of a turn's phases: every blob, by digest.
#[derive(Default)]
struct PhaseStore {
    blobs: std::collections::BTreeMap<super::checkpoint_content::CheckpointContentRef, Vec<u8>>,
}

impl PhaseStore {
    /// The host's pin of `digest`: its first 32 bytes.
    fn pin(digest: &super::checkpoint_content::CheckpointContentRef) -> CheckpointContentPin {
        let mut pin = [0; 32];
        pin.copy_from_slice(&digest.as_str().as_bytes()[..32]);
        CheckpointContentPin::new(pin)
    }

    /// Store `machine`'s phase checkpoint as a host does and answer how many
    /// items it encoded. The checkpoint is the one a full checkpoint takes;
    /// its content holds every blob of that one it does not hold by a pin,
    /// and it holds only blobs an earlier phase stored, by their pins.
    fn phase(&mut self, machine: &mut TurnMachine, phase: &str) -> usize {
        let full = machine.checkpoint();
        let delta = machine.checkpoint_delta();
        assert_eq!(
            serde_json::to_vec(&delta.saved.checkpoint).expect("delta body"),
            serde_json::to_vec(&full.checkpoint).expect("full body"),
            "{phase}: the delta is the full checkpoint"
        );
        for (digest, pin) in &delta.held {
            assert!(
                self.blobs.contains_key(digest) && *pin == Self::pin(digest),
                "{phase}: held blob {digest} was stored under its pin"
            );
            assert!(
                !delta.saved.content.contains(digest),
                "{phase}: held blob {digest} is not stored again"
            );
        }
        for (digest, bytes) in full.content.iter() {
            let held = delta.held.iter().any(|(held, _)| held == digest);
            if !held {
                assert!(
                    delta.saved.content.contains(digest),
                    "{phase}: new blob {digest} is in the delta"
                );
            }
            if let Some(stored) = self.blobs.get(digest) {
                assert_eq!(stored, bytes, "{phase}: blob {digest} is the stored bytes");
            }
        }
        let mut pins = Vec::new();
        for (digest, bytes) in delta.saved.content.iter() {
            assert!(
                full.content.contains(digest),
                "{phase}: the delta's blob {digest} is the full checkpoint's"
            );
            self.blobs.insert(digest.clone(), bytes.to_vec());
            pins.push((digest.clone(), Self::pin(digest)));
        }
        machine.checkpoint_stored(pins);
        delta.encoded
    }
}

/// The items each phase of `rounds` tool rounds and two more encodes, over
/// a window of `count` messages: at each round's model call and at its tool
/// batch. A machine restored from the checkpoint after `rounds` and pinned
/// to what was stored runs the two more rounds beside the uncut machine;
/// from its first tool batch on it encodes what the uncut one does.
fn encoded_per_phase(count: usize, rounds: usize) -> Vec<usize> {
    let window = window(count);
    let mut machine = TurnMachine::in_window(
        test_config(Arc::new(ToolBatchDriver)),
        window.clone(),
        window
            .then(vec![user_message("the turn's own input")])
            .unwrap(),
        Vec::new(),
        0,
    )
    .unwrap();
    let mut store = PhaseStore::default();
    let mut encoded = Vec::new();
    let round = |machine: &mut TurnMachine, store: &mut PhaseStore, index: usize| {
        let effects = drain_effects(machine);
        let llm_id = *find_llm_call(&effects).expect("the round's model call").0;
        let start = store.phase(machine, &format!("round {index} model call"));
        machine.handle_response(Response::LlmComplete {
            id: llm_id,
            text_streamed: false,
            result: Ok(LlmResponse::default()),
        });
        let effects = drain_effects(machine);
        let Some(Effect::ToolCalls { id, .. }) = effects.last() else {
            panic!("round {index} waits on its tools: {effects:?}");
        };
        let tools = *id;
        let done = store.phase(machine, &format!("round {index} tool batch"));
        machine.handle_response(Response::ToolResults {
            id: tools,
            results: ["call-read", "call-search", "call-slow"]
                .into_iter()
                .map(|call| {
                    completed_tool(
                        call,
                        "read_file",
                        serde_json::json!({}),
                        ToolCallOutput::success(serde_json::json!("result ".repeat(40))),
                    )
                })
                .collect(),
        });
        let effects = drain_effects(machine);
        let (checkpoint, _) = find_checkpoint(&effects).expect("the round's checkpoint");
        machine.handle_response(Response::Checkpoint {
            id: checkpoint,
            delivery: CheckpointDelivery::default(),
        });
        [start, done]
    };
    for index in 0..rounds {
        encoded.extend(round(&mut machine, &mut store, index));
    }
    // A cold owner restores the stored checkpoint, learns the pins of what
    // it is stored as, and builds on it as the uncut owner does.
    let saved = machine.checkpoint();
    let mut cold_store = PhaseStore {
        blobs: store.blobs.clone(),
    };
    for (digest, bytes) in saved.content.iter() {
        cold_store.blobs.insert(digest.clone(), bytes.to_vec());
    }
    let mut cold = restored(&saved, Some(window.clone()));
    cold.checkpoint_stored(
        saved
            .content
            .iter()
            .map(|(digest, _)| (digest.clone(), PhaseStore::pin(digest))),
    );
    let uncut = round(&mut machine, &mut store, rounds);
    let resumed = round(&mut cold, &mut cold_store, rounds);
    // Its first model call stores the request whole once: the request an
    // ended call left is kept only by the owner that stored it.
    assert_eq!(
        resumed[1], uncut[1],
        "a restored machine's tool batch encodes what the uncut one's does"
    );
    let next = round(&mut machine, &mut store, rounds + 1);
    assert_eq!(
        round(&mut cold, &mut cold_store, rounds + 1),
        next,
        "after its first model call a restored machine encodes what the uncut one does"
    );
    encoded.extend(uncut);
    encoded.extend(next);
    encoded
}

/// FIG-5955: a phase checkpoint encodes only the items the turn appended
/// since its last stored phase, plus a constant: the same at a window of 4
/// messages and of 400, and the same at every round after the first,
/// however many rounds came before it. The full checkpoint it equals would
/// encode every item the turn holds.
#[test]
fn a_phase_checkpoint_encodes_only_its_new_items() {
    let rounds = 12;
    let shallow = encoded_per_phase(4, rounds);
    let deep = encoded_per_phase(400, rounds);
    eprintln!("items encoded per phase: {shallow:?}");
    assert_eq!(
        shallow, deep,
        "the items a phase encodes do not depend on the window"
    );
    let steady = &shallow[2..4];
    for (round, phases) in shallow.chunks(2).enumerate().skip(1) {
        assert_eq!(
            phases, steady,
            "round {round} encodes what round 1 did: only its new items"
        );
    }
    let mut full = TurnMachine::in_window(
        test_config(Arc::new(ToolBatchDriver)),
        window(4),
        window(4)
            .then(vec![user_message("the turn's own input")])
            .unwrap(),
        Vec::new(),
        0,
    )
    .unwrap();
    let mut first = PhaseStore::default();
    drain_effects(&mut full);
    let first_phase = first.phase(&mut full, "the first model call");
    assert_eq!(
        first_phase, shallow[0],
        "the first phase encodes everything the turn holds"
    );
    assert!(
        steady.iter().all(|&count| count < 16),
        "a round's phases encode a bounded count: {steady:?}"
    );
}

/// FIG-5955: a prompt view the attachment-omission policies narrowed inside
/// the window (ADR 0133) differs from the turn's messages there, and only
/// there. Its checkpoint stores the view's own message and names the rest:
/// the window's messages before it and the turn's messages after it, which
/// are stored once, as the turn's. The view, the messages and the model
/// request restore byte-identical, and later phases store only what they
/// add.
#[test]
fn a_narrowed_prompt_view_stores_only_where_it_differs() {
    let window = window(10);
    let mut machine = TurnMachine::in_window(
        test_config(Arc::new(ToolBatchDriver)),
        window.clone(),
        window
            .then(vec![user_message("the turn's own input")])
            .unwrap(),
        Vec::new(),
        0,
    )
    .unwrap();
    let mut view = machine
        .prompt_message_sequence()
        .iter()
        .cloned()
        .collect::<Vec<_>>();
    view[3].parts = lash_protocol_types::shared_parts(vec![Part::text(
        "w3.p0".to_owned(),
        "[omitted]".to_owned(),
        None,
    )]);
    machine
        .adopt_prepared_messages(MessageSequence::from_owned(view))
        .unwrap();
    let mut store = PhaseStore::default();
    let effects = drain_effects(&mut machine);
    let (llm_id, request) = find_llm_call(&effects).expect("the model call");
    let (llm_id, request) = (*llm_id, request.clone());
    assert!(
        serde_json::to_string(&request)
            .expect("request")
            .contains("[omitted]"),
        "the request is projected from the narrowed view"
    );
    store.phase(&mut machine, "the model call");
    let saved = machine.checkpoint();
    let body = serde_json::to_value(&saved.checkpoint).expect("checkpoint body");
    assert_eq!(body["prompt_messages"]["from_window"], 3);
    assert_eq!(
        body["prompt_messages"]["messages_from"], 4,
        "the view continues with the turn's messages after its own"
    );
    let rest: super::checkpoint_content::CheckpointContentRef =
        serde_json::from_value(body["prompt_messages"]["rest"].clone()).expect("digest");
    let (own, _): (Vec<Message>, _) = saved.content.sequence(&rest).expect("the view's own");
    assert_eq!(own.len(), 1, "the view stores only its narrowed message");

    let mut restored = restored(&saved, Some(window.clone()));
    assert_eq!(encode(&restored.checkpoint()), encode(&saved));
    assert_eq!(
        serde_json::to_value(restored.prompt_message_sequence().as_slice()).expect("view"),
        serde_json::to_value(machine.prompt_message_sequence().as_slice()).expect("view"),
    );
    let effects = drain_effects(&mut restored);
    let (_, redelivered) = find_llm_call(&effects).expect("re-delivered model call");
    assert_eq!(
        serde_json::to_vec(&redelivered).expect("request"),
        serde_json::to_vec(&request).expect("request"),
        "the re-delivered request is the narrowed one"
    );

    machine.handle_response(Response::LlmComplete {
        id: llm_id,
        text_streamed: false,
        result: Ok(LlmResponse::default()),
    });
    drain_effects(&mut machine);
    assert_eq!(
        store.phase(&mut machine, "the tool batch"),
        0,
        "a phase that adds nothing encodes nothing"
    );
}

/// RECORDED-REQUEST: a fresh owner reconstructs the same mode-authored
/// history prefix, without changing committed records or their order.
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
    let mut machine = TurnMachine::in_window(
        config(),
        committed.clone(),
        committed.then(vec![user_message("next")]).unwrap(),
        Vec::new(),
        0,
    )
    .unwrap();
    let effects = drain_effects(&mut machine);
    let (_, request) = find_llm_call(&effects).expect("the recorded model request");
    let saved = machine.checkpoint();
    let mut restored = TurnMachine::restore_from_checkpoint(
        config(),
        roundtrip_checkpoint(saved.clone()),
        Some(window(3)),
    )
    .expect("the mode's history reconstructs on a fresh owner");
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
    assert_eq!(encode(&restored.checkpoint()), encode(&saved));
}
