use super::AdminContext;
use crate::support::{EmbedError, InputItem, PluginMessage, Result, TurnInput};
use lash_sansio::TurnId;

#[derive(Clone)]
/// Facade handle for injection administration.
pub struct InjectionAdmin {
    pub(super) context: AdminContext,
    pub(super) trace: lash_core::TraceCall,
}

impl InjectionAdmin {
    /// Open each injection's `lash.plugin.invoke` producer span under
    /// `context` instead of the caller's ambient one. The injected input
    /// retains the producer as its cause, and the turn that consumes it
    /// links it.
    pub fn trace_context(mut self, context: lash_core::TraceCarrier) -> Self {
        self.trace.caller = lash_core::TraceCaller::Explicit(Some(context));
        self
    }

    /// Open each injection's producer span with no caller: a new trace
    /// root.
    pub fn trace_context_none(mut self) -> Self {
        self.trace.caller = lash_core::TraceCaller::Explicit(None);
        self
    }

    /// How each injection's producer span relates to its caller: its child
    /// (the default), or a new trace root that links it.
    pub fn trace_link_mode(mut self, mode: lash_core::TraceLinkMode) -> Self {
        self.trace.link_mode = mode;
        self
    }

    /// Injects input for the session's next turn.
    pub async fn inject_turn_input(
        &self,
        turn_id: &TurnId,
        id: Option<String>,
        message: PluginMessage,
    ) -> Result<()> {
        self.inject_turn_inputs_for_turn(
            turn_id,
            vec![lash_core::facade_support::InjectedTurnInput { id, message }],
        )
        .await
    }

    async fn inject_turn_inputs_for_turn(
        &self,
        turn_id: &TurnId,
        messages: Vec<lash_core::facade_support::InjectedTurnInput>,
    ) -> Result<()> {
        // The injection's producer opens here, before the first await.
        let target = &self.context.target;
        let mut producer = target.open_producer(
            lash_core::ProducerKind::PluginInvoke,
            &self.trace,
            lash_core::TraceIds {
                session: Some(target.session_id()),
                turn: Some(turn_id.clone()),
                ..Default::default()
            },
        );
        let parent = producer.parent();
        let cause = lash_core::TraceCause::from(parent.clone());
        let accepted = async {
            let mut selected = true;
            let mut head = None;
            for input in messages {
                let source_key = input.id.map(|id| format!("injection:{id}"));
                let turn_input = turn_input_from_plugin_message(input.message);
                let accepted = self
                    .context
                    .runtime
                    .enqueue_turn_input(
                        turn_input,
                        lash_core::TurnInputIngress::active_turn(
                            turn_id,
                            lash_core::TurnInputCheckpointBoundary::AfterWork,
                        ),
                        source_key,
                        parent.clone(),
                    )
                    .await
                    .map_err(EmbedError::Runtime)?;
                selected &= accepted.trace_cause == cause;
                head.get_or_insert(accepted.input_id);
            }
            Ok::<_, EmbedError>((selected, head))
        }
        .await;
        match accepted {
            Ok((selected, head)) => {
                if let Some(head) = head {
                    producer.accepted(head);
                }
                producer.settle(if selected {
                    lash_core::ProducerOutcome::Selected
                } else {
                    lash_core::ProducerOutcome::Reused
                });
                Ok(())
            }
            Err(error) => {
                producer.settle(crate::tracing::refused(&error));
                Err(error)
            }
        }
    }
}

fn turn_input_from_plugin_message(message: PluginMessage) -> TurnInput {
    let mut input = TurnInput::empty();
    for part in message.parts {
        if let Some(attachment) = part.attachment() {
            input
                .items
                .push(InputItem::attachment(attachment.reference.clone()));
        } else if !part.content().is_empty() {
            input.items.push(InputItem::text(part.content()));
        }
    }
    input
}

#[cfg(test)]
mod injected_message_tests {
    use super::*;

    #[test]
    fn mixed_parts_injection_preserves_order_and_attachments() {
        let source = lash_core::AttachmentRef::new(
            lash_core::AttachmentId::parse(
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            )
            .expect("digest"),
            "image/png".parse().unwrap(),
            3,
            None,
            None,
        );
        let mut message = PluginMessage::text(lash_core::MessageRole::User, "before");
        message.parts.push(lash_core::Part::attachment_part(
            String::new(),
            String::new(),
            Some(lash_core::session_model::message::PartAttachment {
                reference: source.clone(),
            }),
        ));
        message
            .parts
            .push(lash_core::Part::text(String::new(), "after".into(), None));
        let input = turn_input_from_plugin_message(message);
        assert!(matches!(input.items.as_slice(),
            [InputItem::Text { text: before }, InputItem::Attachment { reference: actual }, InputItem::Text { text: after }]
                if before == "before" && actual == &source && after == "after"));
    }
}
