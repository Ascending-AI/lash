use crate::SessionId;
use lash_sansio::CancelRequest;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::plugin::PluginError;

use super::events::{ProcessEvent, ProcessTerminal};
use super::model::{
    ProcessChange, ProcessChangeCursor, ProcessExecutionEnvRef, ProcessExternalRef, ProcessId,
    ProcessIdentity, ProcessInput, ProcessLifecycleState, ProcessListFilter, ProcessOriginator,
    ProcessOriginatorFilter, ProcessRecord, ProcessStarted, ProcessStatus, ProcessTombstone,
    SessionScope, WaitState,
};
use super::registry::ProcessRegistry;

#[derive(Clone)]
pub struct ProcessWorkObserver {
    work_limits: lash_trace::ObservationWorkLimits,
    registry: Arc<dyn ProcessRegistry>,
    read_attempts: std::num::NonZeroUsize,
    /// The store whose actor rows say why a process is parked.
    actors: Option<Arc<dyn lash_durable::DurableStore>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcessWorkSnapshot {
    pub session_id: SessionId,
    pub visible_processes: Vec<super::model::ProcessId>,
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

/// A process as a host reads it: the facts of its durable row at
/// `last_event_sequence`, and its actor's park. Every read of a process
/// (one process, a roster, a change, an observation snapshot) answers this
/// shape.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ObservedProcess {
    pub process_id: ProcessId,
    /// Sequence of the newest event folded into this observed record.
    pub last_event_sequence: u64,
    /// The one state the process is in, as its row holds it: running,
    /// waiting with everything it is blocked on (each wait names what it
    /// waits for and the node that waits, never a key that would resolve
    /// it), or terminal with its typed outcome and the time of the
    /// committed fact that ended it.
    pub lifecycle: ProcessLifecycleState,
    /// The recorded lifetime decision: what ends the process.
    pub lifetime: crate::LifetimeDecision,
    /// The recorded ancestry, nearest first; empty for a root start.
    pub ancestry: crate::Ancestry,
    pub identity: ProcessIdentity,
    pub created_at_ms: u64,
    /// When the newest folded event occurred. A fact appended after the
    /// process ended moves it; the terminal time is in `lifecycle`.
    pub updated_at_ms: u64,
    /// Durable execution-started fact, if the row has begun executing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_started: Option<ProcessStarted>,
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
    /// Why the process's actor is parked, while it is: it runs no engine
    /// code until an operator redrives it. A fact of its actor, beside the
    /// lifecycle and never folded into it; `None` from an observer that was
    /// given no actor store to read it from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub park: Option<crate::ProcessParkReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_session_id: Option<SessionId>,
}

/// A bounded canonical roster and the fence from before its scan began.
/// Follow every continuation, then apply global changes after `change_cursor`.
/// If that cursor is pruned, discard the roster and start another scan.
#[derive(Clone, Debug, PartialEq)]
pub struct ProcessRosterPage {
    pub processes: Vec<ObservedProcess>,
    pub continuation: Option<super::ProcessRosterCursor>,
    pub change_cursor: ProcessChangeCursor,
    pub verified_through: ProcessChangeCursor,
}

/// One change of the process roster, as a host reads it: the process as it
/// now stands, or the tombstone of one that was pruned. A page of changes
/// converges on the latest row of each process; it is not every transition.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ObservedProcessChange {
    Upsert { process: Box<ObservedProcess> },
    Deleted { tombstone: ProcessTombstone },
}

/// One lifecycle fact of a process's log, as a host observes it. It travels
/// as its kind's spelling and payload, exactly as the log stores it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    into = "ObservedProcessEventRecord",
    try_from = "ObservedProcessEventRecord"
)]
pub struct ObservedProcessEvent {
    pub sequence: u64,
    pub fact: super::ProcessLifecycleFact,
    pub occurred_at_ms: u64,
}

/// The wire form of an [`ObservedProcessEvent`].
#[derive(Clone, Debug, Serialize, Deserialize)]
struct ObservedProcessEventRecord {
    sequence: u64,
    event_type: String,
    occurred_at_ms: u64,
    payload: serde_json::Value,
}

