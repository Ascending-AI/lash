//! Lash-owned values for the MCP requests a host answers: sampling, roots and
//! elicitation.
//!
//! The SDK's model types stay inside this adapter. Requests arrive validated
//! and converted; host answers are converted back at the wire. SDK input this
//! vocabulary cannot carry is refused to the server as invalid params before
//! any host handler runs.

#![allow(
    deprecated,
    reason = "FIG-6177 retains existing sampling and roots during the MCP deprecation window"
)]

use rmcp::model::{
    AudioContent, ContentBlock, ContextInclusion, CreateMessageRequestParams, CreateMessageResult,
    ElicitResult, ElicitationAction, ElicitationCapability, ErrorCode, ErrorData,
    FormElicitationCapability, ImageContent, ModelPreferences, Role, Root, SamplingMessage,
    SamplingMessageContentBlock, TextContent, ToolChoiceMode, ToolResultContent, ToolUseContent,
    UrlElicitationCapability,
};
use serde_json::{Map, Value};

/// How a host refusal reaches the server, as its JSON-RPC error class.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum McpRefusalKind {
    /// The host does not accept this request as asked (JSON-RPC invalid params).
    InvalidParams,
    /// The host could not complete the request (JSON-RPC internal error).
    Internal,
}

/// A host's refusal of one server-to-client request.
///
/// Lash sends it to the requesting server as a JSON-RPC error of its
/// [`kind`](Self::kind), with the message and data the host chose.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
#[error("{message}")]
pub struct McpHostRefusal {
    kind: McpRefusalKind,
    message: String,
    data: Option<Value>,
}

impl McpHostRefusal {
    /// The host does not accept the request as asked.
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(McpRefusalKind::InvalidParams, message)
    }

    /// The host could not complete the request.
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(McpRefusalKind::Internal, message)
    }

    fn new(kind: McpRefusalKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            data: None,
        }
    }

    /// Structured detail sent as the JSON-RPC error's `data`.
    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }

    pub fn kind(&self) -> McpRefusalKind {
        self.kind
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn data(&self) -> Option<&Value> {
        self.data.as_ref()
    }

    pub(crate) fn into_protocol_error(self) -> ErrorData {
        let code = match self.kind {
            McpRefusalKind::InvalidParams => ErrorCode::INVALID_PARAMS,
            McpRefusalKind::Internal => ErrorCode::INTERNAL_ERROR,
        };
        ErrorData::new(code, self.message, self.data)
    }
}

/// Who wrote a sampling message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpRole {
    User,
    Assistant,
}

/// One content block of a sampling message.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum McpSamplingContent {
    Text {
        text: String,
    },
    /// Base64-encoded image data.
    Image {
        data: String,
        mime_type: String,
    },
    /// Base64-encoded audio data.
    Audio {
        data: String,
        mime_type: String,
    },
    /// A tool call the assistant made; assistant messages only.
    ToolUse {
        id: String,
        name: String,
        input: Map<String, Value>,
    },
    /// A tool call's result; user messages only, and alone in its message.
    ToolResult {
        tool_use_id: String,
        /// The result's MCP content blocks, as their wire JSON.
        content: Vec<Value>,
        structured_content: Option<Value>,
        is_error: Option<bool>,
    },
}

/// One message of a sampling conversation.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct McpSamplingMessage {
    pub role: McpRole,
    pub content: Vec<McpSamplingContent>,
}

/// Which MCP servers' context the server asks the host to include.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum McpContextInclusion {
    None,
    ThisServer,
    AllServers,
}

/// Whether the sampled model may, must or must not call the offered tools.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum McpToolChoice {
    Auto,
    Required,
    None,
}

/// The server's model-selection preferences; each priority is in `0..=1`.
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct McpModelPreferences {
    /// Model names in the server's order of preference.
    pub hints: Vec<String>,
    pub cost_priority: Option<f32>,
    pub speed_priority: Option<f32>,
    pub intelligence_priority: Option<f32>,
}

/// A validated `sampling/createMessage` request.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct McpSamplingParams {
    pub messages: Vec<McpSamplingMessage>,
    pub system_prompt: Option<String>,
    pub max_tokens: u32,
    pub temperature: Option<f32>,
    pub stop_sequences: Vec<String>,
    pub model_preferences: Option<McpModelPreferences>,
    pub include_context: Option<McpContextInclusion>,
    /// Tools offered to the model, as their MCP tool definitions' wire JSON.
    pub tools: Option<Vec<Value>>,
    pub tool_choice: Option<McpToolChoice>,
    /// Server-defined request metadata, passed through untouched.
    pub metadata: Option<Value>,
}

/// Why sampled generation stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum McpStopReason {
    EndTurn,
    StopSequence,
    MaxTokens,
    ToolUse,
    /// A provider-specific reason, sent verbatim.
    Other(String),
}

/// A host's sampled assistant message.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct McpSamplingResult {
    pub content: Vec<McpSamplingContent>,
    /// The model that generated the message.
    pub model: String,
    pub stop_reason: Option<McpStopReason>,
}

