use std::collections::BTreeMap;

use serde::de::{Error as DeError, MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Number, Value};

use crate::{AttachmentRef, Durable};

const TAG_KEY: &str = "$lash_tool_value";
const ATTACHMENT_TAG: &str = "attachment";
const UNTRUSTED_JSON_TAG: &str = "untrusted_json";
const REFERENCE_KEY: &str = "reference";
const VALUE_KEY: &str = "value";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(remote = "Self")]
pub struct ToolCallOutput {
    #[serde(serialize_with = "crate::durable_serde::serialize")]
    pub outcome: ToolCallOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(serialize_with = "crate::durable_serde::serialize")]
    pub control: Option<ToolControl>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<ToolView>,
    /// Optional display projection for a successful result; `outcome` remains the code value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projection_value: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ToolView {
    pub blocks: Vec<ToolViewBlock>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ToolViewMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<String>,
}

impl ToolViewMeta {
    pub fn is_empty(&self) -> bool {
        self.priority.is_none() && self.last_modified.is_none()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolViewBlock {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "ToolViewMeta::is_empty")]
        meta: ToolViewMeta,
    },
    Attachment {
        reference: AttachmentRef,
        #[serde(default, skip_serializing_if = "ToolViewMeta::is_empty")]
        meta: ToolViewMeta,
    },
    ResourceLink {
        uri: String,
        name: String,
        title: Option<String>,
        description: Option<String>,
        mime_type: Option<String>,
        #[serde(default, skip_serializing_if = "ToolViewMeta::is_empty")]
        meta: ToolViewMeta,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(remote = "Self")]
pub struct ToolCallRecord {
    /// Lash's identity for the call (ADR 0117).
    pub call_id: crate::ToolCallId,
    /// The model provider's id for the call, when a model issued it:
    /// correlation only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_call_id: Option<String>,
    pub tool: String,
    pub args: Value,
    #[serde(serialize_with = "crate::durable_serde::serialize")]
    pub output: ToolCallOutput,
}

/// Generates the canonical tool-intent kind set from the vocabulary in
/// [`crate::tool_intents`].

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ToolIntentIdentity {
    /// The runtime whose authority declared the intent: a session, or a
    /// process runtime.
    pub owner: lash_vocabulary::identity::RuntimeOwner,
    /// The enclosing execution-scope id: a turn id for turn scope and a
    /// process id for process scope.
    pub execution_scope_id: String,
    pub tool_call_id: crate::ToolCallId,
    pub intent_index: u32,
    pub replay_key: String,
    /// Replay key of the durable invocation that minted this declaration,
    /// retained so `replay_key` stays re-derivable from the record alone.
    /// `None` for host-submitted intents, whose identity binds no emission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minting_emission_replay_key: Option<String>,
}

impl ToolCallOutput {
    pub fn success(value: impl Into<Value>) -> Self {
        Self::success_tool_value(ToolValue::untrusted_json(value.into()))
    }

    pub fn success_tool_value(value: ToolValue) -> Self {
        Self {
            outcome: ToolCallOutcome::Success(value),
            control: None,
            view: None,
            projection_value: None,
        }
    }

    pub fn failure(failure: ToolFailure) -> Self {
        Self {
            outcome: ToolCallOutcome::Failure(failure),
            control: None,
            view: None,
            projection_value: None,
        }
    }

    pub fn cancelled(cancellation: ToolCancellation) -> Self {
        Self {
            outcome: ToolCallOutcome::Cancelled(cancellation),
            control: None,
            view: None,
            projection_value: None,
        }
    }

    /// A control call's result: the control alone, with no output.
    pub fn turn_control(control: TurnControl) -> Self {
        Self::success_tool_value(ToolValue::untrusted_json(Value::Null))
            .with_control(ToolControl::Turn { control })
    }

    /// End the caller's turn with `value`. The tool must declare
    /// [`TurnControlKind::Finish`](lash_vocabulary::tool_declaration::TurnControlKind::Finish).
    pub fn finish(value: impl Into<ToolValue>) -> Self {
        Self::turn_control(TurnControl::Finish {
            value: value.into(),
            value_schema: None,
        })
    }

