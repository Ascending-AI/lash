//! The `cron.Schedule` trigger source's timer (FIG-5394).
//!
//! Lash dispatches no trigger occurrence on a host's behalf: the host owns
//! its sources. The workbench declares `cron.Schedule({ expr, tz? })` to the
//! model and ticks it itself. Each workbench process runs one timer, and
//! every pass of it:
//!
//! - reads every enabled `cron.Schedule` registration, so a disable or a
//!   delete (a session's close deletes its registrations) stops the ticks at
//!   the next pass, and a re-enable starts again from the enable;
//! - emits, for each session's source, the latest tick at or before now that
//!   the timer has not passed yet, through the host's trigger emit, scoped to
//!   that session. On boot that is the latest tick missed since the
//!   registration last changed, once.
//!
//! The occurrence's idempotency key names the session, the source and the
//! tick instant, so a second workbench over the same store, a restart or a
//! retried emission lands each tick once. A tick's occurrence carries
//! `cron.Tick { fired_at }`, the tick's own instant in RFC 3339.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeZone as _, Utc};
use chrono_tz::Tz;
use croner::Cron;
use croner::parser::{CronParser, Seconds};
use lash::SessionId;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{AppState, CRON_SCHEDULE_SOURCE_TYPE};

/// How often the timer reads the registrations: the finest schedule ticks
/// every second.
const POLL: Duration = Duration::from_millis(250);

/// What a `cron.Schedule(...)` value configures: the source type's declared
/// contract (`cron_schedule_config_type`).
#[derive(Debug, Deserialize)]
struct CronScheduleSource {
    expr: String,
    #[serde(default)]
    tz: Option<String>,
}

/// The configuration a registration's stored source, the `cron.Schedule`
/// host descriptor, carries.
fn config(source: &Value) -> Result<CronScheduleSource, String> {
    let descriptor =
        lash::rlm::lang::HostDescriptor::decode(source).map_err(|error| error.to_string())?;
    if descriptor.source_type != CRON_SCHEDULE_SOURCE_TYPE {
        return Err(format!(
            "a `{}` descriptor is no `{CRON_SCHEDULE_SOURCE_TYPE}`",
            descriptor.source_type
        ));
    }
    descriptor
        .decode_as(&crate::workbench_lashlang_resources())
        .map_err(|error| error.to_string())
}

/// The expression and time zone `source` configures: a cron expression, with
/// an optional leading seconds field, read in the source's `tz` (UTC when it
/// has none).
fn parse(source: &Value) -> Result<(Cron, Tz), String> {
    let CronScheduleSource { expr, tz } = config(source)?;
    let tz = match tz.as_deref().map(str::trim) {
        None | Some("") => Tz::UTC,
        Some(tz) => tz
            .parse::<Tz>()
            .map_err(|error| format!("invalid `tz`: {error}"))?,
    };
    let cron = CronParser::builder()
        .seconds(Seconds::Optional)
        .build()
        .parse(&expr)
        .map_err(|error| format!("invalid cron expression `{expr}`: {error}"))?;
    Ok((cron, tz))
}

/// The source a tick's occurrence names, `{ expr, tz? }`, as the source
/// type's contract declares it: a schedule registered without a time zone
/// names none, so the occurrence omits the key rather than sending `null`.
fn occurrence_source(source: &Value) -> Result<Value, String> {
    let CronScheduleSource { expr, tz } = config(source)?;
    let mut occurrence = serde_json::Map::new();
    occurrence.insert("expr".to_owned(), json!(expr));
    if let Some(tz) = tz {
        occurrence.insert("tz".to_owned(), json!(tz));
    }
    Ok(Value::Object(occurrence))
}

