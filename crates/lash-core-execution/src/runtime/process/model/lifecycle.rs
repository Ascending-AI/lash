//! The durable process record and its closed lifecycle projection.
use super::super::events::{ProcessEventType, ProcessTerminal};
use super::super::validation::prepare_process_registration;
use super::{
    Ancestry, LifetimeDecision, ProcessExecutionEnvRef, ProcessExternalRef, ProcessId,
    ProcessIdentity, ProcessInput, ProcessLineage, ProcessOutcome, ProcessProvenance,
    ProcessRegistration, ProcessStarted, ProcessStatus, SessionId, StartKey,
    process_child_session_id,
};
use lash_sansio::CancelRequest;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitState {
    pub kind: WaitKind,
    pub since_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WaitKind {
    /// A deferring tool call, identified without its bearer completion key.
    Call {
        call_id: crate::ToolCallId,
        tool_id: crate::ToolId,
    },
    Signal {
        name: String,
        event_type: String,
        key: String,
        ordinal: u64,
    },
}

impl WaitState {
    /// Exposes key to store and durable-substrate implementors while persisting and coordinating
    /// durable process execution.
    pub fn key(&self) -> &str {
        match &self.kind {
            WaitKind::Signal { key, .. } => key,
            WaitKind::Call { call_id, .. } => call_id.as_str(),
        }
    }
}

/// Durable process lifecycle fold. Observer membership and wake subscription
/// are queryable edge state, audited by events but deliberately not projected
/// into this record.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProcessRecord {
    /// The minted id: the process's only identity, never reused (ADR 0107).
    pub id: ProcessId,
    /// The key the process was started under, while it is retained.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_key: Option<StartKey>,
    /// Sequence of the newest event folded into this record. Registration
    /// starts at zero; every event append advances the value in the same
    /// transaction that persists the event and projected record.
    pub last_event_sequence: u64,
    pub input: Arc<ProcessInput>,
    /// The recorded lifetime decision: never updated after registration.
    pub lifetime: LifetimeDecision,
    /// The recorded ancestry, nearest first; empty for a root.
    pub ancestry: Ancestry,
    /// The session capability descendants inherit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_capability: Option<SessionId>,
    pub identity: ProcessIdentity,
    #[serde(default)]
    pub event_types: Vec<ProcessEventType>,
    pub provenance: ProcessProvenance,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_ref: Option<ProcessExecutionEnvRef>,
    /// What the process's engine recorded when the row was created: never
    /// updated after registration (FIG-4527).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine_config: Option<serde_json::Value>,
    /// The process's trace scope: the cause and anchor its first
    /// registration retained, started when the row was created. Never
    /// updated after registration; a start that finds the process retained
    /// reads it back whatever it offered. `None` on a record written
    /// without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<lash_trace::DurableTraceScope>,
    #[serde(default)]
    pub created_at_ms: u64,
    #[serde(default)]
    pub updated_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_ref: Option<ProcessExternalRef>,
    /// Durable execution-started fact (ADR 0110). `None` until a
    /// runner records it immediately before executing. Boxed so these
    /// usually-absent facts do not enlarge the pervasive `ProcessRecord` that
    /// flows through the runtime; serde treats `Option<Box<T>>` identically to
    /// `Option<T>`, so the persisted JSON is unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_started: Option<Box<ProcessStarted>>,
    /// The first accepted cancellation request, retained across retries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancel_request: Option<Box<CancelRequest>>,
    /// The one lifecycle state the process is in. Its status, wait and
    /// outcome are read from it ([`Self::status`], [`Self::wait`],
    /// [`Self::terminal`]) and stored nowhere beside it.
    pub lifecycle: ProcessLifecycleState,
}

/// The lifecycle state of a process record: each state owns the facts that
/// exist only in it, so a record cannot hold a wait beside an outcome, or a
/// terminal status without one. A parked process is its actor's state
/// (ADR 0132 §11), not a record fact.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProcessLifecycleState {
    Running {},
    Waiting { wait: WaitState },
    Terminal { outcome: ProcessTerminal },
}

impl ProcessLifecycleState {
    /// The state of a newly registered process.
    pub fn running() -> Self {
        Self::Running {}
    }

    /// A representative state of `status`, for fixtures that need a record
    /// in a status and do not care what put it there: a terminal status
    /// holds a minimal outcome of that status, and `waiting` a signal wait.
    pub fn fixture(status: ProcessStatus) -> Self {
        let settled = |output| Self::Terminal {
            outcome: ProcessTerminal::from_tool_output(output),
        };
        match status {
            ProcessStatus::Running => Self::running(),
            ProcessStatus::Waiting => Self::Waiting {
                wait: WaitState {
                    kind: WaitKind::Signal {
                        name: "fixture".to_string(),
                        event_type: "signal.fixture".to_string(),
                        key: "fixture".to_string(),
                        ordinal: 1,
                    },
                    since_ms: 0,
                },
            },
            ProcessStatus::Completed => {
                settled(crate::ToolCallOutput::success(serde_json::Value::Null))
            }
            ProcessStatus::Failed => {
                settled(crate::ToolCallOutput::failure(crate::ToolFailure::runtime(
                    crate::ToolFailureClass::Execution,
                    "fixture_failure",
                    "fixture failure",
                )))
            }
            ProcessStatus::Cancelled => settled(crate::ToolCallOutput::cancelled(
                crate::ToolCancellation::runtime("fixture cancellation"),
            )),
            ProcessStatus::Abandoned => Self::Terminal {
                outcome: ProcessTerminal::Abandoned {
                    evidence: Box::new(super::super::events::AbandonEvidence {
                        writer: super::super::events::AbandonWriter::Producer,
                        owner: None,
                        epoch_ms: 0,
                    }),
                    control: None,
                },
            },
        }
    }

