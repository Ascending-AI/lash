use std::collections::BTreeMap;

use serde::de::{Error as DeError, MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Number, Value};

use crate::llm::types::AttachmentSource;

const TAG_KEY: &str = "$lash_tool_value";
const ATTACHMENT_TAG: &str = "attachment";
const OBJECT_TAG: &str = "object";
const SOURCE_KEY: &str = "source";
const ENTRIES_KEY: &str = "entries";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCallOutput {
    pub outcome: ToolCallOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control: Option<ToolControl>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCallRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    pub tool: String,
    pub args: Value,
    pub output: ToolCallOutput,
    pub duration_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolIntentKind {
    StartProcess,
    SignalProcess,
    CancelProcess,
    EmitProcessEvent,
}

impl ToolIntentKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::StartProcess => "start_process",
            Self::SignalProcess => "signal_process",
            Self::CancelProcess => "cancel_process",
            Self::EmitProcessEvent => "emit_process_event",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessParentEndPolicy {
    Abandon,
    #[default]
    Cancel,
}

/// Recorded teardown metadata for a successfully started child process.
///
/// The durable tool-batch outcome carries this value so parent-end handling
/// can be reconstructed after a crash without consulting live side state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolIntentParentEnd {
    pub process_id: String,
    pub policy: ProcessParentEndPolicy,
}

/// Compact durable teardown action reconstructed from one recorded start intent.
///
/// Unlike [`ToolIntentExecutionOutcome`], this value omits the child start's
/// result payload. Process lifecycle state only needs the stable identity and
/// declared parent-end policy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolIntentParentEndAction {
    pub identity: ToolIntentIdentity,
    pub parent_end: ToolIntentParentEnd,
}