/// The last tick of the schedule `source` configures at or before `at_ms`.
fn last_tick(source: &Value, at_ms: u64) -> Result<Option<u64>, String> {
    let (cron, tz) = parse(source)?;
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

/// The `cron.Tick` payload of the tick at `tick_ms`.
fn payload(tick_ms: u64) -> Value {
    let fired_at = i64::try_from(tick_ms)
        .ok()
        .and_then(DateTime::<Utc>::from_timestamp_millis)
        .map(|instant| instant.to_rfc3339())
        .unwrap_or_default();
    json!({ "fired_at": fired_at })
}

/// The idempotency key of `source_key`'s tick at `tick_ms` in `session`: one
/// occurrence per session, source and tick, whichever workbench emits it.
fn tick_key(session: &SessionId, source_key: &str, tick_ms: u64) -> String {
    format!("workbench-cron-tick:{session}:{source_key}:{tick_ms}")
}

/// One session's enabled `cron.Schedule` source.
struct Source {
    /// The registration's stored source: its schedule.
    config: Value,
    /// When the earliest of the session's registrations to it last changed:
    /// no tick at or before it fires on a timer that has not seen it yet.
    since_ms: u64,
}

/// The workbench's `cron.Schedule` timer; [`Self::stop`] ends it.
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

    /// Tick every enabled registration `state` serves until [`Self::stop`].
    pub(crate) fn start(&self, state: AppState) {
        let clock = Arc::clone(&self.clock);
        let mut stop = self.stop.subscribe();
        tokio::spawn(async move {
            // The instant up to which each session's source was ticked.
            let mut passed = BTreeMap::new();
            loop {
                pass(&state, clock.as_ref(), &mut passed).await;
                tokio::select! {
                    biased;
                    _ = stop.changed() => break,
                    () = clock.sleep(POLL) => {}
                }
            }
        });
    }

    /// Stop ticking: no tick is emitted after the pass in flight.
    pub(crate) fn stop(&self) {
        self.stop.send_replace(true);
    }
}

/// One pass: emit the due tick of every enabled `cron.Schedule` source, and
/// note in `passed` the instant each was ticked up to.
async fn pass(
    state: &AppState,
    clock: &dyn lash::runtime::Clock,
    passed: &mut BTreeMap<(SessionId, String), u64>,
) {
    let now_ms = u64::try_from(clock.timestamp_datetime().timestamp_millis()).unwrap_or_default();
    let filter = lash::triggers::TriggerSubscriptionFilter {
        source_type: Some(CRON_SCHEDULE_SOURCE_TYPE.to_owned()),
        enabled: Some(true),
        ..Default::default()
    };
    let records = match state.trigger_store.list_subscriptions(filter).await {
        Ok(records) => records,
        Err(error) => {
            eprintln!("agent-workbench cron: the registrations did not read: {error}");
            return;
        }
    };
    let mut sources: BTreeMap<(SessionId, String), Source> = BTreeMap::new();
    for record in records {
        // A registration only a session owns ticks: its occurrence is
        // delivered within that session.
        let Some(session) = record.owner_scope.session_id() else {
            continue;
        };
        if !record.lifecycle.enabled() {
            continue;
        }
        sources
            .entry((session.clone(), record.source_key.clone()))
            .and_modify(|source| source.since_ms = source.since_ms.min(record.updated_at_ms))
            .or_insert(Source {
                config: record.source,
                since_ms: record.updated_at_ms,
            });
    }
    // A source no longer enabled is forgotten: a re-enable starts from the
    // enable.
    passed.retain(|key, _| sources.contains_key(key));
    for (key, source) in sources {
        let (session, source_key) = &key;
        let floor = passed.get(&key).copied().unwrap_or(source.since_ms);
        let tick = match last_tick(&source.config, now_ms) {
            Ok(tick) => tick.filter(|tick| *tick > floor),
            Err(error) => {
                eprintln!(
                    "agent-workbench cron: session {session} source {source_key} names no schedule: {error}"
                );
                None
            }
        };
        if let Some(tick) = tick
            && let Err(error) = emit(state, session, source_key, &source.config, tick).await
        {
            // The next pass emits the source's latest tick again.
            eprintln!(
                "agent-workbench cron: session {session} source {source_key} tick {tick} was not emitted: {error}"
            );
            continue;
        }
        passed.insert(key, now_ms.max(floor));
    }
}

/// Emit `source_key`'s tick at `tick_ms` in `session`.
async fn emit(
    state: &AppState,
    session: &SessionId,
    source_key: &str,
    config: &Value,
    tick_ms: u64,
) -> Result<(), String> {
    let source = occurrence_source(config)?;
    state
        .emit_cron_tick(
            session,
            source_key,
            source,
            payload(tick_ms),
            tick_key(session, source_key, tick_ms),
        )
        .await
        .map(drop)
        .map_err(|error| error.message)
}
