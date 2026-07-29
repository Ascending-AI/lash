use crate::plugin::PluginError;

use super::ProcessCompletionOutcome;
use super::engine::PersistedSegmentHandover;
use super::events::{
    ProcessAwaitOutput, ProcessCompletionAuthority, ProcessEvent, ProcessEventAppendRequest,
    ProcessEventAppendResult, ProcessWakeDelivery,
};
use super::model::{
    AbandonRequest, ProcessChangeCursor, ProcessExecutionWriteAuthority, ProcessExternalRef,
    ProcessHandleDescriptor, ProcessHandleGrant, ProcessHandleGrantEntry, ProcessLease,
    ProcessLeaseClaimOutcome, ProcessLeaseCompletion, ProcessListFilter, ProcessRecord,
    ProcessRegistration, ProcessSessionDeleteReport, ProcessStartOutcome, ProcessStarted,
    SessionScope, WaitState,
};
use super::references::ProcessLiveReferenceSummary;

/// Outcome of [`ProcessRegistry::prune_terminal_processes`]: how many terminal
/// process rows and event rows were physically deleted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcessPruneReport {
    /// Terminal process rows deleted.
    pub pruned_processes: usize,
    /// Event rows deleted across those processes.
    pub pruned_events: usize,
}

pub const DEFAULT_WAKE_DELIVERY_EXPIRY_MS: u64 = 7 * 24 * 60 * 60 * 1_000;

/// Host-owned bound for process-wake redelivery.
///
/// Exactly-once delivery does not depend on comparing clocks across the
/// process registry and target session store. Receiver completion advances one
/// monotone consumed high-water mark per `(session_id, process_id)`. F7
/// guarantees in-sequence enqueue within each target/process group, so every
/// consumed sequence is a contiguous prefix: stale drivers, retries, and host
/// redrives can only reproduce a sequence at or below that prefix and dedupe
/// forever. `delivery_expiry_ms` is only a pending-delivery liveness bound,
/// evaluated with the runtime's injected clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WakeDeliveryConfig {
    pub delivery_expiry_ms: u64,
}

impl Default for WakeDeliveryConfig {
    fn default() -> Self {
        Self {
            delivery_expiry_ms: DEFAULT_WAKE_DELIVERY_EXPIRY_MS,
        }
    }
}

impl WakeDeliveryConfig {
    pub fn new(delivery_expiry_ms: u64) -> Result<Self, PluginError> {
        if delivery_expiry_ms == 0 {
            return Err(PluginError::Session(
                "process wake delivery expiry must be greater than zero".to_string(),
            ));
        }
        Ok(Self { delivery_expiry_ms })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WakeDeliveryState {
    Pending,
    Enqueued,
    Discarded,
}

impl WakeDeliveryState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Enqueued => "enqueued",
            Self::Discarded => "discarded",
        }
    }
}

/// Durable terminal outcome for an undeliverable wake.
///
/// This is non-exhaustive because subscription retargeting will add its typed
/// discard in the wave that introduces the retarget verb.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WakeDiscardReason {
    Expired,
    TargetGone,
}

impl WakeDiscardReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Expired => "expired",
            Self::TargetGone => "target_gone",
        }
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WakeDelivery {
    pub delivery_id: String,
    pub wake: ProcessWakeDelivery,
    pub state: WakeDeliveryState,
    pub attempts: u64,
    pub first_attempt_ms: Option<u64>,
    pub next_attempt_at_ms: u64,
    pub expires_at_ms: u64,
    pub discard_reason: Option<WakeDiscardReason>,
}

impl WakeDelivery {
    pub fn pending(
        wake: ProcessWakeDelivery,
        config: WakeDeliveryConfig,
    ) -> Result<Self, PluginError> {
        let next_attempt_at_ms = wake.created_at_ms;
        let hash = crate::stable_hash::stable_json_sha256_hex(&(
            wake.target_session_id.as_str(),
            wake.process_id.as_str(),
            wake.sequence,
        ))
        .map_err(|error| {
            PluginError::Session(format!(
                "failed to derive wake delivery id for process `{}`: {error}",
                wake.process_id
            ))
        })?;
        Ok(Self {
            delivery_id: format!("wake-delivery:{hash}"),
            expires_at_ms: wake.created_at_ms.saturating_add(config.delivery_expiry_ms),
            wake,
            state: WakeDeliveryState::Pending,
            attempts: 0,
            first_attempt_ms: None,
            next_attempt_at_ms,
            discard_reason: None,
        })
    }

