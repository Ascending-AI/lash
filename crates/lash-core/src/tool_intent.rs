use crate::{ToolIntentIdentity, ToolIntentKind, ToolIntentRefusalReason};
pub use lash_sansio::ProcessParentEndPolicy;
use serde::{Deserialize, Serialize};

/// The only intent-to-command protocol understood by this build.
/// **Integrator class 3: protocol and process-engine implementors.**
pub const TOOL_INTENT_PROTOCOL_V1: u16 = 1;
/// Maximum declarations accepted from one recorded attempt.
/// **Integrator class 3: protocol and process-engine implementors.**
pub const TOOL_INTENT_MAX_COUNT: usize = 32;
/// Maximum canonical JSON bytes accepted from one recorded intent batch.
/// **Integrator class 3: protocol and process-engine implementors.**
pub const TOOL_INTENT_MAX_CANONICAL_BYTES: usize = 64 * 1024;
/// Maximum declarations of any one kind accepted from one recorded attempt.
/// **Integrator class 3: protocol and process-engine implementors.**
pub const TOOL_INTENT_MAX_PER_KIND: usize = 16;

/// Recorded declarations returned by a leaf tool attempt.
/// **Integrator class 3: protocol and process-engine implementors.**
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolIntents {
    /// Version selecting the literal admission and realization contract.
    pub protocol_version: u16,
    /// Ordered declarations whose indexes participate in durable identity.
    pub intents: Vec<ToolIntent>,
}

impl Default for ToolIntents {
    fn default() -> Self {
        Self::v1(Vec::new())
    }
}

impl ToolIntents {
    /// Construct a version-1 declaration batch for protocol and process-engine implementors.
    pub fn v1(intents: Vec<ToolIntent>) -> Self {
        Self {
            protocol_version: TOOL_INTENT_PROTOCOL_V1,
            intents,
        }
    }

    /// Test whether a declaration batch is empty before protocol admission.
    ///
    /// This is an **integrator class 3: protocol and process-engine implementor** seam.
    pub fn is_empty(&self) -> bool {
        self.intents.is_empty()
    }
}

/// Durable follow-on work a recorded leaf attempt may request.
/// **Integrator class 3: protocol and process-engine implementors.**
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", content = "intent", rename_all = "snake_case")]
pub enum ToolIntent {
    StartProcess(Box<StartProcessIntent>),
    SignalProcess(SignalProcessIntent),
    CancelProcess(CancelProcessIntent),
    EmitProcessEvent(EmitProcessEventIntent),
    EmitTrigger(EmitTriggerIntent),
}

impl ToolIntent {
    /// Return the literal command kind used by protocol and process-engine implementors.
    pub fn kind(&self) -> ToolIntentKind {
        match self {
            Self::StartProcess(_) => ToolIntentKind::StartProcess,
            Self::SignalProcess(_) => ToolIntentKind::SignalProcess,
            Self::CancelProcess(_) => ToolIntentKind::CancelProcess,
            Self::EmitProcessEvent(_) => ToolIntentKind::EmitProcessEvent,
            Self::EmitTrigger(_) => ToolIntentKind::EmitTrigger,
        }
    }

    /// Return the session binding checked by protocol and process-engine implementors.
    pub fn session_id(&self) -> &str {
        match self {
            Self::StartProcess(intent) => &intent.session_id,
            Self::SignalProcess(intent) => &intent.session_id,
            Self::CancelProcess(intent) => &intent.session_id,
            Self::EmitProcessEvent(intent) => &intent.session_id,
            Self::EmitTrigger(intent) => &intent.session_id,
        }
    }
}

