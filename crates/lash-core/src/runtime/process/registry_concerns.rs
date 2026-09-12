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
use std::sync::{Arc, Weak};

use crate::{EffectHost, ExecutionScope};

use super::ProcessCompletionOutcome;
use super::events::{
    ProcessAwaitOutput, ProcessCompletionAuthority, ProcessEvent, ProcessEventAppendReceipt,
    ProcessEventAppendRequest,
};
use super::model::{
    AbandonRequest, ProcessChange, ProcessChangeCursor, ProcessExecutionWriteAuthority,
    ProcessExternalRef, ProcessId, ProcessLease, ProcessLeaseClaimOutcome, ProcessLeaseCompletion,
    ProcessListFilter, ProcessObserverBy, ProcessRecord, ProcessRef, ProcessRegistration,
    ProcessSessionDeleteReport, ProcessStartOutcome, ProcessStarted, SessionId, WaitState,
};
use super::references::ProcessLiveReferenceView;
use super::registry::{
    ProcessParentEndPlan, ProcessPruneReport, ProcessRegistry, ProcessWorklistCursor,
    ProcessWorklistPage, ProjectionWatermark, WakeDelivery, WakeDeliveryClaimOutcome,
    WakeDeliveryConfig, WakeDeliveryReport, WakeDeliveryState, WakeDiscardReason,
};

/// Point reads and scans over registered processes.
///
/// Identity resolution, record and listing reads, the trusted change feed, the
/// recovery worklist, and registry-wide aggregates. Registry methods are point
/// reads and writes only; process waits live on the work-driver seam (ADR 0016).
#[async_trait::async_trait]
pub trait ProcessQuery: Send + Sync {
    /// Resolve a host-facing reusable process name to the currently retained
    /// structural identity. Internal durable references must keep the returned
    /// pair rather than resolving the name again.
    async fn resolve_process_ref(&self, process_id: &ProcessId) -> Result<ProcessRef, PluginError> {
        match self.get_process(process_id).await? {
            Some(record) => Ok(ProcessRef::from_record(&record)),
            None => Err(super::registry_transitions::unknown_process(process_id)),
        }
    }

    /// Read one exact process incarnation and refuse a successor with the same
    /// host-facing name.
    async fn get_process_ref(
        &self,
        process_ref: &ProcessRef,
    ) -> Result<Option<ProcessRecord>, PluginError> {
        match self.get_process(&process_ref.process_id).await? {
            Some(record) if record.incarnation == process_ref.incarnation => Ok(Some(record)),
            Some(record) => Err(super::registry_transitions::process_incarnation_superseded(
                process_ref,
                record.incarnation,
            )),
            None => Ok(None),
        }
    }

    async fn get_process(
        &self,
        process_id: &ProcessId,
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

    /// Return one bounded page of non-terminal records in stable `process_id`
    /// order.
    ///
    /// This is the recovery sweep's worklist: every process that was started
    /// but has not reached a terminal event is a candidate for re-execution by
    /// a [`DurableProcessWorker`](crate::DurableProcessWorker) after a crash.
    /// Terminal processes are excluded — they are already done and idempotent
    /// by `process_id`, so re-running them would be wasted work.
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
    /// switching backends. `limit` is non-zero by construction.
    async fn list_non_terminal_page(
        &self,
        limit: NonZeroUsize,
        continuation: Option<ProcessWorklistCursor>,
    ) -> Result<ProcessWorklistPage, PluginError>;

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

    /// Count non-terminal process rows by their captured definition and
    /// execution-environment references.
    ///
    /// This is intentionally a full-scan aggregate. Implementations must read
    /// one consistent snapshot; worklist pagination is not part of this API.
    async fn live_reference_summary(&self) -> Result<Vec<ProcessLiveReferenceView>, PluginError>;

    /// Count every retained process row that is still non-terminal.
    ///
    /// This is the low-level implementation seam for the facade's deployment
    /// drain read. Durable backends should override it with an indexed count
    /// over their authoritative status rows rather than hydrating records.
    #[doc(hidden)]
    async fn count_non_terminal_processes(&self) -> Result<usize, PluginError>;
}

/// Process admission: registration and the durable external backend reference.
#[async_trait::async_trait]
pub trait ProcessRegistrar: Send + Sync {
    /// Process ids may be registered again after their terminal incarnation is
    /// pruned. A durable sender floor retained per `(target_session_id,
    /// process_id)` makes a later incarnation continue above every sequence
    /// allocated to that target, so reuse is safe without a clock precondition.
    /// A sender store restored behind an already-settled receiver floor is
    /// rejected and terminalized by the delivery driver as the typed
    /// `sequence_rewound` discard instead of being silently absorbed.
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
    /// registration transaction itself; every backend additionally lifts the
    /// fence of each host bound through [`Self::bind_effect_host`] before
    /// registration reports success, so a registration that fails leaves the
    /// id fenced and unregistered exactly as before.
    async fn register_process_with_observers(
        &self,
        registration: ProcessRegistration,
        observers: &[SessionId],
    ) -> Result<ProcessRecord, PluginError>;