/// Durable result of applying one recorded start intent's parent-end policy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ToolIntentParentEndOutcome {
    Abandoned {
        identity: ToolIntentIdentity,
        process_id: String,
    },
    Cancelled {
        identity: ToolIntentIdentity,
        process_id: String,
    },
    Refused {
        identity: ToolIntentIdentity,
        process_id: String,
        code: String,
        message: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolIntentIdentity {
    pub session_id: String,
    /// The enclosing execution-scope id: a turn id for turn scope and a
    /// process id for process scope.
    pub execution_scope_id: String,
    pub tool_call_id: String,
    pub intent_index: u32,
    pub replay_key: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum ToolIntentRefusalReason {
    UnsupportedProtocolVersion {
        recorded: u16,
    },
    MissingToolCallId,
    IntentIndexOverflow,
    CountBudgetExceeded {
        actual: usize,
        maximum: usize,
    },
    CanonicalByteBudgetExceeded {
        actual: usize,
        maximum: usize,
    },
    PerKindBudgetExceeded {
        kind: ToolIntentKind,
        actual: usize,
        maximum: usize,
    },
    SessionMismatch {
        expected: String,
        recorded: String,
    },
    CommandFailed {
        code: String,
        message: String,
    },
}

impl ToolIntentRefusalReason {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::UnsupportedProtocolVersion { .. } => "unsupported_protocol_version",
            Self::MissingToolCallId => "missing_tool_call_id",
            Self::IntentIndexOverflow => "intent_index_overflow",
            Self::CountBudgetExceeded { .. } => "count_budget_exceeded",
            Self::CanonicalByteBudgetExceeded { .. } => "canonical_byte_budget_exceeded",
            Self::PerKindBudgetExceeded { .. } => "per_kind_budget_exceeded",
            Self::SessionMismatch { .. } => "session_mismatch",
            Self::CommandFailed { .. } => "command_failed",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ToolIntentExecutionOutcome {
    Executed {
        identity: ToolIntentIdentity,
        kind: ToolIntentKind,
        result: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_end: Option<ToolIntentParentEnd>,
    },
    Refused {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        identity: Option<ToolIntentIdentity>,
        intent_index: u32,
        kind: ToolIntentKind,
        refusal: ToolIntentRefusalReason,
    },
    /// Batch-level protocol refusal used when the recorded batch contains no
    /// declarations to which the refusal could honestly be attached.
    ProtocolRefused { refusal: ToolIntentRefusalReason },
}

impl ToolIntentExecutionOutcome {
    pub fn kind(&self) -> Option<ToolIntentKind> {
        match self {
            Self::Executed { kind, .. } | Self::Refused { kind, .. } => Some(*kind),
            Self::ProtocolRefused { .. } => None,
        }
    }

    pub fn model_addendum(&self) -> String {
        match self {
            Self::Executed {
                identity,
                kind,
                result,
                ..
            } => format!(
                "[tool intent {} #{} executed: {}]",
                kind.as_str(),
                identity.intent_index,
                result
            ),
            Self::Refused {
                intent_index,
                kind,
                refusal,
                ..
            } => format!(
                "[tool intent {} #{} refused: {}]",
                kind.as_str(),
                intent_index,
                refusal.code()
            ),
            Self::ProtocolRefused { refusal } => {
                format!("[tool intent batch refused: {}]", refusal.code())
            }
        }
    }
}

impl ToolCallOutput {
    pub fn success(value: impl Into<ToolValue>) -> Self {
        Self {
            outcome: ToolCallOutcome::Success(value.into()),
            control: None,
        }
    }

    pub fn failure(failure: ToolFailure) -> Self {
        Self {
            outcome: ToolCallOutcome::Failure(failure),
            control: None,
        }
    }

    pub fn cancelled(cancellation: ToolCancellation) -> Self {
        Self {
            outcome: ToolCallOutcome::Cancelled(cancellation),
            control: None,
        }
    }

    pub fn with_control(mut self, control: ToolControl) -> Self {
        self.control = Some(control);
        self
    }

    pub fn is_success(&self) -> bool {
        matches!(self.outcome, ToolCallOutcome::Success(_))
    }

    pub fn status(&self) -> ToolCallStatus {
        match self.outcome {
            ToolCallOutcome::Success(_) => ToolCallStatus::Success,
            ToolCallOutcome::Failure(_) => ToolCallStatus::Failure,
            ToolCallOutcome::Cancelled(_) => ToolCallStatus::Cancelled,
        }
    }

    pub fn value_for_projection(&self) -> Value {
        match &self.outcome {
            ToolCallOutcome::Success(value) => value.to_json_value(),
            ToolCallOutcome::Failure(failure) => failure.to_json_value(),
            ToolCallOutcome::Cancelled(cancellation) => cancellation.to_json_value(),
        }
    }

    pub fn into_value_for_projection(self) -> Value {
        match self.outcome {
            ToolCallOutcome::Success(value) => value.into_json_value(),
            ToolCallOutcome::Failure(failure) => failure.to_json_value(),
            ToolCallOutcome::Cancelled(cancellation) => cancellation.to_json_value(),
        }
    }

    pub fn attachments(&self) -> Vec<AttachmentSource> {
        match &self.outcome {
            ToolCallOutcome::Success(value) => value.attachments(),
            ToolCallOutcome::Failure(failure) => failure
                .raw
                .as_ref()
                .map(ToolValue::attachments)
                .unwrap_or_default(),
            ToolCallOutcome::Cancelled(cancellation) => cancellation
                .raw
                .as_ref()
                .map(ToolValue::attachments)
                .unwrap_or_default(),
        }
    }

    pub fn replace_attachment_source(
        &mut self,
        previous: &AttachmentSource,
        replacement: &AttachmentSource,
    ) {
        match &mut self.outcome {
            ToolCallOutcome::Success(value) => {
                value.replace_attachment_source(previous, replacement)
            }
            ToolCallOutcome::Failure(failure) => {
                if let Some(raw) = failure.raw.as_mut() {
                    raw.replace_attachment_source(previous, replacement);
                }
            }
            ToolCallOutcome::Cancelled(cancellation) => {
                if let Some(raw) = cancellation.raw.as_mut() {
                    raw.replace_attachment_source(previous, replacement);
                }
            }
        }
    }
}

pub fn format_tool_output_content(output: &ToolCallOutput) -> String {
    match &output.outcome {
        ToolCallOutcome::Success(value) => {
            let value = value.to_json_value();
            match value {
                Value::String(text) => text,
                other => serde_json::to_string(&other).unwrap_or_else(|_| "null".to_string()),
            }
        }
        ToolCallOutcome::Failure(failure) => format_failure_message(failure),
        ToolCallOutcome::Cancelled(cancellation) => format_cancellation_message(cancellation),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    Success,
    Failure,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", content = "payload", rename_all = "snake_case")]
pub enum ToolCallOutcome {
    Success(ToolValue),
    Failure(ToolFailure),
    Cancelled(ToolCancellation),
}

#[derive(Clone, Debug, PartialEq)]
pub enum ToolValue {
    Null,
    Bool(bool),
    Number(Number),
    String(String),
    Array(Vec<ToolValue>),
    Object(BTreeMap<String, ToolValue>),
    Attachment(AttachmentSource),
}

impl ToolValue {
    pub fn to_json_value(&self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Bool(value) => Value::Bool(*value),
            Self::Number(value) => Value::Number(value.clone()),
            Self::String(value) => Value::String(value.clone()),
            Self::Array(values) => Value::Array(values.iter().map(Self::to_json_value).collect()),
            Self::Attachment(source) => tagged_attachment_json(source),
            Self::Object(entries) => object_tool_value_to_json(entries),
        }
    }

    pub(crate) fn into_json_value(self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Bool(value) => Value::Bool(value),
            Self::Number(value) => Value::Number(value),
            Self::String(value) => Value::String(value),
            Self::Array(values) => {
                Value::Array(values.into_iter().map(Self::into_json_value).collect())
            }
            Self::Attachment(source) => tagged_attachment_json(&source),
            Self::Object(entries) => object_tool_value_into_json(entries),
        }
    }

    pub(crate) fn from_json_value(value: Value) -> serde_json::Result<Self> {
        serde_json::from_value(value)
    }

    pub fn attachments(&self) -> Vec<AttachmentSource> {
        let mut attachments = Vec::new();
        self.collect_attachments(&mut attachments);
        attachments
    }

    pub(crate) fn model_parts(&self) -> Vec<ModelToolReturnPart> {
        let mut parts = Vec::new();
        match self {
            Self::String(text) => push_text_part(&mut parts, text.clone()),
            Self::Attachment(reference) => {
                parts.push(ModelToolReturnPart::Attachment(reference.clone()))
            }
            Self::Null | Self::Bool(_) | Self::Number(_) | Self::Array(_) | Self::Object(_) => {
                self.push_compact_model_parts(&mut parts);
            }
        }
        parts
    }

    fn collect_attachments(&self, attachments: &mut Vec<AttachmentSource>) {
        match self {
            Self::Attachment(reference) => attachments.push(reference.clone()),
            Self::Array(values) => {
                for value in values {
                    value.collect_attachments(attachments);
                }
            }
            Self::Object(entries) => {
                for value in entries.values() {
                    value.collect_attachments(attachments);
                }
            }
            Self::Null | Self::Bool(_) | Self::Number(_) | Self::String(_) => {}
        }
    }

    fn replace_attachment_source(
        &mut self,
        previous: &AttachmentSource,
        replacement: &AttachmentSource,
    ) {
        match self {
            Self::Attachment(source) if source == previous => *source = replacement.clone(),
            Self::Array(values) => {
                for value in values {
                    value.replace_attachment_source(previous, replacement);
                }
            }
            Self::Object(entries) => {
                for value in entries.values_mut() {
                    value.replace_attachment_source(previous, replacement);
                }
            }
            Self::Null
            | Self::Bool(_)
            | Self::Number(_)
            | Self::String(_)
            | Self::Attachment(_) => {}
        }
    }

    fn push_compact_model_parts(&self, parts: &mut Vec<ModelToolReturnPart>) {
        match self {
            Self::Null => push_text_part(parts, "null"),
            Self::Bool(value) => push_text_part(parts, value.to_string()),
            Self::Number(value) => push_text_part(parts, value.to_string()),
            Self::String(value) => push_text_part(
                parts,
                serde_json::to_string(value).unwrap_or_else(|_| "\"\"".into()),
            ),
            Self::Attachment(reference) => {
                parts.push(ModelToolReturnPart::Attachment(reference.clone()))
            }
            Self::Array(values) => {
                push_text_part(parts, "[");
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        push_text_part(parts, ",");
                    }
                    value.push_compact_model_parts(parts);
                }
                push_text_part(parts, "]");
            }
            Self::Object(entries) => {
                push_text_part(parts, "{");
                for (index, (key, value)) in entries.iter().enumerate() {
                    if index > 0 {
                        push_text_part(parts, ",");
                    }
                    push_text_part(
                        parts,
                        serde_json::to_string(key).unwrap_or_else(|_| "\"\"".into()),
                    );
                    push_text_part(parts, ":");
                    value.push_compact_model_parts(parts);
                }
                push_text_part(parts, "}");
            }
        }
    }
}

fn tagged_attachment_json(source: &AttachmentSource) -> Value {
    let mut map = Map::with_capacity(2);
    map.insert(
        TAG_KEY.to_string(),
        Value::String(ATTACHMENT_TAG.to_string()),
    );
    map.insert(
        SOURCE_KEY.to_string(),
        serde_json::to_value(source).unwrap_or(Value::Null),
    );
    Value::Object(map)
}

fn object_tool_value_to_json(entries: &BTreeMap<String, ToolValue>) -> Value {
    let object = entries
        .iter()
        .map(|(key, value)| (key.clone(), value.to_json_value()))
        .collect::<Map<_, _>>();
    if entries.contains_key(TAG_KEY) {
        escaped_object_tool_value_json(Value::Object(object))
    } else {
        Value::Object(object)
    }
}

fn object_tool_value_into_json(entries: BTreeMap<String, ToolValue>) -> Value {
    let contains_reserved_tag = entries.contains_key(TAG_KEY);
    let object = entries
        .into_iter()
        .map(|(key, value)| (key, value.into_json_value()))
        .collect::<Map<_, _>>();
    if contains_reserved_tag {
        escaped_object_tool_value_json(Value::Object(object))
    } else {
        Value::Object(object)
    }
}

fn escaped_object_tool_value_json(entries: Value) -> Value {
    let mut map = Map::with_capacity(2);
    map.insert(TAG_KEY.to_string(), Value::String(OBJECT_TAG.to_string()));
    map.insert(ENTRIES_KEY.to_string(), entries);
    Value::Object(map)
}

impl From<Value> for ToolValue {
    fn from(value: Value) -> Self {
        match value {
            Value::Null => Self::Null,
            Value::Bool(value) => Self::Bool(value),
            Value::Number(value) => Self::Number(value),
            Value::String(value) => Self::String(value),
            Value::Array(values) => Self::Array(values.into_iter().map(Self::from).collect()),
            Value::Object(values) => Self::Object(
                values
                    .into_iter()
                    .map(|(key, value)| (key, Self::from(value)))
                    .collect(),
            ),
        }
    }
}

impl From<&str> for ToolValue {
    fn from(value: &str) -> Self {
        Self::String(value.to_string())
    }
}

impl From<String> for ToolValue {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

impl Serialize for ToolValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Null => serializer.serialize_none(),
            Self::Bool(value) => serializer.serialize_bool(*value),
            Self::Number(value) => value.serialize(serializer),
            Self::String(value) => serializer.serialize_str(value),
            Self::Array(values) => values.serialize(serializer),
            Self::Attachment(source) => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry(TAG_KEY, ATTACHMENT_TAG)?;
                map.serialize_entry(SOURCE_KEY, source)?;
                map.end()
            }
            Self::Object(entries) => {
                if entries.contains_key(TAG_KEY) {
                    let mut map = serializer.serialize_map(Some(2))?;
                    map.serialize_entry(TAG_KEY, OBJECT_TAG)?;
                    map.serialize_entry(ENTRIES_KEY, entries)?;
                    map.end()
                } else {
                    entries.serialize(serializer)
                }
            }
        }
    }
}

