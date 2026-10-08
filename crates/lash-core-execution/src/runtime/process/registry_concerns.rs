//! The narrow concern traits a process registry backend composes.
//!
//! [`ProcessRegistry`](super::registry::ProcessRegistry) is the composed
//! contract: a supertrait bundle over the traits in this module, blanket
//! implemented for any type that implements every concern. Backends implement
//! each concern in its own `impl` block; decorators intercept only the
//! concern(s) they change and delegate the rest. No concern trait carries
//! another concern's obligations beyond the explicitly documented
//! read-dependency supertraits.

use crate::plugin::PluginError;
use std::num::NonZeroUsize;

use super::ProcessCompletionOutcome;
use super::events::{
    ProcessAwaitOutput, ProcessCompletionAuthority, ProcessEvent, ProcessEventAppendReceipt,
    ProcessEventAppendRequest, ProcessEventPage, ProcessEventQueryMode, ProcessEventReadOutcome,
};
use super::model::{
    PreparedProcessRegistration, ProcessChange, ProcessChangeCursor,
    ProcessExecutionWriteAuthority, ProcessExternalRef, ProcessId, ProcessListFilter,
    ProcessObserverBy, ProcessRecord, ProcessRegistration, ProcessRegistrationReceipt,
    ProcessSessionDeleteReport, ProcessStartOutcome, ProcessStarted, SessionId, WaitState,
};
use super::references::ProcessLiveReferenceView;
use super::registry::{
    NonTerminalProcessPage, ParentEndPlan, ProcessPruneReport, ProcessRegistry,
    ProcessRegistryCursor, ProjectionWatermark,
};

/// Point reads and scans over registered processes.
///
/// Identity resolution, record and listing reads, a bounded non-terminal
/// process scan, the trusted change feed, and registry-wide aggregates.
#[async_trait::async_trait]
pub trait ProcessQuery: Send + Sync {
    /// Refuse unless `process_id` names a retained process, answering the id
    /// back: an id no registration minted refuses as
    /// [`PluginError::ProcessUnknown`] and a pruned one as
    /// [`PluginError::ProcessNoLongerRetained`].
    async fn require_process_id(&self, process_id: &ProcessId) -> Result<ProcessId, PluginError> {
        match self.get_process(process_id).await? {
            Some(record) => Ok(record.id),
            None => Err(super::registry_transitions::unknown_process(process_id)),
        }
    }

    /// Read one process by its minted id.
    ///
    /// A retained process answers `Some`, a pruned one refuses with
    /// [`PluginError::ProcessNoLongerRetained`], and an id no registration
    /// ever minted answers `None`. An id is never reused, so no read can reach
    /// a process other than the one the id was minted for.
    async fn get_process(
        &self,
        process_id: &ProcessId,
    ) -> Result<Option<ProcessRecord>, PluginError>;

    /// Read the process a start key registered, while it is retained
    /// (ADR 0107). `None` once no retained process holds the key: never
    /// started, or pruned. A start key is never a reference a caller resolves
    /// to name a process; this is the registrar's own idempotency answer, read
    /// by an engine that must know whether a start already registered.
    async fn get_process_by_start_key(
        &self,
        start_key: &crate::StartKey,
    ) -> Result<Option<ProcessRecord>, PluginError>;

    async fn list_processes(
        &self,
        filter: &ProcessListFilter,
    ) -> Result<Vec<ProcessRecord>, PluginError>;

    /// Return process records whose persisted row changed strictly after
    /// `cursor`, ordered by the backend's per-store change sequence.
    ///
    /// This is a host-level completeness read for trusted projectors. It is not
    /// scoped by observer edges, and the cursor must be treated as opaque outside
    /// the store that issued it.
    async fn processes_changed_since(
        &self,
        cursor: ProcessChangeCursor,
        limit: usize,
    ) -> Result<(Vec<ProcessChange>, ProcessChangeCursor), PluginError>;

    /// Read one bounded page of non-terminal processes, for admission and
    /// lost-run reconciliation. Terminal processes are excluded.
    ///
    /// A first call (`continuation = None`) captures the greatest non-terminal
    /// `process_id` as an inclusive upper bound. Continuations use keyset
    /// pagination strictly after the last returned id and retain that bound.
    /// Consequently, every row that is non-terminal when the scan starts is
    /// returned exactly once unless it becomes terminal before its page is
    /// read; a row completed between pages is never returned again. Concurrent
    /// inserts cannot move the boundary or cause a scan-start row to be skipped
    /// or duplicated. An insert whose id falls inside the captured range may be
    /// returned if it sorts after the cursor. Process ids are not time ordered:
    /// inserts at or below the cursor, as well as inserts beyond the captured
    /// upper bound, wait for the next scan.
    ///
    /// Cursors are opaque outside the issuing registry and are invalid after
    /// switching backends. `limit` is non-zero by construction and is capped
    /// at [`MAX_NON_TERMINAL_PROCESS_PAGE_SIZE`](super::registry::MAX_NON_TERMINAL_PROCESS_PAGE_SIZE).
    async fn list_non_terminal_processes_page(
        &self,
        limit: NonZeroUsize,
        continuation: Option<ProcessRegistryCursor>,
    ) -> Result<NonTerminalProcessPage, PluginError>;

