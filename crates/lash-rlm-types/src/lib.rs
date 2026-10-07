use lash_sansio::{
    AttachmentRef, CellFailure, CellFailureKind, OutputValue, RetainedOutput, SchemaShape,
    ShapeKind, TurnProtocol,
};

/// Read-only legacy protocol-owned assistant context paired with an RLM
/// trajectory entry.
///
/// New sessions persist this context as ordinary durable assistant messages so
/// provider reasoning replay metadata survives. This event must keep decoding
/// for old session histories, but producers must not write new instances.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct RlmAssistantContent {
    pub id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reasoning: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prose: String,
}

/// What an RLM cell resolved to: still running, failed, or finished with a
/// driver-adjudicated value. One of three — a cell can never carry an error
/// and a terminal value at once.
///
/// `E` is each layer's own error representation: the typed [`CellFailure`]
/// inside a parked driver state and in a durable trajectory entry, and its
/// `{kind, message}` view ([`HistoryCellError`]) in the history a cell reads.
/// `V` is the terminal value's: the value itself inside the driver, an
/// [`OutputValue`] — the value, or its retention — in a trajectory entry
/// ([`HistoryCellOutcome`]), and a [`HistoryValue`] in the cell's view.
///
/// Parked state stores the tagged enum directly. Trajectory entries and
/// history items use flat encodings, where key presence distinguishes a null
/// terminal value from a running cell.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum CellOutcome<E, V = serde_json::Value> {
    /// The cell produced neither an error nor a terminal value.
    #[default]
    Running,
    /// The cell failed.
    Failed(E),
    /// The cell produced a driver-adjudicated terminal value.
    Finished(V),
}

/// A cell outcome as a trajectory entry records it: a failed cell keeps its
/// typed [`CellFailure`], and a finished cell's value is inline, or retained
/// out of history when it was too long (FIG-1643). Guidance for the model is
/// rendered from the failure when a prompt is projected and is never stored.
///
/// The durable spelling is the `error` / `final_output` key pair, with
/// `final_output_retained` in place of `final_output` for a retained value —
/// the outcome flattens into the entry's object, writes at most one of the
/// three keys, and refuses to decode a record that sets more than one.
pub type HistoryCellOutcome = CellOutcome<CellFailure, OutputValue>;

impl<E, V> CellOutcome<E, V> {
    /// Fold an `error` / `terminal value` pair into the single outcome it
    /// A pair carrying both resolves to the failure: a finished cell must not silently discard
    /// its error.
    pub fn from_parts(error: Option<E>, terminal: Option<V>) -> Self {
        match (error, terminal) {
            (Some(error), _) => Self::Failed(error),
            (None, Some(value)) => Self::Finished(value),
            (None, None) => Self::Running,
        }
    }

    /// The failure, when the cell failed.
    pub fn error(&self) -> Option<&E> {
        match self {
            Self::Failed(error) => Some(error),
            _ => None,
        }
    }

    pub fn is_failed(&self) -> bool {
        matches!(self, Self::Failed(_))
    }

    /// The terminal value, when the cell finished.
    pub fn terminal_value(&self) -> Option<&V> {
        match self {
            Self::Finished(value) => Some(value),
            _ => None,
        }
    }

    /// A borrowed view, for remapping the error without cloning.
    pub fn as_ref(&self) -> CellOutcome<&E, V>
    where
        V: Clone,
    {
        match self {
            Self::Running => CellOutcome::Running,
            Self::Failed(error) => CellOutcome::Failed(error),
            Self::Finished(value) => CellOutcome::Finished(value.clone()),
        }
    }

    pub fn map_error<F>(self, op: impl FnOnce(E) -> F) -> CellOutcome<F, V> {
        match self {
            Self::Running => CellOutcome::Running,
            Self::Failed(error) => CellOutcome::Failed(op(error)),
            Self::Finished(value) => CellOutcome::Finished(value),
        }
    }
}

mod history_outcome {
    use super::{CellFailure, CellOutcome, HistoryCellOutcome, OutputValue, RetainedOutput};
    use serde::{Deserialize, Serialize};