impl McpSamplingResult {
    pub fn new(content: Vec<McpSamplingContent>, model: impl Into<String>) -> Self {
        Self {
            content,
            model: model.into(),
            stop_reason: None,
        }
    }

    /// One assistant text block.
    pub fn text(text: impl Into<String>, model: impl Into<String>) -> Self {
        Self::new(vec![McpSamplingContent::Text { text: text.into() }], model)
    }

    pub fn with_stop_reason(mut self, stop_reason: McpStopReason) -> Self {
        self.stop_reason = Some(stop_reason);
        self
    }
}

/// The elicitation modes a host presents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum McpElicitationModes {
    Form,
    Url,
    FormAndUrl,
}

/// A host's answer to one elicitation.
#[derive(Clone, Debug, PartialEq)]
pub enum McpElicitationResult {
    /// The user accepted: a form carries its answer, a URL interaction none.
    Accept { content: Option<Value> },
    /// The user declined; the operation continues without the answer.
    Decline,
    /// The user dismissed the request.
    Cancel,
}

/// A workspace root visible to MCP servers.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct McpRoot {
    pub uri: String,
    pub name: Option<String>,
}

impl McpRoot {
    pub fn new(uri: impl Into<String>) -> Self {
        Self {
            uri: uri.into(),
            name: None,
        }
    }

    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }
}

// -- SDK conversion -----------------------------------------------------------

fn unconvertible(what: &str) -> McpHostRefusal {
    McpHostRefusal::invalid_params(format!("this MCP client does not support {what}"))
}

impl McpSamplingParams {
    pub(crate) fn from_sdk(params: CreateMessageRequestParams) -> Result<Self, McpHostRefusal> {
        params.validate().map_err(McpHostRefusal::invalid_params)?;
        let messages = params
            .messages
            .into_iter()
            .map(McpSamplingMessage::from_sdk)
            .collect::<Result<_, _>>()?;
        let include_context = params
            .include_context
            .map(|inclusion| match inclusion {
                ContextInclusion::None => Ok(McpContextInclusion::None),
                ContextInclusion::ThisServer => Ok(McpContextInclusion::ThisServer),
                ContextInclusion::AllServers => Ok(McpContextInclusion::AllServers),
                _ => Err(unconvertible("this sampling context inclusion")),
            })
            .transpose()?;
        let tool_choice = params
            .tool_choice
            .map(|choice| match choice.mode.unwrap_or_default() {
                ToolChoiceMode::Auto => Ok(McpToolChoice::Auto),
                ToolChoiceMode::Required => Ok(McpToolChoice::Required),
                ToolChoiceMode::None => Ok(McpToolChoice::None),
                _ => Err(unconvertible("this sampling tool choice")),
            })
            .transpose()?;
        let tools = params
            .tools
            .map(|tools| tools.iter().map(wire_json).collect::<Result<_, _>>())
            .transpose()?;
        Ok(Self {
            messages,
            system_prompt: params.system_prompt,
            max_tokens: params.max_tokens,
            temperature: params.temperature,
            stop_sequences: params.stop_sequences.unwrap_or_default(),
            model_preferences: params.model_preferences.map(McpModelPreferences::from_sdk),
            include_context,
            tools,
            tool_choice,
            metadata: params.metadata,
        })
    }
}

fn wire_json(value: &impl serde::Serialize) -> Result<Value, McpHostRefusal> {
    serde_json::to_value(value).map_err(|error| McpHostRefusal::invalid_params(error.to_string()))
}

impl McpModelPreferences {
    fn from_sdk(preferences: ModelPreferences) -> Self {
        Self {
            hints: preferences
                .hints
                .unwrap_or_default()
                .into_iter()
                .filter_map(|hint| hint.name)
                .collect(),
            cost_priority: preferences.cost_priority,
            speed_priority: preferences.speed_priority,
            intelligence_priority: preferences.intelligence_priority,
        }
    }
}

impl McpSamplingMessage {
    fn from_sdk(message: SamplingMessage) -> Result<Self, McpHostRefusal> {
        let role = match message.role {
            Role::User => McpRole::User,
            Role::Assistant => McpRole::Assistant,
        };
        let content = message
            .content
            .into_vec()
            .into_iter()
            .map(McpSamplingContent::from_sdk)
            .collect::<Result<_, _>>()?;
        Ok(Self { role, content })
    }
}

