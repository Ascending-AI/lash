use super::*;
use lash::SessionId;
use lash::TurnId;

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
pub(crate) fn replayed_active_user_rows(
    active_turn: Option<&ActiveTurn>,
    product_messages: &[ChatMessage],
) -> Vec<ChatMessage> {
    let turn_ids = product_messages
        .iter()
        .filter_map(|message| workbench_turn_id_from_user_message_id(&message.id))
        .map(TurnId::from)
        .collect::<BTreeSet<_>>();
    // The turn and its prompt arrive from one read of one lock, so this row
    // can no longer be built from a turn that a concurrent removal has since
    // retired, nor dropped because the prompt read raced the turn read.
    let Some(active_turn) = active_turn else {
        return Vec::new();
    };
    if turn_ids.contains(&active_turn.address.turn_id) {
        return Vec::new();
    }
    let Some(prompt) = active_turn.prompt.as_ref() else {
        return Vec::new();
    };
    vec![ChatMessage {
        id: workbench_turn_user_message_id(&active_turn.address.turn_id),
        role: "user".to_string(),
        text: prompt.text.clone(),
        at: String::new(),
        attachments: prompt
            .attachment_id
            .iter()
            .cloned()
            .map(ChatAttachment::from_id)
            .collect(),
        provenance: None,
    }]
}

/// The user rows this session's product-event log carries on the workbench's
/// own authority, keyed by the turn each one was submitted for.
///
/// Both readers of the product log start here — the `/api/state` snapshot and
/// the settlement republish on the live stream — so the two paths ask
/// `ui_owned_turn_input_replacements` the same question about the same rows.
pub(crate) fn ui_owned_user_rows_by_turn<'a>(
    messages: impl IntoIterator<Item = &'a ChatMessage>,
) -> BTreeMap<TurnId, ChatMessage> {
    messages
        .into_iter()
        .filter_map(|message| {
            workbench_turn_id_from_user_message_id(&message.id)
                .map(|turn_id| (TurnId::from(turn_id), message.clone()))
        })
        .collect()
}

/// The chat rows in a session's product-event log, in publication order.
pub(crate) fn product_chat_messages(state: &AppState, session_id: &SessionId) -> Vec<ChatMessage> {
    state
        .event_tx
        .snapshot(session_id)
        .events
        .iter()
        .filter_map(|event| match &event.item {
            StreamItem::Message { message } => Some(message.clone()),
            StreamItem::TurnInput { .. }
            | StreamItem::ModelCallRecorded { .. }
            | StreamItem::Done { .. } => None,
        })
        .collect()
}

/// The committed message ids a UI-owned user row already stands for, for a
/// reader that needs only the decision and not the replacement row —
/// `republish_committed_ingress_messages` above.
pub(crate) fn ui_owned_committed_message_ids(
    state: &AppState,
    session_id: &SessionId,
    read_view: &lash::persistence::SessionReadView,
) -> BTreeSet<String> {
    let product_messages = product_chat_messages(state, session_id);
    let ui_user_rows = ui_owned_user_rows_by_turn(product_messages.iter());
    ui_owned_turn_input_replacements(read_view, &ui_user_rows)
        .into_keys()
        .collect()
}