    pub fn source_key(&self) -> String {
        crate::process_wake_source_key(&self.wake.process_id, self.wake.sequence)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WakeDeliveryReport {
    pub pending: usize,
    pub enqueued: usize,
    pub discarded: usize,
    pub expired: usize,
    pub target_gone: usize,
}

/// Durability-neutral process registry.
///
/// Process waits are coordination behavior and live on
/// [`ProcessWorkDriver`](crate::ProcessWorkDriver) /
/// [`ProcessAwaiter`](crate::ProcessAwaiter), not on persistence
/// implementations. Registry methods are point reads and writes only. See
/// `docs/adr/0016-process-waits-live-on-the-work-driver-seam.md`.
#[async_trait::async_trait]
pub trait ProcessRegistry: Send + Sync {
    fn wake_delivery_config(&self) -> WakeDeliveryConfig;

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

    /// Process ids must be unique across prune horizons. A receiver's consumed
    /// wake high-water mark deliberately survives sender-side pruning, and event
    /// sequences restart for a re-registered id, so re-registering a previously
    /// pruned process id would have its wakes silently absorbed below the
    /// retained mark. Hosts mint fresh process ids rather than reusing pruned
    /// ones (the ADR 0049 single-use rule for sessions applies to process ids
    /// at the prune horizon).
    async fn register_process(
        &self,
        registration: ProcessRegistration,
    ) -> Result<ProcessRecord, PluginError>;

    /// Persist the bounded engine continuation for exactly one segment.
    /// Repeating an identical write is an idempotent no-op; conflicting data
    /// for the same `(process_id, segment_ordinal)` is rejected.
    async fn put_segment_handover(
        &self,
        process_id: &str,
        handover: PersistedSegmentHandover,
    ) -> Result<(), PluginError>;

    /// Load the continuation for an exact process segment ordinal.
    async fn get_segment_handover(
        &self,
        process_id: &str,
        segment_ordinal: u64,
    ) -> Result<Option<PersistedSegmentHandover>, PluginError>;

    /// Load the highest persisted segment ordinal for recovery.
    async fn latest_segment_handover(
        &self,
        process_id: &str,
    ) -> Result<Option<PersistedSegmentHandover>, PluginError>;

    /// Remove all cross-segment execution state once the process is terminal.
    async fn delete_segment_handovers(&self, process_id: &str) -> Result<(), PluginError>;

    /// Attach a durable backend reference to a registered process.
    ///
    /// Implementations must reject unknown process ids. The first assignment
    /// stores the reference. Repeating the exact same assignment is an
    /// idempotent no-op that returns the existing record unchanged. Assigning a
    /// different reference after one has been stored is a registry model error.
    async fn set_external_ref(
        &self,
        process_id: &str,
        external_ref: ProcessExternalRef,
    ) -> Result<ProcessRecord, PluginError>;

    async fn grant_handle(
        &self,
        session_scope: &SessionScope,
        process_id: &str,
        descriptor: ProcessHandleDescriptor,
    ) -> Result<ProcessHandleGrant, PluginError>;

    async fn revoke_handle(
        &self,
        session_scope: &SessionScope,
        process_id: &str,
    ) -> Result<(), PluginError>;

    async fn transfer_handle_grants(
        &self,
        from_scope: &SessionScope,
        to_scope: &SessionScope,
        process_ids: &[String],
    ) -> Result<(), PluginError>;

    async fn list_handle_grants(
        &self,
        session_scope: &SessionScope,
    ) -> Result<Vec<ProcessHandleGrantEntry>, PluginError>;

    async fn list_live_handle_grants(
        &self,
        session_scope: &SessionScope,
    ) -> Result<Vec<ProcessHandleGrantEntry>, PluginError> {
        Ok(self
            .list_handle_grants(session_scope)
            .await?
            .into_iter()
            .filter(|(_, record)| !record.is_terminal())
            .collect())
    }

    async fn has_handle_grant(
        &self,
        session_scope: &SessionScope,
        process_id: &str,
    ) -> Result<bool, PluginError> {
        Ok(self
            .list_handle_grants(session_scope)
            .await?
            .into_iter()
            .any(|(grant, _)| grant.process_id == process_id))
    }

    async fn handle_grants_for_process(
        &self,
        process_id: &str,
    ) -> Result<Vec<ProcessHandleGrant>, PluginError>;

    async fn delete_session_process_state(
        &self,
        session_id: &str,
    ) -> Result<ProcessSessionDeleteReport, PluginError>;

    /// Append a host-owned event that is not emitted by the process execution.
    ///
    /// This unfenced path is reserved for host signal/cancel coordination.
    /// Process engines receive only [`ProcessEngineProcessContext`](super::engine::ProcessEngineProcessContext);
    /// execution-owned events must use its authority-bound emitter.
    async fn append_event(
        &self,
        process_id: &str,
        request: ProcessEventAppendRequest,
    ) -> Result<ProcessEventAppendResult, PluginError>;

    /// Append an event emitted by the currently executing process attempt.
    ///
    /// Implementations validate `authority` and append in one atomic write.
    async fn append_event_with_authority(
        &self,
        process_id: &str,
        request: ProcessEventAppendRequest,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessEventAppendResult, PluginError>;

    async fn events_after(
        &self,
        process_id: &str,
        after_sequence: u64,
    ) -> Result<Vec<ProcessEvent>, PluginError>;

    /// Count events of `event_type` with `sequence <= up_to_sequence`.
    ///
    /// This is the signal-ordinal query: the Nth occurrence of a signal event
    /// resolves the Nth durable wait key. The default scans the event log;
    /// store backends override it with a COUNT so per-signal cost stays flat
    /// instead of growing with a long-lived process's history.
    async fn count_events_through(
        &self,
        process_id: &str,
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

    /// The most recent `limit` events, in ascending sequence order.
    ///
    /// Observation snapshots use this to show a bounded activity tail without
    /// fetching a process's entire history on every poll. The default scans
    /// the event log; store backends override it with ORDER BY ... LIMIT.
    async fn recent_events(
        &self,
        process_id: &str,
        limit: usize,
    ) -> Result<Vec<ProcessEvent>, PluginError> {
        let mut events = self.events_after(process_id, 0).await?;
        if events.len() > limit {
            events.drain(..events.len() - limit);
        }
        Ok(events)
    }

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
    /// row's declared [`RecoveryDisposition`](super::model::RecoveryDisposition)
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
        process_id: &str,
        await_output: ProcessAwaitOutput,
        authority: ProcessCompletionAuthority,
    ) -> Result<ProcessCompletionOutcome, PluginError>;

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

    /// Record the durable, lease-fenced "execution started" fact (ADR 0019).
    ///
    /// The first attempt stores `started`. An identical replay is idempotent.
    /// Rerunnable recovery replaces the retained fact with the next consecutive
    /// attempt; OwnerBound recovery rejects a distinct execution.
    async fn record_first_started_with_authority(
        &self,
        process_id: &str,
        started: ProcessStarted,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessStartOutcome, PluginError>;

    /// Set the durable, non-terminal Abandon Request marker (ADR 0019).
    ///
    /// First-writer-wins: if a marker is already present the call is an
    /// idempotent no-op returning the existing record unchanged, preserving the
    /// original recorded authorization rather than letting a later requester
    /// clobber it. Setting it on a terminal row is a model error — a terminal
    /// process has already recorded its outcome, so there is nothing to abandon.
    async fn request_process_abandon(
        &self,
        process_id: &str,
        request: AbandonRequest,
    ) -> Result<ProcessRecord, PluginError>;

    async fn set_process_wait_with_authority(
        &self,
        process_id: &str,
        wait: WaitState,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessRecord, PluginError>;

    async fn clear_process_wait_with_authority(
        &self,
        process_id: &str,
        authority: &ProcessExecutionWriteAuthority,
    ) -> Result<ProcessRecord, PluginError>;

    async fn get_process(&self, process_id: &str) -> Option<ProcessRecord>;

    /// Fallible process lookup for correctness-critical execution paths where
    /// a transient store failure must not be mistaken for an absent row.
    async fn try_get_process(
        &self,
        process_id: &str,
    ) -> Result<Option<ProcessRecord>, PluginError> {
        Ok(self.get_process(process_id).await)
    }

    async fn list_processes(
        &self,
        filter: &ProcessListFilter,
    ) -> Result<Vec<ProcessRecord>, PluginError>;

    /// Return process records whose persisted row changed strictly after
    /// `cursor`, ordered by the backend's per-store change sequence.
    ///
    /// This is a host-level completeness read for trusted projectors. It is not
    /// scoped by handle grants, and the cursor must be treated as opaque outside
    /// the store that issued it.
    async fn processes_changed_since(
        &self,
        cursor: ProcessChangeCursor,
        limit: usize,
    ) -> Result<(Vec<ProcessRecord>, ProcessChangeCursor), PluginError>;

    /// Return due group heads and record one delivery attempt for each.
    ///
    /// Implementations must preserve sequence order inside a
    /// `(target_session_id, process_id)` group while selecting fairly across
    /// distinct groups by `next_attempt_at_ms`.
    async fn pending_wake_deliveries(&self, limit: usize)
    -> Result<Vec<WakeDelivery>, PluginError>;

    async fn list_wake_deliveries(
        &self,
        state: Option<WakeDeliveryState>,
    ) -> Result<Vec<WakeDelivery>, PluginError>;

    async fn wake_delivery_report(&self) -> Result<WakeDeliveryReport, PluginError>;

    async fn mark_wake_enqueued(&self, delivery_id: &str) -> Result<(), PluginError>;

    async fn discard_wake_delivery(
        &self,
        delivery_id: &str,
        reason: WakeDiscardReason,
    ) -> Result<(), PluginError>;

    async fn redrive_wake_delivery(&self, delivery_id: &str) -> Result<(), PluginError>;

    /// Defer a retryable non-delivery until the supplied runtime-clock time.
    async fn defer_wake_delivery(
        &self,
        delivery_id: &str,
        next_attempt_at_ms: u64,
    ) -> Result<(), PluginError>;

    /// All non-terminal process records, in stable `process_id` order.
    ///
    /// This is the recovery sweep's worklist: every process that was started
    /// but has not reached a terminal event is a candidate for re-execution by
    /// a [`DurableProcessWorker`](crate::DurableProcessWorker) after a crash.
    /// Terminal processes are excluded — they are already done and idempotent by
    /// `process_id`, so re-running them would be wasted work.
    async fn list_non_terminal(&self) -> Result<Vec<ProcessRecord>, PluginError>;

    /// Return the candidate ids that have no persisted process row, preserving
    /// input order. Durable backends override this with one `NOT EXISTS`
    /// anti-join so recovery does not issue one point read per candidate.
    async fn filter_unregistered_process_ids(
        &self,
        process_ids: &[String],
    ) -> Result<Vec<String>, PluginError> {
        let mut missing = Vec::new();
        for process_id in process_ids {
            if self.get_process(process_id).await.is_none() {
                missing.push(process_id.clone());
            }
        }
        Ok(missing)
    }

    /// Count non-terminal process rows by their captured definition and
    /// execution-environment references.
    async fn live_reference_summary(&self)
    -> Result<Vec<ProcessLiveReferenceSummary>, PluginError>;

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
        process_id: &str,
        owner: &crate::LeaseOwnerIdentity,
        lease_ttl_ms: u64,
    ) -> Result<ProcessLeaseClaimOutcome, PluginError>;

    /// Retry a process lease claim after observing `observed_holder`.
    ///
    /// An unexpired lease remains busy. Once its TTL expires, the caller may
    /// acquire it with a monotonically advanced fencing token.
    async fn reclaim_process_lease(
        &self,
        process_id: &str,
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
        process_id: &str,
    ) -> Result<Option<ProcessLease>, PluginError>;

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

    /// Physically delete terminal process rows whose `updated_at_ms` is older
    /// than `cutoff_epoch_ms`, match `filter` when one is supplied, and have a
    /// process change sequence no later than `up_to_change_seq` when supplied,
    /// together with their events, handle grants, lease rows, and
    /// trigger-delivery reservations whose deterministic process id points at a
    /// pruned row. The same cutoff also prunes trigger-mutation idempotency
    /// receipts, bounding receipt retention under the host's existing cleanup
    /// schedule. Durable backends also release attachment intents and delete
    /// the process-owned `process-env:<id>` and
    /// `process-session-turn:<id>` session stores before deleting the process
    /// row. Backends must fail toward retaining the terminal process if that
    /// cleanup cannot complete.
    /// Host-scheduled retention: hosts that project results/events into their
    /// own store call this to keep the registry bounded. Non-terminal rows are
    /// never touched. Callers must choose a retention window comfortably longer
    /// than any waiter lifetime — a pruned process id becomes "unknown process"
    /// to late awaits. Re-emitting the same trigger occurrence id after its
    /// process has aged out of retention may reserve a fresh delivery process
    /// id; occurrence-level idempotency still holds, and ordinary emit replays
    /// do not straddle a retention window in practice.
    ///
    /// ```no_run
    /// use std::time::{Duration, SystemTime, UNIX_EPOCH};
    /// use lash_core::{PluginError, ProcessRegistry};
    ///
    /// async fn prune_week_old(registry: &dyn ProcessRegistry) -> Result<(), PluginError> {
    ///     let now_ms = SystemTime::now()
    ///         .duration_since(UNIX_EPOCH)
    ///         .expect("clock after epoch")
    ///         .as_millis() as u64;
    ///     // Window must exceed any in-flight await's lifetime (ADR 0017).
    ///     let cutoff = now_ms - Duration::from_secs(7 * 24 * 60 * 60).as_millis() as u64;
    ///     let report = registry.prune_terminal_processes(cutoff, None, None).await?;
    ///     eprintln!(
    ///         "pruned {} processes, {} events",
    ///         report.pruned_processes, report.pruned_events
    ///     );
    ///     Ok(())
    /// }
    /// ```
    async fn prune_terminal_processes(
        &self,
        cutoff_epoch_ms: u64,
        filter: Option<ProcessListFilter>,
        up_to_change_seq: Option<ProcessChangeCursor>,
    ) -> Result<ProcessPruneReport, PluginError>;
}
