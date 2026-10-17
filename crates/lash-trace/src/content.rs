//! Consent, host redaction and terminating bounds shared by every trace consumer.
use serde::{Deserialize, Serialize};

use crate::{
    AttrKey, TraceAttributeWriter, TraceEvent, TraceEventKind, TraceIds, TraceOmission,
    TracePolicySnapshot, TracePosition, TraceSpanKind, TraceValue,
};

mod selector;
mod semconv;
mod truncate;
mod visit;
pub use selector::{CaptureSelector, SelectorView, TraceNarrowing};
pub(crate) use semconv::attributes;
use semconv::{semconv_input, semconv_tools};

/// Whether the host consents to building a category of telemetry content.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Capture {
    #[default]
    Omitted,
    Captured,
}

/// Every content field belongs to one of these categories. Reasoning also
/// requires consent to its message's direction.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ContentCategory {
    SystemInstructions,
    InputMessages,
    OutputMessages,
    Reasoning,
    ToolDefinitions,
    OutputSchema,
    ToolArguments,
    ToolResults,
    Code,
    CodeOutput,
    ProviderPayloads,
    ResponseMetadata,
    DiagnosticsText,
    Extension,
}
impl ContentCategory {
    pub const ALL: [Self; 14] = [
        Self::SystemInstructions,
        Self::InputMessages,
        Self::OutputMessages,
        Self::Reasoning,
        Self::ToolDefinitions,
        Self::OutputSchema,
        Self::ToolArguments,
        Self::ToolResults,
        Self::Code,
        Self::CodeOutput,
        Self::ProviderPayloads,
        Self::ResponseMetadata,
        Self::DiagnosticsText,
        Self::Extension,
    ];
}

/// The node's ceiling: all categories omitted by default. A selector can only narrow it.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
pub struct ContentPolicy {
    categories: [Capture; 14],
}
impl ContentPolicy {
    pub const fn standard() -> Self {
        Self::omitted()
    }
    pub const fn omitted() -> Self {
        Self {
            categories: [Capture::Omitted; 14],
        }
    }
    pub const fn captured() -> Self {
        Self {
            categories: [Capture::Captured; 14],
        }
    }
    pub const fn with(mut self, category: ContentCategory, capture: Capture) -> Self {
        self.categories[category as usize] = capture;
        self
    }
    pub const fn get(&self, category: ContentCategory) -> Capture {
        self.categories[category as usize]
    }
    pub const fn captures(&self, category: ContentCategory) -> bool {
        matches!(self.get(category), Capture::Captured)
    }
    /// Does not evaluate `build` without consent.
    pub fn capture<T: Default>(&self, category: ContentCategory, build: impl FnOnce() -> T) -> T {
        if self.captures(category) {
            build()
        } else {
            T::default()
        }
    }
}

/// A stable address of a content field within its record.
pub type ContentPath = String;
/// How a reader distinguishes omitted, empty, cut and oversized content.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ContentMarks {
    pub captured: Vec<ContentCategory>,
    pub omitted: Vec<ContentPath>,
    pub truncated: Vec<ContentPath>,
    pub oversize: Vec<ContentPath>,
    pub dropped_messages: u64,
    pub hook_failed: bool,
    /// References committed by a normally returning content hook.
    pub references: Vec<(String, TraceValue)>,
}
impl From<ContentPolicy> for ContentMarks {
    fn from(policy: ContentPolicy) -> Self {
        Self {
            captured: ContentCategory::ALL
                .into_iter()
                .filter(|c| policy.captures(*c))
                .collect(),
            ..Self::default()
        }
    }
}

