use crate::ProcessId;
use crate::plugin::PluginError;

use super::model::{ProcessChangeCursor, ProcessRecord};
pub use super::registry_concerns::{
    ProcessClockRebind, ProcessEventLog, ProcessLifecycle, ProcessObserverRegistry, ProcessQuery,
    ProcessRegistrar, ProcessRetention, ProcessToolIntents,
};

/// Outcome of process retention: how many terminal processes and events were
/// physically deleted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcessPruneReport {
    /// Terminal process rows deleted.
    pub pruned_processes: usize,
    /// Event rows deleted across those processes.
    pub pruned_events: usize,
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
/// the handle — the parent-end projection, the effect-summary fold — resolve their `[N-1, N]` read windows from it
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