/// Durable first-submission row for one runtime-owned tool-intent identity.
///
/// This is an **integrator class 3: protocol and process-engine implementor**
/// seam. Process registries persist it so independent facade handles and
/// crash redrives consult the same first writer before realization.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolIntentSubmissionRecord {
    /// Canonical `(session, scope, call, index)` identity and replay key.
    pub identity: ToolIntentIdentity,
    /// First submitted command kind.
    pub kind: ToolIntentKind,
    /// Hash of the first serialized payload.
    pub payload_hash: String,
    /// First payload retained for crash redrive.
    pub intent: ToolIntent,
    /// First typed realization outcome, absent while admission is pending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<crate::ToolIntentExecutionOutcome>,
    /// Whether the retained parent-end action has reached a typed outcome.
    #[serde(default)]
    pub parent_end_settled: bool,
}

impl ToolIntentSubmissionRecord {
    /// Builds the canonical first-submission row for protocol and
    /// process-engine implementors before any intent realization occurs.
    pub fn new(
        identity: ToolIntentIdentity,
        intent: ToolIntent,
    ) -> Result<Self, serde_json::Error> {
        let kind = intent.kind();
        let payload_hash = crate::stable_hash::sha256_hex(&serde_json::to_vec(&intent)?);
        Ok(Self {
            identity,
            kind,
            payload_hash,
            intent,
            outcome: None,
            parent_end_settled: false,
        })
    }
}

/// Atomic result of claiming a runtime-owned tool-intent identity.
///
/// This is an **integrator class 3: protocol and process-engine implementor**
/// seam returned by [`crate::ProcessRegistry`] implementations.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ToolIntentSubmissionAdmission {
    /// This caller durably installed the first submission.
    Admitted,
    /// Another caller or an earlier crash installed the returned first submission.
    Existing(Box<ToolIntentSubmissionRecord>),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