impl<'de> Deserialize<'de> for ToolValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ToolValueVisitor;

        impl<'de> Visitor<'de> for ToolValueVisitor {
            type Value = ToolValue;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a Lash tool value")
            }

            fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
                Ok(ToolValue::Bool(value))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
                Ok(ToolValue::Number(Number::from(value)))
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
                Ok(ToolValue::Number(Number::from(value)))
            }

            fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
            where
                E: DeError,
            {
                Number::from_f64(value)
                    .map(ToolValue::Number)
                    .ok_or_else(|| E::custom("non-finite number is not a valid tool value"))
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
                Ok(ToolValue::String(value.to_string()))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
                Ok(ToolValue::String(value))
            }

            fn visit_none<E>(self) -> Result<Self::Value, E> {
                Ok(ToolValue::Null)
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(ToolValue::Null)
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut values = Vec::new();
                while let Some(value) = seq.next_element()? {
                    values.push(value);
                }
                Ok(ToolValue::Array(values))
            }

            fn visit_map<A>(self, mut access: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut map = Map::new();
                while let Some((key, value)) = access.next_entry::<String, Value>()? {
                    map.insert(key, value);
                }
                decode_object(map).map_err(A::Error::custom)
            }
        }

        deserializer.deserialize_any(ToolValueVisitor)
    }
}

