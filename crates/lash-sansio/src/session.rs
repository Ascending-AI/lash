use crate::llm::types::AttachmentSource;
use crate::{AttachmentRef, ToolCallRecord};

/// One source-level dispatch, optionally joined to the host tool record it produced.
///
/// `operation` and `outcome` are the model-safe execution ledger. Host records
/// are attached only when the source dispatch resolved to a host tool call;
/// trigger and other host-internal dispatches therefore carry `None`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ExecutedCall {
    pub operation: String,
    pub outcome: ExecutedCallOutcome,
    pub host_record: Option<ToolCallRecord>,
}

/// Compact source-level record of an effect the embedded executor actually ran.
///
/// Unlike [`ToolCallRecord`], this deliberately carries neither arguments nor
/// host-operation details. Protocols can safely replay it to a model as an
/// execution ledger without exposing inputs or confusing a source module call
/// with the host tool it resolved to.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct ExecutedCallRecord {
    pub operation: String,
    pub outcome: ExecutedCallOutcome,
}

/// Typed accounting for host tool records omitted from a bounded turn view.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct OmittedToolCalls {
    pub count: usize,
    pub failures: usize,
    pub attachments: Vec<AttachmentSource>,
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ExecutedCallOutcome {
    Ok,
    Err,
}

impl ExecutedCallOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Err => "err",
        }
    }
}

#[derive(
    Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq, schemars::JsonSchema,
)]
pub struct TextProjectionMetadata {
    pub truncated: bool,
    pub original_chars: usize,
    pub projected_chars: usize,
    pub limit_chars: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DegradedBinding {
    pub name: String,
    pub reason: String,
}

/// Why an executed code cell failed.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum CellFailureKind {
    /// The runtime refused the program because it violates a dialect or policy bound.
    Policy,
    /// The authored program failed to compile or execute correctly.
    Program,
    /// Host infrastructure failed while preparing or executing the program.
    Host,
}

/// Structured failure returned by a code executor.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct CellFailure {
    pub kind: CellFailureKind,
    pub message: String,
    /// The measured run limit, kept typed through the plugin and host result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_limit: Option<crate::worker_limit::WorkerLimit>,
    /// The session's `max_tool_calls` refusal, when that is why the cell
    /// failed (FIG-4546), kept typed through the plugin and host result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_limit: Option<crate::session_model::ToolCallLimitExceeded>,
    /// Why the `exec_code` effect failed before the executor produced a
    /// response, when that is the failure: the closed reason, kept typed
    /// through the protocol driver and the trajectory it records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec_failure: Option<ExecCodeFailureReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_admission: Option<Box<crate::SchemaAdmissionError>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_mismatch: Option<Box<crate::ValueMismatch>>,
}

impl CellFailure {
    pub fn new(kind: CellFailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            worker_limit: None,
            tool_call_limit: None,
            exec_failure: None,
            schema_admission: None,
            value_mismatch: None,
        }
    }

    pub fn with_value_mismatch(mut self, source: crate::ValueMismatch) -> Self {
        self.value_mismatch = Some(Box::new(source));
        self
    }

    pub fn with_schema_admission(mut self, source: crate::SchemaAdmissionError) -> Self {
        self.schema_admission = Some(Box::new(source));
        self
    }

    pub fn with_worker_limit(mut self, limit: crate::worker_limit::WorkerLimit) -> Self {
        self.worker_limit = Some(limit);
        self
    }

    pub fn with_tool_call_limit(
        mut self,
        exceeded: crate::session_model::ToolCallLimitExceeded,
    ) -> Self {
        self.tool_call_limit = Some(exceeded);
        self
    }
}

/// Why an `exec_code` effect failed before the executor produced a response.
///
/// This is the closed classification offline trace analysis keys on; the human
/// detail lives in [`ExecCodeFailure::message`]. Serialized snake_case on the
/// durable journal and trace surfaces.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ExecCodeFailureReason {
    /// No code executor is installed on the session.
    ExecutorUnavailable,
    /// The executor's language runtime exited unexpectedly.
    RuntimeStopped,
    /// Any other session-layer failure; `message` carries the detail.
    Session,
    /// A journal entry written before the reason was typed: only the human
    /// message survived the erasure, so no closed reason can be recovered.
    Erased,
}

