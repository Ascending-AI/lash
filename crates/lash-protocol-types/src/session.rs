use crate::{AttachmentRef, ToolCallId, ToolCallRecord};

/// One source-level dispatch a code cell executed.
///
/// `operation` and `outcome` are what the cell ran and how it settled.
/// `call_id` names the [`ToolCallRecord`] of the host tool call the dispatch
/// resolved to. `outcome` carries that record's settle verdict: success or
/// its failure code and a UTF-8 prefix of its message, at most
/// [`ExecutedCallOutcome::MAX_MESSAGE_BYTES`] bytes. A host can render a
/// mirrored cell's refusal without loading each call body.
/// The record, keyed by the same id, keeps the tool, arguments, whole message
/// and typed cause. Hosts read it through `DurableSession::read_tool_call`, pairing
/// `call_id` with the enclosing transcript entry's turn provenance, or through
/// `DurableSession::read_cell_tool_calls` for the whole cell.
/// `None` only for a dispatch lash handled itself,
/// with no host tool call.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct ExecutedCall {
    pub operation: String,
    pub outcome: ExecutedCallOutcome,
    pub call_id: Option<ToolCallId>,
    /// A presentation projection of the call's committed result, never its
    /// source of truth: the [`ToolCallRecord`] under `call_id` is. The
    /// protocol that commits the cell's record derives it from that record,
    /// in the same commit, and never again. `None` in an executor's
    /// response, and for a dispatch with no committed host record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_summary: Option<CallResultSummary>,
}

/// A bounded prefix of a committed call result's canonical compact JSON,
/// cut at a UTF-8 boundary, as a cell's next prompt shows it.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct CallResultSummary {
    pub text: String,
    /// Whether `text` is a cut prefix of the whole result.
    pub truncated: bool,
}

/// A cell dispatch's settled verdict, derived from its committed call output.
/// Failure details have no spelling on a successful verdict.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutedCallOutcome {
    Ok {},
    /// The committed failure's code and bounded message. A cancelled call
    /// uses code `cancelled` and its recorded cancellation message.
    Failed {
        code: String,
        message: String,
    },
}

impl ExecutedCallOutcome {
    /// The fixed byte cap for a call verdict's failure message. The call
    /// ledger retains the whole message; this projection keeps its longest
    /// UTF-8 prefix within 4 KiB, without an added omission marker.
    pub const MAX_MESSAGE_BYTES: usize = 4096;

    /// The short status used in the model's calls block.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ok {} => "ok",
            Self::Failed { .. } => "err",
        }
    }
}

impl From<&crate::ToolCallOutcome> for ExecutedCallOutcome {
    fn from(outcome: &crate::ToolCallOutcome) -> Self {
        let (code, message) = match outcome {
            crate::ToolCallOutcome::Success(_) => return Self::Ok {},
            crate::ToolCallOutcome::Failure(failure) => {
                (failure.code.as_str(), failure.model_text())
            }
            // A cancellation has no failure code in the call body. Keep its
            // message with an explicit cancellation code in this binary verdict.
            crate::ToolCallOutcome::Cancelled(cancellation) => {
                ("cancelled", cancellation.message.as_str())
            }
        };
        let mut end = message.len().min(Self::MAX_MESSAGE_BYTES);
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        Self::Failed {
            code: code.to_owned(),
            message: message[..end].to_owned(),
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
    pub schema_admission: Option<Box<lash_vocabulary::json_schema::SchemaAdmissionError>>,
    /// The rule of a session's cells the program broke, when that is why
    /// the cell failed, kept typed through the plugin and host result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defect: Option<CellDefect>,
    /// Each pure repair callback's identity and answer, in registration order,
    /// including empty answers. Retained with the failure for replay and attribution.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub feedback: Vec<CellFailureRepair>,
}

/// One plugin callback's recorded repair decision for a failed cell.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct CellFailureRepair {
    pub plugin_id: String,
    /// The complete callback key, including its slot prefix.
    pub callback: String,
    pub repairs: Vec<String>,
}

/// A rule of a session's cells that a cell's program broke.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CellDefect {
    /// OP-INTERNAL: an authored unit tried to perform a package-private operation.
    InternalOperation {
        operation: lash_vocabulary::names::EffectName,
        unit: lash_vocabulary::sites::Unit,
    },

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
            defect: None,
            feedback: Vec::new(),
        }
    }

    pub fn with_defect(mut self, defect: CellDefect) -> Self {
        self.defect = Some(defect);
        self
    }

    pub fn with_schema_admission(
        mut self,
        source: lash_vocabulary::json_schema::SchemaAdmissionError,
    ) -> Self {
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
}

