//! Cross-crate implementation seams consumed by `lash-core`.
//!
//! These traits keep runtime-only operations callable across the crate boundary
//! without publishing the same operations as supported `lash_core` host APIs.
//!
//! Every impl below is `#[doc(hidden)]`: the traits are not re-exported through
//! the `lash` facade, so a host can neither name nor call them, and the impl on
//! a facade type is support plumbing rather than part of that type's API (ADR
//! 0051). The facade-completeness check reads hidden impls as outside the
//! facade surface.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::Value;

use crate::append_vec::AppendVec;
use crate::blake3_domains::BLAKE3_DOMAINS;
use crate::llm::types::LlmToolSpec;
use crate::{
    AttachmentId, AttachmentTypeMetadata, BaseRenderCache, ConversationRecord,
    LlmProfileEffortValidationCategory, MediaType, Message, MessageSequence, ModelToolReturn,
    ModelToolReturnPart, ProtocolEvent, SessionAppendNode, ToolCancellation, ToolCatalog,
    ToolContract, ToolDefinition, ToolFailure, ToolFailureClass, ToolId, ToolManifest, ToolValue,
};

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

#[doc(hidden)]
impl AttachmentIdCoreSupport for AttachmentId {
    fn as_str(&self) -> &str {
        AttachmentId::as_str(self)
    }
}

pub trait MediaTypeCoreSupport {
    fn family(&self) -> &str;
}

#[doc(hidden)]
impl MediaTypeCoreSupport for MediaType {
    fn family(&self) -> &str {
        MediaType::family(self)
    }
}

pub trait AttachmentTypeMetadataCoreSupport {
    fn image(width: Option<u32>, height: Option<u32>) -> Self;
}

#[doc(hidden)]
impl AttachmentTypeMetadataCoreSupport for AttachmentTypeMetadata {
    fn image(width: Option<u32>, height: Option<u32>) -> Self {
        AttachmentTypeMetadata::image(width, height)
    }
}

pub trait ModelEffortValidationCategoryCoreSupport {
    fn failure_code(&self) -> crate::session_model::TurnFailureCode;
}

#[doc(hidden)]
impl ModelEffortValidationCategoryCoreSupport for LlmProfileEffortValidationCategory {
    fn failure_code(&self) -> crate::session_model::TurnFailureCode {
        LlmProfileEffortValidationCategory::failure_code(self)
    }
}

/// Mints a turn's reply marker. Only the runtime's commit names a turn's
/// reply, so the constructor stays behind this seam.
pub trait TurnReplyCoreSupport {
    fn mint(turn_id: crate::TurnId, part_id: String) -> Self;
}

#[doc(hidden)]
impl TurnReplyCoreSupport for crate::TurnReply {
    fn mint(turn_id: crate::TurnId, part_id: String) -> Self {
        crate::TurnReply::mint(turn_id, part_id)
    }
}

pub trait MessageCoreSupport {
    fn content_equals(&self, other: &Message) -> bool;
}

#[doc(hidden)]
impl MessageCoreSupport for Message {
    fn content_equals(&self, other: &Message) -> bool {
        crate::session_model::message::message_content_equal(self, other)
    }
}

#[doc(hidden)]
impl MessageCoreSupport for ConversationRecord {
    fn content_equals(&self, other: &Message) -> bool {
        crate::session_model::message::message_content_equal(self, other)
    }
}

pub trait MessageSequenceCoreSupport {
    fn preserved_extension_delta<'a>(&self, next: &'a MessageSequence) -> Option<&'a [Message]>;
    fn from_owned(messages: Vec<Message>) -> Self;
    fn from_base(base: AppendVec<Message>) -> Self;
    fn from_base_and_delta(base: AppendVec<Message>, delta: Vec<Message>) -> Self;
    fn with_base_render_cache(self, cache: Arc<BaseRenderCache>) -> Self;
    fn as_slice(&self) -> &[Message];
    fn shared(&self) -> AppendVec<Message>;
    fn extend(&mut self, messages: Vec<Message>);
}