    pub fn serialize<S: serde::Serializer>(
        outcome: &HistoryCellOutcome,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        #[derive(serde::Serialize)]
        struct Fields<'a> {
            #[serde(skip_serializing_if = "Option::is_none")]
            error: Option<&'a CellFailure>,
            #[serde(skip_serializing_if = "Option::is_none")]
            final_output: Option<&'a serde_json::Value>,
            #[serde(skip_serializing_if = "Option::is_none")]
            final_output_retained: Option<&'a RetainedOutput>,
        }
        let (error, final_output, final_output_retained) = match outcome {
            CellOutcome::Running => (None, None, None),
            CellOutcome::Failed(error) => (Some(error), None, None),
            CellOutcome::Finished(OutputValue::Inline(value)) => (None, Some(value), None),
            CellOutcome::Finished(OutputValue::Retained(retained)) => (None, None, Some(retained)),
        };
        Fields {
            error,
            final_output,
            final_output_retained,
        }
        .serialize(serializer)
    }

    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<HistoryCellOutcome, D::Error> {
        fn present<'de, T: Deserialize<'de>, D: serde::Deserializer<'de>>(
            deserializer: D,
        ) -> Result<Option<T>, D::Error> {
            T::deserialize(deserializer).map(Some)
        }

        #[derive(serde::Deserialize)]
        struct Fields {
            #[serde(default, deserialize_with = "present")]
            error: Option<CellFailure>,
            #[serde(default, deserialize_with = "present")]
            final_output: Option<serde_json::Value>,
            #[serde(default, deserialize_with = "present")]
            final_output_retained: Option<RetainedOutput>,
        }
        let fields = Fields::deserialize(deserializer)?;
        let terminal = match (fields.final_output, fields.final_output_retained) {
            (Some(_), Some(_)) => {
                return Err(serde::de::Error::custom(
                    "a cell outcome cannot carry both `final_output` and `final_output_retained`",
                ));
            }
            (Some(value), None) => Some(OutputValue::Inline(value)),
            (None, Some(retained)) => Some(OutputValue::Retained(retained)),
            (None, None) => None,
        };
        match (fields.error, terminal) {
            (Some(_), Some(_)) => Err(serde::de::Error::custom(
                "a cell outcome cannot carry both `error` and `final_output`",
            )),
            (error, terminal) => Ok(CellOutcome::from_parts(error, terminal)),
        }
    }
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct RlmTrajectoryEntry {
    pub id: String,
    pub protocol_iteration: usize,
    pub code: String,
    /// Complete inline prints below the aggregate retention limit.
    pub output: Vec<RlmPrint>,
    /// One archive for all prints in this step; `output` is empty when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_archive: Option<Box<RetainedOutput>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<AttachmentRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub calls: Vec<RlmExecutedCall>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub calls_omitted: usize,
    /// What the cell resolved to. Serialized flat as the `error` /
    /// `final_output` key pair stored trajectories already carry, or
    /// `final_output_retained` for a retained value; at most one key is ever
    /// written, and decoding refuses a record that sets more.
    #[serde(flatten, with = "history_outcome")]
    pub outcome: HistoryCellOutcome,
}

pub type RlmExecutedCall = lash_sansio::ExecutedCallRecord;

/// One inline print. Oversized steps retain the complete array in one archive.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RlmPrint {
    pub text: String,
    pub value: serde_json::Value,
}

impl From<String> for RlmPrint {
    fn from(text: String) -> Self {
        Self {
            value: serde_json::Value::String(text.clone()),
            text,
        }
    }
}
pub type RlmExecutedCallOutcome = lash_sansio::ExecutedCallOutcome;

fn is_zero(value: &usize) -> bool {
    *value == 0
}

impl RlmTrajectoryEntry {
    pub fn output_chars(&self) -> usize {
        self.output
            .iter()
            .map(|print| print.text.chars().count())
            .sum()
    }
}

#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum RlmHistoryRole {
    User,
    System,
    Assistant,
    Event,
}

#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct RlmAttachmentRef {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<String>")]
    pub media_type: Option<lash_sansio::MediaType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub source: String,
    pub reference: String,
}

#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct RlmImageRef {
    pub id: String,
    #[schemars(with = "String")]
    pub media_type: lash_sansio::MediaType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    pub bytes: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl RlmImageRef {
    /// Build the model-visible image metadata from its durable attachment reference.
    pub fn from_attachment(attachment: &lash_sansio::AttachmentRef) -> Self {
        let (width, height) = match attachment.type_metadata.as_ref() {
            Some(lash_sansio::AttachmentTypeMetadata::Image { width, height }) => (*width, *height),
            None => (None, None),
        };
        Self {
            id: attachment.id.to_string(),
            media_type: attachment.media_type.clone(),
            width,
            height,
            bytes: attachment.byte_len as usize,
            label: attachment.label.clone(),
        }
    }
}

/// The key that marks a history value as Lash's own record rather than a
/// value the cell produced. The `$lash_` prefix is reserved.
pub const HISTORY_VALUE_TAG_KEY: &str = "$lash_history_value";

