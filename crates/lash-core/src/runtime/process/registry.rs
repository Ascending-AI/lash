use crate::ProcessId;
use crate::SessionId;
use crate::plugin::PluginError;

use super::engine::PersistedSegmentHandover;
use super::events::ProcessWakeDelivery;
use super::model::{ProcessChangeCursor, ProcessRecord};
pub use super::registry_concerns::{
    ProcessClockRebind, ProcessEventLog, ProcessLeases, ProcessLifecycle, ProcessObserverRegistry,
    ProcessQuery, ProcessRegistrar, ProcessRegistrationProbe, ProcessRegistryBinding,
    ProcessRetention, ProcessScopeFenceHosts, ProcessToolIntents, ProcessWakeOutbox,
};

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

/// Opaque continuation for a bounded scan of the recovery worklist.
///
/// The cursor belongs to the registry that issued it. Hosts should pass it
/// unchanged to [`ProcessQuery::list_non_terminal_page`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessWorklistCursor {
    backend: String,
    after_process_id: ProcessId,
    through_process_id: ProcessId,
}

impl ProcessWorklistCursor {
    /// Construct a backend-tagged cursor when implementing a [`ProcessRegistry`].
    pub fn new(
        backend: impl Into<String>,
        after_process_id: impl Into<ProcessId>,
        through_process_id: impl Into<ProcessId>,
    ) -> Self {
        Self {
            backend: backend.into(),
            after_process_id: after_process_id.into(),
            through_process_id: through_process_id.into(),
        }
    }

    /// Backend identity used to reject cross-backend cursor reuse.
    pub fn backend(&self) -> &str {
        &self.backend
    }

    /// Exclusive keyset boundary for the next page.
    pub fn after_process_id(&self) -> &str {
        &self.after_process_id
    }

    /// Inclusive upper key captured when the scan began.
    pub fn through_process_id(&self) -> &str {
        &self.through_process_id
    }
}

/// One bounded page from the registry recovery worklist.
#[derive(Clone, Debug, PartialEq)]
pub struct ProcessWorklistPage {
    pub records: Vec<ProcessRecord>,
    pub continuation: Option<ProcessWorklistCursor>,
}

/// Durable teardown work committed atomically with one parent's terminal outcome.
/// **Integrator class 3: store and process-engine implementors.**
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProcessParentEndPlan {
    /// Terminal parent whose completion made the plan executable.
    pub process_id: ProcessId,
    /// Ordered, replay-keyed actions retained for crash redrive.
    pub actions: Vec<crate::ToolIntentParentEndAction>,
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

/// Complete in-memory disposition of a wake delivery.
///
/// State-specific evidence travels with the state that requires it, so an enqueuing delivery
/// cannot exist without its ownership fence and a typed discard cannot exist without its reason.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WakeDeliveryDisposition {
    /// Awaiting its next claim attempt.
    Pending,
    /// Claimed while the receiver enqueue is in flight.
    Enqueuing {
        /// Ownership fence that every transition out of `enqueuing` must present.
        claim_token: String,
    },
    /// Successfully enqueued at the target session.
    Enqueued,
    /// Terminally discarded with a typed classification.
    Discarded {
        /// The classification that made this delivery terminal.
        reason: WakeDiscardReason,
    },
    /// A deliberately representable legacy row whose durable discard reason is `NULL`.
    DiscardedUnattributed,
}

impl WakeDeliveryDisposition {
    /// Returns the stable label-only state represented by this disposition.
    pub fn state(&self) -> WakeDeliveryState {
        match self {
            Self::Pending => WakeDeliveryState::Pending,
            Self::Enqueuing { .. } => WakeDeliveryState::Enqueuing,
            Self::Enqueued => WakeDeliveryState::Enqueued,
            Self::Discarded { .. } | Self::DiscardedUnattributed => WakeDeliveryState::Discarded,
        }
    }

    /// Returns the typed reason carried by a classified discard.
    pub fn discard_reason(&self) -> Option<WakeDiscardReason> {
        match self {
            Self::Discarded { reason } => Some(*reason),
            _ => None,
        }
    }
}

macro_rules! define_wake_discard_ordering_group_rule {
    (
        blocking: [$($blocking:ident),+ $(,)?],
        non_blocking: [$($non_blocking:ident),+ $(,)?],
    ) => {
        /// Stable labels for discarded wakes that do not block later deliveries in their ordering
        /// group. SQL-backed registries bind this list into their claim predicates.
        pub const NON_BLOCKING_ORDERING_GROUP_LABELS: &'static [&'static str] =
            &[$(Self::$non_blocking.as_str()),+];

        /// Whether this discard reason blocks later deliveries in the same ordering group.
        pub const fn blocks_ordering_group(self) -> bool {
            match self {
                $(Self::$blocking => true,)+
                $(Self::$non_blocking => false,)+
            }
        }
    };
}