    /// Return the candidate ids that were never registered, preserving input
    /// order. A terminal process retained only as a tombstone is registered
    /// history and must not be offered back to recovery. Durable backends
    /// override this with one anti-join so recovery does not issue one point
    /// read per candidate.
    async fn filter_unregistered_process_ids(
        &self,
        process_ids: &[ProcessId],
    ) -> Result<Vec<ProcessId>, PluginError> {
        let mut missing = Vec::new();
        for process_id in process_ids {
            match self.get_process(process_id).await {
                Ok(Some(_)) | Err(PluginError::ProcessNoLongerRetained { .. }) => {}
                Ok(None) => missing.push(process_id.clone()),
                Err(error) => return Err(error),
            }
        }
        Ok(missing.into_iter().collect())
    }

    /// Return the candidate ids retained as terminal-process tombstones,
    /// preserving input order.
    ///
    /// Cross-store retention uses this to remove trigger deliveries only after
    /// their deterministic process ids have been durably pruned.
    async fn filter_tombstoned_process_ids(
        &self,
        process_ids: &[ProcessId],
    ) -> Result<Vec<ProcessId>, PluginError> {
        let mut tombstoned = Vec::new();
        for process_id in process_ids {
            match self.get_process(process_id).await {
                Err(PluginError::ProcessNoLongerRetained { .. }) => {
                    tombstoned.push(process_id.clone());
                }
                Ok(_) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(tombstoned.into_iter().collect())
    }

    /// This is intentionally a full-scan aggregate. Implementations must read
    /// one consistent snapshot; non-terminal page pagination is not part of this API.
    async fn live_reference_summary(&self) -> Result<Vec<ProcessLiveReferenceView>, PluginError>;

    /// Count every retained process row that is still non-terminal.
    ///
    /// This is the low-level implementation seam for the facade's deployment
    /// drain read. Durable backends should override it with an indexed count
    /// over their authoritative status rows rather than hydrating records.
    async fn count_non_terminal_processes(&self) -> Result<usize, PluginError>;
}

/// Process admission: registration and the durable external backend reference.
#[async_trait::async_trait]
pub trait ProcessRegistrar: Send + Sync {
    /// Registers a process under an id the registrar mints; an id is never
    /// reused (ADR 0107). A start key makes the start idempotent while the
    /// process minted for it is retained, and after the process is pruned the
    /// same key starts a new process under a new id. A sender store restored
    /// behind an already-settled receiver floor is rejected and terminalized
    /// by the delivery driver as the typed `sequence_rewound` discard instead
    /// of being silently absorbed.
    async fn register_process(
        &self,
        registration: ProcessRegistration,
    ) -> Result<ProcessRecord, PluginError> {
        self.register_process_with_observers(registration, &[])
            .await
    }

    /// Atomically register the process and its explicit initial observer set.
    ///
    /// Registration is the owner of a process scope coming back, so it also
    /// lifts the scope-retirement fence a prune left behind (ADR 0049). A
    /// backend whose fence rows share its database clears the fence in the
    /// registration transaction itself, so a registration that fails leaves
    /// the id fenced and unregistered exactly as before.
    async fn register_process_with_observers(
        &self,
        registration: ProcessRegistration,
        observers: &[SessionId],
    ) -> Result<ProcessRecord, PluginError> {
        Ok(self
            .register_process_reporting_outcome(registration, observers)
            .await?
            .record)
    }

    /// Register as [`Self::register_process_with_observers`] does, and say
    /// whether this call created the row or found one already recorded.
    ///
    /// Registration is idempotent by fingerprint: an exact repeat returns the
    /// existing record rather than failing, so a caller cannot infer "I created
    /// this row" from a successful registration. A caller that compensates a
    /// later failure by writing a terminal onto the row it registered must know
    /// the difference, or a retry will terminalise the first attempt's row —
    /// and its running work — on the second attempt's behalf.
    async fn register_process_reporting_outcome(
        &self,
        registration: ProcessRegistration,
        observers: &[SessionId],
    ) -> Result<ProcessRegistrationReceipt, PluginError> {
        let prepared = self
            .prepare_process_registration(registration, observers)
            .await?;
        let anchor = prepared.trace().anchor().clone();
        self.commit_process_registration(prepared, anchor).await
    }

    /// Prepare the actual id and immutable composition without mutating SQL.
    async fn prepare_process_registration(
        &self,
        registration: ProcessRegistration,
        observers: &[SessionId],
    ) -> Result<PreparedProcessRegistration, PluginError>;
    /// Revalidate existing start and closure fences, then commit exactly the plan.
    async fn commit_process_registration(
        &self,
        prepared: PreparedProcessRegistration,
        anchor: lash_trace::TraceAnchor,
    ) -> Result<ProcessRegistrationReceipt, PluginError>;

    /// Attach a durable backend reference to a registered process.
    ///
    /// Implementations must reject unknown process ids. The first assignment
    /// stores the reference. Repeating the exact same assignment is an
    /// idempotent no-op that returns the existing record unchanged. Assigning a
    /// different reference after one has been stored is a registry model error.
    async fn set_external_ref(
        &self,
        process_id: &ProcessId,
        external_ref: ProcessExternalRef,
    ) -> Result<ProcessRecord, PluginError>;
}

/// Observer edges, subscription targeting, and session-scoped routing cleanup.
///
/// Requires [`ProcessQuery`]: observer semantics are defined against the
/// identity and liveness facts of the observed rows, and the provided methods
/// resolve retirement through point reads.
#[async_trait::async_trait]
pub trait ProcessObserverRegistry: ProcessQuery {
    async fn add_observer(
        &self,
        session_id: &SessionId,
        process_id: &ProcessId,
        by: ProcessObserverBy,
    ) -> Result<(), PluginError>;

    async fn remove_observer(
        &self,
        session_id: &SessionId,
        process_id: &ProcessId,
        by: ProcessObserverBy,
    ) -> Result<(), PluginError>;

    async fn transfer_observers(
        &self,
        from_session_id: &SessionId,
        to_session_id: &SessionId,
        process_ids: &[ProcessId],
        by: ProcessObserverBy,
    ) -> Result<(), PluginError>;

    /// List this session's observed processes matching every supplied filter.
    async fn list_observed_by(
        &self,
        session_id: &SessionId,
        filter: &ProcessListFilter,
    ) -> Result<Vec<ProcessRecord>, PluginError>;

    /// List the observed rows that are still live for the session.
    ///
    /// "Live" is the un-retired partition: exactly the rows the recovery
    /// non-terminal page keeps (`status IN ('running', 'waiting')`).
    async fn list_live_observed_by(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<ProcessRecord>, PluginError> {
        Ok(self
            .list_observed_by(
                session_id,
                &crate::ProcessListFilter {
                    status: crate::ProcessStatusFilter::Any,
                    ..Default::default()
                },
            )
            .await?
            .into_iter()
            .filter(|record| !record.status().is_retired())
            .collect())
    }

    async fn is_observer(
        &self,
        session_id: &SessionId,
        process_id: &ProcessId,
    ) -> Result<bool, PluginError> {
        if self.get_process(process_id).await?.is_none() {
            return Ok(false);
        }
        Ok(self
            .list_observed_by(
                session_id,
                &crate::ProcessListFilter {
                    status: crate::ProcessStatusFilter::Any,
                    ..Default::default()
                },
            )
            .await?
            .into_iter()
            .any(|record| record.id == process_id))
    }

    async fn observers_for_process(
        &self,
        process_id: &ProcessId,
    ) -> Result<Vec<SessionId>, PluginError>;

    /// The session `process_id`'s wake deliveries target: the one its start
    /// recorded, or the latest [`Self::retarget_subscription`] set. `None`
    /// when it wakes nothing. A process whose row is gone is refused as
    /// [`ProcessQuery::get_process`] refuses it.
    async fn wake_target(&self, process_id: &ProcessId) -> Result<Option<SessionId>, PluginError>;

    /// Append a subscription-retarget audit event, update the indexed target,
    /// and discard pending deliveries to the old target atomically.
    async fn retarget_subscription(
        &self,
        process_id: &ProcessId,
        target: Option<&str>,
    ) -> Result<(), PluginError>;

    /// Remove observer edges and wake routing owned by a deleted session.
    ///
    /// This bulk session-lifecycle cleanup deliberately does not append
    /// per-process observer or retarget audit events.
    async fn delete_session_process_state(
        &self,
        session_id: &SessionId,
    ) -> Result<ProcessSessionDeleteReport, PluginError>;
}

/// The per-process append-only event log.
///
/// Host-owned and authority-fenced appends plus sequence-cursor reads.
/// Requires [`ProcessQuery`]: reads resolve a process's retention through
/// point reads.
#[async_trait::async_trait]
pub trait ProcessEventLog: ProcessQuery {
    /// This unfenced path is reserved for host signal/cancel coordination.
    /// Process engines append no event themselves: an engine's events are
    /// its `Emit` actions, appended by the process activation.
    /// Signal events must be constructed by [`super::events::ProcessSignal::append_request`].
    /// Raw `signal.*` requests return [`PluginError::ReservedProcessEvent`],
    /// even if they carry a replay key. This rule also applies to batches
    /// and lifecycle preludes.
    async fn append_event(
        &self,
        process_id: &ProcessId,
        request: ProcessEventAppendRequest,
    ) -> Result<ProcessEventAppendReceipt, PluginError>;

    /// Append one execution-owned event: a batch of one
    /// ([`Self::append_events`]).
    async fn append_event_with_authority(
        &self,
        process_id: &ProcessId,
        request: ProcessEventAppendRequest,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessEventAppendReceipt, PluginError> {
        self.append_events(process_id, vec![request], authority)
            .await?
            .pop()
            .ok_or_else(|| {
                PluginError::Session(format!(
                    "process `{process_id}` answered a one-event append with no receipt"
                ))
            })
    }

    /// Append `requests`, in order, as one atomic batch under `authority`
    /// (FIG-3571).
    ///
    /// Implementations validate `authority` once and run the whole batch in one
    /// transaction: sequences are allocated in request order, each request
    /// goes through the same append rules a single append does (the ADR 0046
    /// fold, replay-key coalescing, the conflicting-payload refusal), the
    /// process record is rewritten once and the change clock advances once.
    /// A refusal of any request commits none of them. The receipts answer the
    /// requests in order. An empty batch writes nothing and answers no
    /// receipts.
    async fn append_events(
        &self,
        process_id: &ProcessId,
        requests: Vec<ProcessEventAppendRequest>,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<Vec<ProcessEventAppendReceipt>, PluginError>;

    /// Read at most `limit` events of one process, strictly after
    /// `after_sequence`.
    ///
    /// A pruned process answers `Pruned`, never an empty page. Implementations
    /// fetch at most one extra row to determine whether another page exists.
    async fn event_page_after(
        &self,
        process_id: &ProcessId,
        after_sequence: u64,
        limit: NonZeroUsize,
        mode: ProcessEventQueryMode,
    ) -> Result<ProcessEventReadOutcome<ProcessEventPage>, PluginError>;

    /// Read the first page of one process's events.
    async fn event_page(
        &self,
        process_id: &ProcessId,
        limit: NonZeroUsize,
        mode: ProcessEventQueryMode,
    ) -> Result<ProcessEventReadOutcome<ProcessEventPage>, PluginError> {
        self.event_page_after(process_id, 0, limit, mode).await
    }

    /// This is the signal-ordinal query: the Nth occurrence of a signal event
    /// resolves the Nth durable wait key. The default scans the event log;
    /// store backends override it with a COUNT so per-signal cost stays flat
    /// instead of growing with a long-lived process's history.
    async fn count_events_through(
        &self,
        process_id: &ProcessId,
        event_type: &str,
        up_to_sequence: u64,
    ) -> Result<u64, PluginError>;

    /// The most recent `limit` events, in ascending sequence order.
    ///
    /// Observation snapshots use this to show a bounded activity tail without fetching a
    /// process's entire history on every poll.
    /// The default scans the event log; store backends override it with ORDER BY ...
    async fn recent_events(
        &self,
        process_id: &ProcessId,
        limit: usize,
    ) -> Result<Vec<ProcessEvent>, PluginError>;
}

/// Durable execution lifecycle transitions.
///
/// The started fact, wait markers, the abandon request, authority-bound
/// terminal completion, and the ended-scope rows that fence a late start.
#[async_trait::async_trait]
pub trait ProcessLifecycle: Send + Sync {
    /// Complete a process under an explicit, auditable completion authority.
    ///
    /// This path is reserved for writers whose single-writer discipline lives
    /// outside the process engine: a workflow-key-coalesced substrate
    /// completing a row it ran. The [`ProcessCompletionAuthority`] names the
    /// discipline; the implementation MUST call
    /// [`authority.validate`](ProcessCompletionAuthority::validate) against the
    /// row inside this operation, so a refused authority is rejected with a
    /// typed error before any terminal event is appended, and MUST record the
    /// authority on the terminal event as audit evidence (via
    /// [`terminal_append_request`](super::events::terminal_append_request)).
    ///
    async fn complete_process(
        &self,
        process_id: &ProcessId,
        await_output: ProcessAwaitOutput,
        authority: ProcessCompletionAuthority,
    ) -> Result<ProcessCompletionOutcome, PluginError> {
        self.complete_process_with_prelude(process_id, await_output, Vec::new(), authority)
            .await
    }

    /// [`Self::complete_process`] with the run's terminal batch (FIG-3571):
    /// `prelude` (the run's still-pending effect-summary occurrences, then its
    /// `process.effect_omissions` record) is appended ahead of the terminal
    /// event in the same transaction, under the same append rules as
    /// [`ProcessEventLog::append_events`], and the record is rewritten once.
    /// A row that is already terminal answers its stored outcome and appends
    /// nothing, so a replayed completion never writes its prelude twice.
    async fn complete_process_with_prelude(
        &self,
        process_id: &ProcessId,
        await_output: ProcessAwaitOutput,
        prelude: Vec<ProcessEventAppendRequest>,
        authority: ProcessCompletionAuthority,
    ) -> Result<ProcessCompletionOutcome, PluginError>;

    /// Load the ledger row for one parent scope: the single durable
    /// scope-close fact. A process's terminal append writes it for the
    /// process's scope; a turn's end and a session's close begin write it in
    /// their own fenced commit (`ProcessWrite::ScopeClosed`). It carries no
    /// action list.
    ///
    /// Registration reads this to fence a late start: a start whose starter
    /// or lifetime scope has closed is refused rather than left unvisited.
    async fn get_parent_end_plan(
        &self,
        parent: &crate::ScopeId,
    ) -> Result<Option<ParentEndPlan>, PluginError>;

    /// Record the durable "execution started" fact (ADR 0110).
    ///
    /// The first attempt stores `started`. An identical replay is idempotent.
    /// A successor execution the engine resumes from its journal replaces the
    /// retained fact with the next consecutive attempt; any other attempt is
    /// refused. Whether a start may run at all is the engine's decision, made
    /// before this write: lash never re-runs started work from scratch.
    async fn record_first_started_with_authority(
        &self,
        process_id: &ProcessId,
        started: ProcessStarted,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessStartOutcome, PluginError>;

    /// Request cancellation of one process.
    ///
    /// The registry stamps the first accepted request with its injected clock.
    /// Same origin and requester is a no-op on a nonterminal row; a different
    /// request is a typed conflict. Optional attribution never replaces the
    /// cancellation's intrinsic replay key. The returned record contains the
    /// first accepted fact, including its original timestamp.
    async fn request_process_cancel(
        &self,
        process_id: &ProcessId,
        origin: crate::CancelOrigin,
        requester: String,
        attribution: Option<crate::RuntimeReplayAttribution>,
    ) -> Result<ProcessRecord, PluginError>;

    /// Request cancellation and report whether this call recorded it.
    ///
    /// The no-op arm of [`Self::request_process_cancel`] is invisible in its
    /// return value: the same record comes back whether this call wrote the
    /// request or found it already recorded. Callers that report replay to a
    /// host need that distinction (FIG-3070), so a store that coalesces repeat
    /// cancellations overrides this and answers with
    /// [`crate::StoreRealization::Coalesced`] on its no-op arm.
    ///
    /// The default runs the plain cancel and reports `Realized`. It exists so
    /// an in-memory double that never coalesces does not owe an
    /// implementation; every store that folds a repeat request onto the
    /// recorded one must override it.
    async fn request_process_cancel_reporting_realization(
        &self,
        process_id: &ProcessId,
        origin: crate::CancelOrigin,
        requester: String,
        attribution: Option<crate::RuntimeReplayAttribution>,
    ) -> Result<(ProcessRecord, crate::StoreRealization), PluginError> {
        let record = self
            .request_process_cancel(process_id, origin, requester, attribution)
            .await?;
        Ok((record, crate::StoreRealization::Realized))
    }

    /// Enter `wait` (`process.waiting`) as a run boundary (FIG-3571): the
    /// run's pending `prelude` is appended ahead of the transition in the same
    /// transaction, under the same append rules as
    /// [`ProcessEventLog::append_events`], and the record is rewritten once.
    /// An unchanged wait still commits the prelude.
    async fn set_process_wait_with_authority(
        &self,
        process_id: &ProcessId,
        wait: WaitState,
        prelude: Vec<ProcessEventAppendRequest>,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessRecord, PluginError>;

    /// Leave the current wait (`process.resumed`) as a run boundary, with the
    /// same `prelude` contract as [`Self::set_process_wait_with_authority`].
    async fn clear_process_wait_with_authority(
        &self,
        process_id: &ProcessId,
        prelude: Vec<ProcessEventAppendRequest>,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessRecord, PluginError>;
}

/// Durable tool-intent submission admission and settlement.
///
/// The submission ledger is the host ingress's first-outcome replay fence.
/// Each row belongs to the session that owns its identity. The
/// retained-evidence lever
/// ([`DeploymentStore::reclaim_retained_evidence`](crate::DeploymentStore::reclaim_retained_evidence))
/// reclaims it only once that session is durably deleted and the row was
/// admitted before the host's bound, and fences the owner in the same
/// transaction (FIG-1509, ADR 0067).
///
/// Every method is an **integrator class 3: store implementor** seam.
#[async_trait::async_trait]
pub trait ProcessToolIntents: Send + Sync {
    /// Atomically bind a host-submitted intent identity to its first payload.
    ///
    /// This is an **integrator class 3: store implementor** seam. The returned
    /// existing row must be the authoritative first writer across processes
    /// and facade handles. The store stamps the row's admission time, which
    /// the retained-evidence bound is compared against. An owner the lever
    /// fenced claims no new row: an identity without one answers
    /// [`ToolIntentSubmissionAdmission::Reclaimed`](crate::ToolIntentSubmissionAdmission::Reclaimed),
    /// and the fence and the claim serialize, so a reclaimed identity is
    /// never admitted again.
    async fn admit_tool_intent_submission(
        &self,
        submission: crate::ToolIntentSubmissionRecord,
    ) -> Result<crate::ToolIntentSubmissionAdmission, PluginError>;

    /// Persist the first realized outcome for an admitted intent identity.
    ///
    /// This is an **integrator class 3: store implementor** seam. Repetition is
    /// idempotent and may not replace an already recorded outcome.
    async fn complete_tool_intent_submission(
        &self,
        replay_key: &str,
        outcome: crate::ToolIntentExecutionOutcome,
    ) -> Result<crate::store::StoreTransition<crate::ToolIntentSubmissionRecord>, PluginError>;
}

/// Physical reclamation of terminal processes and their tombstones.
#[async_trait::async_trait]
pub trait ProcessRetention: Send + Sync {
    /// Delete payload-free tombstones older than `cutoff_epoch_ms` without
    /// outrunning a trusted projection or orphaning outstanding trigger
    /// deliveries. `NoProjector` permits free compaction; `UpTo(cursor)` retains
    /// deletion entries beyond that cursor. When `trigger_store` is configured,
    /// the registry first obtains its complete outstanding-delivery process-id
    /// survey and structurally excludes matching tombstones. A survey failure
    /// aborts compaction. `None` is reserved for runtimes with no trigger store.
    async fn compact_process_tombstones(
        &self,
        cutoff_epoch_ms: u64,
        watermark: ProjectionWatermark,
        trigger_store: Option<&dyn crate::TriggerStore>,
    ) -> Result<usize, PluginError>;

    /// Release the payloads of `process_id`'s events at or below `through`,
    /// while the process stays retained, running or not (FIG-3482).
    ///
    /// The host chooses the prefix; the store clamps it to the process's last
    /// event and never lowers a horizon an earlier release raised, so the
    /// call is idempotent and repeated cleanup reports nothing new. A released
    /// event keeps its row: sequence, type, replay key and a digest of its
    /// payload. Sequence allocation and signal ordinals therefore do not move,
    /// and a writer that re-presents a released event's replay key — a
    /// segment or Run attempt replaying its journal, a host retrying a signal
    /// — coalesces on the digest exactly as it would on the payload, or is
    /// refused as a conflict. Releasing therefore needs no proof that every
    /// such writer has finished, which storage cannot observe.
    ///
    /// Reads strictly after the horizon are unchanged. A page read starting
    /// below it answers [`ProcessEventHistoryRetention::Released`](super::events::ProcessEventHistoryRetention::Released)
    /// rather than skipping the released events, and the recent-event tail
    /// never returns one. The host must finish projecting that prefix and
    /// accept the typed expiry of any event reader that still needs it.
    /// Execution state is retained separately: waits, outcomes and wake
    /// deliveries are held by the process row, the engine and delivery rows.
    /// A pruned process refuses with
    /// [`PluginError::ProcessNoLongerRetained`] and an unknown one with
    /// [`PluginError::ProcessUnknown`].
    async fn release_process_events(
        &self,
        process_id: &ProcessId,
        through: u64,
    ) -> Result<super::events::ProcessEventRelease, PluginError>;

    /// Physically delete terminal process rows whose `updated_at_ms` is older
    /// than `cutoff_epoch_ms`, match `filter` when one is supplied, and have a
    /// process change sequence allowed by the caller's explicit projection
    /// `watermark`, together with their events, observer edges, and lease rows.
    /// Trigger-delivery rows are never deleted by this operation, including in
    /// co-located backends. Callers use [`reconcile_pruned_trigger_deliveries`]
    /// afterward so every backend has one observable reclamation path.
    /// Session-scoped trigger-mutation receipts follow their owner's ADR 0049
    /// deletion frontier during reconciliation; host and platform receipts
    /// remain owned by the trigger store's explicit cutoff lever. A process owns
    /// no session store: the attachments it held are released by its
    /// artifact cleanup (ADR 0124), never by the prune.
    /// Backends must fail toward retaining the terminal process if the prune
    /// cannot complete.
    /// Host-scheduled retention: hosts that project results/events into their
    /// own store call this to keep the registry bounded. Non-terminal rows are
    /// never touched. A late await receives the typed
    /// `ProcessNoLongerRetained` information outcome from the payload-free
    /// tombstone. The API accepts a raw cutoff and the runtime exposes no finite
    /// maximum waiter lifetime, so callers cannot validate this against a
    /// library-owned bound; retaining terminal rows beyond every still-replayable
    /// waiter is currently an explicit host operational responsibility.
    /// Occurrence replay eligibility ends when the committed fan-out becomes
    /// empty during reconciliation. Re-emitting an occurrence id after that
    /// point is a new ingest; callers must retain an occurrence outside this
    /// boundary when its replay horizon is longer than process retention.
    ///
    /// ```no_run
    /// use std::time::{Duration, SystemTime, UNIX_EPOCH};
    /// use lash_core::{PluginError, ProcessRegistry, ProjectionWatermark};
    ///
    /// async fn prune_week_old(registry: &dyn ProcessRegistry) -> Result<(), PluginError> {
    ///     let now_ms = SystemTime::now()
    ///         .duration_since(UNIX_EPOCH)
    ///         .expect("clock after epoch")
    ///         .as_millis() as u64;
    ///     // Host policy must keep this beyond every still-replayable waiter;
    ///     // lash has no finite waiter-lifetime bound to validate here.
    ///     let cutoff = now_ms - Duration::from_secs(7 * 24 * 60 * 60).as_millis() as u64;
    ///     let report = registry
    ///         .prune_terminal_processes(cutoff, None, ProjectionWatermark::NoProjector)
    ///         .await?;
    ///     eprintln!(
    ///         "pruned {} processes, {} events, {} trigger deliveries",
    ///         report.pruned_processes,
    ///         report.pruned_events,
    ///         report.pruned_trigger_deliveries
    ///     );
    ///     Ok(())
    /// }
    /// ```
    async fn prune_terminal_processes(
        &self,
        cutoff_epoch_ms: u64,
        filter: Option<ProcessListFilter>,
        watermark: ProjectionWatermark,
    ) -> Result<ProcessPruneReport, PluginError>;

    /// The process ids [`prune_terminal_processes`](Self::prune_terminal_processes)
    /// would delete right now for the same arguments, in ascending id order,
    /// without deleting anything. The survey applies the prune's complete
    /// eligibility predicate — retired status, `updated_at_ms` before the
    /// cutoff, the projection `watermark`, no pending or enqueuing wake
    /// delivery, no parent-end plan, no consumer hold, and `filter` — so a
    /// caller that must reclaim rows the registry does not own (the process's
    /// durable effect journal and its await-event promises) fences exactly the
    /// rows the prune reclaims and never a process the registry keeps. The
    /// prune re-evaluates the predicate under its own transaction; a process
    /// that becomes ineligible between survey and prune is retained by the
    /// prune and its already-fenced journal stays reclaimed, which is the
    /// conservative direction for a retired row.
    async fn prunable_terminal_processes(
        &self,
        cutoff_epoch_ms: u64,
        filter: Option<ProcessListFilter>,
        watermark: ProjectionWatermark,
    ) -> Result<Vec<ProcessId>, PluginError>;

    /// Releases the consumer hold `key` holds on `process_id` (ADR 0116
    /// §3.6). A held row is never pruned; releasing a hold the row does not
    /// carry, or one already released, changes nothing. The close of a hold's
    /// owning scope releases it too, in the close's own transaction.
    async fn release_consumer_hold(
        &self,
        process_id: &ProcessId,
        key: &str,
    ) -> Result<(), PluginError>;

    /// Marks the consumer hold `key`, owned by `owner`, abandoned and returns
    /// the processes it holds whose call owes them a cancel (ADR 0116 §3.4):
    /// what the opener that cancelled the call must cancel. One transaction
    /// marks and reads, and a registration under a marked key is refused in
    /// its own, so a launch racing the cancel is either returned here or
    /// never registers. Marking again keeps the first mark; the read is empty
    /// once the hold is released. `owner`'s close forgets the mark.
    async fn abandon_consumer_hold(
        &self,
        key: &str,
        owner: &crate::ScopeId,
    ) -> Result<Vec<ProcessId>, PluginError>;
}

/// Rebinding a registry backend to the runtime's clock.
///
/// This is a whole-registry construction concern, so it deliberately speaks in
/// terms of the composed [`ProcessRegistry`] handle.
pub trait ProcessClockRebind: Send + Sync {
    /// First-party persistent registries override this so facade construction
    /// cannot mint wake expiry with a different clock than the driver uses.
    /// Host-owned registries that already own their clock may keep the default.
    fn with_runtime_clock(
        &self,
        _clock: std::sync::Arc<dyn crate::Clock>,
    ) -> Option<std::sync::Arc<dyn ProcessRegistry>> {
        None
    }
}

/// Structural proof that the concerns are separable: a decorator composing
/// only the observer concern (plus its declared [`ProcessQuery`] read
/// dependency) carries no other concern's obligations. Forcing such a wrapper
/// into an unrelated concern is a compile error:
///
/// ```compile_fail,E0277
/// use lash_core::{
///     PluginError, ProcessChange, ProcessChangeCursor, ProcessLiveReferenceView,
///     ProcessListFilter, ProcessObserverBy, ProcessRecord, ProcessSessionDeleteReport,
///     ProcessRegistryCursor, NonTerminalProcessPage, MAX_NON_TERMINAL_PROCESS_PAGE_SIZE, SessionId,
/// };
/// use lash_core::{ProcessLifecycle, ProcessObserverRegistry, ProcessQuery};
/// use std::num::NonZeroUsize;
///
/// struct ObserverOnly;
///
/// #[async_trait::async_trait]
/// impl ProcessQuery for ObserverOnly {
///     async fn get_process(&self, _: &str) -> Result<Option<ProcessRecord>, PluginError> {
///         unimplemented!()
///     }
///     async fn list_processes(
///         &self,
///         _: &ProcessListFilter,
///     ) -> Result<Vec<ProcessRecord>, PluginError> {
///         unimplemented!()
///     }
///     async fn processes_changed_since(
///         &self,
///         _: ProcessChangeCursor,
///         _: usize,
///     ) -> Result<(Vec<ProcessChange>, ProcessChangeCursor), PluginError> {
///         unimplemented!()
///     }
///     async fn list_non_terminal_processes_page(
///         &self,
///         _: NonZeroUsize,
///         _: Option<ProcessRegistryCursor>,
///     ) -> Result<NonTerminalProcessPage, PluginError> {
///         unimplemented!()
///     }
///     async fn live_reference_summary(
///         &self,
///     ) -> Result<Vec<ProcessLiveReferenceView>, PluginError> {
///         unimplemented!()
///     }
///     async fn count_non_terminal_processes(&self) -> Result<usize, PluginError> {
///         unimplemented!()
///     }
/// }
///
/// #[async_trait::async_trait]
/// impl ProcessObserverRegistry for ObserverOnly {
///     async fn add_observer(
///         &self,
///         _: &str,
///         _: &str,
///         _: ProcessObserverBy,
///     ) -> Result<(), PluginError> {
///         unimplemented!()
///     }
///     async fn remove_observer(
///         &self,
///         _: &str,
///         _: &str,
///         _: ProcessObserverBy,
///     ) -> Result<(), PluginError> {
///         unimplemented!()
///     }
///     async fn transfer_observers(
///         &self,
///         _: &str,
///         _: &str,
///         _: &[String],
///         _: ProcessObserverBy,
///     ) -> Result<(), PluginError> {
///         unimplemented!()
///     }
///     async fn list_observed_by(&self, _: &str, filter: &ProcessListFilter) -> Result<Vec<ProcessRecord>, PluginError> {
///         unimplemented!()
///     }
///     async fn observers_for_process(&self, _: &str) -> Result<Vec<SessionId>, PluginError> {
///         unimplemented!()
///     }
///     async fn wake_target(&self, _: &str) -> Result<Option<SessionId>, PluginError> {
///         unimplemented!()
///     }
///     async fn retarget_subscription(&self, _: &str, _: Option<&str>) -> Result<(), PluginError> {
///         unimplemented!()
///     }
///     async fn delete_session_process_state(
///         &self,
///         _: &str,
///     ) -> Result<ProcessSessionDeleteReport, PluginError> {
///         unimplemented!()
///     }
/// }
///
/// fn requires_lifecycle<T: ProcessLifecycle>(_: &T) {}
///
/// // ERROR: the trait bound `ObserverOnly: ProcessLifecycle` is not satisfied.
/// // An observer-only wrapper is not draggable into lifecycle writes.
/// fn deny(wrapper: &ObserverOnly) {
///     requires_lifecycle(wrapper);
/// }
/// ```
///
/// The identical wrapper compiles and answers observer reads when the
/// offending bound is absent; `concern_isolation_tests` below exercises that
/// positive twin against the in-memory registry double.
#[allow(dead_code)]
fn concern_isolation_witness_docs() {}