/// The tag of a [`HistoryValue::Retained`] record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HistoryValueTag {
    Retained,
}

/// A value retained out of history (FIG-1643), as a cell reads it: its
/// witness, its size, and the attachment that holds it whole — a value the
/// cell can hand to a tool that reads attachments.
#[derive(Clone, Debug, PartialEq, serde::Serialize, schemars::JsonSchema)]
pub struct RetainedHistoryValue {
    #[serde(rename = "$lash_history_value")]
    pub tag: HistoryValueTag,
    pub witness: String,
    pub byte_len: u64,
    pub attachment: serde_json::Value,
}

/// A printed or final value as a cell reads it back through `history`: the
/// value itself, or the one reserved-tag record of a value retained out of
/// history. Reading history never loads a retained value.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[serde(untagged)]
pub enum HistoryValue {
    Retained(RetainedHistoryValue),
    Inline(serde_json::Value),
}

impl From<&OutputValue> for HistoryValue {
    fn from(value: &OutputValue) -> Self {
        match value {
            OutputValue::Inline(value) => Self::Inline(value.clone()),
            OutputValue::Retained(retained) => Self::Retained(retained.into()),
        }
    }
}

impl From<&RetainedOutput> for RetainedHistoryValue {
    fn from(retained: &RetainedOutput) -> Self {
        Self {
            tag: HistoryValueTag::Retained,
            witness: retained.witness.clone(),
            byte_len: retained.reference.byte_len,
            attachment: lash_sansio::ToolValue::Attachment(
                lash_sansio::llm::types::AttachmentSource::stored(retained.reference.clone()),
            )
            .to_json_value(),
        }
    }
}

impl schemars::JsonSchema for HistoryValue {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "HistoryValue".into()
    }

    /// Any JSON value: an inline value is whatever the cell produced, and the
    /// retained record is one such object.
    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::Schema::default()
    }
}

/// A cell's failure as a later cell reads it: the closed kind and the
/// failure's own message. Recovery guidance is prompt text, not history.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct HistoryCellError {
    pub kind: CellFailureKind,
    pub message: String,
}

impl From<&CellFailure> for HistoryCellError {
    fn from(failure: &CellFailure) -> Self {
        Self {
            kind: failure.kind,
            message: failure.message.clone(),
        }
    }
}

/// The keys a step's outcome flattens into: at most one is ever written. A
/// finished cell whose value is `null` writes `final_output: null`.
#[derive(serde::Serialize, schemars::JsonSchema)]
struct HistoryStepOutcomeFields<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a HistoryCellError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    final_output: Option<&'a HistoryValue>,
}

fn serialize_history_step_outcome<S: serde::Serializer>(
    outcome: &CellOutcome<HistoryCellError, HistoryValue>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serde::Serialize::serialize(
        &HistoryStepOutcomeFields {
            error: outcome.error(),
            final_output: outcome.terminal_value(),
        },
        serializer,
    )
}

/// One item of the `history` a cell reads. This serialized shape is the
/// model-visible one: [`history_item_shape`] derives the type a prompt
/// declares from it.
#[derive(Clone, Debug, PartialEq, serde::Serialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RlmHistoryItem {
    Message {
        id: String,
        role: RlmHistoryRole,
        content: String,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<RlmAttachmentRef>,
    },
    LashlangStep {
        id: String,
        protocol_iteration: usize,
        code: String,
        output: Vec<serde_json::Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        output_archive: Option<RetainedHistoryValue>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        images: Vec<RlmImageRef>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        calls: Vec<RlmExecutedCall>,
        #[serde(skip_serializing_if = "is_zero")]
        calls_omitted: usize,
        /// What the cell resolved to: `error` for a failed cell,
        /// `final_output` for a finished one, neither for a running one.
        #[serde(flatten, serialize_with = "serialize_history_step_outcome")]
        #[schemars(with = "HistoryStepOutcomeFields<'static>")]
        outcome: CellOutcome<HistoryCellError, HistoryValue>,
    },
}

