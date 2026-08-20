use std::collections::BTreeMap;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use super::model::{ProcessId, ProcessObserverBy, ProcessStatus, RecoveryContract};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProcessEventType {
    pub name: String,
    pub payload_schema: crate::LashSchema,
    pub semantics: ProcessEventSemanticsSpec,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ProcessEventSemanticsSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<ProcessTerminalSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake: Option<ProcessWakeSpec>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProcessTerminalSpec {
    pub status: ProcessStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub await_output: Option<ProcessValueSelector>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProcessWakeSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<ProcessValueSelector>,
    pub input: ProcessValueSelector,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessValueSelector {
    Payload,
    Pointer(String),
    Const(serde_json::Value),
    Template {
        template: String,
        #[serde(default)]
        fields: BTreeMap<String, ProcessValueSelector>,
    },
    Present(String),
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ProcessEventSemantics {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<ProcessTerminalSemantics>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake: Option<ProcessWake>,
}

/// Who wrote an [`ProcessStatus::Abandoned`] terminal — the exactly-one
/// legitimate writer per path (ADR 0019).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AbandonWriter {
    /// The owner abandoned its own OwnerBound work inline at graceful drain,
    /// under its own live lease.
    OwnerDrain,
    /// The recovery substrate abandoned an OwnerBound, started row after
    /// detecting that a different execution had already started it.
    Sweep,
    /// The sweep reconciled a durable Abandon Request into Abandoned once the
    /// row's lease had lapsed.
    ReconciledRequest,
    /// The execution engine exhausted the producer-declared attempt budget or
    /// otherwise gave up retrying a managed process.
    EngineGaveUp,
}

/// Evidence attached to an [`ProcessStatus::Abandoned`] terminal: which
/// path wrote it, the owner identity it was established against
/// (absent for an externally-owned row lash never executed), and when.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AbandonEvidence {
    pub writer: AbandonWriter,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<crate::LeaseOwnerIdentity>,
    pub epoch_ms: u64,
}

/// Authority under which an *unleased* terminal completion
/// ([`ProcessRegistry::complete_process`](super::registry::ProcessRegistry::complete_process))
/// is written.
///
/// Lash-owned workers fence terminal writes with a process lease
/// (`complete_process_with_lease`), which the store validates against the
/// persisted `(owner, lease_token, fencing_token)`. The unleased path is
/// reserved for writers whose single-writer discipline lives *outside* the Lash
/// lease. In-process Rust cannot make such a token unforgeable; the value of
/// this type is instead **explicitness + a single validation choke point per
/// backend + audit evidence** on the terminal write. Every backend calls
/// [`validate`](Self::validate) against the row's declared
/// [`RecoveryContract`] inside its completion operation, and records the
/// authority on the durable terminal event (see [`terminal_append_request`]).
///
/// There is deliberately no `Default`: a caller must name its authority, the
/// same footgun-prevention stance the runtime takes elsewhere.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "authority", rename_all = "snake_case")]
pub enum ProcessCompletionAuthority {
    /// An external actor closes an [`RecoveryContract::ExternallyOwned`] row
    /// it observes (the `shell.start` detach path, ADR 0019).
    /// Rejected on any lash-executed disposition: those have a lease-fenced
    /// single writer.
    ExternalOwner,
    /// A workflow-key-coalesced substrate (e.g. Restate keyed by `process_id`)
    /// completes a row it ran itself. Its single-writer discipline is the
    /// engine's per-key coalescing, not a Lash lease; `workflow_key` records the
    /// key that served as that discipline. Valid for the lash-executed
    /// dispositions ([`RecoveryContract::Rerunnable`] and
    /// [`RecoveryContract::OwnerBound`], which Restate runs), and rejected on
    /// [`RecoveryContract::ExternallyOwned`] rows — a substrate never runs
    /// one, so it may not close one.
    WorkflowKey { workflow_key: String },
    /// The sweep reconciled a durable Abandon Request on an
    /// [`RecoveryContract::ExternallyOwned`] row (whose lease had lapsed, or
    /// which Lash never leased) into an
    /// [`ProcessStatus::Abandoned`] terminal. Carries no owner: the
    /// closure is authorized by the recorded request, not a live writer. Only
    /// ever writes an `Abandoned` terminal.
    ReconciledAbandon,
}

impl ProcessCompletionAuthority {
    /// Construct [`ExternalOwner`](Self::ExternalOwner) authority.
    pub fn external_owner() -> Self {
        Self::ExternalOwner
    }

    /// Construct [`WorkflowKey`](Self::WorkflowKey) authority naming the
    /// coalescing key that serves as the substrate's single-writer discipline.
    pub fn workflow_key(workflow_key: impl Into<String>) -> Self {
        Self::WorkflowKey {
            workflow_key: workflow_key.into(),
        }
    }

    /// Short, stable label for diagnostics.
    pub fn label(&self) -> &'static str {
        match self {
            Self::ExternalOwner => "external-owner",
            Self::WorkflowKey { .. } => "workflow-key",
            Self::ReconciledAbandon => "reconciled-abandon",
        }
    }

    /// Validate this authority against the row's declared recovery disposition
    /// and the terminal outcome being written. This is the single per-backend
    /// choke point that keeps unleased completion honest: each `complete_process`
    /// implementation calls it before appending the terminal event, so the
    /// disposition×authority contract is enforced uniformly across memory,
    /// SQLite, and Postgres rather than at each scattered caller.
    pub fn validate(
        &self,
        process_id: &str,
        disposition: RecoveryContract,
        await_output: &ProcessAwaitOutput,
    ) -> Result<(), crate::PluginError> {
        let reject = |reason: &str| {
            Err(crate::PluginError::Session(format!(
                "process `{process_id}` cannot be completed with {} authority: {reason}",
                self.label()
            )))
        };
        match self {
            Self::ExternalOwner => {
                if disposition != RecoveryContract::ExternallyOwned {
                    return reject(
                        "only externally-owned rows may be completed by an external owner; a \
                         lash-executed row has a lease-fenced single writer",
                    );
                }
            }
            Self::WorkflowKey { .. } => {
                if disposition == RecoveryContract::ExternallyOwned {
                    return reject(
                        "externally-owned rows are never executed by a workflow substrate; they \
                         close through their external owner or a reconciled abandon request",
                    );
                }
            }
            Self::ReconciledAbandon => {
                if disposition != RecoveryContract::ExternallyOwned {
                    return reject(
                        "reconciled-abandon closes only externally-owned rows; a lash-executed \
                         row is abandoned under its lease",
                    );
                }
                if await_output.terminal_status() != Some(ProcessStatus::Abandoned) {
                    return reject("reconciled-abandon writes only an Abandoned terminal");
                }
            }
        }
        Ok(())
    }
}