/// Journaled failure of an `exec_code` effect. `reason` is the closed
/// classification; `message` is the human-readable detail for operators.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ExecCodeFailure {
    pub reason: ExecCodeFailureReason,
    pub message: String,
}

impl ExecCodeFailure {
    pub fn new(reason: ExecCodeFailureReason, message: impl Into<String>) -> Self {
        Self {
            reason,
            message: message.into(),
        }
    }
}

/// An `exec_code` effect that failed before the executor answered is a host
/// failure of the cell; the closed reason travels with it.
impl From<ExecCodeFailure> for CellFailure {
    fn from(failure: ExecCodeFailure) -> Self {
        Self {
            exec_failure: Some(failure.reason),
            ..Self::new(CellFailureKind::Host, failure.message)
        }
    }
}

// Journal entries written before the failure was typed journaled only the
// erased message string; those still decode, with the honest `Erased` reason.
impl<'de> serde::Deserialize<'de> for ExecCodeFailure {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        struct Typed {
            reason: ExecCodeFailureReason,
            message: String,
        }
        #[derive(serde::Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Typed(Typed),
            Erased(String),
        }
        Ok(match Repr::deserialize(deserializer)? {
            Repr::Typed(Typed { reason, message }) => Self { reason, message },
            Repr::Erased(message) => Self::new(ExecCodeFailureReason::Erased, message),
        })
    }
}

/// One printed value as a cell's executor reports it: the rendered text, the
/// typed value — or its retention when its encoding was too long for history
/// (FIG-1643) — and how the text was projected.
///
/// Serialized as `{text, value, projection}` for an inline value, the shape
/// recorded payloads already carry, and `{text, retained, projection}` for a
/// retained one; decoding refuses both or neither.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observation {
    pub text: String,
    pub value: crate::OutputValue,
    pub projection: TextProjectionMetadata,
}