    /// The status this state is. Exhaustive on purpose: the status is a
    /// function of the state and has no home of its own.
    pub fn status(&self) -> ProcessStatus {
        match self {
            Self::Running { .. } => ProcessStatus::Running,
            Self::Waiting { .. } => ProcessStatus::Waiting,
            Self::Terminal { outcome } => outcome.status().into(),
        }
    }

    /// The wait the process is in.
    pub fn wait(&self) -> Option<&WaitState> {
        match self {
            Self::Waiting { wait, .. } => Some(wait),
            Self::Running { .. } | Self::Terminal { .. } => None,
        }
    }

    /// The outcome the process ended in.
    pub fn terminal(&self) -> Option<&ProcessTerminal> {
        match self {
            Self::Terminal { outcome } => Some(outcome),
            Self::Running { .. } | Self::Waiting { .. } => None,
        }
    }
}
/// The lineage a process's body starts children under, from the facts its
/// row records: the process, the session it runs of its own (a `SessionTurn`
/// child session), its ancestry and its session capability.
pub(super) fn recorded_lineage(
    process_id: &ProcessId,
    input: &ProcessInput,
    ancestry: &Ancestry,
    session_capability: Option<&SessionId>,
) -> ProcessLineage {
    let own_session = match input {
        ProcessInput::SessionTurn { create_request, .. } => Some(
            create_request
                .session_id
                .clone()
                .unwrap_or_else(|| process_child_session_id(process_id)),
        ),
        ProcessInput::Engine { .. } => None,
    };
    ProcessLineage::of_process(
        process_id,
        ancestry,
        session_capability,
        own_session.as_ref(),
    )
}

impl ProcessRecord {
    /// The lineage this process's body starts children under, read back from
    /// its row (FIG-3607 R1).
    pub fn lineage(&self) -> ProcessLineage {
        recorded_lineage(
            &self.id,
            &self.input,
            &self.ancestry,
            self.session_capability.as_ref(),
        )
    }

    /// Builds the record of a process the registrar just minted `id` for.
    pub fn from_registration(registration: ProcessRegistration, id: ProcessId) -> Self {
        Self::from_registration_with_clock(registration, id, &crate::SystemClock)
    }

    /// Builds the record of a process the registrar just minted `id` for, at
    /// the clock's time.
    ///
    /// Panics when the registration is invalid, so callers that accept
    /// host-supplied registrations validate them with
    /// `prepare_process_registration` first.
    #[expect(clippy::expect_used, reason = "callers validate first")]
    pub fn from_registration_with_clock(
        registration: ProcessRegistration,
        id: ProcessId,
        clock: &dyn crate::Clock,
    ) -> Self {
        let registration = prepare_process_registration(registration)
            .expect("process registration should be valid before record construction");
        Self::from_prepared_registration(registration, id, clock.timestamp_ms())
    }

    /// Builds the record of a prepared registration under its minted `id`.
    pub fn from_prepared_registration(
        registration: ProcessRegistration,
        id: ProcessId,
        now_ms: u64,
    ) -> Self {
        let trace = registration.trace.into_scope(
            lash_trace::TraceScopeId::admission(lash_trace::TraceScopeOwner::Process {
                process_id: id.clone(),
            }),
            now_ms,
        );
        Self {
            id,
            start_key: registration.start_key,
            last_event_sequence: 0,
            input: registration.input,
            lifetime: registration.lifetime,
            ancestry: registration.ancestry,
            session_capability: registration.session_capability,
            identity: registration.identity,
            event_types: registration.event_types,
            provenance: registration.provenance,
            env_ref: registration.env_ref,
            engine_config: registration.engine_config,
            trace: Some(trace),
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
            external_ref: None,
            first_started: None,
            cancel_request: None,
            lifecycle: ProcessLifecycleState::running(),
        }
    }

    /// The status the record's lifecycle state is: what a store projects
    /// into its `status` column.
    pub fn status(&self) -> ProcessStatus {
        self.lifecycle.status()
    }

    /// The wait the process is in.
    pub fn wait(&self) -> Option<&WaitState> {
        self.lifecycle.wait()
    }

    /// The outcome the process ended in.
    pub fn terminal(&self) -> Option<&ProcessTerminal> {
        self.lifecycle.terminal()
    }

    /// What an await of the process answers once it has ended: its outcome.
    pub fn outcome(&self) -> Option<ProcessOutcome> {
        self.terminal().cloned().map(ProcessOutcome::from)
    }

    /// Lets process-store implementors gate retention on the folded durable status rather than the
    /// presence of an incidental event.
    pub fn is_terminal(&self) -> bool {
        self.terminal().is_some()
    }

    /// Exposes originator id to store and durable-substrate implementors while persisting and
    /// coordinating durable process execution.
    pub fn originator_id(&self) -> String {
        self.provenance.originator.id()
    }
}
