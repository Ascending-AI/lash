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