/// Journaled failure of an `exec_code` effect. `reason` is the closed
/// classification; `message` is the human-readable detail for operators.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
/// failure and a control at once, and a cell that finished with `null` is
/// distinct from one that ran to its end without a control call.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum CellOutcome {
    /// The cell ran to its end without a control call.
    #[default]
    Completed,
    /// The cell failed.
    Failed(CellFailure),
    /// A declared control call ended the cell: the call of `tool_name`
    /// recorded under `call_id`. The control stays in the record, because
    /// call lists are bounded and pruned.
    Controlled {
        tool_name: String,
        call_id: crate::ToolCallId,
        control: CellControl,
    },
}

/// The control that ended a cell, as history records it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CellControl {
    /// The turn finished with this value: inline, or retained out of
    /// history when its encoding was too long for it (FIG-1643).
    Finish {
        #[serde(with = "finish_value")]
        value: crate::OutputValue,
    },
    /// The turn switched to the agent frame `frame_key` names.
    SwitchAgentFrame { frame_key: crate::FrameKey },
}

impl CellOutcome {
    /// The failure, when the cell failed.
    pub fn failure(&self) -> Option<&CellFailure> {
        match self {
            Self::Failed(failure) => Some(failure),
            Self::Completed | Self::Controlled { .. } => None,
        }
    }

    pub fn is_failed(&self) -> bool {
        matches!(self, Self::Failed(_))
    }

    /// The outcome of a cell whose call `call_id` of `tool_name` ended the
    /// turn with `value`.
    pub fn finished_by(
        tool_name: impl Into<String>,
        call_id: crate::ToolCallId,
        value: impl Into<crate::OutputValue>,
    ) -> Self {
        Self::Controlled {
            tool_name: tool_name.into(),
            call_id,
            control: CellControl::Finish {
                value: value.into(),
            },
        }
    }

    /// The finish value as history records it, when the cell finished.
    pub fn finish(&self) -> Option<&crate::OutputValue> {
        match self {
            Self::Controlled {
                control: CellControl::Finish { value },
                ..
            } => Some(value),
            Self::Controlled { .. } | Self::Completed | Self::Failed(_) => None,
        }
    }
}

/// [`crate::OutputValue`] as a cell result spells it: `{"inline": value}` or
/// `{"retained": retention}`.
mod finish_value {
    use crate::{OutputValue, RetainedValue};
    use serde::{Deserialize, Serialize};