#[doc(hidden)]
impl MessageSequenceCoreSupport for MessageSequence {
    fn preserved_extension_delta<'a>(&self, next: &'a MessageSequence) -> Option<&'a [Message]> {
        MessageSequence::preserved_extension_delta(self, next)
    }

    fn from_owned(messages: Vec<Message>) -> Self {
        MessageSequence::from_owned(messages)
    }

    fn from_base(base: AppendVec<Message>) -> Self {
        MessageSequence::from_base(base)
    }

    fn from_base_and_delta(base: AppendVec<Message>, delta: Vec<Message>) -> Self {
        MessageSequence::from_base_and_delta(base, delta)
    }

    fn with_base_render_cache(self, cache: Arc<BaseRenderCache>) -> Self {
        MessageSequence::with_base_render_cache(self, cache)
    }

    fn as_slice(&self) -> &[Message] {
        MessageSequence::as_slice(self)
    }

    fn shared(&self) -> AppendVec<Message> {
        MessageSequence::shared(self)
    }

    fn extend(&mut self, messages: Vec<Message>) {
        MessageSequence::extend(self, messages);
    }
}

pub trait SessionAppendNodeCoreSupport {
    fn protocol_event(event: ProtocolEvent) -> Self;
}

#[doc(hidden)]
impl SessionAppendNodeCoreSupport for SessionAppendNode {
    fn protocol_event(event: ProtocolEvent) -> Self {
        SessionAppendNode::protocol_event(event)
    }
}

pub trait ToolCatalogCoreSupport: Sized {
    fn from_tool_definitions(tools: Vec<ToolDefinition>) -> Self;
    fn from_tools(
        tools: Vec<ToolManifest>,
        contracts: BTreeMap<ToolId, Arc<ToolContract>>,
    ) -> Result<Self, crate::ToolCatalogBuildError>;
    fn tool_names(&self) -> Arc<Vec<String>>;
    fn model_tool_specs(&self) -> Arc<Vec<LlmToolSpec>>;
}

#[doc(hidden)]
impl ToolCatalogCoreSupport for ToolCatalog {
    fn from_tool_definitions(tools: Vec<ToolDefinition>) -> Self {
        ToolCatalog::from_tool_definitions(tools)
    }

    fn from_tools(
        tools: Vec<ToolManifest>,
        contracts: BTreeMap<ToolId, Arc<ToolContract>>,
    ) -> Result<Self, crate::ToolCatalogBuildError> {
        ToolCatalog::from_tools(tools, contracts)
    }

    fn tool_names(&self) -> Arc<Vec<String>> {
        ToolCatalog::tool_names(self)
    }

    fn model_tool_specs(&self) -> Arc<Vec<LlmToolSpec>> {
        ToolCatalog::model_tool_specs(self)
    }
}

pub trait ToolValueCoreSupport {
    fn to_json_value(&self) -> Value;
}

#[doc(hidden)]
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

#[doc(hidden)]
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

#[doc(hidden)]
impl ToolCancellationCoreSupport for ToolCancellation {
    fn runtime(message: impl Into<String>) -> Self {
        ToolCancellation::runtime(message)
    }

    fn to_json_value(&self) -> Value {
        ToolCancellation::to_json_value(self)
    }
}

pub trait ModelToolReturnCoreSupport {
    fn text(tool_name: String, content: impl Into<String>) -> Self;
}

#[doc(hidden)]
impl ModelToolReturnCoreSupport for ModelToolReturn {
    fn text(tool_name: String, content: impl Into<String>) -> Self {
        ModelToolReturn::text(tool_name, content)
    }
}

pub trait ModelToolReturnPartCoreSupport {
    fn text(text: impl Into<String>) -> Self;
}

#[doc(hidden)]
impl ModelToolReturnPartCoreSupport for ModelToolReturnPart {
    fn text(text: impl Into<String>) -> Self {
        ModelToolReturnPart::text(text)
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // FIG-2971: test module is a host; ambient fs/env/process access is sanctioned
mod blake3_domain_tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::{Path, PathBuf};

    use super::BLAKE3_DOMAINS;
    use crate::blake3_domains::RETIRED_BLAKE3_DOMAINS;

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
        let constants = source
            .split("const ")
            .skip(1)
            .filter_map(|declaration| {
                let (declaration, _) = declaration.split_once(';')?;
                let (name, definition) = declaration.split_once(':')?;
                let (kind, value) = definition.split_once('=')?;
                if kind.trim() != "&str" {
                    return None;
                }
                let (domain, _) = value.trim().strip_prefix('"')?.split_once('"')?;
                Some((name.trim(), domain))
            })
            .collect::<BTreeMap<_, _>>();
        let mut domains = BTreeSet::new();
        for (name, suffix) in constructors {
            let needle = format!("{name}{suffix}");
            let mut remaining = source;
            while let Some(offset) = remaining.find(&needle) {
                remaining = &remaining[offset + needle.len()..];
                let argument = remaining.trim_start();
                let domain = if let Some(literal) = argument.strip_prefix('"') {
                    literal.split_once('"').map(|(domain, _)| domain)
                } else {
                    argument
                        .split([',', ')'])
                        .next()
                        .and_then(|name| constants.get(name.trim()).copied())
                };
                if let Some(domain) = domain {
                    domains.insert(domain.to_string());
                }
            }
        }
        domains
    }

