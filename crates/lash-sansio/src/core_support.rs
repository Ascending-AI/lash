//! Cross-crate implementation seams consumed by `lash-core`.
//!
//! These traits keep runtime-only operations callable across the crate boundary
//! without publishing the same operations as supported `lash_core` host APIs.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::Value;

use crate::llm::types::LlmToolSpec;
use crate::{
    AttachmentId, AttachmentMeta, AttachmentRef, AttachmentTypeMetadata, BaseRenderCache,
    ConversationRecord, MediaType, Message, MessageSequence, ModelEffortValidationCategory,
    ModelToolReturn, ModelToolReturnPart, PromptContribution, PromptFingerprint, ProtocolEvent,
    SessionAppendNode, ToolCancellation, ToolCatalog, ToolContract, ToolDefinition, ToolFailure,
    ToolFailureClass, ToolManifest, ToolRetryPolicy, ToolValue,
};

pub trait AttachmentIdCoreSupport {
    fn as_str(&self) -> &str;
}

impl AttachmentIdCoreSupport for AttachmentId {
    fn as_str(&self) -> &str {
        AttachmentId::as_str(self)
    }
}

pub trait MediaTypeCoreSupport {
    fn family(&self) -> &str;
}

impl MediaTypeCoreSupport for MediaType {
    fn family(&self) -> &str {
        MediaType::family(self)
    }
}

pub trait AttachmentTypeMetadataCoreSupport {
    fn image(width: Option<u32>, height: Option<u32>) -> Self;
}

impl AttachmentTypeMetadataCoreSupport for AttachmentTypeMetadata {
    fn image(width: Option<u32>, height: Option<u32>) -> Self {
        AttachmentTypeMetadata::image(width, height)
    }
}

pub trait AttachmentMetaCoreSupport {
    fn new(
        id: AttachmentId,
        media_type: MediaType,
        byte_len: u64,
        type_metadata: Option<AttachmentTypeMetadata>,
        label: Option<String>,
    ) -> Self;

    fn as_ref(&self) -> AttachmentRef;
}

impl AttachmentMetaCoreSupport for AttachmentMeta {
    fn new(
        id: AttachmentId,
        media_type: MediaType,
        byte_len: u64,
        type_metadata: Option<AttachmentTypeMetadata>,
        label: Option<String>,
    ) -> Self {
        AttachmentMeta::new(id, media_type, byte_len, type_metadata, label)
    }

    fn as_ref(&self) -> AttachmentRef {
        AttachmentMeta::as_ref(self)
    }
}

pub trait ModelEffortValidationCategoryCoreSupport {
    fn code(&self) -> &'static str;
}

impl ModelEffortValidationCategoryCoreSupport for ModelEffortValidationCategory {
    fn code(&self) -> &'static str {
        ModelEffortValidationCategory::code(self)
    }
}

pub trait MessageCoreSupport {
    fn content_equals(&self, other: &Message) -> bool;
}

impl MessageCoreSupport for Message {
    fn content_equals(&self, other: &Message) -> bool {
        crate::session_model::message::message_content_equal(self, other)
    }
}

impl MessageCoreSupport for ConversationRecord {
    fn content_equals(&self, other: &Message) -> bool {
        crate::session_model::message::message_content_equal(self, other)
    }
}

pub trait MessageSequenceCoreSupport {
    fn preserved_extension_delta<'a>(&self, next: &'a MessageSequence) -> Option<&'a [Message]>;
    fn from_owned(messages: Vec<Message>) -> Self;
    fn from_base(base: Arc<Vec<Message>>) -> Self;
    fn from_base_and_delta(base: Arc<Vec<Message>>, delta: Vec<Message>) -> Self;
    fn with_base_render_cache(self, cache: Arc<BaseRenderCache>) -> Self;
    fn as_slice(&self) -> &[Message];
    fn shared(&self) -> Arc<Vec<Message>>;
    fn extend(&mut self, messages: Vec<Message>);
}

impl MessageSequenceCoreSupport for MessageSequence {
    fn preserved_extension_delta<'a>(&self, next: &'a MessageSequence) -> Option<&'a [Message]> {
        MessageSequence::preserved_extension_delta(self, next)
    }

    fn from_owned(messages: Vec<Message>) -> Self {
        MessageSequence::from_owned(messages)
    }

    fn from_base(base: Arc<Vec<Message>>) -> Self {
        MessageSequence::from_base(base)
    }

    fn from_base_and_delta(base: Arc<Vec<Message>>, delta: Vec<Message>) -> Self {
        MessageSequence::from_base_and_delta(base, delta)
    }

    fn with_base_render_cache(self, cache: Arc<BaseRenderCache>) -> Self {
        MessageSequence::with_base_render_cache(self, cache)
    }

    fn as_slice(&self) -> &[Message] {
        MessageSequence::as_slice(self)
    }

    fn shared(&self) -> Arc<Vec<Message>> {
        MessageSequence::shared(self)
    }

    fn extend(&mut self, messages: Vec<Message>) {
        MessageSequence::extend(self, messages);
    }
}

