use super::*;

pub(in crate::runtime) async fn send_session_event(
    event_tx: &mpsc::Sender<RuntimeStreamEvent>,
    event: SessionStreamEvent,
) {
    if !event_tx.is_closed() {
        match &event {
            SessionStreamEvent::TokenUsage {
                protocol_iteration,
                usage,
                cumulative,
            } => {
                send_independent_turn_event(
                    event_tx,
                    TurnEvent::Usage {
                        protocol_iteration: *protocol_iteration,
                        usage: usage.clone(),
                        cumulative: cumulative.clone(),
                    },
                )
                .await;
            }
            SessionStreamEvent::LlmRequest {
                protocol_iteration, ..
            } => {
                send_independent_turn_event(
                    event_tx,
                    TurnEvent::ModelRequestStarted {
                        protocol_iteration: *protocol_iteration,
                    },
                )
                .await;
            }
            SessionStreamEvent::RetryStatus {
                wait_seconds,
                attempt,
                max_attempts,
                reason,
                ..
            } => {
                send_independent_turn_event(
                    event_tx,
                    TurnEvent::RetryStatus {
                        wait_seconds: *wait_seconds,
                        attempt: *attempt,
                        max_attempts: *max_attempts,
                        reason: reason.clone(),
                    },
                )
                .await;
            }
            SessionStreamEvent::PluginEvent { plugin_id, event } => {
                send_independent_turn_event(
                    event_tx,
                    TurnEvent::PluginRuntime {
                        plugin_id: plugin_id.clone(),
                        event: event.clone(),
                    },
                )
                .await;
            }
            SessionStreamEvent::InjectedTurnInputAccepted { .. } => {}
            SessionStreamEvent::InjectedMessagesCommitted {
                messages,
                checkpoint,
            } => {
                send_independent_turn_event(
                    event_tx,
                    TurnEvent::QueuedMessagesCommitted {
                        messages: messages.clone(),
                        checkpoint: *checkpoint,
                    },
                )
                .await;
            }
            SessionStreamEvent::Error { message, .. } => {
                send_independent_turn_event(
                    event_tx,
                    TurnEvent::Error {
                        message: message.clone(),
                    },
                )
                .await;
            }
            SessionStreamEvent::TurnOutcome {
                outcome: TurnOutcome::Finished(TurnFinish::FinalValue { value }),
            } => {
                send_independent_turn_event(
                    event_tx,
                    TurnEvent::FinalValue {
                        value: value.clone(),
                    },
                )
                .await;
            }
            SessionStreamEvent::TurnOutcome {
                outcome: TurnOutcome::Finished(TurnFinish::ToolValue { tool_name, value }),
            } => {
                send_independent_turn_event(
                    event_tx,
                    TurnEvent::ToolValue {
                        tool_name: tool_name.clone(),
                        value: value.clone(),
                    },
                )
                .await;
            }
            _ => {}
        }
        let _ = event_tx.send(RuntimeStreamEvent::Session(event)).await;
    }
}

pub(in crate::runtime) async fn send_turn_activity(
    event_tx: &mpsc::Sender<RuntimeStreamEvent>,
    correlation_id: TurnActivityId,
    event: TurnEvent,
) {
    if !event_tx.is_closed() {
        let activity = TurnActivity::new(correlation_id, event);
        let _ = event_tx.send(RuntimeStreamEvent::Turn(activity)).await;
    }
}

pub(in crate::runtime) async fn send_turn_input_applications(
    event_tx: &mpsc::Sender<RuntimeStreamEvent>,
    applications: Vec<crate::TurnInputApplication>,
) {
    if !applications.is_empty() {
        send_independent_turn_event(event_tx, TurnEvent::QueuedInputAccepted { applications })
            .await;
    }
}

async fn send_independent_turn_event(
    event_tx: &mpsc::Sender<RuntimeStreamEvent>,
    event: TurnEvent,
) {
    send_turn_activity(
        event_tx,
        // durable-entropy: live-stream correlation id; never journaled
        TurnActivityId::new(uuid::Uuid::new_v4().to_string()),
        event,
    )
    .await;
}