/// Start declaration consumed by protocol and process-engine implementors.
pub struct StartProcessIntent {
    /// Session whose authority owns the child.
    pub session_id: String,
    /// Complete durable process-start request.
    pub request: crate::ProcessStartRequest,
    /// Ratified action applied when the owning scope ends.
    #[serde(default)]
    pub on_parent_end: ProcessParentEndPolicy,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
/// Signal declaration consumed by protocol and process-engine implementors.
pub struct SignalProcessIntent {
    /// Session whose authority owns the signal.
    pub session_id: String,
    /// Target process id.
    pub process_id: String,
    /// Declared signal name.
    pub signal_name: String,
    /// Signal payload validated by the target event schema.
    pub payload: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
/// Cancellation declaration consumed by protocol and process-engine implementors.
pub struct CancelProcessIntent {
    /// Session whose authority owns the cancellation.
    pub session_id: String,
    /// Target process id.
    pub process_id: String,
    /// Optional durable cancellation reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
/// Event declaration consumed by protocol and process-engine implementors.
pub struct EmitProcessEventIntent {
    /// Session whose authority owns the append.
    pub session_id: String,
    /// Target process id.
    pub process_id: String,
    /// Registered event type.
    pub event_type: String,
    /// Event payload validated by the process registry.
    pub payload: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
/// Trigger declaration consumed by protocol and process-engine implementors.
///
/// A leaf attempt cannot emit a trigger synchronously on ordinal-addressed
/// journal tiers: an emission that outlives a failed attempt would advertise a
/// cause that never committed. Declaring this intent instead moves the emission
/// behind the attempt's own commit, where the recorded occurrence's
/// `idempotency_key` is the exactly-once backstop for redrive.
///
/// Three consequences of carrying a whole [`crate::TriggerOccurrenceRequest`]:
///
/// - A submission's payload hash covers this struct's entire serde shape.
///   Adding a field to `TriggerOccurrenceRequest` without
///   `skip_serializing_if` changes the hash of an unchanged declaration, so a
///   submission recorded before the change is refused as `DuplicateIdentity`
///   after it. New fields belong behind `skip_serializing_if` unless a
///   deliberate identity break is the point.
/// - `request.idempotency_key` is caller-supplied, unlike the replay keys Lash
///   derives for the process kinds. Two distinct declarations that share a key
///   collapse into one occurrence at the store and both report success, so the
///   key must be a function of the committed cause — the attempt's replay key
///   plus whatever the tool made durable.
/// - `session_id` here is the authority the intent executor validates the
///   declaration against; `request.session_id` is the occurrence's own routing
///   scope, which the router carries onto the occurrence record and never
///   checks against it.
pub struct EmitTriggerIntent {
    /// Session whose authority owns the emission. Validated: a declaration
    /// naming another session is refused before it reaches the router.
    pub session_id: String,
    /// Complete durable trigger-occurrence request handed to the router. Its
    /// own `session_id` is the occurrence's routing scope, not an authority.
    pub request: crate::TriggerOccurrenceRequest,
}

/// The single identity seam for the v1 protocol.
pub fn derive_tool_intent_identity(
    session_id: &str,
    execution_scope_id: &str,
    tool_call_id: Option<&str>,
    intent_index: usize,
) -> Result<ToolIntentIdentity, ToolIntentRefusalReason> {
    let tool_call_id = tool_call_id.ok_or(ToolIntentRefusalReason::MissingToolCallId)?;
    let intent_index =
        u32::try_from(intent_index).map_err(|_| ToolIntentRefusalReason::IntentIndexOverflow)?;

    // FIG-1203 rebase point: replace these current-tree components with the
    // frame-key-grade stable call identity supplied by FIG-1203.
    let mut encoder = crate::stable_identity::IdentityEncoder::new("lash.tool-intent", 1);
    encoder.string(session_id);
    encoder.string(execution_scope_id);
    encoder.string(tool_call_id);
    encoder.u32(intent_index);
    let replay_key = crate::stable_identity::rendered_hash("tool-intent", 1, &encoder.finish());
    Ok(ToolIntentIdentity {
        session_id: session_id.to_string(),
        execution_scope_id: execution_scope_id.to_string(),
        tool_call_id: tool_call_id.to_string(),
        intent_index,
        replay_key,
    })
}

/// A completed leaf-provider value. Unlike [`crate::ToolOutcome`], this type has
/// no deferred variant, so a completed result can be paired with intents
/// without making `Pending + intents` representable.
/// **Integrator class 3: protocol and process-engine implementors.**
#[derive(Clone, Debug, PartialEq)]
pub struct ToolOutcomeDone(Box<crate::ToolCallOutput>);

impl ToolOutcomeDone {
    /// Wrap a completed tool output for protocol and process-engine implementors.
    pub fn from_output(output: crate::ToolCallOutput) -> Self {
        Self(Box::new(output))
    }

    /// Construct a successful completed value for protocol and process-engine implementors.
    pub fn ok(result: serde_json::Value) -> Self {
        Self::from_output(crate::ToolCallOutput::success(result))
    }

    /// Construct a failed completed value for protocol and process-engine implementors.
    pub fn failure(failure: crate::ToolFailure) -> Self {
        Self::from_output(crate::ToolCallOutput::failure(failure))
    }

    /// Recover the terminal output for protocol and process-engine implementors.
    pub fn into_output(self) -> crate::ToolCallOutput {
        *self.0
    }
}

/// Value returned by a leaf provider body before the attempt effect records it.
/// The enum is the law: only the completed variant has an intents field.
/// **Integrator class 3: protocol and process-engine implementors.**
#[derive(Clone, Debug)]
pub enum ToolAttemptOutcome {
    /// Terminal provider output with ordered durable declarations.
    Done {
        /// Completed provider result.
        result: ToolOutcomeDone,
        /// Follow-on declarations admitted only after the attempt is recorded.
        intents: ToolIntents,
    },
    /// Deferred provider output with no representable declarations.
    Pending(crate::PendingCompletion),
}

impl ToolAttemptOutcome {
    /// Pair a completed result with its declarations for protocol and process-engine implementors.
    pub fn done(result: ToolOutcomeDone, intents: ToolIntents) -> Self {
        Self::Done { result, intents }
    }

