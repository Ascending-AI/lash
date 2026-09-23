use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::TraceAttachment;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TraceContentBlock {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "is_false")]
        cache_breakpoint: bool,
    },
    Attachment {
        source: Box<TraceAttachment>,
    },
    ToolCall {
        call_id: Option<String>,
        tool_name: String,
        input_json: Value,
        item_id: Option<String>,
        has_signature: bool,
    },
    ToolResult {
        call_id: Option<String>,
        tool_name: Option<String>,
        content: Vec<TraceToolResultBlock>,
    },
    Reasoning {
        text: String,
        item_id: Option<String>,
        summary: Vec<String>,
        has_encrypted: bool,
        redacted: bool,
    },
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// One ordered block of a traced tool result: text, or an attachment at the
/// position the tool's value placed it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TraceToolResultBlock {
    Text { text: String },
    Attachment { source: Box<TraceAttachment> },
}
