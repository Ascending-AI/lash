//! The durable process record and its closed lifecycle projection.
use super::super::events::ProcessTerminal;
use super::super::validation::prepare_process_registration;
use super::{
    Ancestry, EngineLaunch, EngineSettings, LifetimeDecision, ProcessEnvRef, ProcessExternalRef,
    ProcessId, ProcessIdentity, ProcessInput, ProcessLineage, ProcessOutcome, ProcessProvenance,
    ProcessRegistration, ProcessStatus, SessionId, StartKey, process_session_id,
};
use lash_protocol_types::CancelRequest;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// One thing a process is blocked on: what it is, since when, and the node
/// of the engine's workflow document that blocked on it, when the engine named
/// one. It names no completion key or wait id: holding one of those resolves
/// the wait, and a reader of process state is not handed that.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitState {
    pub kind: WaitKind,
    pub since_ms: i64,
    /// The node that blocked, and which occurrence of it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site: Option<lash_protocol_types::EffectId>,
}

/// Why a process wait has no workflow coordinate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WaitSiteUnavailable {
    #[error("the process engine did not name a workflow execution site")]
    EngineDidNotNameSite,
}

impl WaitState {
    /// Workflow waits name their task's occurrence. A document-free engine
    /// may wait without one; callers receive a typed explanation for that case.
    pub fn execution_site(&self) -> Result<&lash_protocol_types::EffectId, WaitSiteUnavailable> {
        self.site
            .as_ref()
            .ok_or(WaitSiteUnavailable::EngineDidNotNameSite)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WaitKind {
    /// A deferring tool call, identified without its bearer completion key.
    Call {
        call_id: crate::ToolCallId,
        tool_id: crate::ToolId,
    },
    /// A key the process's engine pinned and awaits, identified by the name
    /// the engine gave it, without its bearer key.
    Key { name: crate::KeyName },
    /// A durable instant the process sleeps until, in store milliseconds.
    Sleep { until_ms: i64 },
    /// Another process's terminal.
    Process { process_id: ProcessId },
}

/// A non-empty collection indexed by the typed blocker. Entries are sorted
/// by `WaitKind`, and there is exactly one entry per kind. The private storage
/// lets readers borrow a slice without allowing them to empty the collection
/// or introduce duplicate keys.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<WaitState>", into = "Vec<WaitState>")]
pub struct ProcessWaits(Vec<WaitState>);

impl ProcessWaits {
    /// Start a waiting state with its first blocker.
    pub fn new(wait: WaitState) -> Self {
        Self(vec![wait])
    }

    /// Read all blockers in typed key order.
    pub fn as_slice(&self) -> &[WaitState] {
        &self.0
    }

    fn insert(&mut self, wait: WaitState) {
        match self.0.binary_search_by(|held| held.kind.cmp(&wait.kind)) {
            Ok(index) => self.0[index] = wait,
            Err(index) => self.0.insert(index, wait),
        }
    }
}

impl TryFrom<Vec<WaitState>> for ProcessWaits {
    type Error = &'static str;

    fn try_from(mut waits: Vec<WaitState>) -> Result<Self, Self::Error> {
        if waits.is_empty() {
            return Err("a waiting process waits on something");
        }
        waits.sort_by(|a, b| a.kind.cmp(&b.kind));
        if waits.windows(2).any(|pair| pair[0].kind == pair[1].kind) {
            return Err("a waiting process has one entry per blocker");
        }
        Ok(Self(waits))
    }
}

impl From<ProcessWaits> for Vec<WaitState> {
    fn from(waits: ProcessWaits) -> Self {
        waits.0
    }
}

/// Store-author row: the durable process lifecycle fold returned by registry ports.
/// Hosts and plugins read `ObservedProcess` instead. Observer membership is queryable edge
/// state, audited by events but deliberately not projected
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
    pub provenance: ProcessProvenance,
    /// The process's trace scope: the cause and span its first
    /// registration retained, started when the row was created. Never
    /// updated after registration; a start that finds the process retained
    /// reads it back whatever it offered. `None` on a record written
    /// without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<lash_trace::DurableTraceScope>,
    /// Whether the process exports a span per workflow node occurrence,
    /// retained from its first registration and never updated: every owner
    /// that runs the process reads the same choice, so whether node spans
    /// exist is fixed before any child span exists.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub trace_nodes: bool,
    /// The process taxonomy its first registration retained. Every owner
    /// reads it rather than consulting its live engine registry.
    pub trace_class: super::super::ProcessTraceClass,
    #[serde(default)]
    pub created_at_ms: u64,
    #[serde(default)]
    pub updated_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_ref: Option<ProcessExternalRef>,
    /// The first accepted cancellation request, retained across retries.
    /// Boxed so this usually-absent fact does not enlarge the pervasive
    /// `ProcessRecord` that flows through the runtime; serde treats
    /// `Option<Box<T>>` identically to `Option<T>`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancel_request: Option<Box<CancelRequest>>,
    /// The one lifecycle state the process is in. Its status, waits and
    /// outcome are read from it ([`Self::status`], [`Self::waits`],
    /// [`Self::terminal`]) and stored nowhere beside it.
    #[serde(serialize_with = "super::super::durable_serde::serialize")]
    pub lifecycle: ProcessLifecycleState,
}

impl ProcessRecord {
    /// Creation metadata, written once by SQL stores. Execution input and
    /// definition live in [`Self::execution_storage`], since succession replaces
    /// that pair; lifecycle batches never resubmit either object.
    #[doc(hidden)]
    pub fn start_storage(&self) -> impl Serialize + '_ {
        #[derive(Serialize)]
        struct Start<'a> {
            id: &'a ProcessId,
            #[serde(skip_serializing_if = "Option::is_none")]
            start_key: &'a Option<StartKey>,
            lifetime: &'a LifetimeDecision,
            ancestry: &'a Ancestry,
            #[serde(skip_serializing_if = "Option::is_none")]
            session_capability: &'a Option<SessionId>,
            provenance: &'a ProcessProvenance,
            #[serde(skip_serializing_if = "Option::is_none")]
            trace: &'a Option<lash_trace::DurableTraceScope>,
            #[serde(skip_serializing_if = "std::ops::Not::not")]
            trace_nodes: bool,
            trace_class: &'a super::super::ProcessTraceClass,
            created_at_ms: u64,
        }
        // Exhaustive so every new record field must choose its storage owner.
        let Self {
            id,
            start_key,
            lifetime,
            ancestry,
            session_capability,
            provenance,
            trace,
            trace_nodes,
            trace_class,
            created_at_ms,
            input: _,
            identity: _,
            last_event_sequence: _,
            updated_at_ms: _,
            external_ref: _,
            cancel_request: _,
            lifecycle: _,
        } = self;
        Start {
            id,
            start_key,
            lifetime,
            ancestry,
            session_capability,
            provenance,
            trace,
            trace_nodes: *trace_nodes,
            trace_class,
            created_at_ms: *created_at_ms,
        }
    }

    /// The current engine input and its admitted identity. Written at creation
    /// and replaced only by a committed successor (FIG-6121), never by an
    /// ordinary lifecycle change. No unused original input is retained.
    #[doc(hidden)]
    pub fn execution_storage(&self) -> impl Serialize + '_ {
        #[derive(Serialize)]
        struct Execution<'a> {
            input: &'a Arc<ProcessInput>,
            identity: &'a ProcessIdentity,
        }
        Execution {
            input: &self.input,
            identity: &self.identity,
        }
    }

    /// Only the facts an event batch may change. SQL indexes derive their
    /// lifecycle projections from this object alone.
    #[doc(hidden)]
    pub fn lifecycle_storage(&self) -> impl Serialize + '_ {
        #[derive(Serialize)]
        struct Lifecycle<'a> {
            last_event_sequence: u64,
            updated_at_ms: u64,
            #[serde(skip_serializing_if = "Option::is_none")]
            external_ref: &'a Option<ProcessExternalRef>,
            #[serde(skip_serializing_if = "Option::is_none")]
            cancel_request: &'a Option<Box<CancelRequest>>,
            lifecycle: super::super::Durable<&'a ProcessLifecycleState>,
        }
        Lifecycle {
            last_event_sequence: self.last_event_sequence,
            updated_at_ms: self.updated_at_ms,
            external_ref: &self.external_ref,
            cancel_request: &self.cancel_request,
            lifecycle: super::super::Durable(&self.lifecycle),
        }
    }
}

