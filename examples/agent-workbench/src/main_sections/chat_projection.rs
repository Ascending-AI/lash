// Projection of a chat snapshot from the two sources the workbench reads: the
// durable session graph, which is authoritative, and the product-event log the
// UI owns. Where the two carry the same turn's user text, the UI-owned row
// renders and the committed copy stays provenance (FIG-972).

/// The user rows this snapshot renders on the workbench's own authority: the
/// optimistic rows still live in the product-event log, plus the prompt rows the
/// workbench replays for a running turn whose product row a restart lost.
///
/// Returns replayed prompt rows, which the caller appends after the product
/// rows so ordering is unchanged.
fn replayed_active_user_rows(
    state: &AppState,
    active_turns: &[lash::TurnAddress],
    product_messages: &[ChatMessage],
) -> Vec<ChatMessage> {
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
        let Some(prompt) = state
            .active_turns
            .prompt_for(&address.session_id, &address.turn_id)
        else {
            continue;
        };
        turn_ids.insert(address.turn_id.clone());
        replayed_prompts.push(ChatMessage {
            id: workbench_turn_user_message_id(&address.turn_id),
            role: "user".to_string(),
            text: prompt.text,
            at: String::new(),
            attachments: prompt
                .attachment_id
                .into_iter()
                .map(ChatAttachment::from_id)
                .collect(),
        });
    }
    replayed_prompts
}

/// The committed messages this snapshot replaces with UI-owned user rows.
///
/// The runtime stamps every committed turn-input message with
/// `MessageOrigin::TurnInput`, so the workbench recognizes its own send in the
/// durable transcript without pinning or parsing a runtime message id
/// (FIG-972). Only the turn's *opening* input is replaced — one per turn, in
/// commit order — because an input injected mid-turn is a further turn-input
/// message on the same turn that no UI-owned row stands in for. When no UI row
/// survives (the product log was truncated, or the turn came from a trigger or
/// mail rather than the chat box) nothing is replaced and the committed copy
/// is what the transcript renders.
fn ui_owned_turn_input_replacements(
    read_view: &lash::persistence::SessionReadView,
    ui_user_rows: &BTreeMap<String, ChatMessage>,
) -> BTreeMap<String, ChatMessage> {
    let mut replacements = BTreeMap::new();
    let mut turns_already_covered = BTreeSet::new();
    for message in read_view.messages() {
        let Some(lash::messages::MessageOrigin::TurnInput { turn_id, .. }) =
            message.origin.as_ref()
        else {
            continue;
        };
        let Some(ui_row) = ui_user_rows.get(turn_id) else {
            continue;
        };
        if turns_already_covered.insert(turn_id.clone()) {
            replacements.insert(message.id.clone(), ui_row.clone());
        }
    }
    replacements
}

/// The current frame's opening task is protocol state, not another chat send.
///
/// A `continue_as` frame is identified by its typed `AgentFrameReason`; its
/// first typed `MessageOrigin::TurnInput` is the task that drives the follow
/// frame. Hiding it does not depend on either the runtime message id or a
/// textual comparison with the tool arguments. Later turn inputs remain chat
/// rows, including inputs injected while the follow turn is running.
fn continue_as_protocol_state_message_ids(
    read_view: &lash::persistence::SessionReadView,
) -> BTreeSet<String> {
    let graph = read_view.session_graph();
    // Session nodes are sequence-ordered, and frame-open nodes on the active
    // path therefore end with the current frame. Keep this hand-walk local
    // until lash-core exposes a current-frame facade accessor.
    let current_frame_is_continue_as = graph
        .nodes
        .iter()
        .filter(|node| graph.active_path_contains(&node.node_id))
        .filter_map(|node| node.frame_open().map(|(reason, _, _)| reason))
        .next_back()
        .is_some_and(|reason| reason.as_str() == "continue_as");
    if !current_frame_is_continue_as {
        return BTreeSet::new();
    }
    // This first-TurnInput heuristic rests on the follow task always committing
    // first: AgentFrameTask materializes a non-empty input, and `task` is
    // required by the continue_as schema.
    read_view
        .messages()
        .iter()
        .find(|message| {
            matches!(
                message.origin,
                Some(lash::messages::MessageOrigin::TurnInput { .. })
            )
        })
        .map(|message| BTreeSet::from([message.id.clone()]))
        .unwrap_or_default()
}