impl RlmHistoryItem {
    /// The image representations intentionally remain distinct: persisted
    /// entries retain attachment references, while history items expose the
    /// compact image metadata used by the model.
    pub fn from_trajectory_entry(entry: &RlmTrajectoryEntry) -> Self {
        Self::LashlangStep {
            id: entry.id.clone(),
            protocol_iteration: entry.protocol_iteration,
            code: entry.code.clone(),
            output: entry
                .output
                .iter()
                .map(|print| print.value.clone())
                .collect(),
            output_archive: entry
                .output_archive
                .as_deref()
                .map(RetainedHistoryValue::from),
            images: entry
                .images
                .iter()
                .map(RlmImageRef::from_attachment)
                .collect(),
            calls: entry.calls.clone(),
            calls_omitted: entry.calls_omitted,
            outcome: match &entry.outcome {
                CellOutcome::Running => CellOutcome::Running,
                CellOutcome::Failed(failure) => CellOutcome::Failed(failure.into()),
                CellOutcome::Finished(value) => CellOutcome::Finished(value.into()),
            },
        }
    }
}

/// The type of one `history` item, read from [`RlmHistoryItem`]'s own
/// serialized shape so a prompt can never declare a key the item does not
/// write or leave out one it does. A dialect spells it through its schema
/// renderer. `images` drops the step's `images` key for a session whose
/// model is shown no images.
pub fn history_item_shape(images: bool) -> SchemaShape {
    // The serialize contract: a key the item may skip is optional.
    let schema = schemars::generate::SchemaSettings::default()
        .for_serialize()
        .into_generator()
        .into_root_schema_for::<RlmHistoryItem>();
    let mut shape = SchemaShape::from_json_schema(schema.as_value());
    if !images && let ShapeKind::Union(members) = &mut shape.kind {
        for member in members {
            if let ShapeKind::Object(object) = &mut member.kind {
                object.fields.retain(|field| field.name != "images");
            }
        }
    }
    shape
}

#[cfg(test)]
mod rlm_step_serde_tests {
    use std::collections::BTreeSet;

    use lash_sansio::{CellFailure, CellFailureKind, ShapeKind};

    use super::{CellOutcome, RlmHistoryItem, RlmTrajectoryEntry};

    fn populated_entry() -> RlmTrajectoryEntry {
        RlmTrajectoryEntry {
            output_archive: None,
            id: "step-1".to_string(),
            protocol_iteration: 3,
            code: "print('hello')".to_string(),
            output: vec!["hello".to_string().into()],
            images: vec![lash_sansio::AttachmentRef {
                id: "image-1".parse().expect("valid attachment id"),
                media_type: "image/png".parse().expect("valid media type"),
                byte_len: 42,
                type_metadata: Some(lash_sansio::AttachmentTypeMetadata::image(
                    Some(640),
                    Some(480),
                )),
                label: Some("plot".to_string()),
            }],
            calls: vec![lash_sansio::ExecutedCallRecord {
                operation: "math.add".to_string(),
                outcome: lash_sansio::ExecutedCallOutcome::Ok,
            }],
            calls_omitted: 2,
            outcome: CellOutcome::Finished(serde_json::json!({"answer": 42}).into()),
        }
    }

    fn retained(witness: &str) -> lash_sansio::RetainedOutput {
        lash_sansio::RetainedOutput {
            reference: lash_sansio::AttachmentRef {
                id: "retained-1".parse().expect("valid attachment id"),
                media_type: "application/json".parse().expect("valid media type"),
                byte_len: 90_000,
                type_metadata: None,
                label: None,
            },
            witness: witness.to_string(),
        }
    }

    fn program_failure() -> CellFailure {
        CellFailure::new(
            CellFailureKind::Program,
            "ReferenceError: rows is not defined",
        )
    }

    fn history(entry: &RlmTrajectoryEntry) -> serde_json::Value {
        serde_json::to_value(RlmHistoryItem::from_trajectory_entry(entry)).expect("item encodes")
    }

    /// A retained value has one cell-visible spelling (FIG-4658 F66): an archive
    /// and a final value read back as the same reserved-tag record, whose
    /// `attachment` is a value a tool that reads attachments accepts.
    #[test]
    fn a_step_archive_and_retained_final_value_share_the_attachment_record() {
        let entry = RlmTrajectoryEntry {
            output_archive: Some(Box::new(retained("[{\"value\": {\"rows\":["))),
            output: Vec::new(),
            outcome: CellOutcome::Finished(super::OutputValue::Retained(retained("{\"rows\":["))),
            ..populated_entry()
        };
        let item = history(&entry);

        assert_eq!(item["output"], serde_json::json!([]));
        assert_eq!(
            item["output_archive"]["attachment"],
            item["final_output"]["attachment"]
        );
        assert!(item.get("final_output_retained").is_none());
        let record = &item["final_output"];
        assert_eq!(record[super::HISTORY_VALUE_TAG_KEY], "retained");
        assert_eq!(record["witness"], "{\"rows\":[");
        assert_eq!(record["byte_len"], 90_000);
        let adopted: lash_sansio::ToolValue = serde_json::from_value(record["attachment"].clone())
            .expect("the attachment decodes as a tool value");
        assert!(
            matches!(adopted, lash_sansio::ToolValue::Attachment(_)),
            "a tool adopts the record's attachment: {adopted:?}"
        );
    }