/// The lifecycle state of a process record: each state owns the facts that
/// exist only in it, so a record cannot hold a wait beside an outcome, or a
/// terminal status without one. A waiting process lists everything it is
/// blocked on, keyed by `WaitKind`, and never an empty list. A parked process is
/// its actor's state (ADR 0132 §11), not a record fact. An ended process
/// holds its outcome and the time of the committed fact that ended it: no
/// later fact changes either.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    remote = "Self",
    tag = "state",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ProcessLifecycleState {
    Running {},
    Waiting {
        waits: ProcessWaits,
    },
    Terminal {
        #[serde(serialize_with = "super::super::durable_serde::serialize")]
        outcome: ProcessTerminal,
        /// When the committed fact that ended the process occurred. Held
        /// apart from the record's `updated_at_ms`, which every later fact
        /// overwrites.
        occurred_at_ms: u64,
    },
}

impl ProcessLifecycleState {
    /// The state of a newly registered process.
    pub fn running() -> Self {
        Self::Running {}
    }

    /// A representative state of `status`, for fixtures that need a record
    /// in a status and do not care what put it there: a terminal status
    /// holds a minimal outcome of that status, and `waiting` a call wait.
    #[cfg(any(test, feature = "testing"))]
    pub fn fixture(status: ProcessStatus) -> Self {
        let settled = |output| Self::Terminal {
            outcome: ProcessTerminal::from_tool_output(output),
            occurred_at_ms: 0,
        };
        match status {
            ProcessStatus::Running => Self::running(),
            ProcessStatus::Waiting => Self::Waiting {
                waits: ProcessWaits::new(WaitState {
                    kind: WaitKind::Call {
                        call_id: crate::ToolCallId::fixture("fixture"),
                        tool_id: crate::ToolId::from("fixture"),
                    },
                    since_ms: 0,
                    site: None,
                }),
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
                occurred_at_ms: 0,
            },
        }
    }