    /// Bind the effect host whose scope-retirement fence this registry lifts
    /// when a process id is registered again (ADR 0049).
    ///
    /// The facade binds the effect host it was built with; a host that wires
    /// a registry and an effect host together by hand binds them the same
    /// way. Binding is idempotent and the registry holds the host weakly, so
    /// a host that also owns the registry does not leak.
    ///
    /// Binding runs in both directions. The registry hands the host a
    /// [`ProcessRegistryBinding`] through [`EffectHost::bind_process_registry`]:
    /// the database that holds the process-scope fence when the registry
    /// keeps it (the SQLite registry file, whose registration transaction
    /// inserts the process row and deletes the fence row as one commit; the
    /// PostgreSQL registry does the same inside its one database) and a probe
    /// answering whether a process id is registered, which a host whose own
    /// fence is a cache of the registry's (the Restate durable-wait index)
    /// reads through to. A fence the host keeps where the registry cannot
    /// reach it is lifted through [`EffectHost::reinstate_effect_scope`]
    /// after the registration write.
    fn bind_effect_host(&self, effect_host: &Arc<dyn EffectHost>);

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
/// resolve incarnations and retirement through point reads.
#[async_trait::async_trait]
pub trait ProcessObserverRegistry: ProcessQuery {
    async fn add_observer(
        &self,
        session_id: &SessionId,
        process_id: &ProcessId,
        by: ProcessObserverBy,
    ) -> Result<(), PluginError>;

    /// Attach an observer edge to one exact process incarnation.
    async fn add_observer_ref(
        &self,
        session_id: &SessionId,
        process_ref: &ProcessRef,
        by: ProcessObserverBy,
    ) -> Result<(), PluginError> {
        self.get_process_ref(process_ref).await?;
        self.add_observer(session_id, &process_ref.process_id, by)
            .await
    }

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
    /// Stores bound status and retired-row retention before decoding records.
    async fn list_observed_by(
        &self,
        session_id: &SessionId,
        filter: &ProcessListFilter,
    ) -> Result<Vec<ProcessRecord>, PluginError>;

    /// List the observed rows that are still live for the session.
    ///
    /// "Live" is the un-retired partition — exactly the rows the recovery
    /// worklist keeps (`status IN ('running', 'waiting')`), not merely the
    /// non-terminal ones. A [`ProcessStatus::CallerDeparted`](super::model::ProcessStatus::CallerDeparted) row is
    /// non-terminal yet retired: lash will never observe an outcome for it, so
    /// presenting it as live would show a caller a launch still in flight that
    /// nothing can ever advance.
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
            .filter(|record| !record.status.is_retired())
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
/// Requires [`ProcessQuery`]: the incarnation-pinned (`*_ref`) methods resolve
/// one exact process incarnation before touching its log.
#[async_trait::async_trait]
pub trait ProcessEventLog: ProcessQuery {
    /// Append a host-owned event that is not emitted by the process execution.
    ///
    /// This unfenced path is reserved for host signal/cancel coordination.
    /// Process engines receive only [`ProcessEngineProcessContext`](super::engine::ProcessEngineProcessContext);
    /// execution-owned events must use its authority-bound emitter.
    async fn append_event(
        &self,
        process_id: &ProcessId,
        request: ProcessEventAppendRequest,
    ) -> Result<ProcessEventAppendReceipt, PluginError>;

    /// Append a host-owned event to one exact process incarnation.
    async fn append_event_ref(
        &self,
        process_ref: &ProcessRef,
        request: ProcessEventAppendRequest,
    ) -> Result<ProcessEventAppendReceipt, PluginError> {
        self.get_process_ref(process_ref).await?;
        self.append_event(&process_ref.process_id, request).await
    }