    /// Switch the caller to a fresh agent frame whose follow-on runs under
    /// `next_run`. The tool must declare
    /// [`TurnControlKind::SwitchAgentFrame`](lash_vocabulary::tool_declaration::TurnControlKind::SwitchAgentFrame).
    pub fn switch_agent_frame(
        frame_key: crate::FrameKey,
        task: impl Into<String>,
        initial_nodes: Vec<crate::SessionAppendNode>,
        next_run: NextRun,
    ) -> Self {
        Self::turn_control(TurnControl::SwitchAgentFrame {
            frame_key,
            initial_nodes,
            task: task.into(),
            next_run,
        })
    }

    /// The kind of turn control this result is, when it is one: what its
    /// tool's declaration must name.
    pub fn turn_control_kind(&self) -> Option<lash_vocabulary::tool_declaration::TurnControlKind> {
        self.as_turn_control().map(TurnControl::kind)
    }

    /// This result as a call under `declaration` and `output_schema`
    /// settles: the one check every route a call's answer takes passes
    /// through, inline, cached, deferred and after every result transform.
    /// A call that did not succeed stays the failure it is and carries no
    /// turn control. A success's value is admitted by the output schema of
    /// the contract the call was admitted under: it must be an instance of
    /// the whole declared schema (maps, open objects, numeric bounds and
    /// formats included), and decode by the schema's kernel result type, the
    /// rule a code cell reads it by (`K-EFF-007`). A tool that declares a
    /// turn control has no output: its success is null, whatever its body or
    /// a result transform put beside the control. A Finish's value is
    /// admitted the same way by the schema the declaration states, and the
    /// settled Finish keeps that schema as the witness of the check.
    ///
    /// # Errors
    ///
    /// [`DeclarationRefusal::UndeclaredControl`](lash_vocabulary::tool_declaration::DeclarationRefusal::UndeclaredControl):
    /// a control the declaration does not name.
    /// [`DeclarationRefusal::OutputValueMismatch`](lash_vocabulary::tool_declaration::DeclarationRefusal::OutputValueMismatch):
    /// a success whose value `output_schema` does not admit.
    /// [`DeclarationRefusal::UndeclaredOutput`](lash_vocabulary::tool_declaration::DeclarationRefusal::UndeclaredOutput):
    /// a tool that declares a turn control succeeded with a value and no
    /// control.
    /// [`DeclarationRefusal::FinishValueMismatch`](lash_vocabulary::tool_declaration::DeclarationRefusal::FinishValueMismatch):
    /// a Finish whose value the declared schema does not admit.
    pub fn settled(
        mut self,
        declaration: &lash_vocabulary::tool_declaration::ToolDeclaration,
        output_schema: &lash_vocabulary::json_schema::JsonSchema,
    ) -> Result<Self, lash_vocabulary::tool_declaration::DeclarationRefusal> {
        let ToolCallOutcome::Success(value) = &self.outcome else {
            if self.as_turn_control().is_some() {
                self.control = None;
            }
            return Ok(self);
        };
        if let Some(control) = self.turn_control_kind()
            && !declaration.controls.contains(control)
        {
            return Err(
                lash_vocabulary::tool_declaration::DeclarationRefusal::UndeclaredControl {
                    control,
                },
            );
        }
        if declaration.controls.is_empty() {
            crate::result_fit::admit_value(output_schema, || self.value_for_projection()).map_err(
                |mismatch| {
                    lash_vocabulary::tool_declaration::DeclarationRefusal::OutputValueMismatch {
                        output_schema: Box::new(output_schema.clone()),
                        mismatch: Box::new(mismatch),
                    }
                },
            )?;
            return Ok(self);
        }
        if self.as_turn_control().is_none() && !value.to_json_value().is_null() {
            return Err(lash_vocabulary::tool_declaration::DeclarationRefusal::UndeclaredOutput);
        }
        if let Some(ToolControl::Turn {
            control:
                TurnControl::Finish {
                    value,
                    value_schema,
                },
        }) = &mut self.control
            && let Some(declared) = declaration.controls.finish_declaration()
        {
            crate::result_fit::admit_value(&declared.value_schema, || value.to_json_value())
                .map_err(|mismatch| {
                    lash_vocabulary::tool_declaration::DeclarationRefusal::FinishValueMismatch {
                        value_schema: Box::new(declared.value_schema.clone()),
                        mismatch: Box::new(mismatch),
                    }
                })?;
            *value_schema = Some(declared.value_schema.clone());
        }
        self.outcome = ToolCallOutcome::Success(ToolValue::untrusted_json(Value::Null));
        self.view = None;
        self.projection_value = None;
        Ok(self)
    }