impl McpSamplingContent {
    fn from_sdk(block: SamplingMessageContentBlock) -> Result<Self, McpHostRefusal> {
        Ok(match block {
            SamplingMessageContentBlock::Text(text) => Self::Text { text: text.text },
            SamplingMessageContentBlock::Image(image) => Self::Image {
                data: image.data,
                mime_type: image.mime_type,
            },
            SamplingMessageContentBlock::Audio(audio) => Self::Audio {
                data: audio.data,
                mime_type: audio.mime_type,
            },
            SamplingMessageContentBlock::ToolUse(tool_use) => Self::ToolUse {
                id: tool_use.id,
                name: tool_use.name,
                input: tool_use.input,
            },
            SamplingMessageContentBlock::ToolResult(result) => Self::ToolResult {
                tool_use_id: result.tool_use_id,
                content: result
                    .content
                    .iter()
                    .map(wire_json)
                    .collect::<Result<_, _>>()?,
                structured_content: result.structured_content,
                is_error: result.is_error,
            },
            _ => return Err(unconvertible("this sampling content type")),
        })
    }

    /// A host-built block the SDK cannot carry is the host's failure.
    fn into_sdk(self) -> Result<SamplingMessageContentBlock, ErrorData> {
        Ok(match self {
            Self::Text { text } => SamplingMessageContentBlock::Text(TextContent::new(text)),
            Self::Image { data, mime_type } => {
                SamplingMessageContentBlock::Image(ImageContent::new(data, mime_type))
            }
            Self::Audio { data, mime_type } => {
                SamplingMessageContentBlock::Audio(AudioContent::new(data, mime_type))
            }
            Self::ToolUse { id, name, input } => {
                SamplingMessageContentBlock::ToolUse(ToolUseContent::new(id, name, input))
            }
            Self::ToolResult {
                tool_use_id,
                content,
                structured_content,
                is_error,
            } => {
                let content = content
                    .into_iter()
                    .map(serde_json::from_value::<ContentBlock>)
                    .collect::<Result<_, _>>()
                    .map_err(|error| {
                        ErrorData::internal_error(
                            format!(
                                "host sampling result has an invalid tool result block: {error}"
                            ),
                            None,
                        )
                    })?;
                let mut result = ToolResultContent::new(tool_use_id, content);
                result.structured_content = structured_content;
                result.is_error = is_error;
                SamplingMessageContentBlock::ToolResult(result)
            }
        })
    }
}

impl McpSamplingResult {
    pub(crate) fn into_sdk(self) -> Result<CreateMessageResult, ErrorData> {
        let mut content: Vec<_> = self
            .content
            .into_iter()
            .map(McpSamplingContent::into_sdk)
            .collect::<Result<_, _>>()?;
        // One block keeps the single-content wire shape.
        let message = match content.len() {
            1 => SamplingMessage::new(Role::Assistant, content.remove(0)),
            _ => SamplingMessage::new_multiple(Role::Assistant, content),
        };
        let result = CreateMessageResult::new(message, self.model);
        Ok(match self.stop_reason {
            None => result,
            Some(reason) => result.with_stop_reason(match reason {
                McpStopReason::EndTurn => CreateMessageResult::STOP_REASON_END_TURN.to_string(),
                McpStopReason::StopSequence => {
                    CreateMessageResult::STOP_REASON_END_SEQUENCE.to_string()
                }
                McpStopReason::MaxTokens => {
                    CreateMessageResult::STOP_REASON_END_MAX_TOKEN.to_string()
                }
                McpStopReason::ToolUse => CreateMessageResult::STOP_REASON_TOOL_USE.to_string(),
                McpStopReason::Other(reason) => reason,
            }),
        })
    }
}

impl McpElicitationModes {
    pub(crate) fn capability(self) -> ElicitationCapability {
        let capability = ElicitationCapability::new();
        match self {
            Self::Form => capability.with_form(FormElicitationCapability::default()),
            Self::Url => capability.with_url(UrlElicitationCapability::default()),
            Self::FormAndUrl => capability
                .with_form(FormElicitationCapability::default())
                .with_url(UrlElicitationCapability::default()),
        }
    }
}

impl McpElicitationResult {
    pub(crate) fn into_sdk(self) -> ElicitResult {
        match self {
            Self::Accept { content } => {
                let result = ElicitResult::new(ElicitationAction::Accept);
                match content {
                    Some(content) => result.with_content(content),
                    None => result,
                }
            }
            Self::Decline => ElicitResult::new(ElicitationAction::Decline),
            Self::Cancel => ElicitResult::new(ElicitationAction::Cancel),
        }
    }
}

impl McpRoot {
    pub(crate) fn into_sdk(self) -> Root {
        let root = Root::new(self.uri);
        match self.name {
            Some(name) => root.with_name(name),
            None => root,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unconvertible_sampling_input_is_refused_as_invalid_params() {
        // SEP-1577: a tool result answers an assistant tool use. One with no
        // matching use is refused before any host handler sees it.
        let params = CreateMessageRequestParams::new(
            vec![SamplingMessage::user_tool_result("call_1", Vec::new())],
            16,
        );
        let refusal = McpSamplingParams::from_sdk(params).expect_err("unbalanced tool result");
        assert_eq!(refusal.kind(), McpRefusalKind::InvalidParams);
        assert_eq!(
            refusal.into_protocol_error().code,
            ErrorCode::INVALID_PARAMS
        );
    }
}