    /// Append an event emitted by the currently executing process attempt.
    ///
    /// Implementations validate `authority` and append in one atomic write.
    async fn append_event_with_authority(
        &self,
        process_id: &ProcessId,
        request: ProcessEventAppendRequest,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessEventAppendReceipt, PluginError>;

    async fn events_after(
        &self,
        process_id: &ProcessId,
        after_sequence: u64,
    ) -> Result<Vec<ProcessEvent>, PluginError>;

    /// Read an event cursor pinned to one process incarnation.
    async fn events_after_ref(
        &self,
        process_ref: &ProcessRef,
        after_sequence: u64,
    ) -> Result<Vec<ProcessEvent>, PluginError> {
        self.get_process_ref(process_ref).await?;
        self.events_after(&process_ref.process_id, after_sequence)
            .await
    }

    /// Count events of `event_type` with `sequence <= up_to_sequence`.
    ///
    /// This is the signal-ordinal query: the Nth occurrence of a signal event
    /// resolves the Nth durable wait key. The default scans the event log;
    /// store backends override it with a COUNT so per-signal cost stays flat
    /// instead of growing with a long-lived process's history.
    async fn count_events_through(
        &self,
        process_id: &ProcessId,
        event_type: &str,
        up_to_sequence: u64,
    ) -> Result<u64, PluginError> {
        Ok(self
            .events_after(process_id, 0)
            .await?
            .into_iter()
            .filter(|event| event.sequence <= up_to_sequence && event.event_type == event_type)
            .count() as u64)
    }

    /// Count matching events through a cursor pinned to one incarnation.
    async fn count_events_through_ref(
        &self,
        process_ref: &ProcessRef,
        event_type: &str,
        up_to_sequence: u64,
    ) -> Result<u64, PluginError> {
        Ok(self
            .events_after_ref(process_ref, 0)
            .await?
            .into_iter()
            .filter(|event| event.sequence <= up_to_sequence && event.event_type == event_type)
            .count() as u64)
    }

    /// The most recent `limit` events, in ascending sequence order.
    ///
    /// Observation snapshots use this to show a bounded activity tail without
    /// fetching a process's entire history on every poll. The default scans
    /// the event log; store backends override it with ORDER BY ... LIMIT.
    async fn recent_events(
        &self,
        process_id: &ProcessId,
        limit: usize,
    ) -> Result<Vec<ProcessEvent>, PluginError> {
        let mut events = self.events_after(process_id, 0).await?;
        if events.len() > limit {
            events.drain(..events.len() - limit);
        }
        Ok(events)
    }
}

/// Durable execution lifecycle transitions.
///
/// The started fact, wait markers, the abandon request and caller-departure
/// markers, terminal completion (leased and authority-bound), and the
/// parent-end teardown plans retained atomically with a terminal outcome.
#[async_trait::async_trait]
pub trait ProcessLifecycle: Send + Sync {
    /// Complete a process without a Lash process lease, under an explicit,
    /// auditable completion authority.
    ///
    /// This path is reserved for writers whose single-writer discipline lives
    /// *outside* the Lash lease: an external actor closing an externally-owned
    /// row, a workflow-key-coalesced substrate completing a row it ran, or the
    /// sweep reconciling an abandon request. The
    /// [`ProcessCompletionAuthority`] names which of these applies; the
    /// implementation MUST call
    /// [`authority.validate`](ProcessCompletionAuthority::validate) against the
    /// row's declared [`RecoveryContract`](super::model::RecoveryContract)
    /// inside this operation, so a mismatched authority is rejected with a typed
    /// error before any terminal event is appended, and MUST record the
    /// authority on the terminal event as audit evidence (via
    /// [`terminal_append_request`](super::events::terminal_append_request)).
    ///
    /// Lash-owned workers must instead use
    /// [`complete_process_with_lease`](Self::complete_process_with_lease), which
    /// fences the terminal append and lease release in one atomic operation.
    async fn complete_process(
        &self,
        process_id: &ProcessId,
        await_output: ProcessAwaitOutput,
        authority: ProcessCompletionAuthority,
    ) -> Result<ProcessCompletionOutcome, PluginError>;

    /// Complete without a Lash lease and atomically retain parent-end work.
    async fn complete_process_with_parent_end(
        &self,
        process_id: &ProcessId,
        await_output: ProcessAwaitOutput,
        authority: ProcessCompletionAuthority,
        actions: Vec<crate::ToolIntentParentEndAction>,
    ) -> Result<ProcessCompletionOutcome, PluginError> {
        if !actions.is_empty() {
            return Err(PluginError::Session(format!(
                "process registry cannot durably retain {} parent-end actions for `{process_id}`",
                actions.len()
            )));
        }
        self.complete_process(process_id, await_output, authority)
            .await
    }