/// A mutable field, visited in record order. Structured values are whole JSON;
/// replacing one with an invalid typed value omits it rather than leaking the original.
pub struct ContentField<'a> {
    category: ContentCategory,
    path: ContentPath,
    value: FieldValue<'a>,
    omitted: bool,
}
impl ContentField<'_> {
    pub fn category(&self) -> ContentCategory {
        self.category
    }
    pub fn path(&self) -> &ContentPath {
        &self.path
    }
    pub fn value(&mut self) -> ContentValue<'_> {
        match &mut self.value {
            FieldValue::Text(v) => ContentValue::Text(v),
            FieldValue::Json(v) => ContentValue::Json(v.value()),
        }
    }
    pub fn omit(&mut self) {
        self.omitted = true;
        match &mut self.value {
            FieldValue::Text(v) => v.clear(),
            FieldValue::Json(v) => v.omit(),
        }
    }
}
enum FieldValue<'a> {
    Text(&'a mut String),
    Json(&'a mut dyn JsonField),
}
trait JsonField {
    fn value(&mut self) -> &mut serde_json::Value;
    fn omit(&mut self);
}
/// JSON is the facade's declared JSON contract exception.
pub enum ContentValue<'a> {
    Text(&'a mut String),
    Json(&'a mut serde_json::Value),
}

/// One synchronous host hook. It must not block, do I/O or re-enter the engine.
pub trait ContentHook: Send + Sync + 'static {
    fn on_content(&self, view: &ContentView<'_>, content: &mut RecordContent<'_>);
}
pub struct ContentView<'a> {
    pub kind: TraceSpanKind,
    pub position: Option<&'a TracePosition>,
    pub ids: &'a TraceIds,
    pub event_kind: TraceEventKind,
    /// The called tool on tool opener/closer records; absent for other kinds.
    pub tool_name: Option<&'a str>,
    pub policy: &'a TracePolicySnapshot,
}
/// A transaction over a record's captured fields and bounded references.
pub struct RecordContent<'a> {
    event: &'a mut TraceEvent,
    marks: &'a mut ContentMarks,
    policy: ContentPolicy,
    writer: TraceAttributeWriter,
    references: u32,
    reference_bytes: u64,
    limits: crate::TraceLimits,
}
impl RecordContent<'_> {
    pub fn for_each(&mut self, mut f: impl FnMut(&mut ContentField<'_>)) {
        let policy = self.policy;
        let marks = &mut self.marks;
        self.event.visit_content(&mut |field| {
            if policy.captures(field.category()) {
                f(field);
                if field.omitted {
                    marks.omitted.push(field.path.clone());
                }
            }
        });
    }
    pub fn omit(&mut self, category: ContentCategory) {
        self.policy = self.policy.with(category, Capture::Omitted);
        self.marks.captured.retain(|c| *c != category);
        self.event.apply_policy(&self.policy, self.marks);
    }
    pub fn reference(&mut self, key: AttrKey, value: TraceValue) -> Result<(), TraceOmission> {
        let bytes = key.as_str().len() as u64 + value.encoded_len();
        let result = if self.references >= self.limits.references() {
            Err(TraceOmission::References)
        } else if bytes
            > self
                .limits
                .reference_bytes()
                .saturating_sub(self.reference_bytes)
        {
            Err(TraceOmission::ReferenceBytes)
        } else {
            return self.write_reference(key, value, bytes);
        };
        if let Err(reason) = result {
            self.writer.omit(reason);
        }
        result
    }
    fn write_reference(
        &mut self,
        key: AttrKey,
        value: TraceValue,
        bytes: u64,
    ) -> Result<(), TraceOmission> {
        let result = self.writer.set(key, value);
        if result.is_ok() || result == Err(TraceOmission::ValueTruncated) {
            self.references += 1;
            self.reference_bytes += bytes;
        }
        result
    }
    pub fn system_instructions(&mut self) -> Option<&mut String> {
        if !self.policy.captures(ContentCategory::SystemInstructions) {
            return None;
        }
        self.event.request_mut()?.system_instructions.as_mut()
    }
    pub fn messages(&mut self) -> Option<&mut Vec<crate::TraceLlmMessage>> {
        if !self.policy.captures(ContentCategory::InputMessages) {
            return None;
        }
        Some(&mut self.event.request_mut()?.messages)
    }
    pub fn output_parts(
        &mut self,
    ) -> Option<&mut Vec<lash_protocol_types::llm::types::LlmOutputPart>> {
        if !self.policy.captures(ContentCategory::OutputMessages) {
            return None;
        }
        match self.event {
            TraceEvent::LlmCallCompleted { response, .. } => response.parts.as_mut(),
            _ => None,
        }
    }
}

impl crate::TraceRecord {
    /// Applies consent before any consumer and bounds captured values. The host
    /// hook is transactional: a panic clears its mutations and references. The
    /// caller enforces the runtime's panic loudness after receiving the payload.
    pub fn apply_content(
        &mut self,
        policy: &TracePolicySnapshot,
        hook: Option<&dyn ContentHook>,
    ) -> Option<Box<dyn std::any::Any + Send>> {
        self.content = ContentMarks::default();
        self.event.apply_policy(&policy.content, &mut self.content);
        self.event.visit_content(&mut |field| {
            if policy.content.captures(field.category())
                && !self.content.captured.contains(&field.category())
            {
                self.content.captured.push(field.category());
            }
        });
        let mut panic = None;
        if !self.content.captured.is_empty()
            && let Some(hook) = hook
        {
            let ids = self.context.ids();
            let tool_name = match &self.event {
                TraceEvent::ToolCallStarted { name, .. } => Some(name.clone()),
                TraceEvent::ToolCallCompleted { name, .. } => name.name().map(str::to_owned),
                _ => None,
            };
            let view = ContentView {
                tool_name: tool_name.as_deref(),
                kind: self
                    .position
                    .as_ref()
                    .map_or(TraceSpanKind::Point, |p| p.kind),
                position: self.position.as_ref(),
                ids: &ids,
                event_kind: self.event.kind(),
                policy,
            };
            let mut content = RecordContent {
                event: &mut self.event,
                marks: &mut self.content,
                policy: policy.content,
                writer: TraceAttributeWriter::new(view.kind, &policy.limits),
                references: 0,
                reference_bytes: 0,
                limits: policy.limits,
            };
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                hook.on_content(&view, &mut content)
            }));
            let (contributions, omissions) = content.writer.finish();
            self.omissions.merge(&omissions);
            match result {
                Ok(()) => {
                    self.content.references = contributions
                        .added
                        .into_iter()
                        .map(|(key, value)| (key.as_str().to_owned(), value))
                        .collect();
                    self.event.apply_policy(&policy.content, &mut self.content);
                }
                Err(payload) => {
                    self.event
                        .apply_policy(&ContentPolicy::omitted(), &mut self.content);
                    self.content.captured.clear();
                    self.content.references.clear();
                    self.content.hook_failed = true;
                    self.omissions.add(TraceOmission::HookFailed);
                    panic = Some(payload);
                }
            }
        }
        self.event
            .truncate_content(policy.content_limits, &mut self.content);
        panic
    }
}

#[cfg(test)]
pub(crate) mod laws;