    /// The turn control this result is, when it is one.
    pub fn as_turn_control(&self) -> Option<&TurnControl> {
        match &self.control {
            Some(ToolControl::Turn { control }) => Some(control),
            _ => None,
        }
    }

    pub fn with_control(mut self, control: ToolControl) -> Self {
        self.control = Some(control);
        self
    }

    pub fn with_view(mut self, view: ToolView) -> Self {
        self.view = Some(view);
        self
    }

    pub fn with_projection_value(mut self, value: Value) -> Self {
        self.projection_value = Some(value);
        self
    }

    /// The host panic that ends a turn, preserved through process outputs.
    pub fn tool_panic_stop(&self) -> Option<crate::TurnStop> {
        let ToolCallOutcome::Failure(failure) = &self.outcome else {
            return None;
        };
        let ToolFailureCause::Panicked {
            tool_name,
            call_id,
            message,
        } = failure.cause.as_deref()?
        else {
            return None;
        };
        Some(crate::TurnStop::ToolPanicked {
            tool_name: tool_name.clone(),
            call_id: call_id.clone(),
            message: message.clone(),
        })
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

    /// Projects output as typed JSON for host views and persisted projections.
    /// A failure returns the typed failure object (`class`, `code`, `message`,
    /// `source` and optional evidence), not just its message. Trust envelopes in
    /// `raw` are removed while typed attachments are preserved.
    /// For model text use [`ToolFailure::model_text`]; for complete model
    /// content use [`ModelToolReturn::from_output`]. Never stringify this
    /// projection as a failure message.
    pub fn value_for_projection(&self) -> Value {
        match &self.outcome {
            ToolCallOutcome::Success(value) => self
                .projection_value
                .clone()
                .unwrap_or_else(|| value.projected_json_value()),
            ToolCallOutcome::Failure(failure) => failure.to_json_value(),
            ToolCallOutcome::Cancelled(cancellation) => cancellation.to_json_value(),
        }
    }

    /// Consuming form of [`Self::value_for_projection`], with the same contract.
    pub fn into_value_for_projection(self) -> Value {
        match self.outcome {
            ToolCallOutcome::Success(value) => self
                .projection_value
                .unwrap_or_else(|| value.into_projected_json_value()),
            ToolCallOutcome::Failure(failure) => failure.to_json_value(),
            ToolCallOutcome::Cancelled(cancellation) => cancellation.to_json_value(),
        }
    }

    pub fn attachments(&self) -> Vec<AttachmentRef> {
        let mut references = match &self.outcome {
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
        };
        if let Some(view) = &self.view {
            references.extend(view.blocks.iter().filter_map(|block| match block {
                ToolViewBlock::Attachment { reference, .. } => Some(reference.clone()),
                _ => None,
            }));
        }
        references
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    Success,
    Failure,
    Cancelled,
}

impl ToolCallStatus {
    /// Whether the provider must mark the call's result as an error.
    pub fn is_error(self) -> bool {
        !matches!(self, Self::Success)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(remote = "Self")]
#[serde(tag = "status", content = "payload", rename_all = "snake_case")]
pub enum ToolCallOutcome {
    Success(#[serde(serialize_with = "crate::durable_serde::serialize")] ToolValue),
    Failure(#[serde(serialize_with = "crate::durable_serde::serialize")] ToolFailure),
    Cancelled(#[serde(serialize_with = "crate::durable_serde::serialize")] ToolCancellation),
}

/// A typed tool value with an explicit serialization trust envelope.
///
/// # Stable JSON projection contract
///
/// This type does not implement [`Serialize`]. Explicitly encode
/// `Durable(&value)` for persistence or a trusted wire. In
/// particular, an [`UntrustedJson`](Self::UntrustedJson) value serializes with
/// the internal `$lash_tool_value: "untrusted_json"` discriminant and a `value`
/// wrapper. A stable or public JSON projection **MUST** go through
/// [`ToolCallOutput::value_for_projection`], which removes that internal
/// envelope while preserving typed attachment projection.
///
/// # Remote tool hosts
///
/// A provider that forwards a call to a remote tool host returns the host's
/// answer as this value, with [`ToolCallOutput::success_tool_value`]. The
/// host answers the typed value the tool's contract declares
/// ([`ToolContract::output_schema`](lash_vocabulary::tool_contract::ToolContract::output_schema)), never
/// model-facing presentation parts. Lash takes it from there:
///
/// - Lash admits the value against the call's declared output schema when
///   the call settles ([`ToolCallOutput::settled`]). The value must be an
///   instance of the whole schema, as JSON Schema validates it, and decode by
///   the result type the schema gives a code cell (`K-EFF-007`). A value that
///   fails either check settles the call as a typed failure
///   ([`DeclarationRefusal::OutputValueMismatch`](lash_vocabulary::tool_declaration::DeclarationRefusal::OutputValueMismatch))
///   naming the tool and the JSON pointer of the part that does not fit; no
///   cell, model or caller receives it. The host need not check the value
///   itself.
/// - Lash presents the value to the model with
///   [`ModelToolReturn::from_output`]. A host that shows the call, as a card
///   or in a transcript, calls it too instead of deriving its own parts.
///
/// This type's serialized form keeps the envelope, so it can carry the
/// answer over the host's wire. Deserializing it trusts the sender's
/// reserved tags: an attachment the host tags becomes an
/// [`Attachment`](Self::Attachment). A provider that does not trust its host
/// to name attachments reads the answer as plain JSON and wraps it with
/// [`untrusted_json`](Self::untrusted_json).
#[derive(Clone, Debug, PartialEq)]
pub enum ToolValue {
    Null,
    Bool(bool),
    Number(Number),
    String(String),
    Array(Vec<ToolValue>),
    Object(BTreeMap<String, ToolValue>),
    Attachment(AttachmentRef),
    UntrustedJson(Value),
}

impl ToolValue {
    /// The JSON is never scanned for Lash's reserved tags. Serializing this arm
    /// nests the whole value beneath its own tag so foreign objects cannot be
    /// mistaken for typed attachments.
    pub fn untrusted_json(value: Value) -> Self {
        Self::UntrustedJson(value)
    }

    pub fn to_json_value(&self) -> Value {
        self.projected_json_value()
    }

    fn projected_json_value(&self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Bool(value) => Value::Bool(*value),
            Self::Number(value) => Value::Number(value.clone()),
            Self::String(value) => Value::String(value.clone()),
            Self::Array(values) => {
                Value::Array(values.iter().map(Self::projected_json_value).collect())
            }
            Self::Object(entries) => Value::Object(
                entries
                    .iter()
                    .map(|(key, value)| (key.clone(), value.projected_json_value()))
                    .collect(),
            ),
            Self::Attachment(source) => tagged_attachment_json(source),
            Self::UntrustedJson(value) => value.clone(),
        }
    }

    fn into_projected_json_value(self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Bool(value) => Value::Bool(value),
            Self::Number(value) => Value::Number(value),
            Self::String(value) => Value::String(value),
            Self::Array(values) => Value::Array(
                values
                    .into_iter()
                    .map(Self::into_projected_json_value)
                    .collect(),
            ),
            Self::Object(entries) => Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key, value.into_projected_json_value()))
                    .collect(),
            ),
            Self::Attachment(source) => tagged_attachment_json(&source),
            Self::UntrustedJson(value) => value,
        }
    }

    pub(crate) fn from_json_value(value: Value) -> serde_json::Result<Self> {
        serde_json::from_value(value)
    }

    pub fn attachments(&self) -> Vec<AttachmentRef> {
        let mut attachments = Vec::new();
        self.collect_attachments(&mut attachments);
        attachments
    }

    fn collect_attachments(&self, attachments: &mut Vec<AttachmentRef>) {
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
            Self::Null
            | Self::Bool(_)
            | Self::Number(_)
            | Self::String(_)
            | Self::UntrustedJson(_) => {}
        }
    }
}