    /// Atomically append the terminal output while the supplied process lease
    /// is still current, then release that lease in the same transaction.
    ///
    /// Implementations must validate owner incarnation, lease token, fencing
    /// token, and expiry against the persisted lease. A stale or expired writer
    /// is rejected without appending any terminal event or clearing a newer
    /// owner's lease. Replaying the same terminal event after a successful
    /// completion returns the existing terminal record.
    async fn complete_process_with_lease(
        &self,
        lease: &ProcessLease,
        await_output: ProcessAwaitOutput,
    ) -> Result<ProcessCompletionOutcome, PluginError>;

    /// Lease-fenced terminal completion with an atomically retained parent-end plan.
    async fn complete_process_with_lease_and_parent_end(
        &self,
        lease: &ProcessLease,
        await_output: ProcessAwaitOutput,
        actions: Vec<crate::ToolIntentParentEndAction>,
    ) -> Result<ProcessCompletionOutcome, PluginError> {
        if !actions.is_empty() {
            return Err(PluginError::Session(format!(
                "process registry cannot durably retain {} parent-end actions for `{}`",
                actions.len(),
                lease.process_id
            )));
        }
        self.complete_process_with_lease(lease, await_output).await
    }

    /// Return a bounded stable set of terminal parents whose teardown remains pending.
    async fn list_pending_parent_end_plans(
        &self,
        limit: NonZeroUsize,
    ) -> Result<Vec<ProcessParentEndPlan>, PluginError>;

    /// Load the durable post-terminal teardown plan for one process, if any.
    async fn get_pending_parent_end_plan(
        &self,
        process_id: &ProcessId,
    ) -> Result<Option<ProcessParentEndPlan>, PluginError>;

    /// Clear one plan after all replay-keyed commands settle. Repetition is idempotent.
    async fn complete_parent_end_plan(&self, process_id: &ProcessId) -> Result<(), PluginError>;