    #[derive(Serialize)]
    #[serde(rename_all = "snake_case")]
    enum Borrowed<'a> {
        Inline(&'a serde_json::Value),
        Retained(&'a RetainedValue),
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum Owned {
        Inline(serde_json::Value),
        Retained(RetainedValue),
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
    pub prints_retained: Option<crate::RetainedValue>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<AttachmentRef>,
    /// The dispatches the cell executed. An entry's `call_id` is the id of
    /// the host tool call's own [`ToolCallRecord`]; `None` for a dispatch
    /// lash handled with no host tool call.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub calls: Vec<ExecutedCall>,
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
    pub prints_retained: Option<crate::RetainedValue>,
    /// Every source-level dispatch the cell executed, in execution order.
    pub calls: Vec<ExecutedCall>,
    /// The record of each host tool call the cell made, in execution order.
    /// A `calls` entry references its record by `call_id`.
    #[serde(serialize_with = "crate::durable_serde::serialize")]
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
        match self.result.finish() {
            Some(crate::OutputValue::Inline(value)) => Some(value),
            Some(crate::OutputValue::Retained(_)) => self.retained_finish_value.as_ref(),
            None => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FIG-6357: verdict messages keep the longest UTF-8 prefix within
    /// their byte cap; the committed failure remains whole.
    #[test]
    fn a_call_verdict_caps_failure_text_on_a_character_boundary() {
        let cap = ExecutedCallOutcome::MAX_MESSAGE_BYTES;
        for (message, expected) in [
            ("a".repeat(cap), "a".repeat(cap)),
            (
                format!("{}éafter", "a".repeat(cap - 1)),
                "a".repeat(cap - 1),
            ),
            (
                format!("{}🦀after", "a".repeat(cap - 2)),
                "a".repeat(cap - 2),
            ),
            (
                format!("{}éafter", "a".repeat(cap - 2)),
                format!("{}é", "a".repeat(cap - 2)),
            ),
        ] {
            let output =
                crate::ToolCallOutcome::Failure(crate::ToolFailure::new("refused", &message));
            let verdict = ExecutedCallOutcome::from(&output);
            assert_eq!(
                verdict,
                ExecutedCallOutcome::Failed {
                    code: "refused".into(),
                    message: expected,
                }
            );
            let crate::ToolCallOutcome::Failure(failure) = output else {
                unreachable!()
            };
            assert_eq!(failure.message, message, "the ledger message is unchanged");
            let encoded = serde_json::to_value(&verdict).expect("encode verdict");
            assert_eq!(
                serde_json::from_value::<ExecutedCallOutcome>(encoded).expect("decode verdict"),
                verdict
            );
        }
        for malformed in [
            serde_json::json!({"kind": "ok", "message": "stray"}),
            serde_json::json!({"kind": "failed", "code": "missing_message"}),
            serde_json::json!("err"),
            serde_json::json!("ok"),
        ] {
            assert!(
                serde_json::from_value::<ExecutedCallOutcome>(malformed.clone()).is_err(),
                "{malformed}"
            );
        }
    }

    fn retained(witness: &str) -> crate::RetainedValue {
        crate::RetainedValue {
            output: crate::RetainedOutput {
                reference: AttachmentRef {
                    id: ("b".repeat(64)).parse().expect("valid attachment id"),
                    media_type: "application/json".parse().expect("valid media type"),
                    byte_len: 90_000,
                    type_metadata: None,
                    label: None,
                },
                witness: witness.to_string(),
            },
            shape: crate::ValueShape::String { bytes: 89_998 },
        }
    }

    fn controlled(control: CellControl) -> CellOutcome {
        CellOutcome::Controlled {
            tool_name: "finish".to_string(),
            call_id: crate::ToolCallId::fixture("call-1"),
            control,
        }
    }

    fn finished(value: crate::OutputValue) -> CellOutcome {
        controlled(CellControl::Finish { value })
    }

    /// A cell record spells its result as one tagged value (FIG-5527): a
    /// `null` finish, a retained finish, a frame switch, a typed failure and
    /// a cell that ran to its end each read back as themselves, and a result
    /// carrying a failure beside a control has no spelling.
    #[test]
    fn a_cell_record_reads_back_each_result_as_itself() {
        let failure = CellFailure::from(ExecCodeFailure::new(
            ExecCodeFailureReason::ExecutorUnavailable,
            "code execution is not available in this session",
        ));
        for result in [
            CellOutcome::Completed,
            finished(serde_json::Value::Null.into()),
            finished(serde_json::json!({"answer": 42}).into()),
            finished(crate::OutputValue::Retained(retained("{\"rows\":["))),
            controlled(CellControl::SwitchAgentFrame {
                frame_key: crate::FrameKey::from_call_site(
                    &lash_vocabulary::identity::SessionId::from("session"),
                    "lineage",
                    &crate::ToolCallId::fixture("call-1"),
                ),
            }),
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
        let call_id = crate::ToolCallId::fixture("call-1");
        assert_eq!(
            serde_json::to_value(finished(serde_json::Value::Null.into())).expect("encode"),
            serde_json::json!({"kind": "controlled", "value": {
                "tool_name": "finish",
                "call_id": call_id,
                "control": {"type": "finish", "value": {"inline": null}},
            }})
        );
        for malformed in [
            serde_json::json!({"kind": "controlled"}),
            serde_json::json!({"kind": "finished", "value": {"inline": null}}),
            serde_json::json!({"kind": "controlled", "value": {
                "tool_name": "finish",
                "call_id": "call-1",
                "control": {"type": "finish", "value": {"inline": 1, "retained": null}},
            }}),
            serde_json::json!({"kind": "running"}),
        ] {
            assert!(
                serde_json::from_value::<CellOutcome>(malformed.clone()).is_err(),
                "{malformed}"
            );
        }
    }
}
