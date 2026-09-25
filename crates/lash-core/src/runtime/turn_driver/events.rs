use super::*;

impl RuntimeTurnDriver<'_> {
    /// Record an event's committed content, then publish it. Every event the
    /// machine emits, and every terminal event the driver writes itself, goes
    /// through here, in program order.
    pub(super) fn emit_recorded(&mut self, event_tx: &TurnObserver, event: SessionStreamEvent) {
        self.recorded_assembly.record(&event);
        self.turn_observations
            .observe(event_tx, crate::engine::ObservedEvent::Session(event));
    }
}

pub(in crate::runtime) fn send_turn_input_applications(
    event_tx: &TurnObserver,
    cursor: &mut crate::engine::ObservationCursor,
    applications: Vec<crate::TurnInputApplication>,
) {
    if !applications.is_empty() {
        cursor.observe(
            event_tx,
            crate::engine::ObservedEvent::Activity {
                correlation_id: None,
                event: TurnEvent::QueuedInputAccepted { applications },
            },
        );
    }
}

/// Publishes completed response parts the provider never streamed as live
/// blocks, each as a `StreamBlockStarted` + `StreamBlockCompleted` pair.
///
/// Identities minted here are deterministic — provider `item_id`s where they
/// exist, `part:{index}` otherwise — because the completed response is itself
/// deterministic: a replay of this path emits identical block identities.
pub(in crate::runtime) fn emit_semantic_response_parts(
    event_tx: &TurnObserver,
    cursor: &mut crate::engine::ObservationCursor,
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
                let correlation_id = TurnActivityId::new(block.id.clone());
                cursor.observe(
                    event_tx,
                    crate::engine::ObservedEvent::Activity {
                        correlation_id: Some(correlation_id.clone()),
                        event: TurnEvent::StreamBlockStarted {
                            kind: StreamBlockKind::AssistantText,
                            block: block.clone(),
                        },
                    },
                );
                cursor.observe(
                    event_tx,
                    crate::engine::ObservedEvent::Activity {
                        correlation_id: Some(correlation_id.clone()),
                        event: TurnEvent::AssistantProseDelta {
                            text: text.clone().into(),
                            block: block.clone(),
                        },
                    },
                );
                cursor.observe(
                    event_tx,
                    crate::engine::ObservedEvent::Activity {
                        correlation_id: Some(correlation_id),
                        event: TurnEvent::StreamBlockCompleted {
                            kind: StreamBlockKind::AssistantText,
                            block,
                            text: text.into(),
                        },
                    },
                );
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
                    let correlation_id = TurnActivityId::new(block.id.clone());
                    cursor.observe(
                        event_tx,
                        crate::engine::ObservedEvent::Activity {
                            correlation_id: Some(correlation_id.clone()),
                            event: TurnEvent::StreamBlockStarted {
                                kind: StreamBlockKind::Reasoning,
                                block: block.clone(),
                            },
                        },
                    );
                    cursor.observe(
                        event_tx,
                        crate::engine::ObservedEvent::Activity {
                            correlation_id: Some(correlation_id.clone()),
                            event: TurnEvent::ReasoningDelta {
                                text: text.clone().into(),
                                block: block.clone(),
                            },
                        },
                    );
                    cursor.observe(
                        event_tx,
                        crate::engine::ObservedEvent::Activity {
                            correlation_id: Some(correlation_id),
                            event: TurnEvent::StreamBlockCompleted {
                                kind: StreamBlockKind::Reasoning,
                                block,
                                text: text.into(),
                            },
                        },
                    );
                }
            }
            _ => {}
        }
    }
    let full_text = project_assistant_prose(&response.full_text(), prose_projector);
    if !emitted_text && !full_text.is_empty() {
        let block = StreamBlockIdentity::new("response:full-text", next_ordinal);
        let correlation_id = TurnActivityId::new(block.id.clone());
        cursor.observe(
            event_tx,
            crate::engine::ObservedEvent::Activity {
                correlation_id: Some(correlation_id.clone()),
                event: TurnEvent::StreamBlockStarted {
                    kind: StreamBlockKind::AssistantText,
                    block: block.clone(),
                },
            },
        );
        cursor.observe(
            event_tx,
            crate::engine::ObservedEvent::Activity {
                correlation_id: Some(correlation_id.clone()),
                event: TurnEvent::AssistantProseDelta {
                    text: full_text.clone().into(),
                    block: block.clone(),
                },
            },
        );
        cursor.observe(
            event_tx,
            crate::engine::ObservedEvent::Activity {
                correlation_id: Some(correlation_id),
                event: TurnEvent::StreamBlockCompleted {
                    kind: StreamBlockKind::AssistantText,
                    block,
                    text: full_text.into(),
                },
            },
        );
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