    #[test]
    fn domain_usage_follows_source_declared_constants() {
        let source = r#"
const ACTIVE_DOMAIN: &str = "lash-build-generation/v1";
const MULTILINE_DOMAIN: &str =
    "lash-standard-compaction/v1";
fn encode() {
    Blake3DomainHasher::new(ACTIVE_DOMAIN);
    blake3_domain_hash(MULTILINE_DOMAIN, bytes);
}
"#;
        assert_eq!(
            domain_literals(source),
            BTreeSet::from([
                "lash-build-generation/v1".to_string(),
                "lash-standard-compaction/v1".to_string()
            ])
        );
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
        let used = ["crates", "examples", "runbooks"]
            .into_iter()
            .flat_map(|root| rust_sources_below(&workspace_root.join(root)))
            .flat_map(|path| {
                let source = std::fs::read_to_string(&path)
                    .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
                domain_literals(&source)
            })
            .collect::<BTreeSet<_>>();
        let retired = RETIRED_BLAKE3_DOMAINS
            .iter()
            .map(|d| (*d).to_owned())
            .collect::<BTreeSet<_>>();
        assert_eq!(retired.len(), RETIRED_BLAKE3_DOMAINS.len());
        assert!(
            retired.is_subset(&registered),
            "retired domains remain registered"
        );
        assert!(
            used.is_disjoint(&retired),
            "retired domains must never be reused"
        );
        let active = registered
            .difference(&retired)
            .cloned()
            .collect::<BTreeSet<_>>();
        assert_eq!(
            used, active,
            "the generated BLAKE3_DOMAINS must exactly match the BLAKE3 domains used in workspace \
             Rust sources: declare each as a `lash*/vN` identity constant and run \
             `python3 scripts/release_baseline.py tables --write`"
        );
    }
}

#[cfg(test)]
mod retired_identity_tests {
    #[test]
    fn retired_identity_domains_and_removed_message_fields_remain_reserved_or_refused() {
        for label in [
            "lash-rolling-history-compaction/v1",
            "lash-rolling-history-compaction/v2",
            "lash-process-env/v4",
            "lash-process-env/v5",
            "lash-process-lease/v2",
        ] {
            assert!(
                super::BLAKE3_DOMAINS.contains(&label),
                "retired label {label} was released"
            );
        }
        for field in ["lifecycle", "attachment_source", "tool_call_id"] {
            let mut part = serde_json::json!({"id":"m.p0", "kind":"Text", "content":"text"});
            part[field] = serde_json::json!("retired");
            let error =
                serde_json::from_value::<crate::Part>(part).expect_err("removed field refuses");
            assert!(error.to_string().contains(field), "{error}");
        }
        serde_json::from_value::<crate::Part>(
            serde_json::json!({"id":"m.p0", "kind":"Text", "content":"text"}),
        )
        .expect("current shape decodes");
    }
}

/// Internal cross-crate display rendering; not an integrator classification API.
pub trait PartCoreSupport {
    fn render(&self) -> String;
}
#[doc(hidden)]
impl PartCoreSupport for crate::Part {
    fn render(&self) -> String {
        crate::Part::render(self)
    }
}

/// Fold the non-exhaustive host classification inside its owning crate.
#[doc(hidden)]
pub fn fold_part_kind<T>(kind: crate::PartKind, variants: [T; 9]) -> T {
    let [
        text,
        attachment,
        code,
        output,
        error,
        prose,
        tool_call,
        tool_result,
        reasoning,
    ] = variants;
    match kind {
        crate::PartKind::Text => text,
        crate::PartKind::Attachment => attachment,
        crate::PartKind::Code => code,
        crate::PartKind::Output => output,
        crate::PartKind::Error => error,
        crate::PartKind::Prose => prose,
        crate::PartKind::ToolCall => tool_call,
        crate::PartKind::ToolResult => tool_result,
        crate::PartKind::Reasoning => reasoning,
    }
}
