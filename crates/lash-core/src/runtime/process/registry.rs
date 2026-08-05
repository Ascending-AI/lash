use crate::plugin::PluginError;

use super::ProcessCompletionOutcome;
use super::engine::PersistedSegmentHandover;
use super::events::{
    ProcessAwaitOutput, ProcessCompletionAuthority, ProcessEvent, ProcessEventAppendRequest,
    ProcessEventAppendResult, ProcessWakeDelivery,
};
use super::model::{
    AbandonRequest, ProcessChange, ProcessChangeCursor, ProcessExecutionWriteAuthority,
    ProcessExternalRef, ProcessLease, ProcessLeaseClaimOutcome, ProcessLeaseCompletion,
    ProcessListFilter, ProcessObserverBy, ProcessRecord, ProcessRegistration,
    ProcessSessionDeleteReport, ProcessStartOutcome, ProcessStarted, SessionId, WaitState,
};
use super::references::ProcessLiveReferenceSummary;

/// Outcome of process retention: how many terminal processes, events, and
/// coordinated trigger deliveries were physically deleted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcessPruneReport {
    /// Terminal process rows deleted.
    pub pruned_processes: usize,
    /// Event rows deleted across those processes.
    pub pruned_events: usize,
    /// Trigger-delivery rows reconciled after process pruning committed.
    ///
    /// Low-level registry implementations report zero; the public Lash facade
    /// fills this field after coordinating with its configured trigger store.
    pub pruned_trigger_deliveries: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProjectionWatermark {
    UpTo(ProcessChangeCursor),
    NoProjector,
}

pub const DEFAULT_WAKE_DELIVERY_EXPIRY_MS: u64 = 7 * 24 * 60 * 60 * 1_000;
pub const WAKE_ENQUEUING_STALE_AFTER_MS: u64 = 30_000;

/// Host-owned bound for process-wake redelivery.
///
/// Exactly-once delivery does not depend on comparing clocks across the
/// process registry and target session store. Receiver completion advances one
/// monotone receiver allocation floor per `(session_id, process_id)`. Because
/// selected-batch settlement may be out of order, this is a redelivery fence,
/// not a consumption watermark. The process registry separately retains one
/// sender allocation floor per wake target and process, so sequences stay
/// strictly monotone across pruned incarnations without consulting a clock.
/// A live receiver row is idempotent; a no-live-row wake at or below the
/// receiver floor returns the typed store-rewind error.
/// `delivery_expiry_ms` is only a pending-delivery liveness bound, evaluated
/// with the runtime's injected clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WakeDeliveryConfig {
    pub delivery_expiry_ms: u64,
    pub enqueuing_stale_after_ms: u64,
}

impl Default for WakeDeliveryConfig {
    fn default() -> Self {
        Self {
            delivery_expiry_ms: DEFAULT_WAKE_DELIVERY_EXPIRY_MS,
            enqueuing_stale_after_ms: WAKE_ENQUEUING_STALE_AFTER_MS,
        }
    }
}

impl WakeDeliveryConfig {
    /// Constructs wake-retention policy for process-store implementors and rejects a zero expiry so
    /// pending delivery cannot expire at creation.
    pub fn new(delivery_expiry_ms: u64) -> Result<Self, PluginError> {
        if delivery_expiry_ms == 0 {
            return Err(PluginError::Session(
                "process wake delivery expiry must be greater than zero".to_string(),
            ));
        }
        Ok(Self {
            delivery_expiry_ms,
            enqueuing_stale_after_ms: WAKE_ENQUEUING_STALE_AFTER_MS,
        })
    }

    /// Sets the reclaim age for process-store implementors and rejects zero so an active enqueuing
    /// claim is not immediately stale.
    pub fn with_enqueuing_stale_after_ms(
        mut self,
        enqueuing_stale_after_ms: u64,
    ) -> Result<Self, PluginError> {
        if enqueuing_stale_after_ms == 0 {
            return Err(PluginError::Session(
                "process wake enqueuing stale age must be greater than zero".to_string(),
            ));
        }
        self.enqueuing_stale_after_ms = enqueuing_stale_after_ms;
        Ok(self)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WakeDeliveryState {
    Pending,
    Enqueuing,
    Enqueued,
    Discarded,
}

impl WakeDeliveryState {
    /// Exposes the stable snake-case wake-delivery state for process-store implementors.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Enqueuing => "enqueuing",
            Self::Enqueued => "enqueued",
            Self::Discarded => "discarded",
        }
    }
}

