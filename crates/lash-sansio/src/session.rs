use crate::{AttachmentRef, ToolCallId, ToolCallRecord};

/// One source-level dispatch a code cell executed.
///
/// `operation` and `outcome` are what the cell ran and how it settled.
/// `call_id` names the [`ToolCallRecord`] of the host tool call the dispatch
/// resolved to: that record, keyed by the same id, carries the tool, its
/// arguments and its output. `None` only for a dispatch lash handled itself,
/// with no host tool call.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct ExecutedCall {
    pub operation: String,
    pub outcome: ExecutedCallOutcome,
    pub call_id: Option<ToolCallId>,
}

/// Typed accounting for host tool records omitted from a bounded turn view.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct OmittedToolCalls {
    pub count: usize,
    pub failures: usize,
    pub attachments: Vec<AttachmentRef>,
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
    /// The rule of a session's cells the program broke, when that is why
    /// the cell failed, kept typed through the plugin and host result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defect: Option<CellDefect>,
}

/// A rule of a session's cells that a cell's program broke.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CellDefect {
    /// The cell used a binding an earlier cell left that held a function or
    /// a task: neither outlives the cell that created it, so the binding
    /// was not carried.
    BindingNotCarried { binding: String },
    /// The cell ended with tasks it started and did not await: some still
    /// running, some failed with an error nothing observed. Each is named
    /// by the site that started it and which run of that site it was.
    TasksOutstanding {
        unfinished: Vec<String>,
        unobserved: Vec<String>,
    },
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
            defect: None,
        }
    }

    pub fn with_defect(mut self, defect: CellDefect) -> Self {
        self.defect = Some(defect);
        self
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

/// One print of an executed cell: the text the model read, the value
/// printed, and how the text was projected from it. The one shape a print
/// has in the executor's response, in a cell's committed record, in a step's
/// attachment archive and in the completion activity.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CellPrint {
    pub text: String,
    pub value: serde_json::Value,
    pub projection: TextProjectionMetadata,
}

/// A print of the text itself, shown whole.
impl From<String> for CellPrint {
    fn from(text: String) -> Self {
        let chars = text.chars().count();
        Self {
            value: serde_json::Value::String(text.clone()),
            text,
            projection: TextProjectionMetadata {
                truncated: false,
                original_chars: chars,
                projected_chars: chars,
                limit_chars: chars,
            },
        }
    }
}

/// What an executed cell resolved to. One of three: a cell never carries a
/// failure and a finish value at once, and a cell that finished with `null`
/// is distinct from one that ran to its end without finishing.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum CellOutcome {
    /// The cell ran to its end without a finish value.
    #[default]
    Completed,
    /// The cell failed.
    Failed(CellFailure),
    /// The cell finished its turn with this value: inline, or retained out
    /// of history when its encoding was too long for it (FIG-1643).
    #[serde(with = "finish_value")]
    Finished(crate::OutputValue),
}

impl CellOutcome {
    /// The failure, when the cell failed.
    pub fn failure(&self) -> Option<&CellFailure> {
        match self {
            Self::Failed(failure) => Some(failure),
            Self::Completed | Self::Finished(_) => None,
        }
    }

    pub fn is_failed(&self) -> bool {
        matches!(self, Self::Failed(_))
    }

    /// The finish value as history records it, when the cell finished.
    pub fn finish(&self) -> Option<&crate::OutputValue> {
        match self {
            Self::Finished(value) => Some(value),
            Self::Completed | Self::Failed(_) => None,
        }
    }
}

/// [`crate::OutputValue`] as a cell result spells it: `{"inline": value}` or
/// `{"retained": retention}`.
mod finish_value {
    use crate::{OutputValue, RetainedOutput};
    use serde::{Deserialize, Serialize};