    /// Record the durable, lease-fenced "execution started" fact (ADR 0019).
    ///
    /// The first attempt stores `started`. An identical replay is idempotent.
    /// Rerunnable recovery replaces the retained fact with the next consecutive
    /// attempt; OwnerBound recovery rejects a distinct execution.
    async fn record_first_started_with_authority(
        &self,
        process_id: &ProcessId,
        started: ProcessStarted,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessStartOutcome, PluginError>;

    /// Set the durable, non-terminal Abandon Request marker (ADR 0019).
    ///
    /// First-writer-wins: a repeat with the same requester and reason is an
    /// idempotent no-op returning the existing record unchanged, preserving the
    /// original request timestamp. A different requester or reason is a conflict
    /// and cannot clobber the recorded authorization. Setting it on a terminal
    /// row is a model error — a terminal process has already recorded its outcome,
    /// so there is nothing to abandon.
    async fn request_process_abandon(
        &self,
        process_id: &ProcessId,
        request: AbandonRequest,
    ) -> Result<ProcessRecord, PluginError>;

    /// Record that the caller which registered an Externally-Owned row
    /// departed before any outcome could be written (FIG-1383).
    ///
    /// This is the honest closure of the audit-before-side-effect window: the
    /// row committed, the caller then vanished, and lash cannot observe
    /// whether the external work it was recording ever happened. Writing
    /// `Cancelled` or `Failed` here would assert an outcome lash never saw, so
    /// the row instead moves to the durable, non-terminal
    /// [`ProcessStatus::CallerDeparted`](super::model::ProcessStatus::CallerDeparted),
    /// which external reconciliation can
    /// find, awaits refuse instead of parking on, and retention may reclaim.
    ///
    /// Idempotent: a row already in that state is returned unchanged. Refused
    /// for rows that are not Externally-Owned and for rows that already
    /// recorded a terminal outcome.
    async fn record_caller_departure(
        &self,
        process_id: &ProcessId,
    ) -> Result<ProcessRecord, PluginError>;

    async fn set_process_wait_with_authority(
        &self,
        process_id: &ProcessId,
        wait: WaitState,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessRecord, PluginError>;

    async fn clear_process_wait_with_authority(
        &self,
        process_id: &ProcessId,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessRecord, PluginError>;
}

/// Durable tool-intent submission admission and settlement.
///
/// Every method is an **integrator class 3: store implementor** seam.
#[async_trait::async_trait]
pub trait ProcessToolIntents: Send + Sync {
    /// Atomically bind a runtime-owned intent identity to its first payload.
    ///
    /// This is an **integrator class 3: store implementor** seam. The returned
    /// existing row must be the authoritative first writer across processes
    /// and facade handles.
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
    ) -> Result<crate::ToolIntentSubmissionRecord, PluginError>;

    /// Load unsettled ingress parent-end actions for one owning scope.
    ///
    /// This is an **integrator class 3: store implementor** seam used by hosts
    /// to reconstruct teardown after a crash.
    async fn pending_tool_intent_parent_end(
        &self,
        session_id: &SessionId,
        execution_scope_id: &str,
    ) -> Result<Vec<crate::ToolIntentSubmissionRecord>, PluginError>;

    /// Mark one durable ingress parent-end action settled.
    ///
    /// This is an **integrator class 3: store implementor** seam. Repetition is
    /// idempotent so a crash after replay-keyed teardown can redrive safely.
    async fn complete_tool_intent_parent_end(&self, replay_key: &str) -> Result<(), PluginError>;
}

/// The wake-delivery outbox.
///
/// Retention policy plus the claim/settle/redrive protocol that turns pending
/// process wakes into exactly-once queued work at their target sessions.
#[async_trait::async_trait]
pub trait ProcessWakeOutbox: Send + Sync {
    fn wake_delivery_config(&self) -> WakeDeliveryConfig;

    /// Return due group heads and record one delivery attempt for each.
    ///
    /// Implementations must preserve sequence order inside a
    /// `(target_session_id, process_id)` group while selecting fairly across
    /// distinct groups by `next_attempt_at_ms`.
    async fn claim_pending_wake_deliveries(
        &self,
        limit: usize,
    ) -> Result<Vec<WakeDelivery>, PluginError>;

    async fn list_wake_deliveries(
        &self,
        state: Option<WakeDeliveryState>,
    ) -> Result<Vec<WakeDelivery>, PluginError>;

    async fn wake_delivery_report(&self) -> Result<WakeDeliveryReport, PluginError>;

    async fn mark_wake_enqueued(
        &self,
        delivery_id: &str,
        claim_token: &str,
    ) -> Result<WakeDeliveryClaimOutcome, PluginError>;

    async fn discard_wake_delivery(
        &self,
        delivery_id: &str,
        claim_token: &str,
        reason: WakeDiscardReason,
    ) -> Result<WakeDeliveryClaimOutcome, PluginError>;

    async fn redrive_wake_delivery(&self, delivery_id: &str) -> Result<(), PluginError>;

    /// Defer a retryable non-delivery until the supplied runtime-clock time.
    async fn defer_wake_delivery(
        &self,
        delivery_id: &str,
        claim_token: &str,
        next_attempt_at_ms: u64,
    ) -> Result<WakeDeliveryClaimOutcome, PluginError>;
}

/// The durable single-owner process lease protocol.
#[async_trait::async_trait]
pub trait ProcessLeases: Send + Sync {
    /// Claim the durable single-owner lease over a non-terminal process.
    ///
    /// An unexpired lease held by a *different* owner returns
    /// [`ProcessLeaseClaimOutcome::Busy`] carrying the observed holder;
    /// claiming a free or expired lease succeeds and bumps the
    /// `fencing_token`, and the same incarnation re-entering its own live
    /// lease extends it without changing token or fence. The returned
    /// [`ProcessLease`]'s `(owner, lease_token)` plus `fencing_token` are the
    /// contract a worker presents on every subsequent renew/complete — a stale
    /// writer is rejected.
    async fn claim_process_lease(
        &self,
        process_id: &ProcessId,
        owner: &crate::LeaseOwnerIdentity,
        lease_ttl_ms: u64,
    ) -> Result<ProcessLeaseClaimOutcome, PluginError>;

    /// Retry a process lease claim after observing `observed_holder`.
    ///
    /// An unexpired lease remains busy. Once its TTL expires, the caller may
    /// acquire it with a monotonically advanced fencing token.
    async fn reclaim_process_lease(
        &self,
        process_id: &ProcessId,
        owner: &crate::LeaseOwnerIdentity,
        observed_holder: &ProcessLease,
        lease_ttl_ms: u64,
    ) -> Result<ProcessLeaseClaimOutcome, PluginError>;

    /// Extend the expiry of a live lease the caller still owns.
    ///
    /// The lease must match the persisted `(owner, lease_token, fencing_token)`
    /// and be unexpired, else the renewal is rejected (the lease was superseded
    /// or expired). Workers renew across long-running effects so a healthy
    /// process is not swept out from under its live owner.
    async fn renew_process_lease(
        &self,
        lease: &ProcessLease,
        lease_ttl_ms: u64,
    ) -> Result<ProcessLease, PluginError>;

    /// Read the current lease row for a process without claiming it.
    ///
    /// Returns the persisted lease when one is held (owner and token present),
    /// or `None` when the row is unleased or released. The returned lease may be
    /// expired: expiry is a raw fact exposed read-side (ADR 0019) so hosts
    /// classify staleness themselves; this never mutates the lease. Unknown
    /// process ids return `None`.
    async fn get_process_lease(
        &self,
        process_id: &ProcessId,
    ) -> Result<Option<ProcessLease>, PluginError>;

    /// Read current lease rows for `process_ids` in input order.
    ///
    /// The result has exactly one entry per input id; unknown, unleased, and
    /// released processes produce `None`. Durable registries override this
    /// method with one backend query so observation polls do not serialize one
    /// read per process.
    async fn get_process_leases(
        &self,
        process_ids: &[ProcessId],
    ) -> Result<Vec<Option<ProcessLease>>, PluginError> {
        let mut leases = Vec::with_capacity(process_ids.len());
        for process_id in process_ids {
            leases.push(self.get_process_lease(process_id).await?);
        }
        Ok(leases)
    }

    /// Release a lease the caller owns, fenced by the completion's
    /// `(process_id, lease_token)`.
    ///
    /// Mirrors clearing a runtime turn lease: a stale completion (whose token no
    /// longer matches the live lease) is a no-op so it cannot release a lease a
    /// newer owner now holds. Idempotent — completing an already-released lease
    /// succeeds.
    async fn complete_process_lease(
        &self,
        completion: &ProcessLeaseCompletion,
    ) -> Result<(), PluginError>;
}

/// Physical reclamation of terminal processes and their tombstones.
#[async_trait::async_trait]
pub trait ProcessRetention: Send + Sync {
    /// Durable exact release inputs left by Process Prune.
    async fn pending_process_artifact_cleanup(
        &self,
    ) -> Result<Vec<super::model::ProcessArtifactCleanup>, PluginError> {
        Ok(Vec::new())
    }

    /// Acknowledge that all configured artifact stores applied one cleanup.
    async fn complete_process_artifact_cleanup(
        &self,
        _process_id: &ProcessId,
        _incarnation: super::model::ProcessIncarnation,
    ) -> Result<(), PluginError> {
        Ok(())
    }

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

    /// Physically delete terminal process rows whose `updated_at_ms` is older
    /// than `cutoff_epoch_ms`, match `filter` when one is supplied, and have a
    /// process change sequence allowed by the caller's explicit projection
    /// `watermark`, together with their events, observer edges, and lease rows.
    /// Trigger-delivery rows are never deleted by this operation, including in
    /// co-located backends. Callers use [`reconcile_pruned_trigger_deliveries`]
    /// afterward so every backend has one observable reclamation path.
    /// Session-scoped trigger-mutation receipts follow their owner's ADR 0049
    /// deletion frontier during reconciliation; host and platform receipts
    /// remain owned by the trigger store's explicit cutoff lever. Durable backends also
    /// release attachment intents and delete the process-owned `process-env:<id>` and
    /// `process-session-turn:<id>` session stores before deleting the process
    /// row. Backends must fail toward retaining the terminal process if that
    /// cleanup cannot complete.
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
    /// delivery, no parent-end plan, and `filter` — so a caller that must
    /// reclaim rows the registry does not own (the process's durable effect
    /// journal and its await-event promises) fences exactly the rows the
    /// prune reclaims and never a process the registry keeps. The prune
    /// re-evaluates the predicate under its own transaction; a process that
    /// becomes ineligible between survey and prune is retained by the prune
    /// and its already-fenced journal stays reclaimed, which is the conservative
    /// direction for a retired row.
    async fn prunable_terminal_processes(
        &self,
        cutoff_epoch_ms: u64,
        filter: Option<ProcessListFilter>,
        watermark: ProjectionWatermark,
    ) -> Result<Vec<ProcessId>, PluginError>;
}

/// Rebinding a registry backend to the runtime's clock.
///
/// This is a whole-registry construction concern, so it deliberately speaks in
/// terms of the composed [`ProcessRegistry`] handle.
pub trait ProcessClockRebind: Send + Sync {
    /// Return the same registry backend bound to the runtime's clock.
    ///
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
///     ProcessWorklistCursor, ProcessWorklistPage, SessionId,
/// };
/// use lash_core::{ProcessLeases, ProcessObserverRegistry, ProcessQuery};
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
///     async fn list_non_terminal_page(
///         &self,
///         _: NonZeroUsize,
///         _: Option<ProcessWorklistCursor>,
///     ) -> Result<ProcessWorklistPage, PluginError> {
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
/// fn requires_leases<T: ProcessLeases>(_: &T) {}
///
/// // ERROR: the trait bound `ObserverOnly: ProcessLeases` is not satisfied.
/// // An observer-only wrapper is not draggable into the lease concern.
/// fn deny(wrapper: &ObserverOnly) {
///     requires_leases(wrapper);
/// }
/// ```
///
/// The identical wrapper compiles and answers observer reads when the
/// offending bound is absent; `concern_isolation_tests` below exercises that
/// positive twin against the in-memory registry double.
#[allow(dead_code)]
fn concern_isolation_witness_docs() {}

#[cfg(test)]
mod concern_isolation_tests {
    use super::*;
    use crate::runtime::process::testing::TestLocalProcessRegistry;
    use std::sync::Arc;

    /// The positive twin of the module's `compile_fail` witness: a decorator
    /// that composes only the observer concern (plus its declared
    /// [`ProcessQuery`] read dependency) over an inner registry, implementing
    /// nothing else — no leases, no wake outbox, no lifecycle, no retention.
    struct ObserverOnly {
        inner: Arc<TestLocalProcessRegistry>,
    }

    #[async_trait::async_trait]
    impl ProcessQuery for ObserverOnly {
        async fn get_process(
            &self,
            process_id: &ProcessId,
        ) -> Result<Option<ProcessRecord>, PluginError> {
            self.inner.get_process(process_id).await
        }
        async fn list_processes(
            &self,
            filter: &ProcessListFilter,
        ) -> Result<Vec<ProcessRecord>, PluginError> {
            self.inner.list_processes(filter).await
        }
        async fn processes_changed_since(
            &self,
            cursor: ProcessChangeCursor,
            limit: usize,
        ) -> Result<(Vec<ProcessChange>, ProcessChangeCursor), PluginError> {
            self.inner.processes_changed_since(cursor, limit).await
        }
        async fn list_non_terminal_page(
            &self,
            limit: NonZeroUsize,
            continuation: Option<ProcessWorklistCursor>,
        ) -> Result<ProcessWorklistPage, PluginError> {
            self.inner.list_non_terminal_page(limit, continuation).await
        }
        async fn live_reference_summary(
            &self,
        ) -> Result<Vec<ProcessLiveReferenceView>, PluginError> {
            self.inner.live_reference_summary().await
        }
        async fn count_non_terminal_processes(&self) -> Result<usize, PluginError> {
            self.inner.count_non_terminal_processes().await
        }
    }

    #[async_trait::async_trait]
    impl ProcessObserverRegistry for ObserverOnly {
        async fn add_observer(
            &self,
            session_id: &SessionId,
            process_id: &ProcessId,
            by: ProcessObserverBy,
        ) -> Result<(), PluginError> {
            self.inner.add_observer(session_id, process_id, by).await
        }
        async fn remove_observer(
            &self,
            session_id: &SessionId,
            process_id: &ProcessId,
            by: ProcessObserverBy,
        ) -> Result<(), PluginError> {
            self.inner.remove_observer(session_id, process_id, by).await
        }
        async fn transfer_observers(
            &self,
            from_session_id: &SessionId,
            to_session_id: &SessionId,
            process_ids: &[ProcessId],
            by: ProcessObserverBy,
        ) -> Result<(), PluginError> {
            self.inner
                .transfer_observers(from_session_id, to_session_id, process_ids, by)
                .await
        }
        async fn list_observed_by(
            &self,
            session_id: &SessionId,
            filter: &ProcessListFilter,
        ) -> Result<Vec<ProcessRecord>, PluginError> {
            self.inner.list_observed_by(session_id, filter).await
        }
        async fn observers_for_process(
            &self,
            process_id: &ProcessId,
        ) -> Result<Vec<SessionId>, PluginError> {
            self.inner.observers_for_process(process_id).await
        }
        async fn retarget_subscription(
            &self,
            process_id: &ProcessId,
            target: Option<&str>,
        ) -> Result<(), PluginError> {
            self.inner.retarget_subscription(process_id, target).await
        }
        async fn delete_session_process_state(
            &self,
            session_id: &SessionId,
        ) -> Result<ProcessSessionDeleteReport, PluginError> {
            self.inner.delete_session_process_state(session_id).await
        }
    }

    #[tokio::test]
    async fn an_observer_only_wrapper_composes_without_any_other_concern() {
        use super::super::model::{ProcessInput, ProcessProvenance, ProcessRegistration};
        use crate::runtime::process::registry_concerns::ProcessRegistrar as _;

        let inner = Arc::new(TestLocalProcessRegistry::default());
        inner
            .register_process(ProcessRegistration::new(
                "proc-observer-isolation",
                ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                crate::RecoveryContract::ExternallyOwned,
                ProcessProvenance::host(),
            ))
            .await
            .expect("register");
        let wrapper = ObserverOnly {
            inner: Arc::clone(&inner),
        };

        wrapper
            .add_observer(
                &SessionId::from("session-a"),
                &ProcessId::from("proc-observer-isolation"),
                ProcessObserverBy::host("op-observer-isolation"),
            )
            .await
            .expect("add observer through the observer-only wrapper");
        assert!(
            wrapper
                .is_observer(
                    &SessionId::from("session-a"),
                    &ProcessId::from("proc-observer-isolation")
                )
                .await
                .expect("is_observer provided method resolves through ProcessQuery"),
            "observer edge added through the wrapper must be visible through it"
        );
        let observed = wrapper
            .list_observed_by(
                &SessionId::from("session-a"),
                &crate::ProcessListFilter {
                    status: crate::ProcessStatusFilter::Any,
                    ..Default::default()
                },
            )
            .await
            .expect("list observed");
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].id, "proc-observer-isolation");
    }
}

/// Answers whether a process id is currently registered: the registry's
/// truth a host reads through to when its own scope fence is only a cache of
/// the registry's (ADR 0049).
#[async_trait::async_trait]
pub trait ProcessRegistrationProbe: Send + Sync {
    /// Whether `process_id` has a registration row now.
    async fn process_is_registered(&self, process_id: &ProcessId) -> Result<bool, PluginError>;
}

/// What a registry hands the effect host it binds
/// ([`EffectHost::bind_process_registry`]).
#[derive(Clone)]
pub struct ProcessRegistryBinding {
    /// The SQLite database file in which the registry keeps the process-scope
    /// fence beside the process rows, so registration deletes the fence and
    /// inserts the row in one single-file commit and retirement's fence
    /// insert is its one commit point. `None` for a registry with no file of
    /// its own (in memory, or a database the host reaches through its own
    /// connection).
    pub fence_database: Option<std::path::PathBuf>,
    /// The registry's registration truth.
    pub registrations: Arc<dyn ProcessRegistrationProbe>,
}

impl std::fmt::Debug for ProcessRegistryBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProcessRegistryBinding")
            .field("fence_database", &self.fence_database)
            .finish_non_exhaustive()
    }
}
/// The effect hosts a process registry lifts scope fences on at registration.
///
/// Shared by every registry backend: [`bind`](Self::bind) is idempotent and
/// weak, [`reinstate_process_scope`](Self::reinstate_process_scope) lifts the
/// fence of one process scope on every bound host that is still alive. Clones
/// share the same binding set, so a clock-rebound registry copy keeps the
/// bindings of the registry it was derived from.
#[derive(Clone, Default)]
pub struct ProcessScopeFenceHosts {
    hosts: Arc<std::sync::Mutex<Vec<Weak<dyn EffectHost>>>>,
}

impl ProcessScopeFenceHosts {
    /// Bind `effect_host` and hand it the registry's `binding`; binding the
    /// same host twice keeps one entry, and the host's own binding is
    /// idempotent.
    pub fn bind(&self, effect_host: &Arc<dyn EffectHost>, binding: ProcessRegistryBinding) {
        effect_host.bind_process_registry(binding);
        let mut hosts = self
            .hosts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        hosts.retain(|host| host.strong_count() > 0);
        let weak = Arc::downgrade(effect_host);
        if hosts.iter().any(|host| Weak::ptr_eq(host, &weak)) {
            return;
        }
        hosts.push(weak);
    }

    /// Whether any live host is bound.
    pub fn is_empty(&self) -> bool {
        self.hosts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .all(|host| host.strong_count() == 0)
    }

    /// Lift the scope-retirement fence of `process_id` on every bound host.
    pub async fn reinstate_process_scope(&self, process_id: &ProcessId) -> Result<(), PluginError> {
        let hosts: Vec<Arc<dyn EffectHost>> = self
            .hosts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .filter_map(Weak::upgrade)
            .collect();
        let scope = ExecutionScope::process(process_id);
        for host in hosts {
            host.reinstate_effect_scope(&scope)
                .await
                .map_err(|error| {
                    PluginError::Session(format!(
                        "process `{process_id}` registration could not lift its effect-scope fence: {error}"
                    ))
                })?;
        }
        Ok(())
    }
}

impl std::fmt::Debug for ProcessScopeFenceHosts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ProcessScopeFenceHosts(..)")
    }
}