    #[test]
    fn a_print_refuses_both_or_neither_value_key_and_keeps_a_printed_null() {
        let both = serde_json::json!({
            "text": "x", "value": 1, "retained": serde_json::to_value(retained("w")).expect("encode")
        });
        assert!(serde_json::from_value::<super::RlmPrint>(both).is_err());
        assert!(
            serde_json::from_value::<super::RlmPrint>(serde_json::json!({"text": "x"})).is_err()
        );
        let null: super::RlmPrint =
            serde_json::from_value(serde_json::json!({"text": "null", "value": null}))
                .expect("decode");
        assert_eq!(null.value, serde_json::Value::Null);
    }

    /// A durable trajectory entry keeps its cell failure typed (FIG-4658
    /// F21): the `error` key holds the failure's closed kind and its own
    /// message, limits included, and no rendered guidance.
    #[test]
    fn a_failed_trajectory_entry_records_the_typed_failure() {
        for failure in [
            program_failure(),
            CellFailure::new(CellFailureKind::Policy, "top-level await is not allowed"),
            CellFailure::from(lash_sansio::ExecCodeFailure::new(
                lash_sansio::ExecCodeFailureReason::RuntimeStopped,
                "code execution runtime exited unexpectedly",
            )),
        ] {
            let entry = RlmTrajectoryEntry {
                output_archive: None,
                outcome: CellOutcome::Failed(failure.clone()),
                ..populated_entry()
            };
            let encoded = serde_json::to_value(&entry).expect("encode");
            assert_eq!(
                encoded["error"],
                serde_json::to_value(&failure).expect("failure encodes")
            );
            assert_eq!(encoded["error"]["message"], failure.message.as_str());
            assert!(encoded.get("final_output").is_none());
            let decoded: RlmTrajectoryEntry = serde_json::from_value(encoded).expect("decode");
            assert_eq!(decoded.outcome, CellOutcome::Failed(failure));
        }
    }

    #[test]
    fn an_exec_failure_keeps_its_closed_reason_in_the_trajectory() {
        let entry = RlmTrajectoryEntry {
            output_archive: None,
            outcome: CellOutcome::Failed(CellFailure::from(lash_sansio::ExecCodeFailure::new(
                lash_sansio::ExecCodeFailureReason::ExecutorUnavailable,
                "code execution is not available in this session",
            ))),
            ..populated_entry()
        };
        let encoded = serde_json::to_value(&entry).expect("encode");
        assert_eq!(
            encoded["error"],
            serde_json::json!({
                "kind": "host",
                "message": "code execution is not available in this session",
                "exec_failure": "executor_unavailable",
            })
        );
    }

    #[test]
    fn a_null_finish_is_distinct_from_a_running_cell() {
        for (outcome, expected) in [
            (
                CellOutcome::Finished(serde_json::Value::Null.into()),
                Some(serde_json::Value::Null),
            ),
            (CellOutcome::Running, None),
            (
                CellOutcome::Finished(serde_json::json!({"answer": 42}).into()),
                Some(serde_json::json!({"answer": 42})),
            ),
        ] {
            let entry = RlmTrajectoryEntry {
                outcome,
                ..populated_entry()
            };
            let encoded = serde_json::to_value(&entry).expect("encode");
            assert!(encoded.get("error").is_none());
            assert_eq!(encoded.get("final_output"), expected.as_ref());
            assert_eq!(history(&entry).get("final_output"), expected.as_ref());
            assert_eq!(
                serde_json::from_value::<RlmTrajectoryEntry>(encoded).expect("decode"),
                entry
            );
        }
    }