impl serde::Serialize for Observation {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(serde::Serialize)]
        struct Fields<'a> {
            text: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            value: Option<&'a serde_json::Value>,
            #[serde(skip_serializing_if = "Option::is_none")]
            retained: Option<&'a crate::RetainedOutput>,
            projection: &'a TextProjectionMetadata,
        }
        Fields {
            text: &self.text,
            value: self.value.inline(),
            retained: self.value.retained(),
            projection: &self.projection,
        }
        .serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for Observation {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        /// A present `value` of `null` is the printed `null`.
        fn present<'de, D: serde::Deserializer<'de>>(
            deserializer: D,
        ) -> Result<Option<serde_json::Value>, D::Error> {
            <serde_json::Value as serde::Deserialize>::deserialize(deserializer).map(Some)
        }
        #[derive(serde::Deserialize)]
        #[serde(expecting = "struct Observation")]
        struct Fields {
            text: String,
            #[serde(default, deserialize_with = "present")]
            value: Option<serde_json::Value>,
            #[serde(default)]
            retained: Option<crate::RetainedOutput>,
            projection: TextProjectionMetadata,
        }
        let Fields {
            text,
            value,
            retained,
            projection,
        } = Fields::deserialize(deserializer)?;
        let value = match (value, retained) {
            (Some(value), None) => crate::OutputValue::Inline(value),
            (None, Some(retained)) => crate::OutputValue::Retained(retained),
            (Some(_), Some(_)) => {
                return Err(serde::de::Error::custom(
                    "an observation cannot carry both `value` and `retained`",
                ));
            }
            (None, None) => return Err(serde::de::Error::missing_field("value")),
        };
        Ok(Self {
            text,
            value,
            projection,
        })
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ExecResponse {
    pub observations: Vec<Observation>,
    pub calls: Vec<ExecutedCall>,
    pub printed_images: Vec<AttachmentRef>,
    pub error: Option<CellFailure>,
    /// Bindings that could not be restored to a live host reference during
    /// executor setup. The executor leaves each binding loudly unavailable;
    /// the host decides whether to warn, repair, or abort.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub degraded_bindings: Vec<DegradedBinding>,
    /// When the surrounding session uses protocol-specific finish behavior,
    /// this carries the protocol's terminal value. The dispatch loop uses it
    /// as the terminal result of the session. `None` for chat-style sessions
    /// and for typed sessions whose step continued without finishing.
    pub terminal_finish: Option<serde_json::Value>,
    /// The retention of `terminal_finish` when its encoding was too long for
    /// history (FIG-1643): history records this in the value's place, and the
    /// value itself is the turn's answer. `None` whenever `terminal_finish`
    /// is, and for a value history keeps inline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_finish_retained: Option<crate::RetainedOutput>,
    /// The cell stopped at a segment boundary inside it (FIG-4739): a durable
    /// wait it issued was handed to the Run's successor segment, and the
    /// executor holds the cell's state for the execution that resumes it. A
    /// suspended response is not the cell's answer: nothing in it enters
    /// history, and the turn that receives it ends at the boundary.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub suspended: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FIG-2362: journal entries written before the failure was typed carry only
    /// the erased message string; they still decode, under the honest `erased`
    /// reason.
    #[test]
    fn legacy_erased_exec_code_failure_still_decodes() {
        let legacy = serde_json::json!("code execution is not available in this session");
        let decoded: ExecCodeFailure =
            serde_json::from_value(legacy).expect("legacy erased failure decodes");
        assert_eq!(decoded.reason, ExecCodeFailureReason::Erased);
        assert_eq!(
            decoded.message,
            "code execution is not available in this session"
        );

        let typed = ExecCodeFailure::new(ExecCodeFailureReason::RuntimeStopped, "boom");
        let round_trip: ExecCodeFailure =
            serde_json::from_value(serde_json::to_value(&typed).expect("encode typed failure"))
                .expect("typed failure decodes");
        assert_eq!(round_trip, typed);
        assert_eq!(round_trip.reason, ExecCodeFailureReason::RuntimeStopped);
    }

    #[test]
    fn legacy_exec_response_payload_with_images_field_still_decodes() {
        let legacy_json = serde_json::json!({
            "observations": [{
                "text": "step output",
                "value": "step output",
                "projection": {
                    "truncated": false,
                    "original_chars": 11,
                    "projected_chars": 11,
                    "limit_chars": 51200
                }
            }],
            "calls": [],
            "images": [
                {
                    "mime": "image/png",
                    "label": "legacy_image",
                    "data": [1, 2, 3]
                }
            ],
            "printed_images": [],
            "error": null,
            "duration_ms": 42,
            "terminal_finish": null
        });

        let response: ExecResponse = serde_json::from_value(legacy_json)
            .expect("legacy ExecResponse payload with images field should decode");
        assert_eq!(response.observations[0].text, "step output");
    }

    #[test]
    fn paired_observation_lists_are_rejected() {
        let mut paired_json = serde_json::json!({
            "observations": ["step output"],
            "calls": [],
            "printed_images": [],
            "error": null,
            "duration_ms": 42,
            "terminal_finish": null
        });
        paired_json.as_object_mut().unwrap().insert(
            "observation_truncation".to_string(),
            serde_json::json!([{
                "truncated": false,
                "original_chars": 11,
                "projected_chars": 11,
                "original_lines": 1,
                "projected_lines": 1,
                "limit": 51200,
                "limit_mode": "bytes",
                "max_lines": 2000
            }]),
        );

        let error = serde_json::from_value::<ExecResponse>(paired_json)
            .expect_err("paired observation lists must not decode after the hard cutover");
        assert!(
            error.to_string().contains("expected struct Observation"),
            "unexpected decode error: {error}"
        );
    }

    #[test]
    fn two_list_exec_response_payload_is_rejected() {
        let legacy_json = serde_json::json!({
            "observations": [],
            "tool_calls": [],
            "executed_calls": [],
            "printed_images": [],
            "error": null,
            "duration_ms": 42,
            "degraded_bindings": [],
            "terminal_finish": null
        });

        let error = serde_json::from_value::<ExecResponse>(legacy_json)
            .expect_err("the pre-cutover two-list ExecResponse must be refused");
        assert!(
            error.to_string().contains("missing field `calls`"),
            "unexpected decode error: {error}"
        );
    }
}