    /// The status this state is. Exhaustive on purpose: the status is a
    /// function of the state and has no home of its own.
    pub fn status(&self) -> ProcessStatus {
        match self {
            Self::Running { .. } => ProcessStatus::Running,
            Self::Waiting { .. } => ProcessStatus::Waiting,
            Self::Terminal { outcome, .. } => outcome.status().into(),
        }
    }

    /// Everything the process is blocked on; empty unless it waits.
    pub fn waits(&self) -> &[WaitState] {
        match self {
            Self::Waiting { waits } => waits.as_slice(),
            Self::Running { .. } | Self::Terminal { .. } => &[],
        }
    }

    /// The state after `wait` is entered: it joins the waits the process
    /// already has, replacing one of the same identity.
    pub fn entering(&self, wait: &WaitState) -> Self {
        let mut waits = match self {
            Self::Waiting { waits } => waits.clone(),
            _ => {
                return Self::Waiting {
                    waits: ProcessWaits::new(wait.clone()),
                };
            }
        };
        waits.insert(wait.clone());
        Self::Waiting { waits }
    }

    /// The state after `wait` ends: running once no wait is left.
    pub fn leaving(&self, wait: &WaitState) -> Self {
        let waits: Vec<_> = self
            .waits()
            .iter()
            .filter(|held| held.kind != wait.kind)
            .cloned()
            .collect();
        match ProcessWaits::try_from(waits) {
            Ok(waits) => Self::Waiting { waits },
            Err(_) => Self::running(),
        }
    }

    /// The outcome the process ended in.
    pub fn terminal(&self) -> Option<&ProcessTerminal> {
        match self {
            Self::Terminal { outcome, .. } => Some(outcome),
            Self::Running { .. } | Self::Waiting { .. } => None,
        }
    }

    /// When the committed fact that ended the process occurred.
    pub fn terminal_at_ms(&self) -> Option<u64> {
        match self {
            Self::Terminal { occurred_at_ms, .. } => Some(*occurred_at_ms),
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
    run: Option<&crate::TurnAddress>,
) -> ProcessLineage {
    let own_session = match input {
        ProcessInput::SessionTurn { create_request, .. } => {
            Some(process_session_id(process_id, create_request))
        }
        ProcessInput::Engine { .. } => None,
    };
    ProcessLineage::of_process(
        process_id,
        ancestry,
        session_capability,
        own_session.as_ref(),
        run,
    )
}

impl ProcessRecord {
    /// The launch an engine process's start sealed: its environment and
    /// engine settings. A session-turn process has none (D5).
    pub fn launch(&self) -> Option<&EngineLaunch> {
        self.input.launch()
    }

    /// The environment an engine process runs under, read off its input.
    pub fn env_ref(&self) -> Option<&ProcessEnvRef> {
        self.launch().map(|launch| &launch.env)
    }

    /// The engine settings an engine process's start stated, read off its
    /// input. Every run reads these and never a live default (D4).
    pub fn engine_settings(&self) -> Option<&EngineSettings> {
        self.launch().and_then(|launch| launch.settings.as_ref())
    }

    /// The lineage this process's body starts children under, read back from
    /// its row (FIG-3607 R1).
    pub fn lineage(&self) -> ProcessLineage {
        recorded_lineage(
            &self.id,
            &self.input,
            &self.ancestry,
            self.session_capability.as_ref(),
            self.provenance.run.as_ref(),
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
            provenance: registration.provenance,
            trace: Some(trace),
            trace_nodes: registration.trace_nodes,
            trace_class: registration.trace_class,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
            external_ref: None,
            cancel_request: None,
            lifecycle: ProcessLifecycleState::running(),
        }
    }

    /// The status the record's lifecycle state is: what a store projects
    /// into its `status` column.
    pub fn status(&self) -> ProcessStatus {
        self.lifecycle.status()
    }

    /// Everything the process is blocked on; empty unless it waits.
    pub fn waits(&self) -> &[WaitState] {
        self.lifecycle.waits()
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