pub trait SessionAppendNodeCoreSupport {
    fn protocol_event(event: ProtocolEvent) -> Self;
}

impl SessionAppendNodeCoreSupport for SessionAppendNode {
    fn protocol_event(event: ProtocolEvent) -> Self {
        SessionAppendNode::protocol_event(event)
    }
}

pub trait ToolCatalogCoreSupport {
    fn from_tool_definitions(tools: Vec<ToolDefinition>) -> Self;
    fn from_tools(tools: Vec<ToolManifest>, contracts: BTreeMap<String, Arc<ToolContract>>)
    -> Self;
    fn tool_names(&self) -> Arc<Vec<String>>;
    fn tool_names_fingerprint(&self) -> PromptFingerprint;
    fn model_tool_specs(&self) -> Arc<Vec<LlmToolSpec>>;
    fn filter_prompt_contributions(
        &self,
        contributions: Vec<PromptContribution>,
    ) -> Vec<PromptContribution>;
}

impl ToolCatalogCoreSupport for ToolCatalog {
    fn from_tool_definitions(tools: Vec<ToolDefinition>) -> Self {
        ToolCatalog::from_tool_definitions(tools)
    }

    fn from_tools(
        tools: Vec<ToolManifest>,
        contracts: BTreeMap<String, Arc<ToolContract>>,
    ) -> Self {
        ToolCatalog::from_tools(tools, contracts)
    }

    fn tool_names(&self) -> Arc<Vec<String>> {
        ToolCatalog::tool_names(self)
    }

    fn tool_names_fingerprint(&self) -> PromptFingerprint {
        ToolCatalog::tool_names_fingerprint(self)
    }

    fn model_tool_specs(&self) -> Arc<Vec<LlmToolSpec>> {
        ToolCatalog::model_tool_specs(self)
    }

    fn filter_prompt_contributions(
        &self,
        contributions: Vec<PromptContribution>,
    ) -> Vec<PromptContribution> {
        ToolCatalog::filter_prompt_contributions(self, contributions)
    }
}

pub trait ToolRetryPolicyCoreSupport {
    fn idempotent(max_attempts: u32, base_delay_ms: u64, max_delay_ms: u64) -> Self;
    fn max_attempts(self) -> u32;
    fn delay_ms_for_retry(self, retry_index: u32, requested_after_ms: Option<u64>) -> u64;
}

impl ToolRetryPolicyCoreSupport for ToolRetryPolicy {
    fn idempotent(max_attempts: u32, base_delay_ms: u64, max_delay_ms: u64) -> Self {
        ToolRetryPolicy::idempotent(max_attempts, base_delay_ms, max_delay_ms)
    }

    fn max_attempts(self) -> u32 {
        ToolRetryPolicy::max_attempts(self)
    }

    fn delay_ms_for_retry(self, retry_index: u32, requested_after_ms: Option<u64>) -> u64 {
        ToolRetryPolicy::delay_ms_for_retry(self, retry_index, requested_after_ms)
    }
}

pub trait ToolValueCoreSupport {
    fn to_json_value(&self) -> Value;
}

impl ToolValueCoreSupport for ToolValue {
    fn to_json_value(&self) -> Value {
        ToolValue::to_json_value(self)
    }
}

pub trait ToolFailureCoreSupport {
    fn runtime(
        class: ToolFailureClass,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self;
    fn tool(class: ToolFailureClass, code: impl Into<String>, message: impl Into<String>) -> Self;
    fn to_json_value(&self) -> Value;
}

impl ToolFailureCoreSupport for ToolFailure {
    fn runtime(
        class: ToolFailureClass,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        ToolFailure::runtime(class, code, message)
    }

    fn tool(class: ToolFailureClass, code: impl Into<String>, message: impl Into<String>) -> Self {
        ToolFailure::tool(class, code, message)
    }

    fn to_json_value(&self) -> Value {
        ToolFailure::to_json_value(self)
    }
}

pub trait ToolCancellationCoreSupport {
    fn runtime(message: impl Into<String>) -> Self;
    fn to_json_value(&self) -> Value;
}

impl ToolCancellationCoreSupport for ToolCancellation {
    fn runtime(message: impl Into<String>) -> Self {
        ToolCancellation::runtime(message)
    }

    fn to_json_value(&self) -> Value {
        ToolCancellation::to_json_value(self)
    }
}

pub trait ModelToolReturnCoreSupport {
    fn text(call_id: String, tool_name: String, content: impl Into<String>) -> Self;
}

impl ModelToolReturnCoreSupport for ModelToolReturn {
    fn text(call_id: String, tool_name: String, content: impl Into<String>) -> Self {
        ModelToolReturn::text(call_id, tool_name, content)
    }
}

pub trait ModelToolReturnPartCoreSupport {
    fn text(text: impl Into<String>) -> Self;
}

impl ModelToolReturnPartCoreSupport for ModelToolReturnPart {
    fn text(text: impl Into<String>) -> Self {
        ModelToolReturnPart::text(text)
    }
}