fn tagged_attachment_json(source: &AttachmentRef) -> Value {
    let mut map = Map::with_capacity(2);
    map.insert(
        TAG_KEY.to_string(),
        Value::String(ATTACHMENT_TAG.to_string()),
    );
    map.insert(
        REFERENCE_KEY.to_string(),
        serde_json::to_value(source).unwrap_or(Value::Null),
    );
    Value::Object(map)
}

impl From<Value> for ToolValue {
    fn from(value: Value) -> Self {
        Self::untrusted_json(value)
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

impl crate::DurableSerialize for ToolValue {
    fn serialize_durable<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            ToolValue::Null => serializer.serialize_none(),
            ToolValue::Bool(value) => serializer.serialize_bool(*value),
            ToolValue::Number(value) => value.serialize(serializer),
            ToolValue::String(value) => serializer.serialize_str(value),
            ToolValue::Array(values) => Durable(values).serialize(serializer),
            ToolValue::Attachment(source) => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry(TAG_KEY, ATTACHMENT_TAG)?;
                map.serialize_entry(REFERENCE_KEY, source)?;
                map.end()
            }
            ToolValue::UntrustedJson(value) => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry(TAG_KEY, UNTRUSTED_JSON_TAG)?;
                map.serialize_entry(VALUE_KEY, value)?;
                map.end()
            }
            ToolValue::Object(entries) => Durable(entries).serialize(serializer),
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
            if map.len() != 2 || !map.contains_key(REFERENCE_KEY) {
                return Err(serde_json::Error::custom("malformed attachment tool value"));
            }
            let source = serde_json::from_value(
                map.remove(REFERENCE_KEY)
                    .ok_or_else(|| serde_json::Error::custom("missing attachment reference"))?,
            )?;
            Ok(ToolValue::Attachment(source))
        }
        UNTRUSTED_JSON_TAG => {
            if map.len() != 2 || !map.contains_key(VALUE_KEY) {
                return Err(serde_json::Error::custom(
                    "malformed untrusted JSON tool value",
                ));
            }
            Ok(ToolValue::UntrustedJson(map.remove(VALUE_KEY).ok_or_else(
                || serde_json::Error::custom("missing untrusted JSON value"),
            )?))
        }
        other => Err(serde_json::Error::custom(format!(
            "unknown reserved tool value tag `{other}`"
        ))),
    }
}