/// Terminal event type name for a terminal state.
pub fn terminal_event_type_name(status: ProcessStatus) -> &'static str {
    match status {
        ProcessStatus::Completed => "process.completed",
        ProcessStatus::Failed => "process.failed",
        ProcessStatus::Cancelled => "process.cancelled",
        ProcessStatus::Abandoned => "process.abandoned",
        ProcessStatus::Running | ProcessStatus::Waiting | ProcessStatus::CallerDeparted => {
            unreachable!("non-terminal process status has no terminal event")
        }
    }
}

/// Build the replay-keyed terminal event append for a completion.
///
/// The single source of truth for the terminal event's type, replay key, and
/// payload shape, shared by every completion path (leased and unleased) across
/// all backends. When `authority` is supplied — the unleased
/// [`ProcessRegistry::complete_process`](super::registry::ProcessRegistry::complete_process)
/// path — it is recorded alongside `await_output` as durable audit evidence
/// (the leased path's evidence is the lease it releases, so it passes `None`
/// and the payload is byte-identical to the historical shape). The
/// `await_output` selector (`/await_output`) is untouched by the sibling key.
pub fn terminal_append_request(
    process_id: &str,
    await_output: &ProcessAwaitOutput,
    authority: Option<&ProcessCompletionAuthority>,
) -> ProcessEventAppendRequest {
    let event_type = terminal_event_type_name(
        await_output
            .terminal_status()
            .expect("only terminal outcomes may be appended"),
    );
    let mut payload = serde_json::json!({ "await_output": await_output });
    if let Some(authority) = authority {
        payload["completion_authority"] =
            serde_json::to_value(authority).expect("completion authority serializes");
    }
    ProcessEventAppendRequest::new(event_type, payload)
        .with_replay_key(format!("process:{process_id}:terminal:{event_type}"))
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProcessTerminalSemantics {
    pub status: ProcessStatus,
    pub outcome: ProcessAwaitOutput,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProcessAwaitOutput {
    Success {
        value: serde_json::Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        control: Option<crate::ToolControl>,
    },
    Failure {
        class: crate::ToolFailureClass,
        code: String,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        raw: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        control: Option<crate::ToolControl>,
    },
    Cancelled {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        raw: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        control: Option<crate::ToolControl>,
    },
    /// The owner stopped executing without recording an outcome. Written only by
    /// the sweep or an owner's graceful drain, never round-tripped from a tool
    /// (a tool cannot self-report abandonment); see [`AbandonEvidence`]. The
    /// evidence is boxed so this rare terminal does not enlarge the pervasive
    /// `ProcessAwaitOutput` that flows through every tool result.
    Abandoned {
        evidence: Box<AbandonEvidence>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        control: Option<crate::ToolControl>,
    },
    NoLongerRetained {
        terminal_label: String,
        pruned_at_ms: u64,
    },
}

impl ProcessAwaitOutput {
    /// Projects only terminal process outcomes to their durable status for store implementors,
    /// returning `None` for non-terminal or deferred output.
    pub fn terminal_status(&self) -> Option<ProcessStatus> {
        match self {
            Self::Success { .. } => Some(ProcessStatus::Completed),
            Self::Failure { .. } => Some(ProcessStatus::Failed),
            Self::Cancelled { .. } => Some(ProcessStatus::Cancelled),
            Self::Abandoned { .. } => Some(ProcessStatus::Abandoned),
            Self::NoLongerRetained { .. } => None,
        }
    }

    /// Builds a `ProcessAwaitOutput` from tool output data for store and durable-substrate
    /// implementors while persisting and coordinating durable process execution.
    pub fn from_tool_output(output: crate::ToolCallOutput) -> Self {
        let control = output.control;
        match output.outcome {
            crate::ToolCallOutcome::Success(value) => Self::Success {
                value: value.to_json_value(),
                control,
            },
            crate::ToolCallOutcome::Failure(failure) => Self::Failure {
                class: failure.class,
                code: failure.code,
                message: failure.message,
                raw: failure.raw.map(|value| value.to_json_value()),
                control,
            },
            crate::ToolCallOutcome::Cancelled(cancellation) => Self::Cancelled {
                message: cancellation.message,
                raw: cancellation.raw.map(|value| value.to_json_value()),
                control,
            },
        }
    }

    /// Extracts the tool output outcome for store and durable-substrate implementors while
    /// persisting and coordinating durable process execution.
    pub fn into_tool_output(self) -> crate::ToolCallOutput {
        match self {
            Self::Success { value, control } => {
                let mut output = crate::ToolCallOutput::success(value);
                output.control = control;
                output
            }
            Self::Failure {
                class,
                code,
                message,
                raw,
                control,
            } => {
                let mut failure = crate::ToolFailure::tool(class, code, message);
                failure.raw = raw.map(crate::ToolValue::from);
                let mut output = crate::ToolCallOutput::failure(failure);
                output.control = control;
                output
            }
            Self::Cancelled {
                message,
                raw,
                control,
            } => {
                let mut cancellation = crate::ToolCancellation::runtime(message);
                cancellation.raw = raw.map(crate::ToolValue::from);
                let mut output = crate::ToolCallOutput::cancelled(cancellation);
                output.control = control;
                output
            }
            // Abandonment has no `ToolCallOutcome` peer: a tool never self-reports
            // it. To a caller awaiting the result it surfaces one-directionally as
            // an external failure whose raw payload names it abandoned and carries
            // the evidence, while the process layer keeps `Abandoned` a distinct
            // terminal (ADR 0019). `from_tool_output` therefore never reverses this.
            Self::Abandoned { evidence, control } => {
                let raw = serde_json::to_value(&evidence)
                    .ok()
                    .map(crate::ToolValue::from);
                let message = match evidence.writer {
                    AbandonWriter::OwnerDrain => {
                        "process abandoned: owner drained without recording an outcome".to_string()
                    }
                    AbandonWriter::Sweep => {
                        "process abandoned: recovery observed a prior owner-bound execution"
                            .to_string()
                    }
                    AbandonWriter::ReconciledRequest => {
                        "process abandoned: reconciled abandon request after the lease lapsed"
                            .to_string()
                    }
                    AbandonWriter::EngineGaveUp => {
                        "process abandoned: execution engine exhausted its retry budget".to_string()
                    }
                };
                let mut failure = crate::ToolFailure::tool(
                    crate::ToolFailureClass::External,
                    "process_abandoned",
                    message,
                );
                failure.raw = raw;
                let mut output = crate::ToolCallOutput::failure(failure);
                output.control = control;
                output
            }
            Self::NoLongerRetained {
                terminal_label,
                pruned_at_ms,
            } => crate::ToolCallOutput::success(serde_json::json!({
                "type": "information",
                "code": "process_no_longer_retained",
                "message": "process completed, but its outcome is no longer retained",
                "terminal_label": terminal_label,
                "pruned_at_ms": pruned_at_ms,
            })),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProcessWake {
    pub input: String,
}

pub fn process_signal_event_type(signal_name: &str) -> Result<String, crate::PluginError> {
    validate_process_signal_name(signal_name)?;
    Ok(format!("signal.{signal_name}"))
}

pub fn process_signal_name_from_event_type(event_type: &str) -> Option<&str> {
    event_type.strip_prefix("signal.")
}

pub fn process_signal_wait_key(process_id: &str, signal_name: &str, ordinal: u64) -> String {
    format!("process:{process_id}:signal.{signal_name}:{ordinal}")
}

pub fn validate_process_signal_name(signal_name: &str) -> Result<(), crate::PluginError> {
    let valid = !signal_name.is_empty()
        && signal_name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-');
    if valid {
        Ok(())
    } else {
        Err(crate::PluginError::Session(format!(
            "process signal name must be non-empty and contain only ASCII letters, digits, `_`, or `-`, got `{signal_name}`"
        )))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcessEvent {
    pub process_id: ProcessId,
    pub sequence: u64,
    pub event_type: String,
    pub payload: serde_json::Value,
    pub invocation: crate::RuntimeInvocation,
    pub semantics: ProcessEventSemantics,
    pub occurred_at: SystemTime,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcessEventAppendReceipt {
    pub event: ProcessEvent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_delivery: Option<ProcessWakeDelivery>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProcessEventAppendRequest {
    pub event_type: String,
    pub payload: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay: Option<crate::RuntimeReplay>,
}

impl ProcessEventAppendRequest {
    /// Constructs a `ProcessEventAppendRequest` for store and durable-substrate implementors while
    /// persisting and coordinating durable process execution.
    pub fn new(event_type: impl Into<String>, payload: serde_json::Value) -> Self {
        Self {
            event_type: event_type.into(),
            payload,
            replay: None,
        }
    }

    /// Attaches a replay key for process-store implementors so an equivalent event append is
    /// idempotent within that key.
    pub fn with_replay_key(mut self, replay_key: impl Into<String>) -> Self {
        self.replay = Some(crate::RuntimeReplay {
            key: replay_key.into(),
            attribution: None,
        });
        self
    }

    /// Sets the optional replay carried by a `ProcessEventAppendRequest` for store and
    /// durable-substrate implementors while persisting and coordinating durable process execution.
    pub fn with_optional_replay(mut self, replay: Option<crate::RuntimeReplay>) -> Self {
        self.replay = replay;
        self
    }

    /// Builds a cancellation-request event with a fixed-size, versioned replay
    /// address. Repeating the same reason is idempotent; a distinct reason
    /// retains the pre-cutover behavior of naming a distinct request without
    /// copying unbounded caller text into an indexed store key.
    pub fn cancel_requested(process_id: &str, reason: Option<String>) -> Self {
        let replay_key = cancellation_replay_key(process_id, reason.as_deref());
        let payload = serde_json::json!({
            "reason": reason,
        });
        Self::new("process.cancel_requested", payload).with_replay_key(replay_key)
    }

    /// Builds a first-start event for process-store implementors keyed by attempt number so a retry
    /// cannot alias the preceding execution attempt.
    pub fn first_started(
        process_id: &str,
        started: &super::model::ProcessStarted,
        resumed_from_handover: bool,
    ) -> Self {
        Self::new(
            "process.first_started",
            serde_json::json!({
                "started": started,
                "resumed_from_handover": resumed_from_handover,
            }),
        )
        .with_replay_key(format!(
            "process:{process_id}:first-started:attempt:{}",
            started.attempt
        ))
    }

    /// Builds a wait-entry event for process-store implementors keyed by wait identity and start
    /// time so replay cannot duplicate the transition.
    pub fn wait_entered(process_id: &str, wait: &super::model::WaitState) -> Self {
        Self::new("process.waiting", serde_json::json!({ "wait": wait })).with_replay_key(format!(
            "process:{process_id}:wait:{}:since:{}:entered",
            wait.key(),
            wait.since_ms
        ))
    }

    /// Builds a wait-clear event for process-store implementors keyed to the exact wait identity
    /// and start time being resumed.
    pub fn wait_cleared(process_id: &str, wait: &super::model::WaitState) -> Self {
        Self::new("process.resumed", serde_json::json!({ "wait": wait })).with_replay_key(format!(
            "process:{process_id}:wait:{}:since:{}:cleared",
            wait.key(),
            wait.since_ms
        ))
    }

    /// Builds the single replay-stable external-reference event for process-store implementors
    /// binding durable backend work.
    pub fn external_ref_set(
        process_id: &str,
        external_ref: &super::model::ProcessExternalRef,
    ) -> Self {
        Self::new(
            "process.external_ref_set",
            serde_json::json!({ "external_ref": external_ref }),
        )
        .with_replay_key(format!("process:{process_id}:external-ref"))
    }

    /// Builds the replay-stable abandon-request event for process-store implementors; repeated
    /// requests for the process converge on the same append identity.
    pub fn abandon_requested(process_id: &str, request: &super::model::AbandonRequest) -> Self {
        Self::new(
            "process.abandon_requested",
            serde_json::json!({ "request": request }),
        )
        .with_replay_key(format!("process:{process_id}:abandon-requested"))
    }

    /// Builds the replay-stable caller-departure event for process-store
    /// implementors; repeated reports for the process converge on the same
    /// append identity.
    ///
    /// The event carries no payload on purpose. What it records is a lifecycle
    /// transition of the row itself — its registering caller left before any
    /// outcome could be written — and the transition's wall clock is the
    /// event's own `occurred_at`, projected onto `updated_at_ms` like every
    /// other lifecycle append.
    pub fn caller_departed(process_id: &str) -> Self {
        Self::new("process.caller_departed", serde_json::json!({}))
            .with_replay_key(format!("process:{process_id}:caller-departed"))
    }

    /// Builds an observer-add event for process-store implementors whose replay key includes
    /// process, session, and observer authority.
    pub fn observer_added(process_id: &str, session: &str, by: &ProcessObserverBy) -> Self {
        Self::new(
            "process.observer_added",
            serde_json::json!({ "session": session, "by": by }),
        )
        .with_replay_key(format!(
            "process:{process_id}:observer:{session}:add:{}",
            by.replay_component()
        ))
    }

    /// Builds an observer-remove event for process-store implementors whose replay key includes
    /// process, session, and observer authority.
    pub fn observer_removed(process_id: &str, session: &str, by: &ProcessObserverBy) -> Self {
        Self::new(
            "process.observer_removed",
            serde_json::json!({ "session": session, "by": by }),
        )
        .with_replay_key(format!(
            "process:{process_id}:observer:{session}:remove:{}",
            by.replay_component()
        ))
    }

    /// Builds a replay-stable subscription-retarget event for process-store implementors, encoding
    /// an absent target with the reserved `none` replay component.
    pub fn subscription_retargeted(process_id: &str, target: Option<&str>) -> Self {
        Self::new(
            "process.subscription_retargeted",
            serde_json::json!({ "target": target }),
        )
        .with_replay_key(format!(
            "process:{process_id}:subscription-retargeted:{}",
            target.unwrap_or("none")
        ))
    }
}

const PROCESS_CANCELLATION_FAMILY_VERSION: u8 = 1;

/// Permanent tag registry for cancellation replay addresses.
///
/// Reason presence uses the universal option tags 0/1. The present arm frames
/// the complete UTF-8 reason; the rendered key hashes that exhaustive preimage
/// to a backend-safe fixed size.
fn cancellation_replay_preimage(process_id: &str, reason: Option<&str>) -> Vec<u8> {
    let mut identity = crate::stable_identity::IdentityEncoder::new(
        "lash.process-cancellation-request",
        PROCESS_CANCELLATION_FAMILY_VERSION,
    );
    identity.string(process_id);
    identity.optional(reason, |identity, reason| identity.string(reason));
    identity.finish()
}

fn cancellation_replay_key(process_id: &str, reason: Option<&str>) -> String {
    crate::stable_identity::rendered_hash(
        "process-cancellation",
        PROCESS_CANCELLATION_FAMILY_VERSION,
        &cancellation_replay_preimage(process_id, reason),
    )
}

pub const PROCESS_WAKE_DELIVERY_FORMAT_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProcessWakeDelivery {
    #[serde(default = "default_process_wake_delivery_format_version")]
    pub version: u32,
    pub wake_id: String,
    pub target_session_id: String,
    pub process_id: ProcessId,
    pub sequence: u64,
    pub event_type: String,
    pub event_invocation: crate::RuntimeInvocation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_caused_by: Option<crate::CausalRef>,
    /// Authority captured from the durable process originator at event append.
    /// The delivery driver must forward this unchanged into queued work.
    #[serde(default, skip_serializing_if = "process_wake_authority_is_empty")]
    pub authority: crate::QueuedWorkAuthority,
    pub input: String,
    pub created_at_ms: u64,
}

fn default_process_wake_delivery_format_version() -> u32 {
    PROCESS_WAKE_DELIVERY_FORMAT_VERSION
}

fn process_wake_authority_is_empty(authority: &crate::QueuedWorkAuthority) -> bool {
    authority.principal.is_none() && authority.elevation.is_none()
}

pub(super) fn runtime_lifecycle_event_type(name: &str) -> Option<ProcessEventType> {
    match name {
        "process.first_started"
        | "process.waiting"
        | "process.resumed"
        | "process.external_ref_set"
        | "process.abandon_requested"
        | "process.caller_departed"
        | "process.observer_added"
        | "process.observer_removed"
        | "process.subscription_retargeted" => Some(ProcessEventType {
            name: name.to_string(),
            payload_schema: crate::LashSchema::any(),
            semantics: ProcessEventSemanticsSpec::default(),
        }),
        _ => None,
    }
}

pub(super) fn is_runtime_lifecycle_event_type(name: &str) -> bool {
    runtime_lifecycle_event_type(name).is_some()
}

pub(super) fn default_process_event_types() -> Vec<ProcessEventType> {
    let mut event_types = vec![ProcessEventType {
        name: "process.cancel_requested".to_string(),
        payload_schema: crate::LashSchema::any(),
        semantics: ProcessEventSemanticsSpec::default(),
    }];
    event_types.extend(
        [
            "process.first_started",
            "process.waiting",
            "process.resumed",
            "process.external_ref_set",
            "process.abandon_requested",
            "process.caller_departed",
            "process.observer_added",
            "process.observer_removed",
            "process.subscription_retargeted",
        ]
        .into_iter()
        .filter_map(runtime_lifecycle_event_type),
    );
    event_types.extend([
        terminal_event_type("process.completed", ProcessStatus::Completed),
        terminal_event_type("process.failed", ProcessStatus::Failed),
        terminal_event_type("process.cancelled", ProcessStatus::Cancelled),
        terminal_event_type("process.abandoned", ProcessStatus::Abandoned),
    ]);
    event_types
}

fn terminal_event_type(name: &str, status: ProcessStatus) -> ProcessEventType {
    ProcessEventType {
        name: name.to_string(),
        payload_schema: crate::LashSchema::any(),
        semantics: ProcessEventSemanticsSpec {
            terminal: Some(ProcessTerminalSpec {
                status,
                await_output: Some(ProcessValueSelector::Pointer("/await_output".to_string())),
            }),
            ..ProcessEventSemanticsSpec::default()
        },
    }
}

#[cfg(test)]
mod cancellation_identity_tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn cancellation_replay_identity_has_pinned_bounded_grammar() {
        let reason = "λ".repeat(3_200);
        let key = cancellation_replay_key("process\0id", Some(&reason));
        assert_eq!(
            key.len(),
            95,
            "rendered key length must be input-independent"
        );
        assert_eq!(
            key,
            "process-cancellation:v1:sha256:7425a370ae338e834838427bafeae1ecedd7a7e2ed75d2b69e53572e2fc556b2"
        );
        assert_eq!(
            hex(&cancellation_replay_preimage("process\0id", None)),
            "6c6173682d737461626c652d6964656e74697479010100000000000000216c6173682e70726f636573732d63616e63656c6c6174696f6e2d72657175657374000000000000000a70726f6365737300696400"
        );
        assert_ne!(
            cancellation_replay_key("process\0id", None),
            cancellation_replay_key("process\0id", Some("")),
            "None and Some(empty) occupy different permanent option arms"
        );
    }
}