/// Publishes completed response parts the provider never streamed as live
/// blocks, each as a `StreamBlockStarted` + `StreamBlockCompleted` pair.
///
/// Identities minted here are deterministic — provider `item_id`s where they
/// exist, `part:{index}` otherwise — because the completed response is itself
/// deterministic: a replay of this path emits identical block identities.
pub(in crate::runtime) async fn emit_semantic_response_parts(
    event_tx: &mpsc::Sender<RuntimeStreamEvent>,
    response: &LlmResponse,
    prose_projector: Option<&dyn crate::plugin::AssistantProseProjectorPlugin>,
    reasoning_publication: &ReasoningPublicationState,
) {
    let visible_parts = crate::visible_response_parts(response.parts.clone());
    let mut next_ordinal = reasoning_publication.next_block_ordinal();
    let mut emitted_text = false;
    for (part_index, part) in visible_parts.iter().enumerate() {
        match part {
            LlmOutputPart::Text {
                text,
                response_meta,
            } if !text.is_empty() => {
                let text = project_assistant_prose(text, prose_projector);
                if text.is_empty() {
                    continue;
                }
                emitted_text = true;
                let item_id = response_meta.as_ref().and_then(|meta| meta.id.clone());
                let block = StreamBlockIdentity {
                    id: item_id
                        .clone()
                        .unwrap_or_else(|| format!("part:{part_index}")),
                    ordinal: next_ordinal,
                    item_id,
                };
                next_ordinal += 1;
                send_turn_activity(
                    event_tx,
                    TurnActivityId::new(block.id.clone()),
                    TurnEvent::StreamBlockStarted {
                        kind: StreamBlockKind::AssistantText,
                        block: block.clone(),
                    },
                )
                .await;
                send_turn_activity(
                    event_tx,
                    TurnActivityId::new(block.id.clone()),
                    TurnEvent::AssistantProseDelta {
                        text: text.clone().into(),
                        block: block.clone(),
                    },
                )
                .await;
                send_turn_activity(
                    event_tx,
                    TurnActivityId::new(block.id.clone()),
                    TurnEvent::StreamBlockCompleted {
                        kind: StreamBlockKind::AssistantText,
                        block,
                        text: text.into(),
                    },
                )
                .await;
            }
            LlmOutputPart::Reasoning { .. } => {
                if response.expose_thinking == Some(false) {
                    // Hidden thinking stays in `parts` for multi-turn replay
                    // but never reaches the host — same gate the provider
                    // applied to its live block events.
                    continue;
                }
                for (block, text) in
                    reasoning_publication.unpublished_blocks(part_index, part, &mut next_ordinal)
                {
                    send_turn_activity(
                        event_tx,
                        TurnActivityId::new(block.id.clone()),
                        TurnEvent::StreamBlockStarted {
                            kind: StreamBlockKind::Reasoning,
                            block: block.clone(),
                        },
                    )
                    .await;
                    send_turn_activity(
                        event_tx,
                        TurnActivityId::new(block.id.clone()),
                        TurnEvent::ReasoningDelta {
                            text: text.clone().into(),
                            block: block.clone(),
                        },
                    )
                    .await;
                    send_turn_activity(
                        event_tx,
                        TurnActivityId::new(block.id.clone()),
                        TurnEvent::StreamBlockCompleted {
                            kind: StreamBlockKind::Reasoning,
                            block,
                            text: text.into(),
                        },
                    )
                    .await;
                }
            }
            _ => {}
        }
    }
    let full_text = project_assistant_prose(&response.full_text(), prose_projector);
    if !emitted_text && !full_text.is_empty() {
        let block = StreamBlockIdentity::new("response:full-text", next_ordinal);
        send_turn_activity(
            event_tx,
            TurnActivityId::new(block.id.clone()),
            TurnEvent::StreamBlockStarted {
                kind: StreamBlockKind::AssistantText,
                block: block.clone(),
            },
        )
        .await;
        send_turn_activity(
            event_tx,
            TurnActivityId::new(block.id.clone()),
            TurnEvent::AssistantProseDelta {
                text: full_text.clone().into(),
                block: block.clone(),
            },
        )
        .await;
        send_turn_activity(
            event_tx,
            TurnActivityId::new(block.id.clone()),
            TurnEvent::StreamBlockCompleted {
                kind: StreamBlockKind::AssistantText,
                block,
                text: full_text.into(),
            },
        )
        .await;
    }
}

fn project_assistant_prose(
    text: &str,
    projector: Option<&dyn crate::plugin::AssistantProseProjectorPlugin>,
) -> String {
    projector
        .map(|projector| projector.project_assistant_prose(text))
        .unwrap_or_else(|| text.to_string())
}