    #[derive(Serialize)]
    #[serde(rename_all = "snake_case")]
    enum Borrowed<'a> {
        Inline(&'a serde_json::Value),
        Retained(&'a RetainedOutput),
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum Owned {
        Inline(serde_json::Value),
        Retained(RetainedOutput),
    }

    pub fn serialize<S: serde::Serializer>(
        value: &OutputValue,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            OutputValue::Inline(value) => Borrowed::Inline(value),
            OutputValue::Retained(retained) => Borrowed::Retained(retained),
        }
        .serialize(serializer)
    }

    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<OutputValue, D::Error> {
        Ok(match Owned::deserialize(deserializer)? {
            Owned::Inline(value) => OutputValue::Inline(value),
            Owned::Retained(retained) => OutputValue::Retained(retained),
        })
    }
}

/// What a cell's committed transition did to its session's bindings, by
/// name. A cell whose run failed, or whose result was discarded, committed
/// no transition and reports none; a cell the protocol adjudicated as failed
/// after its run ended keeps what that run committed.
///
/// Each list is in name order and a name is in at most one of them. A record
/// keeps as many names as its session's recorded bound allows, taken in the
/// order `added`, `changed`, `removed`, `not_carried`, and counts the rest
/// in `omitted`.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BindingChanges {
    /// Bound after the cell and not before it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub added: Vec<String>,
    /// Bound before and after the cell, to different data.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed: Vec<String>,
    /// Bound before the cell and not after it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed: Vec<String>,
    /// Left by the cell holding a value a session does not carry (a function
    /// or a task): not bound after it, whatever it held before.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub not_carried: Vec<String>,
    /// Names beyond the recorded bound.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub omitted: usize,
}

impl BindingChanges {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.changed.is_empty()
            && self.removed.is_empty()
            && self.not_carried.is_empty()
            && self.omitted == 0
    }

    /// These changes with at most `max_names` names kept and the rest
    /// counted.
    #[must_use]
    pub fn bounded(mut self, max_names: usize) -> Self {
        let mut room = max_names;
        for names in [
            &mut self.added,
            &mut self.changed,
            &mut self.removed,
            &mut self.not_carried,
        ] {
            let kept = names.len().min(room);
            self.omitted += names.len() - kept;
            names.truncate(kept);
            room -= kept;
        }
        self
    }
}