/// Durable terminal outcome for an undeliverable wake.
///
/// Non-exhaustive so future delivery-terminal reasons remain additive.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WakeDiscardReason {
    Expired,
    TargetGone,
    Retargeted,
    SequenceRewound,
}

impl WakeDiscardReason {
    /// Exposes the stable snake-case discard reason for process-store implementors and durable
    /// diagnostics.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Expired => "expired",
            Self::TargetGone => "target_gone",
            Self::Retargeted => "retargeted",
            Self::SequenceRewound => "sequence_rewound",
        }
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WakeDelivery {
    pub delivery_id: String,
    pub wake: ProcessWakeDelivery,
    pub state: WakeDeliveryState,
    /// Ownership fence minted for the current `enqueuing` claim.
    ///
    /// Every transition out of `enqueuing` must present this exact token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_token: Option<String>,
    pub attempts: u64,
    pub first_attempt_ms: Option<u64>,
    pub next_attempt_at_ms: u64,
    pub expires_at_ms: u64,
    pub discard_reason: Option<WakeDiscardReason>,
}

impl WakeDelivery {
    /// Creates a pending wake for process-store implementors with a content-derived ID, zero
    /// attempts, immediate eligibility, and saturating expiry from creation time.
    pub fn pending(
        wake: ProcessWakeDelivery,
        config: WakeDeliveryConfig,
    ) -> Result<Self, PluginError> {
        if !super::wake::is_process_wake_id(&wake.wake_id) {
            return Err(PluginError::InvalidProcessWakeIdentity {
                wake_id: wake.wake_id,
            });
        }
        let next_attempt_at_ms = wake.created_at_ms;
        let delivery_id = wake.wake_id.clone();
        Ok(Self {
            delivery_id,
            expires_at_ms: wake.created_at_ms.saturating_add(config.delivery_expiry_ms),
            wake,
            state: WakeDeliveryState::Pending,
            claim_token: None,
            attempts: 0,
            first_attempt_ms: None,
            next_attempt_at_ms,
            discard_reason: None,
        })
    }

    /// Returns the exact enqueuing ownership fence process-store implementors must present for
    /// settlement, or an error when the claimed row carries no token.
    pub fn claim_token(&self) -> Result<&str, PluginError> {
        self.claim_token.as_deref().ok_or_else(|| {
            PluginError::Session(format!(
                "enqueuing wake delivery `{}` is missing its claim token",
                self.delivery_id
            ))
        })
    }
}

#[cfg(test)]
mod wake_delivery_identity_tests {
    use super::*;

    #[test]
    fn delivery_row_reuses_structural_wake_id() {
        let wake = ProcessWakeDelivery {
            wake_id: format!("wake:v1:sha256:{}", "a".repeat(64)),
            target_session_id: "session".to_string(),
            process_id: "process".to_string(),
            sequence: 1,
            event_type: "process.wake".to_string(),
            event_invocation: crate::RuntimeInvocation::effect(
                crate::RuntimeScope::new("session"),
                "effect",
                crate::RuntimeEffectKind::Process,
                "replay",
            ),
            process_caused_by: None,
            input: "wake".to_string(),
            created_at_ms: 10,
        };
        let delivery = WakeDelivery::pending(wake, WakeDeliveryConfig::default()).unwrap();
        assert_eq!(
            delivery.delivery_id,
            format!("wake:v1:sha256:{}", "a".repeat(64))
        );
    }

