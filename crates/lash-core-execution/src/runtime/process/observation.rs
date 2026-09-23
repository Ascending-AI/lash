use crate::SessionId;
use lash_sansio::CancelRequest;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::plugin::PluginError;

use super::events::{ProcessAwaitOutput, ProcessEvent};
use super::model::{
    AbandonRequest, ProcessExecutionEnvRef, ProcessExternalRef, ProcessId, ProcessIdentity,
    ProcessIncarnation, ProcessInput, ProcessLease, ProcessLifecyclePolicy, ProcessListFilter,
    ProcessOriginator, ProcessOriginatorFilter, ProcessRecord, ProcessStarted, ProcessStatus,
    RecoveryContract, SessionScope, WaitState,
};
use super::registry::ProcessRegistry;

#[derive(Clone)]
pub struct ProcessWorkObserver {
    registry: Arc<dyn ProcessRegistry>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcessWorkSnapshot {
    pub session_id: SessionId,
    pub visible_processes: Vec<super::model::ProcessRef>,
    pub items: Vec<ObservedWorkItem>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ObservedWorkItem {
    pub process: ObservedProcess,
    pub events: Vec<ObservedProcessEvent>,
}

/// The record/event-tail coherence of an [`ObservedWorkItem`], derived from
/// the carried record and events rather than stored. Consumers must not
/// present lifecycle state from an item whose [`ObservedWorkItem::state`]
/// derives [`ObservedWorkItemState::EventTailMismatch`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ObservedWorkItemState {
    Coherent,
    EventTailMismatch {
        record_sequence: u64,
        event_tail_sequence: u64,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ObservedProcess {
    pub process_id: ProcessId,
    pub incarnation: ProcessIncarnation,
    /// Sequence of the newest event folded into this observed record.
    pub last_event_sequence: u64,
    pub lifecycle: ProcessStatus,
    /// Declared parent scope and parent-end action. `lifecycle` above is the
    /// status fold; this is the policy the host chose at registration.
    pub policy: ProcessLifecyclePolicy,
    pub identity: ProcessIdentity,
    /// Declared recovery contract (ADR 0019). Raw fact; hosts classify.
    pub disposition: RecoveryContract,
    /// Human-readable summary of the terminal failure, for display only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Typed classification of the same terminal failure. Present exactly when
    /// `error` is, so a polling host discriminates a failure class or a
    /// cancellation without matching the display string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<crate::ObservedProcessFailure>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    /// Durable execution-started fact, if the row has begun executing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_started: Option<ProcessStarted>,
    /// Current lease holder identity, if the row is leased (ADR 0019). Raw
    /// fact for host-side staleness classification — no derived "stuck" verdict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_holder: Option<crate::LeaseOwnerIdentity>,
    /// Current lease expiry, paired with `lease_holder`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_expires_at_ms: Option<u64>,
    /// Pending Abandon Request the sweep reconciles once the lease lapses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abandon_request: Option<AbandonRequest>,
    /// The first accepted process cancellation request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancel_request: Option<CancelRequest>,
    pub input: ProcessInput,
    pub originator: ProcessOriginator,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_ref: Option<ProcessExecutionEnvRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caused_by: Option<crate::CausalRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_ref: Option<ProcessExternalRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<WaitState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_session_id: Option<SessionId>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ObservedProcessEvent {
    pub sequence: u64,
    pub event_type: String,
    pub occurred_at_ms: u64,
    pub payload: serde_json::Value,
}

/// Payload-free event metadata for list and timeline views.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedProcessEventLite {
    pub sequence: u64,
    pub event_type: String,
}

pub type ObservedProcessEventPage =
    super::ProcessEventPage<ObservedProcessEvent, ObservedProcessEventLite>;
pub type ObservedProcessEventReadOutcome = super::ProcessEventReadOutcome<ObservedProcessEventPage>;

impl ObservedWorkItem {
    /// The observed process's identity kind.
    pub fn kind(&self) -> &str {
        self.process.kind()
    }

    /// The observed process's display label.
    pub fn label(&self) -> &str {
        self.process.label()
    }

    /// Sequence of the newest event carried by `events`, or zero for an empty
    /// tail. Computed rather than carried: a stored copy could only ever agree
    /// or lie, so comparing this with `process.last_event_sequence` is the one
    /// spelling of a mis-paired record/event snapshot.
    pub fn event_tail_sequence(&self) -> u64 {
        self.events.last().map_or(0, |event| event.sequence)
    }

    /// Whether the independently read process record and event tail describe
    /// one coherent event position. Derived on read so a decoded or hand-built
    /// item cannot hold a verdict that disagrees with its carried fields.
    pub fn state(&self) -> ObservedWorkItemState {
        let event_tail_sequence = self.event_tail_sequence();
        if self.process.last_event_sequence == event_tail_sequence {
            ObservedWorkItemState::Coherent
        } else {
            ObservedWorkItemState::EventTailMismatch {
                record_sequence: self.process.last_event_sequence,
                event_tail_sequence,
            }
        }
    }

    /// Reports whether the bounded observer retry still left independently
    /// read record and event-tail positions mis-paired.
    pub fn has_mispaired_event_tail(&self) -> bool {
        matches!(
            self.state(),
            ObservedWorkItemState::EventTailMismatch { .. }
        )
    }
}

/// Per-item event tail in session snapshots. Snapshots are polled by
/// docks/UIs, so per-poll cost must stay bounded instead of growing with a
/// process's full event history; detail views page through `event_page`
/// with a cursor.
pub const SNAPSHOT_EVENT_TAIL: usize = 32;
const SNAPSHOT_READ_ATTEMPTS: usize = 2;

impl ProcessWorkObserver {
    pub fn new(registry: Arc<dyn ProcessRegistry>) -> Self {
        Self { registry }
    }

    pub async fn snapshot_for_session(
        &self,
        session_id: impl Into<SessionId>,
    ) -> Result<ProcessWorkSnapshot, PluginError> {
        let session_id = session_id.into();
        let entries = self
            .registry
            .list_observed_by(
                &session_id,
                &crate::ProcessListFilter {
                    status: crate::ProcessStatusFilter::Any,
                    ..Default::default()
                },
            )
            .await?;
        let mut items = Vec::new();
        for record in entries {
            items.push(self.work_item_from_record(record).await?);
        }
        items.sort_by(|left, right| {
            right
                .process
                .updated_at_ms
                .cmp(&left.process.updated_at_ms)
                .then_with(|| right.process.created_at_ms.cmp(&left.process.created_at_ms))
                .then_with(|| left.process.process_id.cmp(&right.process.process_id))
        });
        let visible_processes = items
            .iter()
            .map(|item| {
                super::model::ProcessRef::new(
                    item.process.process_id.clone(),
                    item.process.incarnation,
                )
            })
            .collect();
        Ok(ProcessWorkSnapshot {
            session_id,
            visible_processes,
            items,
        })
    }

    /// Snapshot every process matching `filter`, including the bounded event
    /// tail used by host work rails. Unlike [`Self::snapshot_for_session`],
    /// this is the runtime-wide observation surface: it does not depend on a
    /// session observer edge and therefore continues to expose processes whose
    /// originating session has been deleted.
    /// Because observer edges are bypassed, the host must authorize access; routing
    /// identity is not authorization.
    pub async fn snapshot_all(
        &self,
        filter: &ProcessListFilter,
    ) -> Result<Vec<ObservedWorkItem>, PluginError> {
        let records = self.registry.list_processes(filter).await?;
        let mut items = Vec::with_capacity(records.len());
        for record in records {
            items.push(self.work_item_from_record(record).await?);
        }
        items.sort_by(|left, right| {
            right
                .process
                .updated_at_ms
                .cmp(&left.process.updated_at_ms)
                .then_with(|| right.process.created_at_ms.cmp(&left.process.created_at_ms))
                .then_with(|| left.process.process_id.cmp(&right.process.process_id))
        });
        Ok(items)
    }

    pub(crate) async fn work_item_from_record(
        &self,
        mut record: ProcessRecord,
    ) -> Result<ObservedWorkItem, PluginError> {
        for attempt in 0..SNAPSHOT_READ_ATTEMPTS {
            let process_id = record.id.clone();
            let events: Vec<_> = self
                .registry
                .recent_events(&process_id, SNAPSHOT_EVENT_TAIL)
                .await?
                .into_iter()
                .map(ObservedProcessEvent::from)
                .collect();
            let lease = self.registry.get_process_lease(&process_id).await?;
            let process = ObservedProcess::from_record(record, lease);
            let item = ObservedWorkItem { process, events };
            if !item.has_mispaired_event_tail() || attempt + 1 == SNAPSHOT_READ_ATTEMPTS {
                return Ok(item);
            }
            let Some(refreshed) = self.registry.get_process(&process_id).await? else {
                return Ok(item);
            };
            record = refreshed;
        }
        unreachable!("snapshot read attempt bound is non-zero")
    }

    pub async fn process(
        &self,
        process_id: &ProcessId,
    ) -> Result<Option<ObservedProcess>, PluginError> {
        let Some(record) = self.registry.get_process(process_id).await? else {
            return Ok(None);
        };
        let lease = self.registry.get_process_lease(process_id).await?;
        Ok(Some(ObservedProcess::from_record(record, lease)))
    }

    pub async fn list(
        &self,
        filter: &ProcessListFilter,
    ) -> Result<Vec<ObservedProcess>, PluginError> {
        let records = self.registry.list_processes(filter).await?;
        self.observe_records(records).await
    }

    /// List processes a session may address — the observer filter. A process is
    /// visible here only if `scope.session_id` has an observer edge. This is the
    /// single home for the observer-scoped view; the session facade sugar is a thin
    /// caller of this method, never a parallel implementation.
    pub async fn list_observed_by(
        &self,
        scope: &SessionScope,
        filter: &ProcessListFilter,
    ) -> Result<Vec<ObservedProcess>, PluginError> {
        let records = self
            .registry
            .list_observed_by(&scope.session_id, filter)
            .await?;
        self.observe_records(records).await
    }

    /// List processes a session originated — the provenance filter (ADR 0019 /
    /// process design grill). "Originated by" is the lineage lens, distinct from
    /// the observer lens: a process matches when its recorded originator is a
    /// session whose id equals `scope.session_id` (and its agent frame, when
    /// `scope` names one), regardless of which sessions currently observe it.
    ///
    /// The scope is handed to the store as a typed `ProcessOriginatorFilter`
    /// rather than pre-flattened to an id string: the store pushes the session
    /// id down to its `originator_id` index and the shared Rust predicate
    /// narrows to the named agent frame, so the lens no longer has a
    /// caller-side copy of the match rule that could drift from the store's.
    /// A filter the caller already populated with an originator is replaced,
    /// not intersected — this lens owns that field.
    pub async fn list_originated_by(
        &self,
        scope: &SessionScope,
        filter: &ProcessListFilter,
    ) -> Result<Vec<ObservedProcess>, PluginError> {
        let filter = ProcessListFilter {
            originator: Some(ProcessOriginatorFilter::Session(scope.clone())),
            ..filter.clone()
        };
        let records = self.registry.list_processes(&filter).await?;
        self.observe_records(records).await
    }

    async fn observe_records(
        &self,
        records: Vec<ProcessRecord>,
    ) -> Result<Vec<ObservedProcess>, PluginError> {
        let process_ids = records
            .iter()
            .map(|record| record.id.clone())
            .collect::<Vec<_>>();
        let leases = self.registry.get_process_leases(&process_ids).await?;
        if records.len() != leases.len() {
            return Err(PluginError::Session(format!(
                "process registry batch lease read returned {} rows for {} process ids",
                leases.len(),
                records.len()
            )));
        }
        Ok(records
            .into_iter()
            .zip(leases)
            .map(|(record, lease)| ObservedProcess::from_record(record, lease))
            .collect())
    }

    pub async fn event_page(
        &self,
        process_id: &ProcessId,
        limit: std::num::NonZeroUsize,
        mode: super::ProcessEventQueryMode,
        continuation: Option<super::ProcessEventPageToken>,
    ) -> Result<ObservedProcessEventReadOutcome, PluginError> {
        let outcome = self
            .registry
            .event_page(process_id, limit, mode, continuation)
            .await?;
        Ok(match outcome {
            super::ProcessEventReadOutcome::NoLongerRetained(retention) => {
                super::ProcessEventReadOutcome::NoLongerRetained(retention)
            }
            super::ProcessEventReadOutcome::Retained(page) => {
                let events = match page.events {
                    super::ProcessEventPageEvents::Full(events) => {
                        super::ProcessEventPageEvents::Full(
                            events.into_iter().map(ObservedProcessEvent::from).collect(),
                        )
                    }
                    super::ProcessEventPageEvents::Lite(events) => {
                        super::ProcessEventPageEvents::Lite(
                            events
                                .into_iter()
                                .map(|event| ObservedProcessEventLite {
                                    sequence: event.sequence,
                                    event_type: event.event_type,
                                })
                                .collect(),
                        )
                    }
                };
                super::ProcessEventReadOutcome::Retained(super::ProcessEventPage {
                    events,
                    more: page.more,
                })
            }
        })
    }
}

impl ObservedProcess {
    /// `lease` is the current lease row (if any), read separately so the observer exposes
    /// holder identity and expiry as raw facts — no derived "stuck" classification (ADR 0019).
    fn from_record(record: ProcessRecord, lease: Option<ProcessLease>) -> Self {
        let lifecycle = record.status;
        let input = record.input.as_ref().clone();
        let identity = record.identity;
        let process_id = record.id;
        let incarnation = record.incarnation;
        let last_event_sequence = record.last_event_sequence;
        let (lease_holder, lease_expires_at_ms) = match lease {
            Some(lease) => (Some(lease.owner), Some(lease.expires_at_epoch_ms)),
            None => (None, None),
        };
        Self {
            process_id,
            incarnation,
            last_event_sequence,
            lifecycle,
            policy: record.lifecycle,
            identity,
            disposition: record.disposition,
            error: terminal_error(record.outcome.as_ref()),
            error_code: terminal_error_code(record.outcome.as_ref()),
            created_at_ms: record.created_at_ms,
            updated_at_ms: record.updated_at_ms,
            first_started: record.first_started.map(|started| *started),
            lease_holder,
            lease_expires_at_ms,
            abandon_request: record.abandon_request.map(|request| *request),
            cancel_request: record.cancel_request.map(|request| *request),
            originator: record.provenance.originator,
            env_ref: record.env_ref,
            caused_by: record.provenance.caused_by,
            external_ref: record.external_ref,
            wait: record.wait,
            child_session_id: child_session_id(&input).map(Into::into),
            input,
        }
    }

    /// Stable identity of this incarnation in a host work graph.
    ///
    /// Computed rather than carried: it is a function of the process id and
    /// incarnation, so a transport that shipped it could only ever agree or
    /// lie.
    pub fn graph_key(&self) -> String {
        format!(
            "process:{}:incarnation:{}",
            self.process_id, self.incarnation
        )
    }

    /// The identity kind this process was registered under.
    pub fn kind(&self) -> &str {
        self.identity.kind.as_str()
    }

    /// The display label: the registered label, else the kind.
    pub fn label(&self) -> &str {
        self.identity
            .label
            .as_deref()
            .unwrap_or(self.identity.kind.as_str())
    }

    /// The storage label of the current lifecycle status.
    pub fn status_label(&self) -> &'static str {
        self.lifecycle.label()
    }

    /// Whether the lifecycle status is terminal.
    pub fn terminal(&self) -> bool {
        self.lifecycle.is_terminal()
    }
}

impl From<ProcessEvent> for ObservedProcessEvent {
    fn from(event: ProcessEvent) -> Self {
        Self {
            sequence: event.sequence,
            event_type: event.event_type,
            occurred_at_ms: event.occurred_at,
            payload: event.payload,
        }
    }
}

/// The typed classification of the same terminal outcome `terminal_error`
/// renders as prose. The two are produced from one settled outcome and are
/// present or absent together.
fn terminal_error_code(
    outcome: Option<&ProcessAwaitOutput>,
) -> Option<crate::ObservedProcessFailure> {
    match outcome? {
        ProcessAwaitOutput::Settled { output } => match &output.outcome {
            crate::ToolCallOutcome::Failure(failure) => {
                Some(crate::ObservedProcessFailure::Failed {
                    class: failure.class.clone(),
                    code: failure.code.clone(),
                })
            }
            crate::ToolCallOutcome::Cancelled(cancellation) => {
                Some(crate::ObservedProcessFailure::Cancelled {
                    origin: cancellation.origin,
                })
            }
            crate::ToolCallOutcome::Success(_) => None,
        },
        ProcessAwaitOutput::Abandoned { .. } | ProcessAwaitOutput::NoLongerRetained { .. } => None,
    }
}

fn terminal_error(outcome: Option<&ProcessAwaitOutput>) -> Option<String> {
    match outcome? {
        ProcessAwaitOutput::Settled { output } => match &output.outcome {
            crate::ToolCallOutcome::Failure(failure) => Some(failure.message.clone()),
            crate::ToolCallOutcome::Cancelled(cancellation) => Some(cancellation.message.clone()),
            crate::ToolCallOutcome::Success(_) => None,
        },
        // Abandonment is not a reported failure; the status label conveys it and
        // the evidence rides the terminal event. No derived error string here.
        ProcessAwaitOutput::Abandoned { .. } | ProcessAwaitOutput::NoLongerRetained { .. } => None,
    }
}

fn child_session_id(input: &ProcessInput) -> Option<String> {
    match input {
        ProcessInput::SessionTurn { create_request, .. } => {
            create_request.session_id.clone().map(Into::into)
        }
        ProcessInput::ToolCall { .. }
        | ProcessInput::Engine { .. }
        | ProcessInput::External { .. } => None,
    }
}