mod failure_cause;
pub use failure_cause::ToolFailureCause;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(remote = "Self")]
pub struct ToolFailure {
    pub class: ToolFailureClass,
    pub code: String,
    pub message: String,
    pub source: ToolFailureSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggested_delay_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<Box<ToolFailureCause>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(serialize_with = "crate::durable_serde::serialize")]
    pub raw: Option<ToolValue>,
}

impl ToolFailure {
    /// The plain message to present to the model, without JSON or added markers.
    /// Structured diagnostics and provider evidence stay in the other fields.
    pub fn model_text(&self) -> &str {
        &self.message
    }

    pub fn with_cause(mut self, cause: ToolFailureCause) -> Self {
        self.cause = Some(Box::new(cause));
        self
    }
    /// A tool's execution failure with an explicit code and plain message.
    /// Attach provider evidence with [`Self::with_raw`].
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::tool(ToolFailureClass::Execution, code, message)
    }

    /// Retains structured provider evidence without changing the code or message.
    pub fn with_raw(mut self, raw: impl Into<ToolValue>) -> Self {
        self.raw = Some(raw.into());
        self
    }

    pub fn runtime(
        class: ToolFailureClass,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            class,
            code: code.into(),
            message: message.into(),
            source: ToolFailureSource::Runtime,
            suggested_delay_ms: None,
            cause: None,
            raw: None,
        }
    }

    pub fn tool(
        class: ToolFailureClass,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            source: ToolFailureSource::Tool,
            ..Self::runtime(class, code, message)
        }
    }

    pub fn invalid_request(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::tool(ToolFailureClass::InvalidRequest, code, message)
    }

    /// The refusal of a step's control call to `tool_name` because another
    /// call of the step failed or was cancelled: its body never ran.
    pub fn control_sibling_failed(tool_name: &str) -> Self {
        Self::invalid_request(
            "control_sibling_failed",
            format!(
                "`{tool_name}` was not called: another call of the same step failed, so the turn does not end on it"
            ),
        )
        .with_cause(ToolFailureCause::ControlSiblingFailed)
    }

    pub fn io(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::tool(ToolFailureClass::Io, code, message)
    }

    pub fn with_suggested_delay(
        class: ToolFailureClass,
        code: impl Into<String>,
        message: impl Into<String>,
        after_ms: Option<u64>,
    ) -> Self {
        let mut failure = Self::tool(class, code, message);
        failure.suggested_delay_ms = after_ms;
        failure
    }

    pub fn to_json_value(&self) -> Value {
        project_raw_tool_value(
            serde_json::to_value(Durable(self))
                .unwrap_or_else(|_| Value::String(self.message.clone())),
            self.raw.as_ref(),
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolFailureSource {
    Runtime,
    Tool,
    Plugin,
    Policy,
    Cancellation,
}

/// The runtime or actor decision that requested cancellation of a process
/// or of a tool call's attempt. `RunClosing` is a call's owning tool Run
/// discharging its cancellation at Closing while the turn itself runs on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CancelOrigin {
    TurnStopped,
    ParentEnded,
    OperatorRequested,
    ModelRequested,
    StartFailed,
    RunClosing,
}

/// A process cancellation fact. Its first timestamp survives retries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CancelRequest {
    pub origin: CancelOrigin,
    pub requester: String,
    pub requested_at_ms: u64,
}

