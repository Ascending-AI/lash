//! The `cron.Schedule` trigger source's schedule (FIG-5348).
//!
//! The workbench declares `cron.Schedule({ expr, tz? })` to the model and
//! registers this schedule for it on its core, so lash fires each tick of a
//! registered schedule on the durable substrate, from the session that owns
//! the registration. A tick's occurrence carries `cron.Tick { fired_at }`,
//! the tick's own instant in RFC 3339: a redelivered tick names the same
//! instant.

use chrono::{DateTime, TimeZone as _, Utc};
use chrono_tz::Tz;
use croner::Cron;
use croner::parser::{CronParser, Seconds};
use lash::triggers::{TriggerSchedule, TriggerScheduleError};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::CRON_SCHEDULE_SOURCE_TYPE;

/// The schedule of `cron.Schedule` sources: a cron expression, with an
/// optional leading seconds field, read in the source's `tz` (UTC when it
/// has none).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct CronSchedule;

/// What a `cron.Schedule(...)` value configures: the source type's declared
/// contract (`cron_schedule_config_type`).
#[derive(Debug, Deserialize)]
struct CronScheduleSource {
    expr: String,
    #[serde(default)]
    tz: Option<String>,
}

impl CronSchedule {
    /// The configuration a registration's stored source, the `cron.Schedule`
    /// host descriptor, carries.
    fn config(source: &Value) -> Result<CronScheduleSource, TriggerScheduleError> {
        let descriptor = lash::rlm::lang::HostDescriptor::decode(source)
            .map_err(|error| TriggerScheduleError::new(error.to_string()))?;
        if descriptor.source_type != CRON_SCHEDULE_SOURCE_TYPE {
            return Err(TriggerScheduleError::new(format!(
                "a `{}` descriptor is no `{CRON_SCHEDULE_SOURCE_TYPE}`",
                descriptor.source_type
            )));
        }
        descriptor
            .decode_as(&crate::workbench_lashlang_resources())
            .map_err(|error| TriggerScheduleError::new(error.to_string()))
    }

    /// The expression and time zone `source` configures.
    fn parse(source: &Value) -> Result<(Cron, Tz), TriggerScheduleError> {
        let CronScheduleSource { expr, tz } = Self::config(source)?;
        let tz = match tz.as_deref().map(str::trim) {
            None | Some("") => Tz::UTC,
            Some(tz) => tz
                .parse::<Tz>()
                .map_err(|error| TriggerScheduleError::new(format!("invalid `tz`: {error}")))?,
        };
        let cron = CronParser::builder()
            .seconds(Seconds::Optional)
            .build()
            .parse(&expr)
            .map_err(|error| {
                TriggerScheduleError::new(format!("invalid cron expression `{expr}`: {error}"))
            })?;
        Ok((cron, tz))
    }
}

/// `ms` in `tz`, truncated to its second: cron ticks fall on whole seconds.
fn second_of(ms: u64, tz: Tz) -> Result<DateTime<Tz>, TriggerScheduleError> {
    let seconds = i64::try_from(ms / 1000)
        .map_err(|_| TriggerScheduleError::new(format!("instant {ms} is out of range")))?;
    Utc.timestamp_opt(seconds, 0)
        .single()
        .map(|instant| instant.with_timezone(&tz))
        .ok_or_else(|| TriggerScheduleError::new(format!("instant {ms} is out of range")))
}

/// `instant` as epoch milliseconds, `None` before the epoch.
fn epoch_ms(instant: DateTime<Tz>) -> Option<u64> {
    u64::try_from(instant.timestamp_millis()).ok()
}

impl TriggerSchedule for CronSchedule {
    /// `{ expr, tz? }`: a schedule registered without a time zone names
    /// none, so the occurrence omits the key rather than sending `null`.
    fn occurrence_source(&self, source: &Value) -> Result<Value, TriggerScheduleError> {
        let CronScheduleSource { expr, tz } = Self::config(source)?;
        let mut occurrence = serde_json::Map::new();
        occurrence.insert("expr".to_owned(), json!(expr));
        if let Some(tz) = tz {
            occurrence.insert("tz".to_owned(), json!(tz));
        }
        Ok(Value::Object(occurrence))
    }

    fn next_tick(
        &self,
        source: &Value,
        after_ms: u64,
    ) -> Result<Option<u64>, TriggerScheduleError> {
        let (cron, tz) = Self::parse(source)?;
        // The first tick after the second `after_ms` falls in is after it.
        Ok(cron
            .find_next_occurrence(&second_of(after_ms, tz)?, false)
            .ok()
            .and_then(epoch_ms))
    }

    fn last_tick(&self, source: &Value, at_ms: u64) -> Result<Option<u64>, TriggerScheduleError> {
        let (cron, tz) = Self::parse(source)?;
        Ok(cron
            .find_previous_occurrence(&second_of(at_ms, tz)?, true)
            .ok()
            .and_then(epoch_ms))
    }

    fn payload(&self, tick_ms: u64) -> Value {
        let fired_at = i64::try_from(tick_ms)
            .ok()
            .and_then(DateTime::<Utc>::from_timestamp_millis)
            .map(|instant| instant.to_rfc3339())
            .unwrap_or_default();
        json!({ "fired_at": fired_at })
    }
}
