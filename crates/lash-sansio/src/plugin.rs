use std::sync::Arc;

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

/// Gate on Tool Catalog membership: a contribution is kept when at least one
/// of `tools` is a member of the catalog. There is no minimum-tier dimension.
#[derive(
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
pub struct PromptContributionGate {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
}

impl PromptContributionGate {
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

#[derive(
    Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
pub struct PromptContribution {
    pub slot: crate::PromptSlot,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<Arc<str>>,
    #[serde(default)]
    pub priority: i32,
    #[serde(default, skip_serializing_if = "PromptContributionGate::is_empty")]
    pub gate: PromptContributionGate,
    pub content: Arc<str>,
}

/// Contribution payload whose slot identity belongs exclusively to its map key.
///
/// A map value cannot encode a conflicting slot:
/// ```compile_fail,E0609
/// use lash_sansio::{PromptContributionBody, PromptSlot};
/// fn contradict_key(body: &mut PromptContributionBody) {
///     body.slot = PromptSlot::Guidance;
/// }
/// ```
#[derive(
    Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct PromptContributionBody {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<Arc<str>>,
    #[serde(default)]
    pub priority: i32,
    #[serde(default, skip_serializing_if = "PromptContributionGate::is_empty")]
    pub gate: PromptContributionGate,
    pub content: Arc<str>,
}

impl From<PromptContribution> for PromptContributionBody {
    fn from(value: PromptContribution) -> Self {
        let PromptContribution {
            slot: _,
            title,
            priority,
            gate,
            content,
        } = value;
        Self {
            title,
            priority,
            gate,
            content,
        }
    }
}
impl PromptContributionBody {
    /// Reattach the sole slot identity from the containing map key.
    pub fn in_slot(self, slot: crate::PromptSlot) -> PromptContribution {
        let Self {
            title,
            priority,
            gate,
            content,
        } = self;
        PromptContribution {
            slot,
            title,
            priority,
            gate,
            content,
        }
    }
}

impl PromptContribution {
    pub fn new(
        slot: crate::PromptSlot,
        title: impl Into<Arc<str>>,
        content: impl Into<Arc<str>>,
    ) -> Self {
        let title: Arc<str> = title.into();
        let title = (!title.trim().is_empty()).then_some(title);
        Self {
            slot,
            title,
            priority: 0,
            gate: PromptContributionGate { tools: Vec::new() },
            content: content.into(),
        }
    }

    pub fn with_priority(mut self, priority: i32) -> Self {
        self.priority = priority;
        self
    }

    pub fn requires_tool(mut self, tool_name: impl Into<String>) -> Self {
        self.gate = PromptContributionGate {
            tools: vec![tool_name.into()],
        };
        self
    }

    pub fn requires_any_tool(
        mut self,
        tool_names: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.gate = PromptContributionGate {
            tools: tool_names.into_iter().map(Into::into).collect(),
        };
        self
    }

    pub fn intro(title: impl Into<Arc<str>>, content: impl Into<Arc<str>>) -> Self {
        Self::new(crate::PromptSlot::Intro, title, content)
    }

    pub fn execution(title: impl Into<Arc<str>>, content: impl Into<Arc<str>>) -> Self {
        Self::new(crate::PromptSlot::Execution, title, content)
    }

    pub fn guidance(title: impl Into<Arc<str>>, content: impl Into<Arc<str>>) -> Self {
        Self::new(crate::PromptSlot::Guidance, title, content)
    }

    pub fn project_instructions(content: impl Into<Arc<str>>) -> Self {
        Self::new(
            crate::PromptSlot::ProjectInstructions,
            "Project Instructions",
            content,
        )
    }

    pub fn runtime_context(content: impl Into<Arc<str>>) -> Self {
        Self::new(
            crate::PromptSlot::RuntimeContext,
            "Runtime Context",
            content,
        )
    }

    pub fn environment(title: impl Into<Arc<str>>, content: impl Into<Arc<str>>) -> Self {
        Self::new(crate::PromptSlot::Environment, title, content)
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