/// Republishes a settling turn's committed ingress messages onto the live
/// stream, so a page that joined mid-turn ends the turn holding the exact
/// committed graph projection that `/api/state` and resume read.
///
/// Every committed ingress message is republished except the ones a UI-owned
/// row already stands for. Re-publishing is otherwise harmless because the
/// browser deduplicates committed message ids, but it cannot recognize its own
/// `workbench-user:{turn_id}` row as the same text as the runtime's
/// `m_ingress_{input_id}` commit — the two ids are in deliberately separate
/// namespaces (FIG-972) — so republishing the turn's opening input appended a
/// second copy of the operator's own words above the reply, which stood until
/// the next snapshot rebuilt the transcript (FIG-3206). Asking
/// `ui_owned_committed_message_ids` keeps one decision for the live stream and
/// the snapshot; a mid-turn injected input has no UI row and still republishes.
pub(crate) fn republish_committed_ingress_messages(state: &AppState, session: &lash::LashSession) {
    let session_id = session.session_id();
    let read_view = session.read_view();
    let ui_owned = ui_owned_committed_message_ids(state, &session_id, &read_view);
    for message in read_view
        .messages()
        .iter()
        .filter(|message| message.id.starts_with("m_ingress_") && !ui_owned.contains(&message.id))
    {
        state.publish_for_session_identified(
            &session_id,
            format!("message:{}", message.id),
            StreamItem::Message {
                message: chat_message_from_committed(message),
            },
        );
    }
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
pub(crate) fn ui_owned_turn_input_replacements(
    read_view: &lash::persistence::SessionReadView,
    ui_user_rows: &BTreeMap<TurnId, ChatMessage>,
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

pub(crate) fn chat_message_from_committed(message: &lash::messages::Message) -> ChatMessage {
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
        provenance: match message.origin.as_ref() {
            Some(lash::messages::MessageOrigin::TurnOutput { turn_id, .. }) => {
                Some(ChatMessageProvenance::TurnOutput {
                    turn_id: turn_id.clone(),
                })
            }
            _ => None,
        },
    }
}

pub(crate) fn committed_turn_output_turn_ids(messages: &[ChatMessage]) -> BTreeSet<TurnId> {
    messages
        .iter()
        .filter_map(|message| {
            if message.role != "assistant" {
                return None;
            }

            message
                .provenance
                .as_ref()
                .map(|ChatMessageProvenance::TurnOutput { turn_id }| turn_id.clone())
        })
        .collect()
}

pub(crate) fn is_committed_turn_output_copy(
    message: &ChatMessage,
    committed_turn_output_turn_ids: &BTreeSet<TurnId>,
) -> bool {
    message.role == "assistant"
        && match message.provenance.as_ref() {
            Some(ChatMessageProvenance::TurnOutput { turn_id }) => {
                committed_turn_output_turn_ids.contains(turn_id)
            }
            None => false,
        }
}

pub(crate) fn committed_chat_text(message: &lash::messages::Message) -> String {
    message
        .parts
        .iter()
        .filter(|part| !matches!(part.kind, lash::messages::PartKind::Reasoning))
        .map(|part| part.content.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn is_durable_internal_rlm_message(message: &lash::messages::Message) -> bool {
    matches!(
        message.origin.as_ref(),
        Some(lash::messages::MessageOrigin::Plugin {
            plugin_id,
            transient: false,
        }) if plugin_id == lash_protocol_rlm::RLM_PROTOCOL_PLUGIN_ID
    ) || matches!(
        message.origin.as_ref(),
        Some(lash::messages::MessageOrigin::TurnOutput {
            source: lash::messages::TurnOutputSource::Plugin { plugin_id },
            ..
        }) if plugin_id == lash_protocol_rlm::RLM_PROTOCOL_PLUGIN_ID
    )
}

/// Whether this plugin-authored RLM message could be a turn's committed reply:
/// an assistant message carrying visible prose. The protocol's system copies —
/// finish reminders, retry copy, cell diagnostics — never can be, and its
/// reasoning-only messages carry nothing for a chat row to say.
pub(crate) fn is_rlm_assistant_prose_message(message: &lash::messages::Message) -> bool {
    is_durable_internal_rlm_message(message)
        && lash::message_role(message) == "assistant"
        && message.parts.iter().any(|part| {
            matches!(part.kind, lash::messages::PartKind::Prose) && !part.content.trim().is_empty()
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
/// the reply and the plugin message behind it is superseded context.
///
/// What settles a candidate is a *turn change*, and a turn input is neither
/// necessary nor sufficient to mark one. A cause-only turn — a process wake or
/// a queued drain — commits no input at all, only its typed cause as an `Event`
/// message; reading turn inputs alone as boundaries would fold that wake's
/// prose into the previous turn and retract an answer the user already read.
/// An input injected into a *running* turn, conversely, commits a turn input
/// carrying that turn's own id and opens nothing, so it is skipped: settling
/// there would render the turn's mid-turn prose alongside its answer, one turn
/// as two agent rows. The open turn is therefore tracked by the typed `turn_id`
/// the runtime publishes, never by parsing a message id (FIG-972/984). A cause
/// delivered into a running turn is indistinguishable from one that opens a
/// turn, and settling the candidate is the safe reading of the two: an extra
/// durable row is a smaller injury than an answer that disappears.
///
/// A running turn's *own* trailing candidate is withheld: while the turn runs
/// its live workbench-owned row speaks for the answer, and admitting a mid-turn
/// copy underneath it is the two-namespaces-one-message defect FIG-984 closed.
/// It renders once that turn settles and its live row retires. The test is
/// per-turn on purpose — the active-turn registry is persistent and is written
/// before the next turn's input commits, so a session-wide test would blink the
/// previous answer out on every send and hide it for good behind an entry whose
/// process died mid-turn.
pub(crate) fn durable_rlm_reply_message_ids(
    messages: &[lash::messages::Message],
    running_turn_ids: &BTreeSet<TurnId>,
) -> BTreeSet<String> {
    let mut replies = BTreeSet::new();
    let mut candidate: Option<String> = None;
    // The turn the walk is inside, when the transcript names it. A cause-only
    // turn never does, and its candidate is judged as an unnamed turn's.
    let mut open_turn_id: Option<TurnId> = None;
    for message in messages {
        match message.origin.as_ref() {
            Some(lash::messages::MessageOrigin::TurnInput { turn_id, .. }) => {
                if open_turn_id.as_deref() == Some(turn_id.as_str()) {
                    continue;
                }
                replies.extend(candidate.take());
                open_turn_id = Some(turn_id.clone());
            }
            _ if lash::message_role(message) == "event" => {
                replies.extend(candidate.take());
                open_turn_id = None;
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
    let candidate_turn_is_running = open_turn_id
        .as_ref()
        .is_some_and(|turn_id| running_turn_ids.contains(turn_id));
    if !candidate_turn_is_running {
        replies.extend(candidate);
    }
    replies
}

pub(crate) fn project_committed_chat_message(
    message: &lash::messages::Message,
    rlm_reply_ids: &BTreeSet<String>,
) -> Option<ChatMessage> {
    (!is_durable_internal_rlm_message(message) || rlm_reply_ids.contains(&message.id))
        .then(|| chat_message_from_committed(message))
}

pub(crate) fn durable_rlm_reasoning_rows(message: &lash::messages::Message) -> Vec<TranscriptRow> {
    message
        .parts
        .iter()
        .filter(|part| {
            matches!(part.kind, lash::messages::PartKind::Reasoning)
                && !part.content.trim().is_empty()
        })
        .map(|part| TranscriptRow::Reasoning {
            id: part.id.clone(),
            text: part.content.clone(),
        })
        .collect()
}

pub(crate) fn transcript_tool(call: lash_rlm_types::RlmExecutedCall) -> TranscriptTool {
    let status = match call.outcome {
        lash_rlm_types::RlmExecutedCallOutcome::Ok => "success",
        lash_rlm_types::RlmExecutedCallOutcome::Err => "failure",
    };
    TranscriptTool::DurableSummary {
        operation: call.operation,
        status,
    }
}

pub(crate) fn transcript_tools(
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

pub(crate) fn transcript_rows_from_committed(
    read_view: &lash::persistence::SessionReadView,
    user_replacements: &BTreeMap<String, ChatMessage>,
    rlm_reply_ids: &BTreeSet<String>,
) -> Vec<TranscriptRow> {
    // TypeScript is the sole RLM language (ADR 0096), so every cell carries
    // the same label.
    let language = RLM_LANGUAGE_ID;
    read_view
        .chronological_projection()
        .into_entries()
        .into_iter()
        .flat_map(|entry| match entry.payload {
            lash::persistence::ChronologicalPayload::Message(message) => {
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

/// Place each unknown-terminal disclosure beside the turn it speaks for.
///
/// The note belongs where the turn's own rows are, not at the end of the
/// transcript: a session that ran another turn after the break-glass abort
/// would otherwise show the disclosure under the wrong conversation. A turn
/// whose rows are all gone keeps its note at the end, because a disclosure with
/// nowhere to sit still has to be readable.
pub(crate) fn splice_unknown_turn_terminal_notes(
    transcript: &mut Vec<TranscriptRow>,
    unknown_turn_terminals: &[UnknownTurnTerminal],
) {
    for record in unknown_turn_terminals {
        let note = TranscriptRow::Note {
            id: format!("workbench-unknown-terminal:{}", record.turn_id),
            turn_id: record.turn_id.clone(),
            text: record.note.to_string(),
        };
        match transcript
            .iter()
            .rposition(|row| transcript_row_speaks_for_turn(row, &record.turn_id))
        {
            Some(index) => transcript.insert(index + 1, note),
            None => transcript.push(note),
        }
    }
}

fn transcript_row_speaks_for_turn(row: &TranscriptRow, turn_id: &TurnId) -> bool {
    let TranscriptRow::Message { message } = row else {
        return false;
    };
    message.id == workbench_turn_user_message_id(turn_id)
        || message.id == workbench_turn_assistant_message_id(turn_id)
        || matches!(
            message.provenance.as_ref(),
            Some(ChatMessageProvenance::TurnOutput { turn_id: owner }) if owner == turn_id
        )
}

pub(crate) struct ChatProjection {
    pub(crate) messages: Vec<ChatMessage>,
    pub(crate) transcript: Vec<TranscriptRow>,
}

/// Builds the two public chat projections from one set of replacement,
/// historical-row and stable-id deduplication rules.
pub(crate) fn project_chat(
    read_view: &lash::persistence::SessionReadView,
    active_turn: Option<&ActiveTurn>,
    current_frame_input_turn_ids: &BTreeSet<TurnId>,
    product_messages: Vec<ChatMessage>,
) -> ChatProjection {
    let replayed_active_rows = replayed_active_user_rows(active_turn, &product_messages);
    let ui_user_rows =
        ui_owned_user_rows_by_turn(product_messages.iter().chain(replayed_active_rows.iter()));
    let user_replacements = ui_owned_turn_input_replacements(read_view, &ui_user_rows);
    let running_turn_ids = active_turn
        .map(|active_turn| active_turn.address.turn_id.clone())
        .into_iter()
        .collect::<BTreeSet<_>>();
    let rlm_reply_ids = durable_rlm_reply_message_ids(read_view.messages(), &running_turn_ids);
    let replaced_committed_ids = user_replacements.keys().cloned().collect::<BTreeSet<_>>();
    // A submitted user row whose turn the current frame does not carry belongs
    // to the session's history: the frame it was sent into has been retired by
    // `continue_as`, so the runtime's committed copy is no longer readable and
    // this row is the only surviving record of what the operator sent. History
    // renders ahead of the current frame, in the order the rows were submitted.
    // Requiring the turn to be in `committed_input_turn_ids` made this depend
    // on the workbench having won a race against the durable commit, which is
    // the race FIG-3143 removes; a running turn is excluded because its row is
    // placed by the product log's own anchoring instead.
    let historical_ui_rows = product_messages
        .iter()
        .filter(|message| {
            workbench_turn_id_from_user_message_id(&message.id).is_some_and(|turn_id| {
                !current_frame_input_turn_ids.contains(turn_id)
                    && !running_turn_ids.contains(turn_id)
            })
        })
        .cloned()
        .collect::<Vec<_>>();

    let committed_messages = read_view
        .messages()
        .iter()
        .filter_map(|message| {
            user_replacements
                .get(&message.id)
                .cloned()
                .or_else(|| project_committed_chat_message(message, &rlm_reply_ids))
        })
        .collect::<Vec<_>>();
    let committed_turn_output_turn_ids = committed_turn_output_turn_ids(&committed_messages);
    let mut messages = historical_ui_rows.clone();
    messages.extend(committed_messages);
    let mut transcript = historical_ui_rows
        .into_iter()
        .map(|message| TranscriptRow::Message { message })
        .collect::<Vec<_>>();
    transcript.extend(transcript_rows_from_committed(
        read_view,
        &user_replacements,
        &rlm_reply_ids,
    ));

    let committed_turn_output_ids = committed_turn_output_message_ids(&messages);
    let mut message_ids = messages
        .iter()
        .map(|message| message.id.clone())
        .collect::<BTreeSet<_>>();

    // The product log is an arrival-ordered record, so the rows it holds that
    // the graph never committed — button and mail trigger occurrences, mock
    // account connections — happened between two committed rows, and the log
    // says which. Anchor each one to the newest committed row pushed before it
    // and re-insert it there. Appending them instead put every event row under
    // the newest chat, so each `/api/state` rebuild re-sank the rows away from
    // the queued-turn replies they caused, even though the committed graph had
    // those replies in the right place.
    let mut unanchored_rows: Vec<ChatMessage> = Vec::new();
    let mut anchored_rows: BTreeMap<String, Vec<ChatMessage>> = BTreeMap::new();
    let mut anchor: Option<String> = None;
    for message in product_messages.into_iter().chain(replayed_active_rows) {
        if is_committed_turn_output_copy(&message, &committed_turn_output_turn_ids) {
            if let Some(ChatMessageProvenance::TurnOutput { turn_id }) = message.provenance.as_ref()
                && let Some(committed_id) = committed_turn_output_ids.get(turn_id)
            {
                anchor = Some(committed_id.clone());
            }
            continue;
        }
        // A replaced committed message stays replaced however it reached this
        // list: the workbench mirrors committed ingress messages into the
        // product log, and that mirror is the same runtime copy for which the
        // UI-owned row already speaks.
        if replaced_committed_ids.contains(&message.id) {
            anchor = Some(message.id.clone());
            continue;
        }
        if !message_ids.insert(message.id.clone()) {
            // Already placed: this product row is a mirror of a committed row,
            // which makes it the newest committed row the log has seen.
            anchor = Some(message.id.clone());
            continue;
        }
        match anchor.as_ref() {
            Some(anchor_id) => anchored_rows
                .entry(anchor_id.clone())
                .or_default()
                .push(message),
            // Nothing committed had been pushed when this row arrived, so the
            // log places it nowhere: it keeps the old position at the end.
            None => unanchored_rows.push(message),
        }
    }

    splice_anchored_product_rows(
        &mut messages,
        &mut transcript,
        unanchored_rows,
        anchored_rows,
    );

    ChatProjection {
        messages,
        transcript,
    }
}

/// The committed message id that carries each turn's output, so a product-log
/// mirror of that output can name the committed row it duplicates.
fn committed_turn_output_message_ids(committed: &[ChatMessage]) -> BTreeMap<TurnId, String> {
    committed
        .iter()
        .filter(|message| message.role == "assistant")
        .filter_map(|message| {
            message
                .provenance
                .as_ref()
                .map(|ChatMessageProvenance::TurnOutput { turn_id }| {
                    (turn_id.clone(), message.id.clone())
                })
        })
        .collect()
}

/// The turn a projected chat row belongs to, as far as its id or provenance says.
fn chat_message_turn_id(message: &ChatMessage) -> Option<TurnId> {
    workbench_turn_id_from_user_message_id(&message.id)
        .or_else(|| workbench_turn_id_from_assistant_message_id(&message.id))
        .map(TurnId::from)
        .or_else(|| {
            message
                .provenance
                .as_ref()
                .map(|ChatMessageProvenance::TurnOutput { turn_id }| turn_id.clone())
        })
}

/// Where an anchored product row belongs: after the last row of the anchor's
/// turn, so an occurrence that happened after a turn's reply renders after that
/// reply rather than between the prompt and the answer.
fn anchor_insertion_index(messages: &[ChatMessage], anchor_id: &str) -> Option<usize> {
    let anchor_at = messages
        .iter()
        .rposition(|message| message.id == anchor_id)?;
    let anchor_turn = chat_message_turn_id(&messages[anchor_at]);
    let mut index = anchor_at + 1;
    if let Some(anchor_turn) = anchor_turn {
        while let Some(next) = messages.get(index) {
            if chat_message_turn_id(next).as_ref() == Some(&anchor_turn) {
                index += 1;
            } else {
                break;
            }
        }
    }
    Some(index)
}

/// Insert each product-log row at the point in the committed transcript it was
/// pushed at, leaving every other row where the committed projection put it.
/// Rows with no anchor were pushed before any committed row and lead the
/// transcript; rows whose anchor this projection no longer renders keep the old
/// position at the end, which is still the newest position the snapshot can
/// honestly claim for them.
fn splice_anchored_product_rows(
    messages: &mut Vec<ChatMessage>,
    transcript: &mut Vec<TranscriptRow>,
    unanchored_rows: Vec<ChatMessage>,
    anchored_rows: BTreeMap<String, Vec<ChatMessage>>,
) {
    let mut insertions: Vec<(usize, Vec<ChatMessage>)> = Vec::new();
    let mut trailing: Vec<ChatMessage> = unanchored_rows;
    for (anchor_id, rows) in anchored_rows {
        match anchor_insertion_index(messages, &anchor_id) {
            Some(index) => insertions.push((index, rows)),
            None => trailing.extend(rows),
        }
    }
    messages.extend(trailing);
    insertions.sort_by_key(|insertion| std::cmp::Reverse(insertion.0));
    for (index, rows) in &insertions {
        messages.splice(index..index, rows.iter().cloned());
    }

    // The transcript carries reasoning and code rows between its message rows,
    // so it is re-ordered onto the message order just settled rather than
    // spliced by index.
    let mut placed: BTreeMap<String, Vec<TranscriptRow>> = BTreeMap::new();
    let mut message_rows: BTreeMap<String, TranscriptRow> = BTreeMap::new();
    let mut leading_extras: Vec<TranscriptRow> = Vec::new();
    let mut previous: Option<String> = None;
    for row in transcript.drain(..) {
        match row {
            TranscriptRow::Message { message } => {
                let id = message.id.clone();
                message_rows.insert(id.clone(), TranscriptRow::Message { message });
                previous = Some(id);
            }
            extra => match previous.as_ref() {
                Some(id) => placed.entry(id.clone()).or_default().push(extra),
                None => leading_extras.push(extra),
            },
        }
    }
    let mut ordered = leading_extras;
    for message in messages.iter() {
        ordered.push(
            message_rows
                .remove(&message.id)
                .unwrap_or_else(|| TranscriptRow::Message {
                    message: message.clone(),
                }),
        );
        if let Some(extras) = placed.remove(&message.id) {
            ordered.extend(extras);
        }
    }
    for (_, extras) in placed {
        ordered.extend(extras);
    }
    *transcript = ordered;
}