impl CancelRequest {
    /// Construct a cancellation fact with an explicit actor or causal reference.
    pub fn new(origin: CancelOrigin, requester: impl Into<String>, requested_at_ms: u64) -> Self {
        Self {
            origin,
            requester: requester.into(),
            requested_at_ms,
        }
    }

    /// Whether these attempts request the same cancellation. A retry's clock
    /// does not change the request identity or replace the first timestamp.
    pub fn same_cancellation_as(&self, other: &Self) -> bool {
        self.origin == other.origin && self.requester == other.requester
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(remote = "Self")]
pub struct ToolCancellation {
    pub message: String,
    pub source: ToolFailureSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<CancelOrigin>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(serialize_with = "crate::durable_serde::serialize")]
    pub raw: Option<ToolValue>,
    /// Whether lash forced the end: a cancelled process whose engine did not
    /// end it within its cancel grace (ADR 0132 §10).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub forced: bool,
}

impl ToolCancellation {
    /// Attach the origin of the durable process cancellation being settled.
    pub fn with_origin(mut self, origin: CancelOrigin) -> Self {
        self.origin = Some(origin);
        self
    }

    pub fn runtime(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            source: ToolFailureSource::Cancellation,
            origin: None,
            raw: None,
            forced: false,
        }
    }

    /// Mark the cancellation forced: lash ended the process at its grace.
    #[must_use]
    pub fn forced(mut self) -> Self {
        self.forced = true;
        self
    }

    pub fn to_json_value(&self) -> Value {
        project_raw_tool_value(
            serde_json::to_value(Durable(self))
                .unwrap_or_else(|_| Value::String(self.message.clone())),
            self.raw.as_ref(),
        )
    }
}

fn project_raw_tool_value(mut value: Value, raw: Option<&ToolValue>) -> Value {
    if let (Value::Object(entries), Some(raw)) = (&mut value, raw) {
        entries.insert("raw".to_string(), raw.projected_json_value());
    }
    value
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(remote = "Self")]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TurnControl {
    /// End the turn with `value`.
    Finish {
        #[serde(serialize_with = "crate::durable_serde::serialize")]
        value: ToolValue,
        /// The schema `value` was checked against when its call settled: the
        /// admitted declaration's ([`FinishDeclaration`](lash_vocabulary::tool_declaration::FinishDeclaration)).
        /// A body's own Finish states none; settlement stamps it, and every
        /// later reader — a completion candidate, a replay, a batch that
        /// folds the call — keeps the schema the value settled under rather
        /// than reading a live one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value_schema: Option<lash_vocabulary::json_schema::JsonSchema>,
    },
    /// End the turn by switching to a fresh agent frame that starts on
    /// `task` with `initial_nodes`, its follow-on running under `next_run`.
    SwitchAgentFrame {
        frame_key: crate::FrameKey,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        initial_nodes: Vec<crate::SessionAppendNode>,
        task: String,
        next_run: NextRun,
    },
}

/// The run spec a frame switch's follow-on runs under (FIG-6021), in its
/// serialized form. The spec type belongs to the store vocabulary above
/// this crate, which converts it both ways; the sans-io core carries it
/// unread. The producer of the switch states it whole: inheriting the
/// switching run's shape is passing that run's recorded spec back, and a
/// different spec is exactly that spec. The switch's commit mails the
/// follow-on under it, so every retry of that commit mails the same one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NextRun(Value);

impl NextRun {
    /// The serialized spec `spec`, as the store vocabulary encodes it.
    pub fn from_encoded(spec: Value) -> Self {
        Self(spec)
    }

    /// The serialized spec, for the store vocabulary to decode.
    pub fn encoded(&self) -> &Value {
        &self.0
    }
}

