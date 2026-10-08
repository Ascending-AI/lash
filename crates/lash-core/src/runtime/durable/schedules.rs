//! A session's scheduled trigger sources on the durable substrate
//! (FIG-5348).
//!
//! A session that owns an enabled subscription to a scheduled source (a
//! source type the deployment registered a [`TriggerSchedule`] for) fires
//! the source's ticks itself. Its activation keeps the earliest next tick as
//! the actor's `ScheduleTick` due time, so a session with nothing else to do
//! releases as `waiting` until then and the ordinary claim of a due actor
//! wakes it. Nothing in process memory times a tick.
//!
//! - **Which tick.** At store time `now`, a source fires its last tick at or
//!   before `now`, when that tick is later than the moment the earliest of
//!   its enabled subscriptions became enabled. A tick that passed while every
//!   subscription was disabled, or before any was created, never fires, and
//!   ticks missed while no node served the session fire once, as the latest
//!   of them.
//! - **Exactly once.** The occurrence's idempotency key names the session,
//!   the source and the tick, so a re-emission after a crash, a lost
//!   acknowledgement or a failover finds the occurrence held and starts
//!   nothing more.
//! - **Boundaries.** A disable, re-enable or delete commits with a wake of
//!   the owning session (the trigger store's mutation transaction), so the
//!   session recomputes its schedules at the next pass: a disabled or
//!   deleted subscription never fires at its next boundary, and a re-enabled
//!   one fires again under the same subscription.

use std::collections::BTreeMap;
use std::sync::Arc;

use lash_durable::{DueSource, DurableError, DurableInstant, StoreFailure, StoreFailureKind};

use super::session::TurnError;
use crate::{
    ActorContext, AdmittedScope, PluginError, SessionId, TriggerOccurrenceRequest, TriggerRouter,
    TriggerSchedule, TriggerSchedules, TriggerStore, TriggerSubscriptionFilter,
    TriggerSubscriptionRecord,
};

/// The deployment's scheduled sources and what fires their ticks.
#[derive(Clone)]
pub struct ScheduledTriggers {
    store: Arc<dyn TriggerStore>,
    router: TriggerRouter,
    schedules: TriggerSchedules,
}

impl std::fmt::Debug for ScheduledTriggers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScheduledTriggers")
            .field("schedules", &self.schedules)
            .finish_non_exhaustive()
    }
}

impl ScheduledTriggers {
    /// Fire `schedules`' ticks through `router` over `store`, the trigger
    /// store the router emits into.
    #[must_use]
    pub fn new(
        store: Arc<dyn TriggerStore>,
        router: TriggerRouter,
        schedules: TriggerSchedules,
    ) -> Self {
        Self {
            store,
            router,
            schedules,
        }
    }