impl From<ObservedProcessEvent> for ObservedProcessEventRecord {
    fn from(event: ObservedProcessEvent) -> Self {
        Self {
            sequence: event.sequence,
            event_type: event.fact.event_type().to_owned(),
            occurred_at_ms: event.occurred_at_ms,
            payload: event.fact.payload(),
        }
    }
}

impl TryFrom<ObservedProcessEventRecord> for ObservedProcessEvent {
    type Error = PluginError;

    fn try_from(record: ObservedProcessEventRecord) -> Result<Self, Self::Error> {
        Ok(Self {
            sequence: record.sequence,
            // The wire carries what this release's reader already lifted, so
            // it is read in the newest fleet's window: no stored `F` applies.
            fact: super::ProcessLifecycleFact::decode(
                &record.event_type,
                record.payload,
                crate::FleetFormat::current(),
            )?,
            occurred_at_ms: record.occurred_at_ms,
        })
    }
}

/// Payload-free event metadata for list and timeline views.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedProcessEventLite {
    pub sequence: u64,
    #[serde(rename = "event_type")]
    pub kind: super::ProcessEventKind,
}

pub type ObservedProcessEventPage =
    super::ProcessEventPage<ObservedProcessEvent, ObservedProcessEventLite>;
pub type ObservedProcessEventReadOutcome = super::ProcessEventReadOutcome<ObservedProcessEventPage>;

impl ObservedWorkItem {
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

impl ProcessWorkObserver {
    pub fn new(registry: Arc<dyn ProcessRegistry>) -> Self {
        Self {
            registry,
            work_limits: lash_trace::ObservationWorkLimits::standard(),
            read_attempts: std::num::NonZeroUsize::MIN.saturating_add(1),
            actors: None,
        }
    }

    /// Read each observed process's park from `actors`, the durable store
    /// its actor row lives in.
    #[must_use]
    pub fn with_actor_parks(mut self, actors: Arc<dyn lash_durable::DurableStore>) -> Self {
        self.actors = Some(actors);
        self
    }

    /// `record` as a host observes it, with its actor's park.
    async fn observed(&self, record: ProcessRecord) -> Result<ObservedProcess, PluginError> {
        let park = match &self.actors {
            Some(actors) if !record.is_terminal() => {
                crate::runtime::actor::process::park_of(actors.as_ref(), &record.id)
                    .await
                    .map_err(|error| {
                        PluginError::RuntimeEffectController(
                            crate::RuntimeEffectControllerError::new(
                                crate::RuntimeErrorCode::StoreCommitFailed,
                                error.to_string(),
                            ),
                        )
                    })?
            }
            _ => None,
        };
        let mut process = ObservedProcess::from_record(record);
        process.park = park;
        Ok(process)
    }

    /// Set the bounded record/event-tail pairing retries. The standard preset
    /// attempts twice, a historical choice without workload measurements.
    pub fn with_read_attempts(mut self, attempts: std::num::NonZeroUsize) -> Self {
        self.read_attempts = attempts;
        self
    }