impl TurnControl {
    pub fn kind(&self) -> lash_vocabulary::tool_declaration::TurnControlKind {
        match self {
            Self::Finish { .. } => lash_vocabulary::tool_declaration::TurnControlKind::Finish,
            Self::SwitchAgentFrame { .. } => {
                lash_vocabulary::tool_declaration::TurnControlKind::SwitchAgentFrame
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(remote = "Self")]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolControl {
    /// A turn-ending control the tool declared
    /// ([`TurnControls`](lash_vocabulary::tool_declaration::TurnControls)): the call's whole result.
    Turn {
        #[serde(serialize_with = "crate::durable_serde::serialize")]
        control: TurnControl,
    },
    Fail {
        #[serde(serialize_with = "crate::durable_serde::serialize")]
        failure: ToolFailure,
    },
    /// A plugin's tool check stopped the owning logical Run (ADR 0128). The
    /// call's outcome carries the failure the model reads; this is the Run
    /// control, under the aborting plugin's namespaced code.
    AbortRun {
        code: crate::FailureCode,
        message: String,
    },
}

/// Why a request omitted an attachment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentMaterializationReason {
    /// The target model and its route cannot accept this MIME/position pair.
    TargetModelDoesNotAcceptMimeAndPosition,
}

/// Typed information explaining why an attachment is absent from model input.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentMaterializationNotice {
    pub attachment_id: crate::AttachmentId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub media_type: crate::MediaType,
    pub position: crate::llm::attachment_delivery::AttachmentPosition,
    /// Typed reason the attachment was omitted.
    pub reason: AttachmentMaterializationReason,
}

impl AttachmentMaterializationNotice {
    /// Build the notice for a target model that cannot accept this attachment.
    pub fn target_model_does_not_accept(
        reference: &AttachmentRef,
        position: crate::llm::attachment_delivery::AttachmentPosition,
    ) -> Self {
        Self {
            attachment_id: reference.id.clone(),
            label: reference.label.clone(),
            media_type: reference.media_type.clone(),
            position,
            reason: AttachmentMaterializationReason::TargetModelDoesNotAcceptMimeAndPosition,
        }
    }
}

/// The execution fact on a tool return, independent of its presented body.
/// Only failure and cancellation variants can carry their typed causes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", content = "payload", rename_all = "snake_case")]
pub enum ModelToolOutcome {
    Completed,
    Failure(#[serde(serialize_with = "crate::durable_serde::serialize")] ToolFailure),
    Cancelled(#[serde(serialize_with = "crate::durable_serde::serialize")] ToolCancellation),
}

impl From<&ToolCallOutcome> for ModelToolOutcome {
    fn from(outcome: &ToolCallOutcome) -> Self {
        match outcome {
            ToolCallOutcome::Success(_) => Self::Completed,
            ToolCallOutcome::Failure(failure) => Self::Failure(failure.clone()),
            ToolCallOutcome::Cancelled(cancellation) => Self::Cancelled(cancellation.clone()),
        }
    }
}

impl ModelToolOutcome {
    /// The bounded terminal tag projected into transcript and provider blocks.
    /// Full causes remain in the recorded return and the presenter's input.
    pub fn status(&self) -> ToolCallStatus {
        match self {
            Self::Completed => ToolCallStatus::Success,
            Self::Failure(_) => ToolCallStatus::Failure,
            Self::Cancelled(_) => ToolCallStatus::Cancelled,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelToolReturn {
    pub tool_name: String,
    pub outcome: ModelToolOutcome,
    pub parts: Vec<ModelToolReturnPart>,
}

impl ModelToolReturn {
    /// Carries the call's typed outcome and attachments without authoring failure
    /// or cancellation text. The mode's presenter owns those words.
    pub fn from_output(tool_name: String, output: &ToolCallOutput) -> Self {
        let parts = match &output.outcome {
            ToolCallOutcome::Success(value) => Self::parts_from_value(value),
            ToolCallOutcome::Failure(failure) => failure
                .raw
                .as_ref()
                .map(ToolValue::attachments)
                .unwrap_or_default()
                .into_iter()
                .map(ModelToolReturnPart::Attachment)
                .collect(),
            ToolCallOutcome::Cancelled(cancellation) => cancellation
                .raw
                .as_ref()
                .map(ToolValue::attachments)
                .unwrap_or_default()
                .into_iter()
                .map(ModelToolReturnPart::Attachment)
                .collect(),
        };
        Self {
            tool_name,
            outcome: ModelToolOutcome::from(&output.outcome),
            parts,
        }
    }

    fn parts_from_value(value: &ToolValue) -> Vec<ModelToolReturnPart> {
        let mut parts = Vec::new();
        match value {
            ToolValue::String(text) => push_text_part(&mut parts, text.clone()),
            ToolValue::Attachment(reference) => {
                parts.push(ModelToolReturnPart::Attachment(reference.clone()));
            }
            _ => Self::write_compact_value(value, &mut parts),
        }
        parts
    }

    fn write_compact_value(value: &ToolValue, parts: &mut Vec<ModelToolReturnPart>) {
        match value {
            ToolValue::Null => push_text_part(parts, "null"),
            ToolValue::Bool(value) => push_text_part(parts, value.to_string()),
            ToolValue::Number(value) => push_text_part(parts, value.to_string()),
            ToolValue::String(value) => push_text_part(
                parts,
                serde_json::to_string(value).unwrap_or_else(|_| "\"\"".into()),
            ),
            ToolValue::UntrustedJson(value) => push_text_part(
                parts,
                serde_json::to_string(value).unwrap_or_else(|_| "null".into()),
            ),
            ToolValue::Attachment(reference) => {
                parts.push(ModelToolReturnPart::Attachment(reference.clone()));
            }
            ToolValue::Array(values) => {
                push_text_part(parts, "[");
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        push_text_part(parts, ",");
                    }
                    Self::write_compact_value(value, parts);
                }
                push_text_part(parts, "]");
            }
            ToolValue::Object(entries) => {
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
                    Self::write_compact_value(value, parts);
                }
                push_text_part(parts, "}");
            }
        }
    }

    pub(crate) fn text(tool_name: String, content: impl Into<String>) -> Self {
        Self {
            tool_name,
            outcome: ModelToolOutcome::Completed,
            parts: vec![ModelToolReturnPart::text(content)],
        }
    }
}

/// One ordered block of a tool's model-facing result: text, an attachment
/// at the position the tool's value placed it, or output retained out of
/// history (FIG-1643). The same blocks travel unchanged into the
/// transcript's one [`crate::Part::ToolResult`] and the provider request's
/// one tool-result block.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelToolReturnPart {
    Text {
        text: String,
    },
    Attachment(AttachmentRef),
    /// Text too long for history, retained as a session attachment. A reader
    /// of the result sees the witness as text; the reference is never
    /// materialized into a request, so retention never expands a prompt.
    Retained(crate::RetainedOutput),
}

impl ModelToolReturnPart {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    /// The attachment this block carries; `None` for text and for retained
    /// output, whose reference is history's, not the model's.
    pub fn attachment(&self) -> Option<&AttachmentRef> {
        match self {
            Self::Text { .. } | Self::Retained(_) => None,
            Self::Attachment(source) => Some(source),
        }
    }