    /// Whether no source type is scheduled: such a deployment's sessions
    /// never read their subscriptions for ticks.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.schedules.is_empty()
    }

    /// Fire every tick of `session`'s scheduled sources due at store time
    /// `now`, and note the earliest next tick as the session's
    /// `ScheduleTick` due time. Answers that tick.
    ///
    /// A source whose configuration names no schedule, or whose tick the
    /// router refuses for good, is skipped with a warning: no pass would
    /// clear it, and the session's own work goes on.
    ///
    /// # Errors
    ///
    /// [`TurnError::Durable`] when the subscriptions do not read or an
    /// emission meets a failure that may pass by itself; the next pass
    /// retries the tick.
    pub async fn fire_due(
        &self,
        cx: &ActorContext,
        session: &SessionId,
        now: DurableInstant,
    ) -> Result<Option<DurableInstant>, TurnError> {
        cx.clear_due(DueSource::ScheduleTick);
        let mut filter = TriggerSubscriptionFilter::for_session(session.clone());
        filter.enabled = Some(true);
        let subscriptions = self
            .store
            .list_subscriptions(filter)
            .await
            .map_err(transient)?;
        let now_ms = u64::try_from(now.0).unwrap_or_default();
        let mut next: Option<u64> = None;
        for source in self.sources(&subscriptions) {
            let tick = match source.due_tick(now_ms) {
                Ok(tick) => tick,
                Err(error) => {
                    tracing::warn!(
                        %session,
                        source_type = %source.source_type,
                        source_key = %source.source_key,
                        %error,
                        "a scheduled trigger source names no schedule; it never ticks"
                    );
                    continue;
                }
            };
            if let Some(tick) = tick {
                self.emit(cx, session, &source, tick).await?;
            }
            match source.schedule.next_tick(source.config, now_ms) {
                Ok(Some(after)) => next = Some(next.map_or(after, |next| next.min(after))),
                Ok(None) => {}
                Err(error) => tracing::warn!(
                    %session,
                    source_type = %source.source_type,
                    %error,
                    "a scheduled trigger source has no next tick"
                ),
            }
        }
        let next = next.map(|at| DurableInstant(i64::try_from(at).unwrap_or(i64::MAX)));
        if let Some(at) = next {
            cx.note_due(DueSource::ScheduleTick, at);
        }
        Ok(next)
    }

    /// The scheduled sources `subscriptions` subscribe to, each with the
    /// earliest moment one of them became enabled.
    fn sources<'a>(
        &'a self,
        subscriptions: &'a [TriggerSubscriptionRecord],
    ) -> Vec<ScheduledSource<'a>> {
        let mut sources: BTreeMap<(&str, &str), ScheduledSource<'a>> = BTreeMap::new();
        for subscription in subscriptions {
            if !subscription.lifecycle.enabled() {
                continue;
            }
            let Some(schedule) = self.schedules.get(&subscription.source_type) else {
                continue;
            };
            sources
                .entry((&subscription.source_type, &subscription.source_key))
                .and_modify(|source| {
                    source.enabled_since_ms =
                        source.enabled_since_ms.min(subscription.updated_at_ms);
                })
                .or_insert(ScheduledSource {
                    schedule: schedule.as_ref(),
                    source_type: &subscription.source_type,
                    source_key: &subscription.source_key,
                    config: &subscription.source,
                    enabled_since_ms: subscription.updated_at_ms,
                });
        }
        sources.into_values().collect()
    }

    /// Emit `source`'s tick at `tick` for `session`, once.
    async fn emit(
        &self,
        cx: &ActorContext,
        session: &SessionId,
        source: &ScheduledSource<'_>,
        tick: u64,
    ) -> Result<(), TurnError> {
        let key = tick_key(session, source, tick);
        let occurrence_source = match source.schedule.occurrence_source(source.config) {
            Ok(occurrence_source) => occurrence_source,
            Err(error) => {
                tracing::warn!(
                    %session,
                    source_type = %source.source_type,
                    %error,
                    "a scheduled trigger source names no schedule; it never ticks"
                );
                return Ok(());
            }
        };
        let operation = cx
            .scoped(AdmittedScope::runtime_operation(key.clone()))
            .map_err(TurnError::Runtime)?;
        let request = TriggerOccurrenceRequest::new(
            source.source_type,
            source.source_key,
            source.schedule.payload(tick),
            key,
        )
        .with_source(occurrence_source)
        .for_session(session.clone());
        match self.router.emit(request, &operation).await {
            Ok(_) => Ok(()),
            Err(error) if error.is_terminal() => {
                tracing::warn!(
                    %session,
                    source_type = %source.source_type,
                    tick,
                    %error,
                    "a scheduled trigger tick was refused; it does not fire"
                );
                Ok(())
            }
            Err(error) => Err(transient(error)),
        }
    }
}

/// One scheduled source a session subscribes to.
struct ScheduledSource<'a> {
    /// What its configuration means in time.
    schedule: &'a dyn TriggerSchedule,
    source_type: &'a str,
    source_key: &'a str,
    config: &'a serde_json::Value,
    /// When the earliest of its enabled subscriptions became enabled: no
    /// tick at or before it fires.
    enabled_since_ms: u64,
}

impl ScheduledSource<'_> {
    /// The tick due at `now_ms`: the last one at or before it, when it came
    /// after the source became enabled.
    fn due_tick(&self, now_ms: u64) -> Result<Option<u64>, crate::TriggerScheduleError> {
        Ok(self
            .schedule
            .last_tick(self.config, now_ms)?
            .filter(|tick| *tick > self.enabled_since_ms))
    }
}

/// The idempotency key of `source`'s tick at `tick` in `session`: one
/// occurrence per session, source and tick.
fn tick_key(session: &SessionId, source: &ScheduledSource<'_>, tick: u64) -> String {
    format!(
        "lash-schedule-tick:{session}:{}:{}:{tick}",
        source.source_type, source.source_key
    )
}

/// A failure that may pass by itself: the next pass retries, and it never
/// counts toward the session's park.
fn transient(error: PluginError) -> TurnError {
    TurnError::Durable(DurableError::Store(StoreFailure {
        kind: StoreFailureKind::Unavailable,
        message: format!("a scheduled trigger tick: {error}"),
    }))
}
