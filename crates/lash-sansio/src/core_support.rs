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

/// Reserved BLAKE3 domains used by workspace hash owners. Entries are
/// append-only so a retired domain cannot be silently reused.
const BLAKE3_DOMAINS: &[&str] = &[
    "lash-append-request/v2",
    "lash-attachment/v2",
    "lash-blob/v2",
    "lash-composition-tool/v2",
    "lash-derived-trigger-subscription/v2",
    "lash-draft-node/v3",
    "lash-frame-node/v3",
    "lash-google-upload-credential-scope/v2",
    "lash-history-node/v3",
    "lash-intent/v2",
    "lash-lashlang-content/v2",
    "lash-lashlang-execution-site/v2",
    "lash-lashlang-program/v2",
    "lash-model-facing-composition/v2",
    "lash-openai-responses-request/v2",
    "lash-plugin-snapshot-revision/v2",
    "lash-process-env/v4",
    "lash-process-lease/v2",
    "lash-queued-work-batch/v2",
    "lash-queued-work-claim-lease/v2",
    "lash-rlm-execution-state-leaf/v2",
    "lash-rlm-stall-reply/v2",
    "lash-runtime-effect-envelope/v2",
    "lash-runtime-usage-payload/v2",
    "lash-session-append-draft-fallback/v2",
    "lash-stable-identity/v2",
    "lash-tool-catalog-authority/v2",
    "lash-tool-intent-payload/v2",
    "lash-tool-output-spill/v2",
    "lash-tool-schema-cache/v2",
    "lash-turn-input/v2",
    "lash-workflow-edge/v2",
    "lash-workflow-node/v2",
    "lash-workflow-source/v2",
    "lash.agent-frame-key/v2",
    "lashlang-process-start/v2",
];

/// BLAKE3 hasher initialized with Lash's mandatory length-prefixed domain tag.
///
/// This is an internal cross-crate seam. Durable identity owners still own the
/// bytes written after the tag and must version their domain when that format
/// changes.
pub struct Blake3DomainHasher(blake3::Hasher);

impl Blake3DomainHasher {
    pub fn new(domain: &str) -> Self {
        debug_assert!(
            BLAKE3_DOMAINS.contains(&domain),
            "BLAKE3 domain `{domain}` is missing from BLAKE3_DOMAINS"
        );
        let mut hasher = blake3::Hasher::new();
        hasher.update(&(domain.len() as u64).to_be_bytes());
        hasher.update(domain.as_bytes());
        Self(hasher)
    }

    pub fn update(&mut self, bytes: impl AsRef<[u8]>) {
        self.0.update(bytes.as_ref());
    }

    pub fn finalize(self) -> [u8; 32] {
        self.0.finalize().into()
    }

    pub fn finalize_hex(self) -> String {
        self.0.finalize().to_hex().to_string()
    }
}

pub fn blake3_domain_hash(domain: &str, bytes: impl AsRef<[u8]>) -> [u8; 32] {
    let mut hasher = Blake3DomainHasher::new(domain);
    hasher.update(bytes);
    hasher.finalize()
}

pub fn blake3_domain_hash_hex(domain: &str, bytes: impl AsRef<[u8]>) -> String {
    let mut hasher = Blake3DomainHasher::new(domain);
    hasher.update(bytes);
    hasher.finalize_hex()
}

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

#[cfg(test)]
mod blake3_domain_tests {
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};

    use super::BLAKE3_DOMAINS;

    fn rust_sources_below(root: &Path) -> Vec<PathBuf> {
        fn visit(directory: &Path, sources: &mut Vec<PathBuf>) {
            let mut entries = std::fs::read_dir(directory)
                .unwrap_or_else(|error| panic!("read {}: {error}", directory.display()))
                .map(|entry| entry.expect("read workspace source entry").path())
                .collect::<Vec<_>>();
            entries.sort();
            for path in entries {
                if path.is_dir() {
                    visit(&path, sources);
                } else if path.extension().is_some_and(|extension| extension == "rs") {
                    sources.push(path);
                }
            }
        }

        let mut sources = Vec::new();
        visit(root, &mut sources);
        sources
    }

    fn domain_literals(source: &str) -> BTreeSet<String> {
        let constructors = [
            ("Blake3DomainHasher", "::new("),
            ("blake3_domain_hash", "("),
            ("blake3_domain_hash_hex", "("),
            ("blake3_hex", "("),
            ("domain_hash", "("),
            ("hex_digest", "("),
        ];
        let mut domains = BTreeSet::new();
        for (name, suffix) in constructors {
            let needle = format!("{name}{suffix}");
            let mut remaining = source;
            while let Some(offset) = remaining.find(&needle) {
                remaining = &remaining[offset + needle.len()..];
                let argument = remaining.trim_start();
                let Some(literal) = argument.strip_prefix('"') else {
                    continue;
                };
                let Some((domain, _)) = literal.split_once('"') else {
                    continue;
                };
                domains.insert(domain.to_string());
            }
        }
        domains
    }

    #[test]
    fn blake3_domains_are_unique_and_match_workspace_usage() {
        let registered = BLAKE3_DOMAINS
            .iter()
            .map(|domain| (*domain).to_string())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            registered.len(),
            BLAKE3_DOMAINS.len(),
            "BLAKE3 domains are permanently reserved and must be unique"
        );

        let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let used = rust_sources_below(&workspace_root.join("crates"))
            .into_iter()
            .flat_map(|path| {
                let source = std::fs::read_to_string(&path)
                    .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
                domain_literals(&source)
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(
            used, registered,
            "BLAKE3_DOMAINS must exactly match BLAKE3 domain literals used in workspace Rust sources"
        );
    }
}