    /// Construct a completed attempt with no declarations for protocol and process-engine implementors.
    pub fn done_without_intents(result: ToolOutcomeDone) -> Self {
        Self::done(result, ToolIntents::default())
    }

    /// Construct a pending attempt for protocol and process-engine implementors.
    pub fn pending(pending: crate::PendingCompletion) -> Self {
        Self::Pending(pending)
    }

    pub(crate) fn from_tool_result(result: crate::ToolOutcome) -> Self {
        match result {
            crate::ToolOutcome::Done(output) => Self::done_without_intents(ToolOutcomeDone(output)),
            crate::ToolOutcome::Pending(pending) => Self::Pending(pending),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intent_identity_has_a_literal_stable_oracle() {
        let identity = derive_tool_intent_identity("session-fig1292", "turn-7", Some("call-3"), 2)
            .expect("identity");
        assert_eq!(
            identity,
            ToolIntentIdentity {
                session_id: "session-fig1292".to_string(),
                execution_scope_id: "turn-7".to_string(),
                tool_call_id: "call-3".to_string(),
                intent_index: 2,
                replay_key: "tool-intent:v1:sha256:8cd893b79abe66a4a894753ab43053964c7bc4253a841916235b90aeedf50719".to_string(),
            }
        );
    }

    #[test]
    fn missing_call_id_is_a_typed_refusal() {
        assert_eq!(
            derive_tool_intent_identity("session", "turn", None, 0),
            Err(ToolIntentRefusalReason::MissingToolCallId)
        );
    }

    #[test]
    fn intent_identity_is_distinct_across_turn_and_process_execution_scopes() {
        let turn_7 = derive_tool_intent_identity("session", "turn-7", Some("call"), 0)
            .expect("turn 7 identity");
        let turn_8 = derive_tool_intent_identity("session", "turn-8", Some("call"), 0)
            .expect("turn 8 identity");
        let process = derive_tool_intent_identity("session", "process-7", Some("call"), 0)
            .expect("process identity");
        assert_eq!(turn_7.execution_scope_id, "turn-7");
        assert_eq!(turn_8.execution_scope_id, "turn-8");
        assert_eq!(process.execution_scope_id, "process-7");
        assert_ne!(turn_7.replay_key, turn_8.replay_key);
        assert_ne!(turn_7.replay_key, process.replay_key);
        assert_ne!(turn_8.replay_key, process.replay_key);
    }

    #[test]
    fn parent_end_policy_defaults_to_cancel_and_rejects_unreleased_terminate_bytes() {
        assert_eq!(
            ProcessParentEndPolicy::default(),
            ProcessParentEndPolicy::Cancel
        );
        assert_eq!(
            serde_json::from_str::<ProcessParentEndPolicy>(r#""cancel""#)
                .expect("decode the v1 default policy"),
            ProcessParentEndPolicy::Cancel
        );
        assert_eq!(
            serde_json::from_str::<ProcessParentEndPolicy>(r#""abandon""#)
                .expect("decode the v1 abandon policy"),
            ProcessParentEndPolicy::Abandon
        );
        assert_eq!(
            serde_json::from_str::<ProcessParentEndPolicy>(r#""terminate""#)
                .expect_err("the unreleased terminate byte shape must stay fenced")
                .to_string(),
            "unknown variant `terminate`, expected `abandon` or `cancel` at line 1 column 11"
        );
    }

    #[test]
    fn start_process_intent_without_a_parent_policy_decodes_as_cancel() {
        let intent = serde_json::from_value::<StartProcessIntent>(serde_json::json!({
            "session_id": "session",
            "request": crate::ProcessStartRequest::external(
                "child",
                crate::ProcessOriginator::host_scoped("parent-policy-default-law"),
                serde_json::json!({"recorded": true}),
            ),
        }))
        .expect("decode a start intent without an explicit parent policy");
        assert_eq!(intent.on_parent_end, ProcessParentEndPolicy::Cancel);
    }
}