impl WakeDiscardReason {
    /// Exposes the stable snake-case discard reason for process-store implementors and durable
    /// diagnostics.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Expired => "expired",
            Self::TargetGone => "target_gone",
            Self::Retargeted => "retargeted",
            Self::SequenceRewound => "sequence_rewound",
        }
    }

    define_wake_discard_ordering_group_rule! {
        blocking: [Expired, TargetGone, Retargeted],
        non_blocking: [SequenceRewound],
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WakeDelivery {
    pub delivery_id: String,
    pub wake: ProcessWakeDelivery,
    pub disposition: WakeDeliveryDisposition,
    pub attempts: u64,
    pub first_attempt_ms: Option<u64>,
    pub next_attempt_at_ms: u64,
    pub expires_at_ms: u64,
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
            disposition: WakeDeliveryDisposition::Pending,
            attempts: 0,
            first_attempt_ms: None,
            next_attempt_at_ms,
        })
    }

    /// Returns the label-only state represented by this delivery's disposition.
    pub fn state(&self) -> WakeDeliveryState {
        self.disposition.state()
    }

    /// Returns the exact enqueuing ownership fence process-store implementors must present for
    /// settlement, or an error when the delivery is not enqueuing.
    pub fn claim_token(&self) -> Result<&str, PluginError> {
        match &self.disposition {
            WakeDeliveryDisposition::Enqueuing { claim_token } => Ok(claim_token),
            _ => Err(PluginError::Session(format!(
                "wake delivery `{}` is not enqueuing",
                self.delivery_id
            ))),
        }
    }
}

#[cfg(test)]
mod wake_delivery_identity_tests {
    use super::*;

    #[test]
    fn delivery_row_reuses_structural_wake_id() {
        let wake = ProcessWakeDelivery {
            version: crate::PROCESS_WAKE_DELIVERY_FORMAT_VERSION,
            wake_id: format!("wake:v1:blake3:{}", "a".repeat(64)),
            target_session_id: SessionId::from("session"),
            process_id: ProcessId::from("process"),
            process_incarnation: crate::ProcessIncarnation::from_registration_sequence(1),
            sequence: 1,
            event_type: "process.wake".to_string(),
            event_invocation: crate::RuntimeInvocation::effect(
                crate::RuntimeScope::new("session"),
                "effect",
                crate::RuntimeEffectKind::Process,
                "replay",
            ),
            process_caused_by: None,
            authority: crate::QueuedWorkAuthority::default(),
            input: "wake".to_string(),
            created_at_ms: 10,
        };
        let delivery = WakeDelivery::pending(wake, WakeDeliveryConfig::default()).unwrap();
        assert_eq!(
            delivery.delivery_id,
            format!("wake:v1:blake3:{}", "a".repeat(64))
        );
    }