fn decode_object(mut map: Map<String, Value>) -> serde_json::Result<ToolValue> {
    let Some(tag) = map.get(TAG_KEY) else {
        return Ok(ToolValue::Object(
            map.into_iter()
                .map(|(key, value)| Ok((key, ToolValue::from_json_value(value)?)))
                .collect::<serde_json::Result<_>>()?,
        ));
    };
    let tag = tag
        .as_str()
        .ok_or_else(|| serde_json::Error::custom("reserved tool value tag must be a string"))?;
    match tag {
        ATTACHMENT_TAG => {
            if map.len() != 2 || !map.contains_key(SOURCE_KEY) {
                return Err(serde_json::Error::custom("malformed attachment tool value"));
            }
            let source = serde_json::from_value(
                map.remove(SOURCE_KEY)
                    .ok_or_else(|| serde_json::Error::custom("missing attachment source"))?,
            )?;
            Ok(ToolValue::Attachment(source))
        }
        OBJECT_TAG => {
            if map.len() != 2 || !map.contains_key(ENTRIES_KEY) {
                return Err(serde_json::Error::custom(
                    "malformed escaped object tool value",
                ));
            }
            serde_json::from_value(
                map.remove(ENTRIES_KEY)
                    .ok_or_else(|| serde_json::Error::custom("missing escaped object entries"))?,
            )
            .map(ToolValue::Object)
        }
        other => Err(serde_json::Error::custom(format!(
            "unknown reserved tool value tag `{other}`"
        ))),
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolFailure {
    pub class: ToolFailureClass,
    pub code: String,
    pub message: String,
    pub source: ToolFailureSource,
    pub retry: ToolRetryDisposition,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<ToolValue>,
}

impl ToolFailure {
    pub(crate) fn new(
        class: ToolFailureClass,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            class,
            code: code.into(),
            message: message.into(),
            source: ToolFailureSource::Runtime,
            retry: ToolRetryDisposition::Never,
            raw: None,
        }
    }

    pub fn runtime(
        class: ToolFailureClass,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self::new(class, code, message)
    }

    pub fn tool(
        class: ToolFailureClass,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            source: ToolFailureSource::Tool,
            ..Self::new(class, code, message)
        }
    }

    /// Constructs a non-retryable invalid-request failure reported by a tool.
    pub fn invalid_request(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::tool(ToolFailureClass::InvalidRequest, code, message)
    }

    /// Constructs a non-retryable filesystem or transport I/O failure reported by a tool.
    pub fn io(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::tool(ToolFailureClass::Io, code, message)
    }

    pub fn safe_retry(
        class: ToolFailureClass,
        code: impl Into<String>,
        message: impl Into<String>,
        after_ms: Option<u64>,
    ) -> Self {
        let mut failure = Self::tool(class, code, message);
        failure.retry = ToolRetryDisposition::Safe { after_ms };
        failure
    }

    pub fn to_json_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or_else(|_| Value::String(self.message.clone()))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolFailureClass {
    InvalidRequest,
    Io,
    Unavailable,
    PermissionDenied,
    Timeout,
    Execution,
    External,
    ResourceLimit,
    Internal,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolFailureSource {
    Runtime,
    Tool,
    Plugin,
    Policy,
    Cancellation,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolRetryDisposition {
    Never,
    Safe {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after_ms: Option<u64>,
    },
    Exhausted {
        attempts: u32,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCancellation {
    pub message: String,
    pub source: ToolFailureSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<ToolValue>,
}

impl ToolCancellation {
    pub fn runtime(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            source: ToolFailureSource::Cancellation,
            raw: None,
        }
    }

    pub fn to_json_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or_else(|_| Value::String(self.message.clone()))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolControl {
    SwitchAgentFrame {
        frame_key: crate::FrameKey,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        initial_nodes: Vec<crate::SessionAppendNode>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task: Option<String>,
    },
    Finish {
        value: ToolValue,
    },
    Fail {
        failure: ToolFailure,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelToolReturn {
    pub call_id: String,
    pub tool_name: String,
    pub parts: Vec<ModelToolReturnPart>,
}

impl ModelToolReturn {
    pub fn from_output(call_id: String, tool_name: String, output: &ToolCallOutput) -> Self {
        let parts = model_parts_from_tool_output(output);
        Self {
            call_id,
            tool_name,
            parts,
        }
    }

    pub(crate) fn text(call_id: String, tool_name: String, content: impl Into<String>) -> Self {
        Self {
            call_id,
            tool_name,
            parts: vec![ModelToolReturnPart::text(content)],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelToolReturnPart {
    Text { text: String },
    Attachment(AttachmentSource),
}

impl ModelToolReturnPart {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }
}

pub fn model_parts_from_tool_output(output: &ToolCallOutput) -> Vec<ModelToolReturnPart> {
    match &output.outcome {
        ToolCallOutcome::Success(value) => value.model_parts(),
        ToolCallOutcome::Failure(failure) => {
            let mut parts = vec![ModelToolReturnPart::text(format_failure_message(failure))];
            if let Some(raw) = &failure.raw {
                parts.extend(
                    raw.attachments()
                        .into_iter()
                        .map(ModelToolReturnPart::Attachment),
                );
            }
            parts
        }
        ToolCallOutcome::Cancelled(cancellation) => {
            let mut parts = vec![ModelToolReturnPart::text(format_cancellation_message(
                cancellation,
            ))];
            if let Some(raw) = &cancellation.raw {
                parts.extend(
                    raw.attachments()
                        .into_iter()
                        .map(ModelToolReturnPart::Attachment),
                );
            }
            parts
        }
    }
}

fn push_text_part(parts: &mut Vec<ModelToolReturnPart>, text: impl Into<String>) {
    let text = text.into();
    if text.is_empty() {
        return;
    }
    if let Some(ModelToolReturnPart::Text { text: existing }) = parts.last_mut() {
        existing.push_str(&text);
    } else {
        parts.push(ModelToolReturnPart::text(text));
    }
}

fn format_failure_message(failure: &ToolFailure) -> String {
    if failure.message.is_empty() {
        "[Tool execution failed]".to_string()
    } else {
        format!("[Tool execution failed]\n{}", failure.message)
    }
}

fn format_cancellation_message(cancellation: &ToolCancellation) -> String {
    if cancellation.message.is_empty() {
        "[Tool execution cancelled]".to_string()
    } else {
        format!("[Tool execution cancelled]\n{}", cancellation.message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AttachmentId, AttachmentMeta, AttachmentTypeMetadata, MediaType};

    fn attachment_source(id: &str) -> AttachmentSource {
        AttachmentSource::stored(
            AttachmentMeta::new(
                AttachmentId::new(id),
                MediaType::parse("image/png").unwrap(),
                3,
                Some(AttachmentTypeMetadata::image(Some(1), Some(1))),
                Some("tiny".to_string()),
            )
            .as_ref(),
        )
    }

    #[test]
    fn tool_value_serializes_nested_attachments() {
        let value = ToolValue::Array(vec![ToolValue::Attachment(attachment_source("img"))]);

        let json = serde_json::to_value(&value).unwrap();

        assert_eq!(json[0][TAG_KEY], ATTACHMENT_TAG);
        assert_eq!(json[0][SOURCE_KEY]["attachment_ref"]["id"], "img");
        assert_eq!(serde_json::from_value::<ToolValue>(json).unwrap(), value);
    }

    #[test]
    fn tool_value_escapes_user_reserved_key() {
        let value = ToolValue::Object(BTreeMap::from([(
            TAG_KEY.to_string(),
            ToolValue::String("user".into()),
        )]));

        let json = serde_json::to_value(&value).unwrap();

        assert_eq!(json[TAG_KEY], OBJECT_TAG);
        assert!(json[ENTRIES_KEY].is_object());
        assert_eq!(serde_json::from_value::<ToolValue>(json).unwrap(), value);
    }

    #[test]
    fn consuming_projection_matches_tool_value_serialization() {
        let value = ToolValue::Object(BTreeMap::from([
            (
                "attachment".to_string(),
                ToolValue::Attachment(attachment_source("img")),
            ),
            (
                TAG_KEY.to_string(),
                ToolValue::Array(vec![ToolValue::String("user".into())]),
            ),
        ]));
        let serialized = serde_json::to_value(&value).unwrap();
        assert_eq!(value.to_json_value(), serialized);

        let output = ToolCallOutput::success(value);
        assert_eq!(output.into_value_for_projection(), serialized);
    }

    #[test]
    fn tool_value_rejects_malformed_reserved_object() {
        let json = serde_json::json!({ TAG_KEY: ATTACHMENT_TAG, "extra": true });

        assert!(serde_json::from_value::<ToolValue>(json).is_err());
    }

    #[test]
    fn tool_value_model_parts_preserve_attachment_position() {
        let value = ToolValue::Array(vec![
            ToolValue::String("before".into()),
            ToolValue::Attachment(attachment_source("img")),
            ToolValue::String("after".into()),
        ]);

        assert_eq!(
            value.model_parts(),
            vec![
                ModelToolReturnPart::text("[\"before\","),
                ModelToolReturnPart::Attachment(attachment_source("img")),
                ModelToolReturnPart::text(",\"after\"]"),
            ]
        );
    }

    #[test]
    fn tool_output_failure_projects_raw_attachments_after_failure_text() {
        let attachment = attachment_source("img");
        let output = ToolCallOutput::failure(ToolFailure {
            class: ToolFailureClass::Execution,
            code: "boom".into(),
            message: "boom".into(),
            source: ToolFailureSource::Tool,
            retry: ToolRetryDisposition::Never,
            raw: Some(ToolValue::Object(BTreeMap::from([(
                "image".into(),
                ToolValue::Attachment(attachment.clone()),
            )]))),
        });

        assert_eq!(
            model_parts_from_tool_output(&output),
            vec![
                ModelToolReturnPart::text("[Tool execution failed]\nboom"),
                ModelToolReturnPart::Attachment(attachment),
            ]
        );
    }

    #[test]
    fn model_tool_return_text_part_serializes() {
        let part = ModelToolReturnPart::text("hello");

        let json = serde_json::to_value(&part).unwrap();

        assert_eq!(json, serde_json::json!({ "type": "text", "text": "hello" }));
        assert_eq!(
            serde_json::from_value::<ModelToolReturnPart>(json).unwrap(),
            part
        );
    }

    #[test]
    fn tool_output_status_distinguishes_cancelled_from_failure() {
        let failure = ToolCallOutput::failure(ToolFailure::tool(
            ToolFailureClass::Execution,
            "boom",
            "boom",
        ));
        let cancelled = ToolCallOutput::cancelled(ToolCancellation::runtime("stopped"));

        assert_eq!(failure.status(), ToolCallStatus::Failure);
        assert_eq!(cancelled.status(), ToolCallStatus::Cancelled);
        assert!(!cancelled.is_success());
    }

    #[test]
    fn typed_tool_failure_constructors_set_class_code_source_and_retry() {
        let invalid = ToolFailure::invalid_request("invalid_glob", "bad pattern");
        assert_eq!(invalid.class, ToolFailureClass::InvalidRequest);
        assert_eq!(invalid.code, "invalid_glob");
        assert_eq!(invalid.source, ToolFailureSource::Tool);
        assert_eq!(invalid.retry, ToolRetryDisposition::Never);

        let io = ToolFailure::io("read_failed", "could not read file");
        assert_eq!(io.class, ToolFailureClass::Io);
        assert_eq!(io.code, "read_failed");
        assert_eq!(io.source, ToolFailureSource::Tool);
        assert_eq!(io.retry, ToolRetryDisposition::Never);
    }
}