fn chat_message_from_committed(message: &lash::messages::Message) -> ChatMessage {
    ChatMessage {
        id: message.id.clone(),
        role: lash::message_role(message).to_string(),
        text: committed_chat_text(message),
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

fn committed_chat_text(message: &lash::messages::Message) -> String {
    message
        .parts
        .iter()
        .filter(|part| !matches!(part.kind, lash_core::PartKind::Reasoning))
        .map(|part| part.content.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_durable_internal_rlm_message(message: &lash::messages::Message) -> bool {
    matches!(
        message.origin.as_ref(),
        Some(lash::messages::MessageOrigin::Plugin {
            plugin_id,
            transient: false,
        }) if plugin_id == lash_protocol_rlm::RLM_PROTOCOL_PLUGIN_ID
    )
}

/// Whether this plugin-authored RLM message could be a turn's committed reply:
/// an assistant message carrying visible prose. The protocol's system copies —
/// finish reminders, retry copy, cell diagnostics — never can be, and its
/// reasoning-only messages carry nothing for a chat row to say.
fn is_rlm_assistant_prose_message(message: &lash::messages::Message) -> bool {
    is_durable_internal_rlm_message(message)
        && lash::message_role(message) == "assistant"
        && message.parts.iter().any(|part| {
            matches!(part.kind, lash_core::PartKind::Prose) && !part.content.trim().is_empty()
        })
}

/// The plugin-authored messages that *are* a turn's user-visible reply.
///
/// The RLM protocol commits the model's prose as a plugin-origin assistant
/// message on every protocol iteration. Mid-turn copies are context for the
/// next request — the transcript keeps only their reasoning — but a turn whose
/// answer carries reasoning has its **answer** committed the same way, and then
/// the runtime mints no terminal message of its own: `materialize_terminal_output`
/// finds that text already last in the transcript and returns. Treating every
/// plugin-origin message as internal therefore dropped the reply itself and
/// re-admitted only its reasoning (FIG-1406).
///
/// The reply is the last assistant message a turn committed, so this walks the
/// transcript in commit order and keeps, per turn, the final plugin-authored
/// assistant prose message — abandoning the candidate as soon as an ordinary
/// assistant message follows it, because that runtime or workbench copy is then
/// the reply and the plugin message behind it is superseded context. Turns are
/// delimited by their typed `MessageOrigin::TurnInput`, never by parsing a
/// message id (FIG-972/984).
///
/// A *running* turn's trailing candidate is withheld: while the turn runs its
/// live workbench-owned row speaks for the answer, and admitting a mid-turn
/// copy underneath it is the two-namespaces-one-message defect FIG-984 closed.
/// It renders once the turn settles and its live row retires.
fn durable_rlm_reply_message_ids(
    messages: &[lash::messages::Message],
    a_turn_is_running: bool,
) -> BTreeSet<String> {
    let mut replies = BTreeSet::new();
    let mut candidate: Option<String> = None;
    for message in messages {
        match message.origin.as_ref() {
            Some(lash::messages::MessageOrigin::TurnInput { .. }) => {
                replies.extend(candidate.take());
            }
            _ if is_rlm_assistant_prose_message(message) => {
                candidate = Some(message.id.clone());
            }
            _ if lash::message_role(message) == "assistant" => {
                candidate = None;
            }
            _ => {}
        }
    }
    if !a_turn_is_running {
        replies.extend(candidate);
    }
    replies
}

fn project_committed_chat_message(
    message: &lash::messages::Message,
    rlm_reply_ids: &BTreeSet<String>,
) -> Option<ChatMessage> {
    (!is_durable_internal_rlm_message(message) || rlm_reply_ids.contains(&message.id))
        .then(|| chat_message_from_committed(message))
}

fn durable_rlm_reasoning_rows(message: &lash::messages::Message) -> Vec<TranscriptRow> {
    message
        .parts
        .iter()
        .filter(|part| {
            matches!(part.kind, lash_core::PartKind::Reasoning)
                && !part.content.trim().is_empty()
        })
        .map(|part| TranscriptRow::Reasoning {
            id: part.id.clone(),
            text: part.content.clone(),
        })
        .collect()
}

fn transcript_tool(call: lash_rlm_types::RlmExecutedCall) -> TranscriptTool {
    let status = match call.outcome {
        lash_rlm_types::RlmExecutedCallOutcome::Ok => "success",
        lash_rlm_types::RlmExecutedCallOutcome::Err => "failure",
    };
    TranscriptTool::DurableSummary {
        operation: call.operation,
        status,
    }
}

fn transcript_tools(
    calls: Vec<lash_rlm_types::RlmExecutedCall>,
    calls_omitted: usize,
) -> Vec<TranscriptTool> {
    let mut tools = calls.into_iter().map(transcript_tool).collect::<Vec<_>>();
    if calls_omitted > 0 {
        tools.push(TranscriptTool::Omitted {
            count: calls_omitted,
        });
    }
    tools
}

fn transcript_rows_from_committed(
    read_view: &lash::persistence::SessionReadView,
    user_replacements: &BTreeMap<String, ChatMessage>,
    protocol_state_message_ids: &BTreeSet<String>,
    rlm_reply_ids: &BTreeSet<String>,
) -> Vec<TranscriptRow> {
    // The label comes from the dialect the session recorded, not from this
    // process's ambient configuration: the recorded pin is what the executor
    // actually ran, and the two differ exactly when a store outlives a config
    // change — the case a language label exists to disambiguate.
    let language = {
        use lash::rlm::RlmSessionReadViewExt as _;
        read_view.rlm_dialect().language_id()
    };
    read_view
        .chronological_projection()
        .into_entries()
        .into_iter()
        .flat_map(|entry| match entry.payload {
            lash::persistence::ChronologicalPayload::Message(message) => {
                if protocol_state_message_ids.contains(&message.id) {
                    return Vec::new();
                }
                if is_durable_internal_rlm_message(&message) {
                    // The reply's reasoning still renders as its own collapsed
                    // row, ahead of the prose it reasoned toward.
                    let mut rows = durable_rlm_reasoning_rows(&message);
                    if rlm_reply_ids.contains(&message.id) {
                        rows.push(TranscriptRow::Message {
                            message: chat_message_from_committed(&message),
                        });
                    }
                    return rows;
                }
                let message = user_replacements
                    .get(&message.id)
                    .cloned()
                    .unwrap_or_else(|| chat_message_from_committed(&message));
                vec![TranscriptRow::Message { message }]
            }
            lash::persistence::ChronologicalPayload::ProtocolEvent(event) => {
                match lash_protocol_rlm::decode_rlm_protocol_event(&event) {
                    Some(lash_rlm_types::RlmProtocolEvent::RlmAssistantContent(content))
                        if !content.reasoning.trim().is_empty() =>
                    {
                        vec![TranscriptRow::Reasoning {
                            id: content.id,
                            text: content.reasoning,
                        }]
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
                        vec![TranscriptRow::CodeBlock {
                            id: step.id,
                            language: language.to_string(),
                            code: step.code,
                            output,
                            success: step.error.is_none(),
                            error: step.error,
                            tools: transcript_tools(step.calls, step.calls_omitted),
                        }]
                    }
                    _ => Vec::new(),
                }
            }
        })
        .collect()
}

struct ChatProjection {
    messages: Vec<ChatMessage>,
    transcript: Vec<TranscriptRow>,
}

/// Builds the two public chat projections from one set of replacement,
/// historical-row, protocol-state, and stable-id deduplication rules.
fn project_chat(
    state: &AppState,
    read_view: &lash::persistence::SessionReadView,
    active_turns: &[lash::TurnAddress],
    committed_input_turn_ids: &BTreeSet<String>,
    current_frame_input_turn_ids: &BTreeSet<String>,
    product_messages: Vec<ChatMessage>,
) -> ChatProjection {
    let replayed_active_rows = replayed_active_user_rows(state, active_turns, &product_messages);
    let ui_user_rows = product_messages
        .iter()
        .chain(replayed_active_rows.iter())
        .filter_map(|message| {
            workbench_turn_id_from_user_message_id(&message.id)
                .map(|turn_id| (turn_id.to_string(), message.clone()))
        })
        .collect::<BTreeMap<_, _>>();
    let user_replacements = ui_owned_turn_input_replacements(read_view, &ui_user_rows);
    let protocol_state_message_ids = continue_as_protocol_state_message_ids(read_view);
    let rlm_reply_ids = durable_rlm_reply_message_ids(read_view.messages(), !active_turns.is_empty());
    let replaced_committed_ids = user_replacements.keys().cloned().collect::<BTreeSet<_>>();
    let historical_ui_rows = product_messages
        .iter()
        .filter(|message| {
            workbench_turn_id_from_user_message_id(&message.id).is_some_and(|turn_id| {
                committed_input_turn_ids.contains(turn_id)
                    && !current_frame_input_turn_ids.contains(turn_id)
            })
        })
        .cloned()
        .collect::<Vec<_>>();

    let mut messages = historical_ui_rows.clone();
    messages.extend(read_view.messages().iter().filter_map(|message| {
        if protocol_state_message_ids.contains(&message.id) {
            return None;
        }
        user_replacements
            .get(&message.id)
            .cloned()
            .or_else(|| project_committed_chat_message(message, &rlm_reply_ids))
    }));
    let mut transcript = historical_ui_rows
        .into_iter()
        .map(|message| TranscriptRow::Message { message })
        .collect::<Vec<_>>();
    transcript.extend(transcript_rows_from_committed(
        read_view,
        &user_replacements,
        &protocol_state_message_ids,
        &rlm_reply_ids,
    ));

    let mut message_ids = messages
        .iter()
        .map(|message| message.id.clone())
        .collect::<BTreeSet<_>>();
    for message in product_messages.into_iter().chain(replayed_active_rows) {
        // A replaced committed message stays replaced however it reached this
        // list: the workbench mirrors committed ingress messages into the
        // product log, and that mirror is the same runtime copy for which the
        // UI-owned row already speaks.
        if replaced_committed_ids.contains(&message.id) {
            continue;
        }
        if message_ids.insert(message.id.clone()) {
            transcript.push(TranscriptRow::Message {
                message: message.clone(),
            });
            messages.push(message);
        }
    }

    ChatProjection {
        messages,
        transcript,
    }
}