    #[test]
    fn malformed_outcome_presence_is_refused_in_a_trajectory_entry() {
        let failure = serde_json::to_value(program_failure()).expect("failure encodes");
        for fields in [
            serde_json::json!({"error": failure, "final_output": null}),
            serde_json::json!({"error": failure, "final_output": {"answer": 42}}),
            serde_json::json!({"error": null}),
            serde_json::json!({"error": null, "final_output": null}),
            serde_json::json!({"error": null, "final_output": 42}),
            serde_json::json!({"final_output_retained": null}),
            // The rendered-guidance string the entry once stored.
            serde_json::json!({"error": "boom\n\nNext: fix the cause named above."}),
        ] {
            let mut entry = serde_json::to_value(RlmTrajectoryEntry {
                output_archive: None,
                outcome: CellOutcome::Running,
                ..populated_entry()
            })
            .expect("encode");
            entry
                .as_object_mut()
                .expect("an entry is an object")
                .extend(fields.as_object().expect("fields").clone());
            assert!(
                serde_json::from_value::<RlmTrajectoryEntry>(entry.clone()).is_err(),
                "{entry}"
            );
            assert!(
                serde_json::from_str::<RlmTrajectoryEntry>(&entry.to_string()).is_err(),
                "{entry}"
            );
        }
    }

    /// Every history item this build can serialize, with every optional key
    /// present somewhere.
    fn history_exemplars() -> Vec<serde_json::Value> {
        let message = RlmHistoryItem::Message {
            id: "m1".to_string(),
            role: super::RlmHistoryRole::User,
            content: "hello".to_string(),
            attachments: vec![super::RlmAttachmentRef {
                id: "a1".to_string(),
                media_type: Some("text/plain".parse().expect("valid media type")),
                label: Some("notes".to_string()),
                source: "stored".to_string(),
                reference: "a1".to_string(),
            }],
        };
        let bare_message = RlmHistoryItem::Message {
            id: "m2".to_string(),
            role: super::RlmHistoryRole::Assistant,
            content: "hi".to_string(),
            attachments: Vec::new(),
        };
        let mut items = vec![
            serde_json::to_value(message).expect("message encodes"),
            serde_json::to_value(bare_message).expect("message encodes"),
            history(&populated_entry()),
            history(&RlmTrajectoryEntry {
                output_archive: None,
                id: "step-0".to_string(),
                code: "1".to_string(),
                ..RlmTrajectoryEntry::default()
            }),
        ];
        for outcome in [
            CellOutcome::Finished(serde_json::json!({"answer": 42}).into()),
            CellOutcome::Finished(super::OutputValue::Retained(retained("{\"answer\""))),
            CellOutcome::Failed(program_failure()),
            CellOutcome::Running,
        ] {
            items.push(history(&RlmTrajectoryEntry {
                output_archive: Some(Box::new(retained("ordered step observations"))),
                output: Vec::new(),
                outcome,
                ..populated_entry()
            }));
        }
        items
    }

    /// The model-visible `HistoryItem` type is read from the item's own
    /// serialized shape (FIG-4658 F66): every key an item writes is declared
    /// for its `kind`, every required key is always written, and the
    /// declaration names no key that no item writes.
    #[test]
    fn the_declared_history_item_shape_is_the_shape_items_serialize_as() {
        let shape = super::history_item_shape(true);
        let ShapeKind::Union(members) = &shape.kind else {
            panic!("a history item is one of its kinds: {shape:?}");
        };
        let exemplars = history_exemplars();
        let mut kinds = BTreeSet::new();
        for member in members {
            let fields = member.fields();
            let kind_field = fields
                .iter()
                .find(|field| field.name == "kind")
                .expect("every kind declares its tag");
            let ShapeKind::Literals(literals) = &kind_field.shape.kind else {
                panic!("the tag is a literal: {kind_field:?}");
            };
            let [kind] = literals.as_slice() else {
                panic!("one tag per kind: {literals:?}");
            };
            kinds.insert(kind.as_str().expect("a string tag").to_string());
            let of_kind = exemplars
                .iter()
                .filter(|item| &item["kind"] == kind)
                .map(|item| item.as_object().expect("an item is an object"))
                .collect::<Vec<_>>();
            assert!(!of_kind.is_empty(), "no exemplar of kind {kind}");
            let declared = fields
                .iter()
                .map(|field| field.name.as_str())
                .collect::<BTreeSet<_>>();
            let written = of_kind
                .iter()
                .flat_map(|item| item.keys().map(String::as_str))
                .collect::<BTreeSet<_>>();
            assert_eq!(declared, written, "kind {kind}");
            for field in fields.iter().filter(|field| field.required) {
                assert!(
                    of_kind.iter().all(|item| item.contains_key(&field.name)),
                    "kind {kind} declares `{}` required",
                    field.name
                );
            }
        }
        assert_eq!(
            kinds,
            exemplars
                .iter()
                .map(|item| item["kind"].as_str().expect("a string tag").to_string())
                .collect()
        );
    }

