//! Projection of a plugin-authored message into the durable message shape.

use crate::{Message, PluginMessage};
use lash_sansio::session_model::reassign_part_ids;
use std::sync::Arc;

pub fn plugin_message_to_message(plugin_message: &PluginMessage, fallback_id: &str) -> Message {
    let message_id = plugin_message
        .id
        .as_deref()
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| fallback_id.to_string());
    let mut parts = plugin_message.parts.clone();
    reassign_part_ids(&message_id, &mut parts);
    Message {
        id: message_id,
        role: plugin_message.role,
        parts: Arc::new(parts),
        origin: plugin_message.origin.clone().or_else(|| {
            Some(crate::MessageOrigin::Plugin {
                plugin_id: "plugin".to_string(),
                transient: false,
            })
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_input_and_durable_projection_preserve_interleaved_parts() {
        let source = crate::AttachmentSource::Inline {
            media_type: crate::MediaType::parse("image/png").unwrap(),
            bytes: vec![0, 255],
        };
        let mut input = crate::TurnInput::empty();
        input.items = vec![
            crate::InputItem::text("first"),
            crate::InputItem::attachment(source.clone()),
            crate::InputItem::text("last"),
        ];
        let plugin = crate::turn_input_vocabulary::plugin_message_from_turn_input(&input).unwrap();
        let message = plugin_message_to_message(&plugin, "message");
        assert_eq!(message.parts.len(), 3);
        assert_eq!(message.parts[0].content(), "first");
        assert_eq!(message.parts[1].attachment().unwrap().source, source);
        assert_eq!(message.parts[2].content(), "last");
        assert_eq!(
            message
                .parts
                .iter()
                .map(crate::Part::id)
                .collect::<Vec<_>>(),
            ["message.p0", "message.p1", "message.p2"]
        );
    }
}