/// One executed code cell, as committed history records it and a transcript
/// returns it: the protocol that ran the cell appends this record, and a
/// reader decodes the same record back. Its prints and result are the
/// executor's [`ExecResponse`] values, unconverted; the result differs from
/// the response's only where the protocol adjudicated it (a finish value its
/// declared schema refuses is recorded as the failure it is).
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CellRecord {
    /// The cell's identity within its session. A committed message that
    /// carries the cell's assistant context names it in its origin.
    pub id: String,
    pub protocol_iteration: usize,
    pub language: String,
    pub code: String,
    /// Inline prints, in order; empty when `prints_retained` holds them.
    pub prints: Vec<CellPrint>,
    /// One archive of every print, when they exceeded the inline limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prints_retained: Option<crate::RetainedOutput>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<AttachmentRef>,
    /// The dispatches the cell executed. An entry's `call_id` is the id of
    /// the host tool call's own [`ToolCallRecord`]; `None` for a dispatch
    /// lash handled with no host tool call.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub calls: Vec<ExecutedCall>,
    /// Calls the cell made beyond the recorded `calls`.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub calls_omitted: usize,
    /// What the cell's committed transition did to the session's bindings.
    #[serde(default, skip_serializing_if = "BindingChanges::is_empty")]
    pub bindings: BindingChanges,
    pub result: CellOutcome,
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ExecResponse {
    /// Inline prints, in order; empty when `prints_retained` holds them.
    pub prints: Vec<CellPrint>,
    /// The complete ordered prints when their aggregate exceeds the history
    /// limit. Inline prints are empty whenever this is present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prints_retained: Option<crate::RetainedOutput>,
    /// Every source-level dispatch the cell executed, in execution order.
    pub calls: Vec<ExecutedCall>,
    /// The record of each host tool call the cell made, in execution order.
    /// A `calls` entry references its record by `call_id`.
    pub tool_calls: Vec<ToolCallRecord>,
    pub printed_images: Vec<AttachmentRef>,
    /// What the cell resolved to, as history records it. A finish value is
    /// the surrounding protocol's terminal value: the dispatch loop uses it
    /// as the terminal result of the session.
    pub result: CellOutcome,
    /// The finish value itself when `result` records only its retention
    /// (FIG-1643): history keeps the retention, and the value stays the
    /// turn's answer. `None` for every other result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retained_finish_value: Option<serde_json::Value>,
    /// Bindings that could not be restored to a live host reference during
    /// executor setup. The executor leaves each binding loudly unavailable;
    /// the host decides whether to warn, repair, or abort.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub degraded_bindings: Vec<DegradedBinding>,
    /// What the cell did to the session's bindings, every name: empty unless
    /// the cell's run ended and the session took what it left.
    #[serde(default, skip_serializing_if = "BindingChanges::is_empty")]
    pub bindings: Box<BindingChanges>,
    /// The cell stopped at a segment boundary inside it (FIG-4739): a durable
    /// wait it issued was handed to the Run's successor segment, and the
    /// executor holds the cell's state for the execution that resumes it. A
    /// suspended response is not the cell's answer: nothing in it enters
    /// history, and the turn that receives it ends at the boundary.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub suspended: bool,
}

impl ExecResponse {
    /// The failure, when the cell failed.
    pub fn error(&self) -> Option<&CellFailure> {
        self.result.failure()
    }

    /// The finish value itself, whether history keeps it inline or retained.
    pub fn finish_value(&self) -> Option<&serde_json::Value> {
        match &self.result {
            CellOutcome::Finished(crate::OutputValue::Inline(value)) => Some(value),
            CellOutcome::Finished(crate::OutputValue::Retained(_)) => {
                self.retained_finish_value.as_ref()
            }
            CellOutcome::Completed | CellOutcome::Failed(_) => None,
        }
    }
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

    fn retained(witness: &str) -> crate::RetainedOutput {
        crate::RetainedOutput {
            reference: AttachmentRef {
                id: ("b".repeat(64)).parse().expect("valid attachment id"),
                media_type: "application/json".parse().expect("valid media type"),
                byte_len: 90_000,
                type_metadata: None,
                label: None,
            },
            witness: witness.to_string(),
        }
    }

    /// A cell record spells its result as one tagged value (FIG-5527): a
    /// `null` finish, a retained finish, a typed failure and a cell that ran
    /// to its end each read back as themselves, and a result carrying a
    /// failure beside a finish value has no spelling.
    #[test]
    fn a_cell_record_reads_back_each_result_as_itself() {
        let failure = CellFailure::from(ExecCodeFailure::new(
            ExecCodeFailureReason::ExecutorUnavailable,
            "code execution is not available in this session",
        ));
        for result in [
            CellOutcome::Completed,
            CellOutcome::Finished(serde_json::Value::Null.into()),
            CellOutcome::Finished(serde_json::json!({"answer": 42}).into()),
            CellOutcome::Finished(crate::OutputValue::Retained(retained("{\"rows\":["))),
            CellOutcome::Failed(failure.clone()),
        ] {
            let record = CellRecord {
                id: "step-1".to_string(),
                protocol_iteration: 3,
                language: "typescript".to_string(),
                code: "print('hello')".to_string(),
                prints: vec!["hello".to_string().into()],
                result: result.clone(),
                ..CellRecord::default()
            };
            let encoded = serde_json::to_value(&record).expect("encode");
            let decoded: CellRecord = serde_json::from_value(encoded.clone()).expect("decode");
            assert_eq!(decoded, record, "{encoded}");
        }
        assert_eq!(
            serde_json::to_value(CellOutcome::Failed(failure)).expect("encode"),
            serde_json::json!({"kind": "failed", "value": {
                "kind": "host",
                "message": "code execution is not available in this session",
                "exec_failure": "executor_unavailable",
            }})
        );
        assert_eq!(
            serde_json::to_value(CellOutcome::Finished(serde_json::Value::Null.into()))
                .expect("encode"),
            serde_json::json!({"kind": "finished", "value": {"inline": null}})
        );
        for malformed in [
            serde_json::json!({"kind": "finished"}),
            serde_json::json!({"kind": "finished", "value": {"inline": 1, "retained": null}}),
            serde_json::json!({"kind": "running"}),
        ] {
            assert!(
                serde_json::from_value::<CellOutcome>(malformed.clone()).is_err(),
                "{malformed}"
            );
        }
    }
}
