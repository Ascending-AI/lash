//! Scheduled trigger sources (FIG-5348): a source whose occurrences are its
//! schedule's ticks.
//!
//! A host registers a [`TriggerSchedule`] for a source type it declares. The
//! session actor that owns an enabled subscription to such a source fires
//! each tick on the durable substrate: it keeps the earliest next tick of its
//! schedules as its due time, and the claim that due time causes emits the
//! tick's occurrence under a key the tick names, so the tick fires once
//! across a crash or a failover. Lash owns the driving; the host owns only
//! what a source's configuration means in time.

use std::collections::BTreeMap;
use std::sync::Arc;

/// What a scheduled source type's configuration means in time, for one source
/// type. Every instant is epoch milliseconds on the store clock, and every
/// `source` is a subscription's stored source, as its registration wrote it.
pub trait TriggerSchedule: Send + Sync + 'static {
    /// The source a tick's occurrence names, as the source type's contract
    /// declares it: what each delivery's start checks the occurrence
    /// against.
    ///
    /// # Errors
    ///
    /// [`TriggerScheduleError`] when `source` names no schedule.
    fn occurrence_source(
        &self,
        source: &serde_json::Value,
    ) -> Result<serde_json::Value, TriggerScheduleError>;

    /// The first tick of the source configured by `source` strictly after
    /// `after_ms`; `None` when it never ticks again.
    ///
    /// # Errors
    ///
    /// [`TriggerScheduleError`] when `source` names no schedule.
    fn next_tick(
        &self,
        source: &serde_json::Value,
        after_ms: u64,
    ) -> Result<Option<u64>, TriggerScheduleError>;

    /// The last tick of the source configured by `source` at or before
    /// `at_ms`; `None` when it never ticked by then.
    ///
    /// # Errors
    ///
    /// [`TriggerScheduleError`] when `source` names no schedule.
    fn last_tick(
        &self,
        source: &serde_json::Value,
        at_ms: u64,
    ) -> Result<Option<u64>, TriggerScheduleError>;

    /// The payload of the occurrence the tick at `tick_ms` emits.
    fn payload(&self, tick_ms: u64) -> serde_json::Value;
}

/// A source configuration that names no schedule.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the schedule's source configuration is invalid: {reason}")]
pub struct TriggerScheduleError {
    /// Why, as operator text.
    pub reason: String,
}

impl TriggerScheduleError {
    /// A refusal for `reason`.
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

/// The deployment's scheduled source types, each with its schedule.
#[derive(Clone, Default)]
pub struct TriggerSchedules {
    by_source_type: BTreeMap<String, Arc<dyn TriggerSchedule>>,
}

impl std::fmt::Debug for TriggerSchedules {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_set().entries(self.by_source_type.keys()).finish()
    }
}

impl TriggerSchedules {
    /// No scheduled source types.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Schedule `source_type`'s occurrences by `schedule`, replacing any
    /// schedule it had.
    pub fn register(&mut self, source_type: impl Into<String>, schedule: Arc<dyn TriggerSchedule>) {
        self.by_source_type.insert(source_type.into(), schedule);
    }

    /// The schedule of `source_type`, when it is scheduled.
    #[must_use]
    pub fn get(&self, source_type: &str) -> Option<&Arc<dyn TriggerSchedule>> {
        self.by_source_type.get(source_type)
    }

    /// Whether no source type is scheduled.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_source_type.is_empty()
    }
}