    #[test]
    fn a_session_without_images_declares_no_images_key() {
        let shape = super::history_item_shape(false);
        let ShapeKind::Union(members) = &shape.kind else {
            panic!("a history item is one of its kinds: {shape:?}");
        };
        assert!(
            members
                .iter()
                .flat_map(|member| member.fields())
                .all(|field| field.name != "images")
        );
    }
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct RlmGlobalsPatchPluginBody {
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub set_default: serde_json::Map<String, serde_json::Value>,
}

impl RlmGlobalsPatchPluginBody {
    pub fn is_empty(&self) -> bool {
        self.set_default.is_empty()
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub enum RlmProtocolEvent {
    RlmAssistantContent(RlmAssistantContent),
    RlmTrajectoryEntry(RlmTrajectoryEntry),
    RlmGlobalsPatch(RlmGlobalsPatchPluginBody),
    RlmSeed(RlmSeedPluginBody),
    RlmDiagnostic(RlmDiagnosticEvent),
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct RlmDiagnosticEvent {
    pub phase: String,
    #[serde(default)]
    pub payload: serde_json::Value,
}

/// How an RLM turn may end, and what an explicit finish value must match.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RlmTermination {
    /// Prose alone never ends the turn: only `finish` does, with a value
    /// matching `schema` when one is stated.
    FinishRequired {
        schema: Option<lash_sansio::JsonSchema>,
    },
    /// Prose ends the turn as the answer, and so does `finish`. A finish value
    /// must match `schema` when one is stated; a mismatch fails the program
    /// and asks the model to finish again (FIG-5104).
    Natural {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<lash_sansio::JsonSchema>,
    },
}

impl Default for RlmTermination {
    fn default() -> Self {
        Self::Natural { schema: None }
    }
}

impl RlmTermination {
    /// Whether a prose-only reply ends the turn as its answer.
    pub fn prose_ends_turn(&self) -> bool {
        matches!(self, Self::Natural { .. })
    }

    /// The schema an explicit finish value must match, if any.
    pub fn finish_schema(&self) -> Option<&lash_sansio::JsonSchema> {
        match self {
            Self::FinishRequired { schema } | Self::Natural { schema } => schema.as_ref(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RlmFinalAnswerFormat {
    Markdown,
    Custom { guidance: String },
    RawFinalValue,
}

/// RLM protocol session config. Natural turns finish with prose-only model
/// responses or the RLM language's explicit `finish` operation. Programmatic
/// turns can require an explicit finish value. Either termination can validate
/// a finish value against a schema.
/// `final_answer_format` is a session presentation preference; schema-required
/// turns ignore it.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RlmCreateExtras {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render: Option<RlmRenderPatch>,
    /// Session-wide termination requirement. Absence is the `Natural` default.
    ///
    /// Absence is a distinct statement from an explicit `Natural`: options that
    /// say nothing about termination must leave a recorded `FinishRequired`
    /// alone, and only a value that is genuinely stated participates in the
    /// set-if-unset guard (ADR 0066).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub termination: Option<RlmTermination>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_answer_format: Option<RlmFinalAnswerFormat>,
}

/// The RLM options a *single turn* may state again (FIG-1979).
///
/// There is currently no language choice to state: TypeScript is the only shipped RLM dialect
/// and nothing — a turn bag, a session bag, a create contract — names one.
///
/// These are the RLM owner's run options: the owner applies each stated
/// field over the session's recorded value, and an unstated one leaves it
/// alone. Nothing else is a field, so a payload that names a session pin
/// does not decode (FIG-4652).
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RlmTurnOptions {
    /// Termination requirement for this turn. Absence is the `Natural`
    /// default, and leaves whatever the session recorded alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub termination: Option<RlmTermination>,
    /// Presentation preference for this turn's final answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_answer_format: Option<RlmFinalAnswerFormat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render: Option<RlmRenderPatch>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RlmRenderPatch {
    #[serde(
        default,
        skip_serializing_if = "lash_render::RenderParamsPatch::is_empty"
    )]
    pub print: lash_render::RenderParamsPatch,
    #[serde(
        default,
        skip_serializing_if = "lash_render::RenderParamsPatch::is_empty"
    )]
    pub preview: lash_render::RenderParamsPatch,
}

impl RlmTurnOptions {
    /// The termination this bag means, resolving absence to the default.
    pub fn effective_termination(&self) -> RlmTermination {
        self.termination.clone().unwrap_or_default()
    }
}

/// The durable RLM facts a session has recorded, read as recorded.
///
/// Every field is `Option`-shaped on purpose: `None` means the session has
/// stated nothing yet, which is a different answer from the value the default
/// resolves to. A host that cannot tell those apart has to peek at raw payload
/// keys to label a fresh session honestly — the hack this type replaces.
///
/// The facts are recorded once, when the session is created (FIG-4379): no
/// config command changes them, and a turn states them again through its run's
/// protocol turn options.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RlmSessionConfig {
    pub final_answer_format: Option<RlmFinalAnswerFormat>,
    pub termination: Option<RlmTermination>,
}

impl RlmSessionConfig {
    /// An empty request, stating nothing.
    pub fn new() -> Self {
        Self::default()
    }

