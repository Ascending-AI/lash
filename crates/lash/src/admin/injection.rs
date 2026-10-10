use super::AdminContext;
use crate::support::{EmbedError, InputItem, PluginMessage, Result, TurnInput};
use lash_sansio::TurnId;

#[derive(Clone)]
/// Facade handle for injection administration.
pub struct InjectionAdmin {
    pub(super) context: AdminContext,
}

impl InjectionAdmin {
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
        for input in messages {
            let source_key = input.id.map(|id| format!("injection:{id}"));
            let turn_input = turn_input_from_plugin_message(input.message);
            self.context
                .runtime
                .enqueue_turn_input(
                    turn_input,
                    lash_core::TurnInputIngress::active_turn(
                        turn_id,
                        lash_core::TurnInputCheckpointBoundary::AfterWork,
                    ),
                    source_key,
                )
                .await
                .map(|_| ())
                .map_err(EmbedError::Runtime)?;
        }
        Ok(())
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
