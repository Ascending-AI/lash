//! Prompt-layer envelopes: templates, slots, and contributions.

use std::collections::HashMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemotePromptLayer {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<RemotePromptTemplate>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub slots: HashMap<RemotePromptSlot, RemotePromptSlotLayer>,
}

impl RemotePromptLayer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.template.is_none() && self.slots.is_empty()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemotePromptBuiltin {
    MainAgentIntro,
    ExecutionInstructions,
    CoreGuidance,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemotePromptSlot {
    Intro,
    Execution,
    Guidance,
    ProjectInstructions,
    RuntimeContext,
    Environment,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RemotePromptTemplateEntry {
    Text { content: String },
    Builtin { builtin: RemotePromptBuiltin },
    Slot { slot: RemotePromptSlot },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct RemotePromptTemplateSection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<RemotePromptTemplateEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct RemotePromptTemplate {
    pub sections: Vec<RemotePromptTemplateSection>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemotePromptSlotLayer {
    #[serde(default)]
    pub reset: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contributions: Vec<RemotePromptContribution>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemotePromptContribution {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default)]
    pub priority: i32,
    #[serde(
        default,
        skip_serializing_if = "RemotePromptContributionGate::is_empty"
    )]
    pub gate: RemotePromptContributionGate,
    pub content: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemotePromptContributionGate {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
}

impl RemotePromptContributionGate {
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}