    #[test]
    fn delivery_row_rejects_untrusted_wake_identity() {
        for wake_id in ["", "wake:v1:blake3:abc", "wake:v1:sha256:0000"] {
            let wake = ProcessWakeDelivery {
                version: crate::PROCESS_WAKE_DELIVERY_FORMAT_VERSION,
                wake_id: wake_id.to_string(),
                target_session_id: SessionId::from("session"),
                process_id: ProcessId::from("process"),
                process_incarnation: crate::ProcessIncarnation::from_registration_sequence(1),
                sequence: 1,
                event_type: "process.wake".to_string(),
                event_invocation: crate::RuntimeInvocation::effect(
                    crate::RuntimeScope::new("session"),
                    "effect",
                    crate::RuntimeEffectKind::Process,
                    "replay",
                ),
                process_caused_by: None,
                authority: crate::QueuedWorkAuthority::default(),
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
    pub target_session_id: SessionId,
    pub process_id: ProcessId,
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
            match &delivery.disposition {
                WakeDeliveryDisposition::Pending => report.pending += 1,
                WakeDeliveryDisposition::Enqueuing { .. } => report.enqueuing += 1,
                WakeDeliveryDisposition::Enqueued => report.enqueued += 1,
                WakeDeliveryDisposition::Discarded { reason } => {
                    report.discarded += 1;
                    match reason {
                        WakeDiscardReason::Expired => report.expired += 1,
                        WakeDiscardReason::TargetGone => report.target_gone += 1,
                        WakeDiscardReason::Retargeted => report.retargeted += 1,
                        WakeDiscardReason::SequenceRewound => report.sequence_rewound += 1,
                    }
                }
                WakeDeliveryDisposition::DiscardedUnattributed => report.discarded += 1,
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
                    delivery.state(),
                    WakeDeliveryState::Pending | WakeDeliveryState::Enqueuing
                )
            }) else {
                continue;
            };
            if let Some(delivery) = group[..last_active_index].iter().find(|delivery| {
                delivery.state() == WakeDeliveryState::Discarded
                    && delivery
                        .disposition
                        .discard_reason()
                        .is_some_and(WakeDiscardReason::blocks_ordering_group)
            }) {
                let reason = delivery
                    .disposition
                    .discard_reason()
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
        process_id: &ProcessId,
        handover: PersistedSegmentHandover,
    ) -> Result<(), PluginError>;

    async fn get_segment_handover(
        &self,
        process_id: &ProcessId,
        segment_ordinal: u64,
    ) -> Result<Option<PersistedSegmentHandover>, PluginError>;

    async fn latest_segment_handover(
        &self,
        process_id: &ProcessId,
    ) -> Result<Option<PersistedSegmentHandover>, PluginError>;

    async fn delete_segment_handovers(&self, process_id: &ProcessId) -> Result<(), PluginError>;
}

/// Test-only probes on a process registry.
///
/// Compiled only under `cfg(any(test, feature = "testing"))` and never a
/// supertrait of [`ProcessRegistry`]: the conformance suites take
/// [`ConformanceProcessRegistry`] (`ProcessRegistry + ProcessRegistryTestSupport`)
/// and reach the probes through that handle. Backends implement it under the
/// same gate they forward to `lash-core/testing`; the production trait never
/// requires a testing method, and a build with `lash-core/testing` on but a
/// backend's `testing` off still compiles.
#[cfg(any(test, feature = "testing"))]
#[async_trait::async_trait]
pub trait ProcessRegistryTestSupport: Send + Sync {
    /// Raw sender-floor probe for cross-backend conformance tests.
    async fn wake_allocation_floor_for_testing(
        &self,
        target_session_id: &SessionId,
        process_id: &ProcessId,
    ) -> Result<Option<u64>, PluginError> {
        let _ = (target_session_id, process_id);
        Ok(None)
    }
}

/// A process registry together with its test-only probes: the registry type
/// the conformance suites take. Blanket-implemented under the same gate as
/// [`ProcessRegistryTestSupport`]; an `Arc<dyn ConformanceProcessRegistry>`
/// upcasts to `Arc<dyn ProcessRegistry>` wherever production code is exercised.
#[cfg(any(test, feature = "testing"))]
pub trait ConformanceProcessRegistry: ProcessRegistry + ProcessRegistryTestSupport {}

#[cfg(any(test, feature = "testing"))]
impl<T> ConformanceProcessRegistry for T where
    T: ProcessRegistry + ProcessRegistryTestSupport + ?Sized
{
}

/// Durability-neutral process registry.
///
/// Process waits are coordination behavior and live on
/// [`ProcessWorkSubstrate`](crate::ProcessWorkSubstrate) and native awaiter,
/// not on persistence
/// implementations. Registry methods are point reads and writes only. See
/// `docs/adr/0016-process-waits-live-on-the-work-driver-seam.md`.
///
/// No production registry method is a `*_for_testing` hook and this trait
/// carries no test-only obligation: the probes live on the gated
/// [`ProcessRegistryTestSupport`], reached through
/// [`ConformanceProcessRegistry`] by the conformance suites (see
/// [`StoreMaintenance`](crate::store::StoreMaintenance) for the store-side
/// norm).
pub trait ProcessRegistry:
    ProcessQuery
    + ProcessRegistrar
    + ProcessObserverRegistry
    + ProcessEventLog
    + ProcessLifecycle
    + ProcessToolIntents
    + ProcessWakeOutbox
    + ProcessLeases
    + ProcessRetention
    + ProcessClockRebind
{
}

impl<T> ProcessRegistry for T where
    T: ProcessQuery
        + ProcessRegistrar
        + ProcessObserverRegistry
        + ProcessEventLog
        + ProcessLifecycle
        + ProcessToolIntents
        + ProcessWakeOutbox
        + ProcessLeases
        + ProcessRetention
        + ProcessClockRebind
        + ?Sized
{
}

#[derive(Debug)]
struct TriggerDeliveryReconciliationPlan {
    surveyed_count: usize,
    candidates: Vec<crate::TriggerDeliveryRetentionCandidate>,
    deleted_session_ids: Vec<SessionId>,
}

async fn prepare_pruned_trigger_delivery_reconciliation(
    registry: &dyn ProcessRegistry,
    trigger_store: &dyn crate::TriggerStore,
    session_store_factory: Option<&dyn crate::SessionStoreFactory>,
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
    let mut deleted_session_ids = Vec::new();
    if let Some(session_store_factory) = session_store_factory {
        let owner_ids = trigger_store.list_session_owner_ids_for_retention().await?;
        for session_id in owner_ids {
            if session_store_factory
                .session_was_deleted(&session_id)
                .await
                .map_err(|error| {
                    PluginError::Session(format!(
                        "failed to read deleted-session frontier for `{session_id}`: {error}"
                    ))
                })?
            {
                deleted_session_ids.push(session_id);
            }
        }
    }
    Ok(TriggerDeliveryReconciliationPlan {
        surveyed_count: surveyed.len(),
        candidates,
        deleted_session_ids,
    })
}

async fn apply_pruned_trigger_delivery_reconciliation(
    registry: &dyn ProcessRegistry,
    trigger_store: &dyn crate::TriggerStore,
    plan: TriggerDeliveryReconciliationPlan,
) -> Result<crate::TriggerRetentionReconciliationReport, PluginError> {
    // Classification and deletion live in separate stores. Revalidate at the
    // action boundary so a process id reused after the survey fails toward
    // retaining its delivery; the exact row keys below independently prevent a
    // replacement row from being swept into this stale decision. If the process
    // is re-registered after this revalidation, deleting the observed delivery
    // is still safe: the new live row is itself recovery evidence through
    // `list_non_terminal_page`, so recovery cannot lose the re-registered process.
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
    let report = match trigger_store
        .reconcile_trigger_retention(&candidates, &plan.deleted_session_ids)
        .await
    {
        Ok(report) => report,
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
    if report != crate::TriggerRetentionReconciliationReport::default() {
        tracing::info!(
            candidate_count = plan.surveyed_count,
            attempted_delete_count = candidates.len(),
            deleted_deliveries = report.reclaimed_delivery_count,
            deleted_occurrences = report.reclaimed_occurrence_count,
            deleted_subscriptions = report.reclaimed_subscription_count,
            deleted_mutation_receipts = report.reclaimed_mutation_receipt_count,
            deleted_candidates = ?candidates,
            deletion_result = "deleted_observed_rows",
            "completed trigger-delivery retention reconciliation"
        );
    } else {
        tracing::debug!(
            candidate_count = plan.surveyed_count,
            attempted_delete_count = candidates.len(),
            deleted_deliveries = report.reclaimed_delivery_count,
            deletion_result = "observed_rows_changed_or_already_deleted",
            "trigger-delivery retention reconciliation made no change"
        );
    }
    Ok(report)
}

async fn reconcile_pruned_trigger_deliveries_inner<F, Fut>(
    registry: &dyn ProcessRegistry,
    trigger_store: &dyn crate::TriggerStore,
    session_store_factory: Option<&dyn crate::SessionStoreFactory>,
    after_classification: F,
) -> Result<crate::TriggerRetentionReconciliationReport, PluginError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let plan = prepare_pruned_trigger_delivery_reconciliation(
        registry,
        trigger_store,
        session_store_factory,
    )
    .await?;
    after_classification().await;
    apply_pruned_trigger_delivery_reconciliation(registry, trigger_store, plan).await
}

#[cfg(any(test, feature = "testing"))]
pub async fn reconcile_pruned_trigger_deliveries_interleaved<F, Fut>(
    registry: &dyn ProcessRegistry,
    trigger_store: &dyn crate::TriggerStore,
    session_store_factory: Option<&dyn crate::SessionStoreFactory>,
    after_classification: F,
) -> Result<crate::TriggerRetentionReconciliationReport, PluginError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    reconcile_pruned_trigger_deliveries_inner(
        registry,
        trigger_store,
        session_store_factory,
        after_classification,
    )
    .await
}

/// Reconcile trigger retention after deterministic process ids are pruned.
///
/// Process and trigger state may live in separate durable stores. This
/// coordinator preserves those ownership boundaries: the process registry
/// identifies durable tombstones, the session factory classifies permanent
/// ADR 0049 deletion, and the trigger store owns the atomic deletion. The
/// trigger transaction reclaims exact deliveries, empty-fan-out occurrences,
/// and dead-session subscriptions plus receipts. Re-running it repairs a prior
/// partial cleanup safely.
pub async fn reconcile_pruned_trigger_deliveries(
    registry: &dyn ProcessRegistry,
    trigger_store: &dyn crate::TriggerStore,
    session_store_factory: Option<&dyn crate::SessionStoreFactory>,
) -> Result<crate::TriggerRetentionReconciliationReport, PluginError> {
    reconcile_pruned_trigger_deliveries_inner(
        registry,
        trigger_store,
        session_store_factory,
        || async {},
    )
    .await
}
