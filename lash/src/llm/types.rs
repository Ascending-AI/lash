use tokio::sync::mpsc::UnboundedSender;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LlmRole {
    User,
    Assistant,
    System,
}

#[derive(Clone, Debug)]
pub struct LlmMessage {
    pub role: LlmRole,
    pub content: String,
    pub kind: String,
    pub image_idx: i64,
}

#[derive(Clone, Debug)]
pub struct LlmAttachment {
    pub mime: String,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct LlmRequest {
    pub model: String,
    pub system_prompt: String,
    pub messages: Vec<LlmMessage>,
    pub attachments: Vec<LlmAttachment>,
    pub reasoning_effort: Option<String>,
    pub stream_events: Option<UnboundedSender<LlmStreamEvent>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LlmUsage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_input_tokens: i64,
}

#[derive(Clone, Debug)]
pub enum LlmStreamEvent {
    Delta(String),
    Usage(LlmUsage),
}

#[derive(Clone, Debug, Default)]
pub struct LlmResponse {
    pub full_text: String,
    pub deltas: Vec<String>,
    pub usage: LlmUsage,
    pub request_body: Option<String>,
    pub http_summary: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ModelSelection {
    pub model: &'static str,
    pub reasoning_effort: Option<&'static str>,
}
