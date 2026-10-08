//! The `cron` trigger source's timer.
//!
//! The workbench owns its sources. Each workbench process runs one timer, and
//! every pass of it:
//!
//! - reads every `cron` registration, so a deleted registration (a session's
//!   close deletes its registrations) stops ticking at the next pass;
//! - fires, for each registration, the latest tick at or before now that the
//!   timer has not passed yet. On boot that is the latest tick missed since
//!   the registration was made, once.
//!
//! A tick's occurrence id names the registration and the tick instant, so a
//! restart or a second firing records each tick once; a second workbench over
//! the same store starts each tick's process once under its start key. A tick
//! delivers `{ fired_at }`, the tick's own instant in RFC 3339.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeZone as _, Utc};
use chrono_tz::Tz;
use croner::Cron;
use croner::parser::{CronParser, Seconds};

use crate::AppState;
use crate::host_triggers::{CronTick, TriggerSource};

/// How often the timer reads the registrations: the finest schedule ticks
/// every second.
const POLL: Duration = Duration::from_millis(250);

/// The schedule `expr` describes, read in `tz` (UTC when it has none): a
/// cron expression with an optional leading seconds field.
pub(crate) fn schedule(expr: &str, tz: Option<&str>) -> Result<(Cron, Tz), String> {
    let tz = match tz.map(str::trim) {
        None | Some("") => Tz::UTC,
        Some(tz) => tz
            .parse::<Tz>()
            .map_err(|error| format!("invalid `tz`: {error}"))?,
    };
    let cron = CronParser::builder()
        .seconds(Seconds::Optional)
        .build()
        .parse(expr)
        .map_err(|error| format!("invalid cron expression `{expr}`: {error}"))?;
    Ok((cron, tz))
}

/// The last tick of `expr` in `tz` at or before `at_ms`.
fn last_tick(expr: &str, tz: Option<&str>, at_ms: u64) -> Result<Option<u64>, String> {
    let (cron, tz) = schedule(expr, tz)?;
    let seconds =
        i64::try_from(at_ms / 1000).map_err(|_| format!("instant {at_ms} is out of range"))?;
    // Cron ticks fall on whole seconds.
    let at = Utc
        .timestamp_opt(seconds, 0)
        .single()
        .map(|instant| instant.with_timezone(&tz))
        .ok_or_else(|| format!("instant {at_ms} is out of range"))?;
    Ok(cron
        .find_previous_occurrence(&at, true)
        .ok()
        .and_then(|tick| u64::try_from(tick.timestamp_millis()).ok()))
}

/// The event of the tick at `tick_ms`.
fn tick(tick_ms: u64) -> CronTick {
    CronTick {
        fired_at: i64::try_from(tick_ms)
            .ok()
            .and_then(DateTime::<Utc>::from_timestamp_millis)
            .map(|instant| instant.to_rfc3339())
            .unwrap_or_default(),
    }
}

/// The workbench's cron timer; [`Self::stop`] ends it.
#[derive(Clone)]
pub(crate) struct CronTimer {
    clock: Arc<dyn lash::runtime::Clock>,
    stop: Arc<tokio::sync::watch::Sender<bool>>,
}

impl CronTimer {
    /// A timer reading time from `clock`, the store set's clock; it ticks
    /// once [`Self::start`]ed.
    pub(crate) fn new(clock: Arc<dyn lash::runtime::Clock>) -> Self {
        Self {
            clock,
            stop: Arc::new(tokio::sync::watch::channel(false).0),
        }
    }

    /// Tick every registration `state` serves until [`Self::stop`].
    pub(crate) fn start(&self, state: AppState) {
        let clock = Arc::clone(&self.clock);
        let mut stop = self.stop.subscribe();
        tokio::spawn(async move {
            // The instant up to which each registration was ticked.
            let mut passed = BTreeMap::new();
            loop {
                pass(&state, clock.as_ref(), &mut passed);
                tokio::select! {
                    biased;
                    _ = stop.changed() => break,
                    () = clock.sleep(POLL) => {}
                }
            }
        });
    }

    /// Stop ticking: no tick is fired after the pass in flight.
    pub(crate) fn stop(&self) {
        self.stop.send_replace(true);
    }
}

/// One pass: fire the due tick of every cron registration, and note in
/// `passed` the instant each was ticked up to.
fn pass(state: &AppState, clock: &dyn lash::runtime::Clock, passed: &mut BTreeMap<String, u64>) {
    let now_ms = u64::try_from(clock.timestamp_datetime().timestamp_millis()).unwrap_or_default();
    let subscriptions = match state.host_triggers.subscriptions(None) {
        Ok(subscriptions) => subscriptions,
        Err(error) => {
            eprintln!("agent-workbench cron: the registrations did not read: {error}");
            return;
        }
    };
    let mut live = BTreeMap::new();
    for subscription in subscriptions {
        let TriggerSource::Cron { expr, tz } = subscription.source else {
            continue;
        };
        let since_ms = u64::try_from(subscription.created_at_ms).unwrap_or_default();
        live.insert(subscription.id, (expr, tz, since_ms));
    }
    // A deleted registration is forgotten.
    passed.retain(|id, _| live.contains_key(id));
    for (id, (expr, tz, since_ms)) in live {
        let floor = passed.get(&id).copied().unwrap_or(since_ms);
        let due = match last_tick(&expr, tz.as_deref(), now_ms) {
            Ok(due) => due.filter(|due| *due > floor),
            Err(error) => {
                eprintln!("agent-workbench cron: registration {id} names no schedule: {error}");
                None
            }
        };
        if let Some(due) = due
            && let Err(error) = state.host_triggers.fire_cron_tick(&id, tick(due), due)
        {
            // The next pass fires the registration's latest tick again.
            eprintln!(
                "agent-workbench cron: registration {id} tick {due} was not recorded: {error}"
            );
            continue;
        }
        passed.insert(id, now_ms.max(floor));
    }
}