    #[test]
    fn delivery_row_rejects_untrusted_wake_identity() {
        for wake_id in ["", "wake:v1:sha256:abc", "wake:vx:sha256:0000"] {
            let wake = ProcessWakeDelivery {
                wake_id: wake_id.to_string(),
                target_session_id: "session".to_string(),
                process_id: "process".to_string(),
                sequence: 1,
                event_type: "process.wake".to_string(),
                event_invocation: crate::RuntimeInvocation::effect(
                    crate::RuntimeScope::new("session"),
                    "effect",
                    crate::RuntimeEffectKind::Process,
                    "replay",
                ),
                process_caused_by: None,
                input: "wake".to_string(),
                created_at_ms: 10,
            };
            assert!(matches!(
                WakeDelivery::pending(wake, WakeDeliveryConfig::default()),
                Err(PluginError::InvalidProcessWakeIdentity { wake_id: rejected })
                    if rejected == wake_id
            ));
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WakeDeliveryClaimOutcome {
    Applied,
    ClaimLost { state: WakeDeliveryState },
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WakeDeliveryBlockedGroup {
    pub target_session_id: String,
    pub process_id: String,
    pub blocking_delivery_id: String,
    pub blocking_sequence: u64,
    pub reason: WakeDiscardReason,
    /// Pass this id to `redrive_wake_delivery` to unblock the group.
    pub redrive_delivery_id: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WakeDeliveryReport {
    pub pending: usize,
    pub enqueuing: usize,
    pub enqueued: usize,
    pub discarded: usize,
    pub expired: usize,
    pub target_gone: usize,
    pub retargeted: usize,
    pub sequence_rewound: usize,
    /// Ordering groups stopped by a discarded head while later work remains.
    pub blocked_groups: Vec<WakeDeliveryBlockedGroup>,
}

impl WakeDeliveryReport {
    /// Counts delivery states and discard reasons for process-store embedders, then identifies each
    /// target/process ordering group blocked behind a discarded head with later work.
    pub fn from_deliveries<'a>(deliveries: impl IntoIterator<Item = &'a WakeDelivery>) -> Self {
        let deliveries = deliveries.into_iter().collect::<Vec<_>>();
        let mut report = Self::default();
        for delivery in &deliveries {
            match delivery.state {
                WakeDeliveryState::Pending => report.pending += 1,
                WakeDeliveryState::Enqueuing => report.enqueuing += 1,
                WakeDeliveryState::Enqueued => report.enqueued += 1,
                WakeDeliveryState::Discarded => {
                    report.discarded += 1;
                    match delivery.discard_reason {
                        Some(WakeDiscardReason::Expired) => report.expired += 1,
                        Some(WakeDiscardReason::TargetGone) => report.target_gone += 1,
                        Some(WakeDiscardReason::Retargeted) => report.retargeted += 1,
                        Some(WakeDiscardReason::SequenceRewound) => report.sequence_rewound += 1,
                        None => {}
                    }
                }
            }
        }

        let mut groups = std::collections::BTreeMap::<(&str, &str), Vec<&WakeDelivery>>::new();
        for delivery in &deliveries {
            groups
                .entry((
                    delivery.wake.target_session_id.as_str(),
                    delivery.wake.process_id.as_str(),
                ))
                .or_default()
                .push(delivery);
        }
        for group in groups.values_mut() {
            group.sort_by_key(|delivery| delivery.wake.sequence);
            let Some(last_active_index) = group.iter().rposition(|delivery| {
                matches!(
                    delivery.state,
                    WakeDeliveryState::Pending | WakeDeliveryState::Enqueuing
                )
            }) else {
                continue;
            };
            if let Some(delivery) = group[..last_active_index].iter().find(|delivery| {
                delivery.state == WakeDeliveryState::Discarded
                    && delivery
                        .discard_reason
                        .is_some_and(|reason| reason != WakeDiscardReason::SequenceRewound)
            }) {
                let reason = delivery
                    .discard_reason
                    .expect("discarded delivery filtered to a typed reason");
                report.blocked_groups.push(WakeDeliveryBlockedGroup {
                    target_session_id: delivery.wake.target_session_id.clone(),
                    process_id: delivery.wake.process_id.clone(),
                    blocking_delivery_id: delivery.delivery_id.clone(),
                    blocking_sequence: delivery.wake.sequence,
                    reason,
                    redrive_delivery_id: delivery.delivery_id.clone(),
                });
            }
        }
        report.blocked_groups.sort_by(|left, right| {
            (
                &left.target_session_id,
                &left.process_id,
                left.blocking_sequence,
            )
                .cmp(&(
                    &right.target_session_id,
                    &right.process_id,
                    right.blocking_sequence,
                ))
        });
        report
    }
}

/// Substrate-scoped durable continuation storage. This is not part of the
/// uniform process registry because only segmented execution substrates need
/// it.
#[async_trait::async_trait]
pub trait ProcessContinuationStore: Send + Sync {
    async fn put_segment_handover(
        &self,
        process_id: &str,
        handover: PersistedSegmentHandover,
    ) -> Result<(), PluginError>;

    async fn get_segment_handover(
        &self,
        process_id: &str,
        segment_ordinal: u64,
    ) -> Result<Option<PersistedSegmentHandover>, PluginError>;

    async fn latest_segment_handover(
        &self,
        process_id: &str,
    ) -> Result<Option<PersistedSegmentHandover>, PluginError>;

    async fn delete_segment_handovers(&self, process_id: &str) -> Result<(), PluginError>;
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
    async fn register_process_with_observers(
        &self,
        registration: ProcessRegistration,
        observers: &[SessionId],
    ) -> Result<ProcessRecord, PluginError>;

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

    async fn add_observer(
        &self,
        session_id: &str,
        process_id: &str,
        by: ProcessObserverBy,
    ) -> Result<(), PluginError>;

    async fn remove_observer(
        &self,
        session_id: &str,
        process_id: &str,
        by: ProcessObserverBy,
    ) -> Result<(), PluginError>;

    async fn transfer_observers(
        &self,
        from_session_id: &str,
        to_session_id: &str,
        process_ids: &[String],
        by: ProcessObserverBy,
    ) -> Result<(), PluginError>;

    async fn list_observed_by(&self, session_id: &str) -> Result<Vec<ProcessRecord>, PluginError>;

    async fn list_live_observed_by(
        &self,
        session_id: &str,
    ) -> Result<Vec<ProcessRecord>, PluginError> {
        Ok(self
            .list_observed_by(session_id)
            .await?
            .into_iter()
            .filter(|record| !record.is_terminal())
            .collect())
    }

    async fn is_observer(&self, session_id: &str, process_id: &str) -> Result<bool, PluginError> {
        if self.get_process(process_id).await?.is_none() {
            return Ok(false);
        }
        Ok(self
            .list_observed_by(session_id)
            .await?
            .into_iter()
            .any(|record| record.id == process_id))
    }

    async fn observers_for_process(&self, process_id: &str) -> Result<Vec<SessionId>, PluginError>;

    /// Append a subscription-retarget audit event, update the indexed target,
    /// and discard pending deliveries to the old target atomically.
    async fn retarget_subscription(
        &self,
        process_id: &str,
        target: Option<&str>,
    ) -> Result<(), PluginError>;

    /// Remove observer edges and wake routing owned by a deleted session.
    ///
    /// This bulk session-lifecycle cleanup deliberately does not append
    /// per-process observer or retarget audit events.
    async fn delete_session_process_state(
        &self,
        session_id: &str,
    ) -> Result<ProcessSessionDeleteReport, PluginError>;

    /// Raw sender-floor probe for cross-backend conformance tests.
    #[doc(hidden)]
    async fn wake_allocation_floor_for_testing(
        &self,
        target_session_id: &str,
        process_id: &str,
    ) -> Result<Option<u64>, PluginError> {
        let _ = (target_session_id, process_id);
        Ok(None)
    }

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

    async fn get_process(&self, process_id: &str) -> Result<Option<ProcessRecord>, PluginError>;

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

    /// All non-terminal process records, in stable `process_id` order.
    ///
    /// This is the recovery sweep's worklist: every process that was started
    /// but has not reached a terminal event is a candidate for re-execution by
    /// a [`DurableProcessWorker`](crate::DurableProcessWorker) after a crash.
    /// Terminal processes are excluded — they are already done and idempotent by
    /// `process_id`, so re-running them would be wasted work.
    async fn list_non_terminal(&self) -> Result<Vec<ProcessRecord>, PluginError>;

    /// Return the candidate ids that were never registered, preserving input
    /// order. A terminal process retained only as a tombstone is registered
    /// history and must not be offered back to recovery. Durable backends
    /// override this with one anti-join so recovery does not issue one point
    /// read per candidate.
    async fn filter_unregistered_process_ids(
        &self,
        process_ids: &[String],
    ) -> Result<Vec<String>, PluginError> {
        let mut missing = Vec::new();
        for process_id in process_ids {
            match self.get_process(process_id).await {
                Ok(Some(_)) | Err(PluginError::ProcessNoLongerRetained { .. }) => {}
                Ok(None) => missing.push(process_id.clone()),
                Err(error) => return Err(error),
            }
        }
        Ok(missing)
    }

    /// Return the candidate ids retained as terminal-process tombstones,
    /// preserving input order.
    ///
    /// Cross-store retention uses this to remove trigger deliveries only after
    /// their deterministic process ids have been durably pruned.
    async fn filter_tombstoned_process_ids(
        &self,
        process_ids: &[String],
    ) -> Result<Vec<String>, PluginError> {
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
        Ok(tombstoned)
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
    /// process change sequence allowed by the caller's explicit projection
    /// `watermark`, together with their events, observer edges, and lease rows.
    /// Trigger-delivery rows are never deleted by this operation, including in
    /// co-located backends. Callers use [`reconcile_pruned_trigger_deliveries`]
    /// afterward so every backend has one observable reclamation path.
    /// Trigger-mutation idempotency receipts are owned by the trigger store's
    /// explicit retention lever, not process retention. Durable backends also
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
    /// Occurrence-level idempotency survives retention: re-emitting the same
    /// trigger occurrence id after its process has aged out reserves nothing
    /// new, because the occurrence record — not the delivery row — carries that
    /// identity.
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
}

#[derive(Debug)]
struct TriggerDeliveryReconciliationPlan {
    surveyed_count: usize,
    candidates: Vec<crate::TriggerDeliveryRetentionCandidate>,
}

async fn prepare_pruned_trigger_delivery_reconciliation(
    registry: &dyn ProcessRegistry,
    trigger_store: &dyn crate::TriggerStore,
) -> Result<TriggerDeliveryReconciliationPlan, PluginError> {
    let surveyed = match trigger_store.list_delivery_retention_candidates().await {
        Ok(surveyed) => surveyed,
        Err(err) => {
            tracing::warn!(
                failure_stage = "list_delivery_rows",
                error = %err,
                "trigger-delivery retention reconciliation failed"
            );
            return Err(err);
        }
    };
    let process_ids = surveyed
        .iter()
        .map(|candidate| candidate.process_id.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    tracing::debug!(
        candidate_count = surveyed.len(),
        candidate_process_count = process_ids.len(),
        "surveyed trigger-delivery retention candidates"
    );
    tracing::trace!(candidates = ?surveyed, "surveyed trigger-delivery row identities");
    let tombstoned = match registry.filter_tombstoned_process_ids(&process_ids).await {
        Ok(tombstoned) => tombstoned,
        Err(err) => {
            tracing::warn!(
                failure_stage = "classify_process_history",
                candidate_count = surveyed.len(),
                error = %err,
                "trigger-delivery retention reconciliation failed"
            );
            return Err(err);
        }
    };
    let tombstoned = tombstoned
        .into_iter()
        .collect::<std::collections::HashSet<_>>();
    let candidates = surveyed
        .iter()
        .filter(|candidate| tombstoned.contains(&candidate.process_id))
        .cloned()
        .collect::<Vec<_>>();
    tracing::debug!(
        candidate_count = surveyed.len(),
        classified_for_deletion = candidates.len(),
        "classified trigger-delivery retention candidates"
    );
    Ok(TriggerDeliveryReconciliationPlan {
        surveyed_count: surveyed.len(),
        candidates,
    })
}

async fn apply_pruned_trigger_delivery_reconciliation(
    registry: &dyn ProcessRegistry,
    trigger_store: &dyn crate::TriggerStore,
    plan: TriggerDeliveryReconciliationPlan,
) -> Result<usize, PluginError> {
    if plan.candidates.is_empty() {
        return Ok(0);
    }

    // Classification and deletion live in separate stores. Revalidate at the
    // action boundary so a process id reused after the survey fails toward
    // retaining its delivery; the exact row keys below independently prevent a
    // replacement row from being swept into this stale decision. If the process
    // is re-registered after this revalidation, deleting the observed delivery
    // is still safe: the new live row is itself recovery evidence through
    // `list_non_terminal`, so recovery cannot lose the re-registered process.
    let process_ids = plan
        .candidates
        .iter()
        .map(|candidate| candidate.process_id.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let still_tombstoned = match registry.filter_tombstoned_process_ids(&process_ids).await {
        Ok(still_tombstoned) => still_tombstoned,
        Err(err) => {
            tracing::warn!(
                failure_stage = "revalidate_process_history",
                candidate_count = plan.surveyed_count,
                classified_for_deletion = plan.candidates.len(),
                error = %err,
                "trigger-delivery retention reconciliation failed"
            );
            return Err(err);
        }
    };
    let still_tombstoned = still_tombstoned
        .into_iter()
        .collect::<std::collections::HashSet<_>>();
    let candidates = plan
        .candidates
        .into_iter()
        .filter(|candidate| still_tombstoned.contains(&candidate.process_id))
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        tracing::debug!(
            candidate_count = plan.surveyed_count,
            deletion_result = "preserved_after_revalidation",
            "trigger-delivery retention reconciliation made no change"
        );
        return Ok(0);
    }
    let deleted = match trigger_store
        .delete_delivery_retention_candidates(&candidates)
        .await
    {
        Ok(deleted) => deleted,
        Err(err) => {
            tracing::warn!(
                failure_stage = "delete_observed_delivery_rows",
                candidate_count = plan.surveyed_count,
                attempted_delete_count = candidates.len(),
                attempted_candidates = ?candidates,
                error = %err,
                "trigger-delivery retention reconciliation failed"
            );
            return Err(err);
        }
    };
    if deleted > 0 {
        tracing::info!(
            candidate_count = plan.surveyed_count,
            attempted_delete_count = candidates.len(),
            deleted_deliveries = deleted,
            deleted_candidates = ?candidates,
            deletion_result = "deleted_observed_rows",
            "completed trigger-delivery retention reconciliation"
        );
    } else {
        tracing::debug!(
            candidate_count = plan.surveyed_count,
            attempted_delete_count = candidates.len(),
            deleted_deliveries = deleted,
            deletion_result = "observed_rows_changed_or_already_deleted",
            "trigger-delivery retention reconciliation made no change"
        );
    }
    Ok(deleted)
}

async fn reconcile_pruned_trigger_deliveries_inner<F, Fut>(
    registry: &dyn ProcessRegistry,
    trigger_store: &dyn crate::TriggerStore,
    after_classification: F,
) -> Result<usize, PluginError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let plan = prepare_pruned_trigger_delivery_reconciliation(registry, trigger_store).await?;
    after_classification().await;
    apply_pruned_trigger_delivery_reconciliation(registry, trigger_store, plan).await
}

#[cfg(any(test, feature = "testing"))]
pub(crate) async fn reconcile_pruned_trigger_deliveries_interleaved<F, Fut>(
    registry: &dyn ProcessRegistry,
    trigger_store: &dyn crate::TriggerStore,
    after_classification: F,
) -> Result<usize, PluginError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    reconcile_pruned_trigger_deliveries_inner(registry, trigger_store, after_classification).await
}

/// Delete trigger deliveries whose deterministic process ids have been pruned.
///
/// Process and trigger state may live in separate durable stores. This
/// coordinator preserves those ownership boundaries: the process registry
/// identifies durable tombstones, while the trigger store owns deletion of its
/// delivery rows. Re-running it repairs a prior partial cleanup safely.
pub async fn reconcile_pruned_trigger_deliveries(
    registry: &dyn ProcessRegistry,
    trigger_store: &dyn crate::TriggerStore,
) -> Result<usize, PluginError> {
    reconcile_pruned_trigger_deliveries_inner(registry, trigger_store, || async {}).await
}
