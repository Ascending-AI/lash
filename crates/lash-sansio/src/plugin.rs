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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PluginRuntimeEvent {
    Status {
        key: String,
        label: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    /// A built-in tool check selected one terminal reply over others.
    ToolCheckConflict(ToolCheckConflict),
    Custom {
        name: String,
        payload: serde_json::Value,
    },
}

/// The built-in tool check phase a reply was given in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolCheckPhase {
    ToolArgsCheck,
    ToolResultCheck,
}

impl ToolCheckPhase {
    pub fn code(self) -> &'static str {
        match self {
            Self::ToolArgsCheck => "tool_args_check",
            Self::ToolResultCheck => "tool_result_check",
        }
    }
}

/// What a tool check callback decided, without the decision's payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolCheckVerdictKind {
    Allow,
    Cached,
    Deny,
    Cancel,
    AbortRun,
}

/// One callback's reply to a tool check, attributed to its plugin.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ToolCheckReply {
    pub plugin_id: String,
    pub callback: String,
    pub verdict: ToolCheckVerdictKind,
}

/// The terminal reply a tool check's reduction selected and every terminal
/// reply it displaced, in reduction order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ToolCheckConflict {
    pub phase: ToolCheckPhase,
    pub winner: ToolCheckReply,
    pub displaced: Vec<ToolCheckReply>,
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

/// How a declared plugin failure settles, independent of its display text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PluginFailureClass {
    Retryable,
    Redrivable,
    Terminal,
    Parked,
}

impl PluginFailureClass {
    /// Authority loss and unavailable code take precedence over application rejection.
    pub fn precedence(self) -> u8 {
        match self {
            Self::Parked => 0,
            Self::Redrivable => 1,
            Self::Retryable => 2,
            Self::Terminal => 3,
        }
    }
}

/// The declared code that produced a failure. Lash stamps this at dispatch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PluginFailureOrigin {
    pub plugin_id: String,
    pub behavior_revision: std::num::NonZeroU32,
    pub operation: String,
}

/// A failure whose payload remains intact when its codec is unavailable.
#[derive(
    Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, thiserror::Error,
)]
#[error("{message}")]
pub struct PluginOperationFailure {
    pub error_type: String,
    pub error_version: std::num::NonZeroU32,
    pub payload: serde_json::Value,
    pub class: PluginFailureClass,
    pub code: crate::FailureCode,
    pub message: String,
    pub origin: Option<PluginFailureOrigin>,
}

/// One failed callback in registration order, including its original typed payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PluginHookFailure {
    pub origin: PluginFailureOrigin,
    pub failure: PluginOperationFailure,
}