    /// Configure the event tail of snapshots, independently of durable history.
    #[must_use]
    pub fn with_work_limits(mut self, limits: lash_trace::ObservationWorkLimits) -> Self {
        self.work_limits = limits;
        self
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
            .map(|item| item.process.process_id.clone())
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
        for attempt in 0..self.read_attempts.get() {
            let process_id = record.id.clone();
            let events: Vec<_> = self
                .registry
                .recent_events(&process_id, self.work_limits.process_snapshot_event_tail)
                .await?
                .into_iter()
                .map(ObservedProcessEvent::from)
                .collect();
            let process = self.observed(record).await?;
            let item = ObservedWorkItem { process, events };
            if !item.has_mispaired_event_tail() || attempt + 1 == self.read_attempts.get() {
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
        Ok(Some(self.observed(record).await?))
    }

    pub async fn list(
        &self,
        filter: &ProcessListFilter,
        limit: std::num::NonZeroUsize,
        continuation: Option<super::ProcessRosterCursor>,
    ) -> Result<ProcessRosterPage, PluginError> {
        let page = self
            .registry
            .list_processes_page(filter, limit, continuation)
            .await?;
        Ok(ProcessRosterPage {
            processes: self.observe_records(page.records).await?,
            continuation: page.continuation,
            change_cursor: page.change_cursor,
            verified_through: page.verified_through,
        })
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

    /// List processes a session originated — the provenance filter (ADR 0011 /
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

    /// Read the roster changes after `cursor`, oldest first, and the cursor
    /// that continues them. Unscoped: it reads every process on the store, so
    /// the host must authorize access.
    pub async fn changed_since(
        &self,
        cursor: ProcessChangeCursor,
        limit: usize,
    ) -> Result<(Vec<ObservedProcessChange>, ProcessChangeCursor), PluginError> {
        let (changes, next) = self.registry.processes_changed_since(cursor, limit).await?;
        let mut observed = Vec::with_capacity(changes.len());
        for change in changes {
            observed.push(match change {
                ProcessChange::Upsert { record } => ObservedProcessChange::Upsert {
                    process: Box::new(self.observed(*record).await?),
                },
                ProcessChange::Deleted { tombstone } => {
                    ObservedProcessChange::Deleted { tombstone }
                }
            });
        }
        Ok((observed, next))
    }

    async fn observe_records(
        &self,
        records: Vec<ProcessRecord>,
    ) -> Result<Vec<ObservedProcess>, PluginError> {
        let mut observed = Vec::with_capacity(records.len());
        for record in records {
            observed.push(self.observed(record).await?);
        }
        Ok(observed)
    }

    /// Read a page of one exact process lifetime strictly after
    /// `after_sequence`.
    pub async fn event_page(
        &self,
        process_id: &super::ProcessId,
        after_sequence: u64,
        limit: std::num::NonZeroUsize,
        mode: super::ProcessEventQueryMode,
    ) -> Result<ObservedProcessEventReadOutcome, PluginError> {
        let outcome = self
            .registry
            .event_page_after(process_id, after_sequence, limit, mode)
            .await?;
        Ok(Self::observed_page(outcome))
    }

    /// Read the first page of the lifetime `process_id` currently names; a
    /// pruned process is a typed no-longer-retained outcome.
    pub async fn first_event_page(
        &self,
        process_id: &ProcessId,
        limit: std::num::NonZeroUsize,
        mode: super::ProcessEventQueryMode,
    ) -> Result<ObservedProcessEventReadOutcome, PluginError> {
        let outcome = self.registry.event_page(process_id, limit, mode).await?;
        Ok(Self::observed_page(outcome))
    }

    fn observed_page(
        outcome: super::ProcessEventReadOutcome<super::ProcessEventPage>,
    ) -> ObservedProcessEventReadOutcome {
        match outcome {
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
                                    kind: event.kind,
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
        }
    }
}

impl ObservedProcess {
    /// `record` as a host reads it, without its actor's park.
    fn from_record(record: ProcessRecord) -> Self {
        let child_session_id = match record.input.as_ref() {
            ProcessInput::SessionTurn { .. } => record.lineage().session().cloned(),
            ProcessInput::Engine { .. } => None,
        };
        let input = record.input.as_ref().clone();
        Self {
            process_id: record.id,
            last_event_sequence: record.last_event_sequence,
            lifecycle: record.lifecycle,
            lifetime: record.lifetime,
            ancestry: record.ancestry,
            identity: record.identity,
            created_at_ms: record.created_at_ms,
            updated_at_ms: record.updated_at_ms,
            first_started: record.first_started.map(|started| *started),
            cancel_request: record.cancel_request.map(|request| *request),
            originator: record.provenance.originator,
            env_ref: record.env_ref,
            caused_by: record.provenance.caused_by,
            external_ref: record.external_ref,
            park: None,
            child_session_id,
            input,
        }
    }

    /// The status the lifecycle state is.
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
}

impl From<ProcessEvent> for ObservedProcessEvent {
    fn from(event: ProcessEvent) -> Self {
        Self {
            sequence: event.sequence,
            fact: event.fact,
            occurred_at_ms: event.occurred_at,
        }
    }
}
