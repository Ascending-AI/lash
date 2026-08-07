// Projection of a chat snapshot from the two sources the workbench reads: the
// durable session graph, which is authoritative, and the product-event log the
// UI owns. Where the two carry the same turn's user text, the UI-owned row
// renders and the committed copy stays provenance (FIG-972).

/// The user rows this snapshot renders on the workbench's own authority: the
/// optimistic rows still live in the product-event log, plus the prompt rows the
/// workbench replays for a running turn whose product row a restart lost.
///
/// Returns the turns those rows speak for and the replayed prompt rows, which
/// the caller appends after the product rows so ordering is unchanged.
fn ui_owned_user_rows(
    state: &AppState,
    active_turns: &[lash::TurnAddress],
    product_messages: &[ChatMessage],
) -> (BTreeSet<String>, Vec<ChatMessage>) {
    let mut turn_ids = product_messages
        .iter()
        .filter_map(|message| workbench_turn_id_from_user_message_id(&message.id))
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    let mut replayed_prompts = Vec::new();
    for address in active_turns {
        if turn_ids.contains(&address.turn_id) {
            continue;
        }
        let Some(text) = state
            .active_turns
            .prompt_for(&address.session_id, &address.turn_id)
        else {
            continue;
        };
        turn_ids.insert(address.turn_id.clone());
        replayed_prompts.push(ChatMessage {
            id: workbench_turn_user_message_id(&address.turn_id),
            role: "user".to_string(),
            text,
            at: String::new(),
            attachments: Vec::new(),
        });
    }
    (turn_ids, replayed_prompts)
}

/// The committed messages this snapshot must not render because a UI-owned row
/// already speaks for them.
///
/// The runtime stamps every committed turn-input message with
/// `MessageOrigin::TurnInput`, so the workbench recognizes its own send in the
/// durable transcript without pinning or parsing a runtime message id
/// (FIG-972). Only the turn's *opening* input is suppressed — one per turn, in
/// commit order — because an input injected mid-turn is a further turn-input
/// message on the same turn that no UI-owned row stands in for. When no UI row
/// survives (the product log was truncated, or the turn came from a trigger or
/// mail rather than the chat box) nothing is suppressed and the committed copy
/// is what the transcript renders.
fn suppressed_turn_input_message_ids(
    read_view: &lash::persistence::SessionReadView,
    ui_user_turn_ids: &BTreeSet<String>,
) -> BTreeSet<String> {
    let mut suppressed = BTreeSet::new();
    let mut turns_already_covered = BTreeSet::new();
    for message in read_view.messages() {
        let Some(lash::messages::MessageOrigin::TurnInput { turn_id, .. }) =
            message.origin.as_ref()
        else {
            continue;
        };
        if !ui_user_turn_ids.contains(turn_id) {
            continue;
        }
        if turns_already_covered.insert(turn_id.clone()) {
            suppressed.insert(message.id.clone());
        }
    }
    suppressed
}

fn chat_message_from_committed(message: &lash::messages::Message) -> ChatMessage {
    ChatMessage {
        id: message.id.clone(),
        role: lash::message_role(message).to_string(),
        text: lash::message_text(message),
        // The durable session graph records ordering but not a presentation
        // timestamp. The workbench does not render this field, so keep the
        // established wire shape without fabricating a time during resume.
        at: String::new(),
        attachments: message
            .parts
            .iter()
            .filter_map(|part| part.attachment.as_ref()?.source.stored_ref())
            .map(|attachment| ChatAttachment::from_id(attachment.id.to_string()))
            .collect(),
    }
}

fn transcript_rows_from_committed(
    read_view: &lash::persistence::SessionReadView,
    suppressed_message_ids: &BTreeSet<String>,
) -> Vec<TranscriptRow> {
    read_view
        .chronological_projection()
        .into_entries()
        .into_iter()
        .filter_map(|entry| match entry.payload {
            lash::persistence::ChronologicalPayload::Message(message)
                if !suppressed_message_ids.contains(&message.id) =>
            {
                Some(TranscriptRow::Message {
                    message: chat_message_from_committed(&message),
                })
            }
            lash::persistence::ChronologicalPayload::Message(_) => None,
            lash::persistence::ChronologicalPayload::ProtocolEvent(event) => {
                match lash_protocol_rlm::decode_rlm_protocol_event(&event) {
                    Some(lash_rlm_types::RlmProtocolEvent::RlmAssistantContent(content))
                        if !content.reasoning.trim().is_empty() =>
                    {
                        Some(TranscriptRow::Reasoning {
                            id: content.id,
                            text: content.reasoning,
                        })
                    }
                    Some(lash_rlm_types::RlmProtocolEvent::RlmTrajectoryEntry(step))
                        if !step.code.trim().is_empty() =>
                    {
                        let mut output = step.output.join("\n");
                        if let Some(final_output) = step.final_output {
                            let final_output = serde_json::to_string_pretty(&final_output)
                                .unwrap_or_else(|_| final_output.to_string());
                            if !output.is_empty() {
                                output.push('\n');
                            }
                            output.push_str(&final_output);
                        }
                        Some(TranscriptRow::CodeBlock {
                            id: step.id,
                            language: "lashlang".to_string(),
                            code: step.code,
                            output,
                            success: step.error.is_none(),
                            error: step.error,
                        })
                    }
                    _ => None,
                }
            }
        })
        .collect()
}
