use crate::{MessageOrigin, MessageRole, Part};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginMessage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub role: MessageRole,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<MessageOrigin>,
    pub parts: Vec<Part>,
}

impl PluginMessage {
    pub fn text(role: MessageRole, content: impl Into<String>) -> Self {
        Self {
            id: None,
            role,
            origin: None,
            parts: vec![Part::text(String::new(), content.into(), None)],
        }
    }
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }
    pub fn with_origin(mut self, origin: MessageOrigin) -> Self {
        self.origin = Some(origin);
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PluginRuntimeEvent {
    Status {
        key: String,
        label: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    Custom {
        name: String,
        payload: serde_json::Value,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointKind {
    AfterWork,
    BeforeCompletion,
}

#[cfg(test)]
mod message_body_tests {
    use super::*;

    #[test]
    fn removed_body_fields_and_missing_parts_are_rejected() {
        let current =
            serde_json::to_value(PluginMessage::text(MessageRole::User, "hello")).unwrap();
        for (field, value) in [
            ("content", serde_json::json!("ignored")),
            ("attachments", serde_json::json!([])),
        ] {
            let mut legacy = current.clone();
            legacy[field] = value;
            assert!(
                serde_json::from_value::<PluginMessage>(legacy).is_err(),
                "{field}"
            );
        }
        let mut missing = current;
        missing.as_object_mut().unwrap().remove("parts");
        assert!(serde_json::from_value::<PluginMessage>(missing).is_err());
    }

    #[test]
    fn removed_part_lifecycle_field_is_rejected_even_when_intact() {
        let mut part = serde_json::to_value(Part::text("p0".into(), "hello".into(), None)).unwrap();
        part["prune_state"] = serde_json::json!("Intact");
        assert!(serde_json::from_value::<Part>(part.clone()).is_err());
        let message = serde_json::json!({"role":"User", "parts":[part]});
        assert!(serde_json::from_value::<PluginMessage>(message).is_err());
    }
}