    /// The retained output this block stands in for; `None` for text and
    /// attachments.
    pub fn retained(&self) -> Option<&crate::RetainedOutput> {
        match self {
            Self::Retained(retained) => Some(retained),
            Self::Text { .. } | Self::Attachment(_) => None,
        }
    }

    /// The text a reader of this block sees: a text block's text, a retained
    /// block's witness, nothing for an attachment.
    pub fn visible_text(&self) -> Option<&str> {
        match self {
            Self::Text { text } => Some(text),
            Self::Retained(retained) => Some(&retained.witness),
            Self::Attachment(_) => None,
        }
    }
}

impl crate::llm::types::LlmContentBlock {
    /// Every attachment ref this block carries, in content order: an
    /// attachment block's ref, or the attachment blocks of a tool result.
    pub fn attachments(&self) -> impl Iterator<Item = &AttachmentRef> {
        use crate::llm::types::LlmContentBlock;
        let (own, result): (Option<&AttachmentRef>, &[ModelToolReturnPart]) = match self {
            LlmContentBlock::Attachment { reference } => (Some(reference.as_ref()), &[]),
            LlmContentBlock::ToolResult { content, .. } => (None, content.as_slice()),
            LlmContentBlock::Text { .. }
            | LlmContentBlock::ToolCall { .. }
            | LlmContentBlock::Reasoning { .. } => (None, &[]),
        };
        own.into_iter()
            .chain(result.iter().filter_map(ModelToolReturnPart::attachment))
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

#[path = "tool_output/attachment_adoption.rs"]
mod attachment_adoption;

#[doc(hidden)]
#[path = "tool_output/graph.rs"]
pub mod graph;

#[cfg(test)]
mod tests;
