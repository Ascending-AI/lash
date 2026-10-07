use crate::ProcessId;
use crate::SessionId;
use crate::plugin::PluginError;

use super::model::{ProcessChangeCursor, ProcessRecord};
pub use super::registry_concerns::{
    ProcessClockRebind, ProcessEventLog, ProcessLifecycle, ProcessObserverRegistry, ProcessQuery,
    ProcessRegistrar, ProcessRetention, ProcessToolIntents,
};

/// Outcome of process retention: how many terminal processes, events, and
/// coordinated trigger deliveries were physically deleted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcessPruneReport {
    /// Terminal process rows deleted.
    pub pruned_processes: usize,
    /// Event rows deleted across those processes.
    pub pruned_events: usize,
    /// Low-level registry implementations report zero; the public Lash facade
    /// fills this field after coordinating with its configured trigger store.
    pub pruned_trigger_deliveries: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProjectionWatermark {
    UpTo(ProcessChangeCursor),
    NoProjector,
}

/// Hard ceiling for a page of non-terminal process records.
pub const MAX_NON_TERMINAL_PROCESS_PAGE_SIZE: usize = 256;

/// Opaque continuation for a bounded scan of non-terminal process records.
///
/// The cursor belongs to the registry that issued it. Hosts should pass it
/// unchanged to [`ProcessQuery::list_non_terminal_processes_page`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessRegistryCursor {
    backend: String,
    after_process_id: ProcessId,
    through_process_id: ProcessId,
}

impl ProcessRegistryCursor {
    pub fn new(
        backend: impl Into<String>,
        after_process_id: ProcessId,
        through_process_id: ProcessId,
    ) -> Self {
        Self {
            backend: backend.into(),
            after_process_id,
            through_process_id,
        }
    }

    /// Backend identity used to reject cross-backend cursor reuse.
    pub fn backend(&self) -> &str {
        &self.backend
    }

    /// Exclusive keyset boundary for the next page.
    pub fn after_process_id(&self) -> &ProcessId {
        &self.after_process_id
    }

    /// Inclusive upper key captured when the scan began.
    pub fn through_process_id(&self) -> &ProcessId {
        &self.through_process_id
    }
}

/// One bounded page of non-terminal process records.
#[derive(Clone, Debug, PartialEq)]
pub struct NonTerminalProcessPage {
    pub records: Vec<ProcessRecord>,
    pub continuation: Option<ProcessRegistryCursor>,
}

/// One durable ledger row recording that a parent scope has ended.
///
/// The row carries no action list. The work is a query: the children whose
/// recorded lifetime is `Until` this scope. A row is written by the
/// scope's own end, for every parent kind alike, and is settled once that
/// query returns nothing left to do.
///
/// **Integrator class 3: store and process-engine implementors.**
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ParentEndPlan {
    /// Scope whose end made the plan executable.
    pub parent: crate::ScopeId,
    /// Registry-stamped instant at which the scope ended.
    pub ended_at_ms: u64,
}

/// Compiled only under `cfg(any(test, feature = "testing"))` and never a
/// supertrait of [`ProcessRegistry`]: the conformance suites take
/// [`ConformanceProcessRegistry`] (`ProcessRegistry + ProcessRegistryTestSupport`)
/// and reach the probes through that handle. Backends implement it under the
/// same gate they forward to `lash-core/testing`; the production trait never
/// requires a testing method, and a build with `lash-core/testing` on but a
/// backend's `testing` off still compiles.
#[cfg(any(test, feature = "testing"))]
pub trait ProcessRegistryTestSupport: Send + Sync {}

/// Small-fixture event-log convenience, excluded from production builds.
///
/// Tests using this helper assert that their complete history fits in one
/// page. Production consumers must make pagination and retention explicit.
#[cfg(any(test, feature = "testing"))]
#[async_trait::async_trait]
pub trait ProcessEventLogTestSupport: ProcessEventLog {
    async fn full_event_window(
        &self,
        process_id: &ProcessId,
        after_sequence: u64,
    ) -> Result<Vec<super::events::ProcessEvent>, PluginError> {
        let limit = std::num::NonZeroUsize::new(4_096).unwrap_or(std::num::NonZeroUsize::MIN);
        let outcome = self
            .event_page_after(
                process_id,
                after_sequence,
                limit,
                super::events::ProcessEventQueryMode::Full,
            )
            .await?;
        match outcome {
            super::events::ProcessEventReadOutcome::Retained(super::events::ProcessEventPage {
                events: super::events::ProcessEventPageEvents::Full(events),
                more: super::events::ProcessEventPageMore::Complete,
            }) => Ok(events),
            super::events::ProcessEventReadOutcome::Retained(super::events::ProcessEventPage {
                more: super::events::ProcessEventPageMore::More { .. },
                ..
            }) => Err(PluginError::Session(
                "test fixture process history did not fit in one complete page".to_string(),
            )),
            super::events::ProcessEventReadOutcome::Retained(_) => {
                unreachable!("full query returned lite page")
            }
            super::events::ProcessEventReadOutcome::NoLongerRetained(
                super::events::ProcessEventHistoryRetention::Pruned {
                    terminal_label,
                    pruned_at_ms,
                },
            ) => Err(PluginError::ProcessNoLongerRetained {
                terminal_label,
                pruned_at_ms,
            }),
            super::events::ProcessEventReadOutcome::NoLongerRetained(
                super::events::ProcessEventHistoryRetention::Released { released_through },
            ) => Err(PluginError::ProcessEventsReleased {
                process_id: process_id.clone(),
                released_through,
            }),
        }
    }
}

#[cfg(any(test, feature = "testing"))]
impl<T> ProcessEventLogTestSupport for T where T: ProcessEventLog + ?Sized {}

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
/// The registry also answers the `F` its backend recorded through
/// [`FleetFormatStore`](crate::store::FleetFormatStore): durable readers on
/// the handle — the wake-delivery and parent-end projections, the
/// effect-summary fold — resolve their `[N-1, N]` read windows from it
/// (FIG-3796). A registry with no recorded row — an in-memory fake — answers
/// [`FleetFormat::current`](crate::FleetFormat::current).
pub trait ProcessRegistry:
    crate::store::FleetFormatStore
    + ProcessQuery
    + ProcessRegistrar
    + ProcessObserverRegistry
    + ProcessEventLog
    + ProcessLifecycle
    + ProcessToolIntents
    + ProcessRetention
    + ProcessClockRebind
{
}

impl<T> ProcessRegistry for T where
    T: crate::store::FleetFormatStore
        + ProcessQuery
        + ProcessRegistrar
        + ProcessObserverRegistry
        + ProcessEventLog
        + ProcessLifecycle
        + ProcessToolIntents
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
    session_store_factory: Option<&dyn crate::DeploymentStore>,
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
                .lookup_session(&session_id)
                .await
                .map_err(|error| {
                    PluginError::of_store_error(
                        format!("failed to read deleted-session frontier for `{session_id}`"),
                        error,
                    )
                })?
                == crate::store::SessionLookup::Deleted
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
    // `list_non_terminal_processes_page`, so recovery cannot lose the re-registered process.
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
    session_store_factory: Option<&dyn crate::DeploymentStore>,
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
    session_store_factory: Option<&dyn crate::DeploymentStore>,
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
    session_store_factory: Option<&dyn crate::DeploymentStore>,
) -> Result<crate::TriggerRetentionReconciliationReport, PluginError> {
    reconcile_pruned_trigger_deliveries_inner(
        registry,
        trigger_store,
        session_store_factory,
        || async {},
    )
    .await
}