    pub fn final_answer_format(mut self, format: RlmFinalAnswerFormat) -> Self {
        self.final_answer_format = Some(format);
        self
    }

    pub fn termination(mut self, termination: RlmTermination) -> Self {
        self.termination = Some(termination);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.final_answer_format.is_none() && self.termination.is_none()
    }
}

impl From<&RlmCreateExtras> for RlmSessionConfig {
    fn from(extras: &RlmCreateExtras) -> Self {
        Self {
            final_answer_format: extras.final_answer_format.clone(),
            termination: extras.termination.clone(),
        }
    }
}

impl From<&RlmSessionConfig> for RlmCreateExtras {
    fn from(config: &RlmSessionConfig) -> Self {
        Self {
            render: None,
            termination: config.termination.clone(),
            final_answer_format: config.final_answer_format.clone(),
        }
    }
}

/// One durable projected seed binding.
///
/// The explicit tag keeps the entry disjoint from ordinary JSON: materialized
/// data can spell any object keys — `kind` and `value` included — without
/// acquiring entry semantics during restore.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum RlmProjectedSeedEntry {
    Materialized(serde_json::Value),
}

/// Wire-format snapshot of a set of projected bindings. Pairs of
/// `(name, entry)` get re-projected as host bindings on the child session at
/// creation time. This is the serializable form of
/// `lash_protocol_rlm::RlmProjectedBindings`; lash-rlm-types stays free of any
/// runtime dependency on lashlang itself.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RlmProjectedSeedSnapshot {
    pub entries: Vec<(String, RlmProjectedSeedEntry)>,
}

impl RlmProjectedSeedSnapshot {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, name: impl Into<String>, entry: RlmProjectedSeedEntry) {
        self.entries.push((name.into(), entry));
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RlmSeedPluginBody {
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub globals: serde_json::Map<String, serde_json::Value>,
    #[serde(default, skip_serializing_if = "RlmProjectedSeedSnapshot::is_empty")]
    pub projected: RlmProjectedSeedSnapshot,
}

impl RlmSeedPluginBody {
    pub fn is_empty(&self) -> bool {
        self.globals.is_empty() && self.projected.is_empty()
    }
}

/// Reserved JSON key used as the canonical wire encoding for
/// `lashlang::Value::Projected` across the lashlang→host bridge. When the
/// model passes a projected source as a tool argument, lashlang serializes it
/// as `{"__projected__": <tagged seed entry>}`.
pub const PROJECTED_JSON_TAG: &str = "__projected__";

#[cfg(test)]
mod projected_seed_tests {
    use super::*;

    #[test]
    fn projected_seed_entry_serde_is_tagged_and_rejects_the_legacy_shape() {
        let entry = RlmProjectedSeedEntry::Materialized(serde_json::json!({
            "__projection_ref__": {
                "kind": "memory",
                "key": "data",
            }
        }));

        assert_eq!(
            serde_json::to_value(&entry).expect("serialize seed entry"),
            serde_json::json!({
                "kind": "materialized",
                "value": {
                    "__projection_ref__": {
                        "kind": "memory",
                        "key": "data",
                    }
                }
            })
        );
        assert!(
            serde_json::from_value::<RlmProjectedSeedEntry>(serde_json::json!({
                "__projection_ref__": {
                    "kind": "memory",
                    "key": "data",
                }
            }))
            .is_err(),
            "the untagged legacy seed entry must not decode"
        );
        assert!(
            serde_json::from_value::<RlmProjectedSeedEntry>(serde_json::json!({
                "kind": "ref",
                "value": {
                    "kind": "memory",
                    "key": "data",
                }
            }))
            .is_err(),
            "a durable projection ref must not decode"
        );
    }
}

#[derive(Clone, Debug)]
pub struct RlmTurnProtocol;

impl TurnProtocol for RlmTurnProtocol {
    type IntentOutcome = ();
    type Event = RlmProtocolEvent;
    type Termination = RlmTermination;
    type DriverState = serde_json::Value;
}
