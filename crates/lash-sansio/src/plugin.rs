use std::sync::Arc;

use crate::llm::types::AttachmentSource;
use crate::{MessageOrigin, MessageRole, Part};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PluginMessage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub role: MessageRole,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<MessageOrigin>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<Part>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<AttachmentSource>,
}

impl PluginMessage {
    pub fn text(role: MessageRole, content: impl Into<String>) -> Self {
        Self {
            id: None,
            role,
            content: content.into(),
            origin: None,
            parts: Vec::new(),
            attachments: Vec::new(),
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

    pub fn first_text(&self) -> Option<&str> {
        if !self.content.is_empty() {
            return Some(self.content.as_str());
        }
        self.parts.iter().find_map(|part| {
            matches!(part.kind, crate::PartKind::Text | crate::PartKind::Prose)
                .then_some(part.content.as_str())
        })
    }
}

/// Gate on Tool Catalog membership: a contribution is kept when at least one
/// of `tools` is a member of the catalog. There is no minimum-tier dimension.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PromptContributionGate {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
}

impl PromptContributionGate {
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointKind {
    AfterWork,
    BeforeCompletion,
}
