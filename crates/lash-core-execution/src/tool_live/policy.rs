//! Validated publisher and retention bounds, their derived capacity, and
//! typed reasons a setting is refused.
use super::*;

/// Framing charged to every record beyond its fact and call address: the
/// record scope, emission identity (producer epoch and ordinal) and payload
/// tags. It bounds the serialized record from above.
pub const TOOL_LIVE_ENVELOPE_BYTES: usize = 256;
/// A sealed call's slot outlives its last flushed item by this long, so a
/// late publication on a retained handle is still attributed to the call.
pub const TOOL_LIVE_COMPLETION_LINGER: Duration = Duration::from_secs(2);
/// A store write that has not finished by then counts as failed.
pub const TOOL_LIVE_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// Per-owner publisher admission.
///
/// Memory is split three ways, and the three parts never borrow from each
/// other, so `bytes` bounds everything the publisher holds:
///
/// - the final-gauge reserve, `calls × progress_bytes`, holds each admitted
///   call's latest progress. Admitting a call admits its reservation, so a
///   call's final gauge is never refused for want of room;
/// - the log budget, half of the rest, holds logs waiting for a write
///   (at most `logs` of them);
/// - the write budget, the other half, holds the batch a store write has in
///   flight (at most `events` records).
///
/// Every record is charged its fact and call address serialized, plus
/// [`TOOL_LIVE_ENVELOPE_BYTES`]. Defaults: 64 records and 16 logs per 100 ms
/// interval, 512 KiB, 64 KiB per fact, 1 KiB per progress record and 128
/// calls (a 128 KiB reserve and 192 KiB each for waiting logs and the write).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToolLivePolicy {
    pub(super) events: usize,
    pub(super) logs: usize,
    pub(super) bytes: usize,
    pub(super) fact_bytes: usize,
    pub(super) progress_bytes: usize,
    pub(super) calls: usize,
    pub(super) interval: Duration,
}
impl Default for ToolLivePolicy {
    fn default() -> Self {
        Self {
            events: 64,
            logs: 16,
            bytes: 524288,
            fact_bytes: 65536,
            progress_bytes: 1024,
            calls: 128,
            interval: Duration::from_millis(100),
        }
    }
}

/// A policy or retention field.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolLivePolicyField {
    Events,
    Logs,
    Bytes,
    FactBytes,
    ProgressBytes,
    Calls,
    Interval,
    RetentionEvents,
    RetentionBytes,
    RetentionAge,
    RetentionOwners,
}

/// A supplied or bounding value: a count of records, calls or bytes, or a
/// duration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ToolLivePolicyValue {
    Count(usize),
    Duration(Duration),
}
impl std::fmt::Display for ToolLivePolicyValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Count(count) => write!(f, "{count}"),
            Self::Duration(duration) => write!(f, "{duration:?}"),
        }
    }
}

/// A relationship between fields that a policy must keep.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolLivePolicyRelation {
    /// `logs < events`: every interval leaves room for progress.
    LogsBelowEvents,
    /// `calls × progress_bytes ≤ bytes / 2`: the final-gauge reserve.
    ReserveWithinHalfBytes,
    /// `fact_bytes + envelope ≤ (bytes − reserve) / 2`: one largest fact fits
    /// the log budget and the write budget.
    FactWithinLogBudget,
    /// `progress_bytes ≤ fact_bytes`.
    ProgressWithinFact,
}

/// What the supplied value violated.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolLivePolicyBound {
    /// The field's own inclusive range.
    Range {
        min: ToolLivePolicyValue,
        max: ToolLivePolicyValue,
    },
    /// A relationship with other fields; `limit` is what it allowed here.
    Relation {
        relation: ToolLivePolicyRelation,
        limit: ToolLivePolicyValue,
    },
}
impl std::fmt::Display for ToolLivePolicyBound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Range { min, max } => write!(f, "range {min}..={max}"),
            Self::Relation { relation, limit } => write!(f, "{relation:?} (limit {limit})"),
        }
    }
}

/// A refused policy or retention setting: the field, the value supplied for
/// it, and the bound or relationship it violated.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("tool-live {field:?} = {value} violates {bound}")]
pub struct ToolLivePolicyError {
    pub field: ToolLivePolicyField,
    pub value: ToolLivePolicyValue,
    pub bound: ToolLivePolicyBound,
}

fn count_in(
    field: ToolLivePolicyField,
    value: usize,
    range: std::ops::RangeInclusive<usize>,
) -> Result<(), ToolLivePolicyError> {
    if range.contains(&value) {
        return Ok(());
    }
    Err(ToolLivePolicyError {
        field,
        value: ToolLivePolicyValue::Count(value),
        bound: ToolLivePolicyBound::Range {
            min: ToolLivePolicyValue::Count(*range.start()),
            max: ToolLivePolicyValue::Count(*range.end()),
        },
    })
}
fn duration_in(
    field: ToolLivePolicyField,
    value: Duration,
    range: std::ops::RangeInclusive<Duration>,
) -> Result<(), ToolLivePolicyError> {
    if range.contains(&value) {
        return Ok(());
    }
    Err(ToolLivePolicyError {
        field,
        value: ToolLivePolicyValue::Duration(value),
        bound: ToolLivePolicyBound::Range {
            min: ToolLivePolicyValue::Duration(*range.start()),
            max: ToolLivePolicyValue::Duration(*range.end()),
        },
    })
}
fn relation(
    field: ToolLivePolicyField,
    value: usize,
    relation: ToolLivePolicyRelation,
    limit: usize,
) -> Result<(), ToolLivePolicyError> {
    if value <= limit {
        return Ok(());
    }
    Err(ToolLivePolicyError {
        field,
        value: ToolLivePolicyValue::Count(value),
        bound: ToolLivePolicyBound::Relation {
            relation,
            limit: ToolLivePolicyValue::Count(limit),
        },
    })
}

impl ToolLivePolicy {
    /// Validates every field's range, then the relationships between them.
    /// Ranges: events 2..=4096, logs 0..=1024, bytes 4 KiB..=16 MiB, fact
    /// bytes 1 KiB..=1 MiB, progress bytes 512..=64 KiB, calls 1..=4096 and
    /// interval 10 ms..=60 s.
    pub fn new(
        events: usize,
        logs: usize,
        bytes: usize,
        fact_bytes: usize,
        progress_bytes: usize,
        calls: usize,
        interval: Duration,
    ) -> Result<Self, ToolLivePolicyError> {
        use ToolLivePolicyField as Field;
        count_in(Field::Events, events, 2..=4096)?;
        count_in(Field::Logs, logs, 0..=1024)?;
        count_in(Field::Bytes, bytes, 4096..=16777216)?;
        count_in(Field::FactBytes, fact_bytes, 1024..=1048576)?;
        count_in(Field::ProgressBytes, progress_bytes, 512..=65536)?;
        count_in(Field::Calls, calls, 1..=4096)?;
        duration_in(
            Field::Interval,
            interval,
            Duration::from_millis(10)..=Duration::from_secs(60),
        )?;
        relation(
            Field::Logs,
            logs,
            ToolLivePolicyRelation::LogsBelowEvents,
            events - 1,
        )?;
        relation(
            Field::ProgressBytes,
            progress_bytes,
            ToolLivePolicyRelation::ProgressWithinFact,
            fact_bytes,
        )?;
        relation(
            Field::Calls,
            calls,
            ToolLivePolicyRelation::ReserveWithinHalfBytes,
            bytes / 2 / progress_bytes,
        )?;
        let policy = Self {
            events,
            logs,
            bytes,
            fact_bytes,
            progress_bytes,
            calls,
            interval,
        };
        relation(
            Field::FactBytes,
            fact_bytes,
            ToolLivePolicyRelation::FactWithinLogBudget,
            policy.log_bytes().saturating_sub(TOOL_LIVE_ENVELOPE_BYTES),
        )?;
        Ok(policy)
    }
    /// Records one store write carries per interval, of every kind.
    pub fn events(&self) -> usize {
        self.events
    }
    /// Logs admitted to wait for a write, and emitted per interval.
    pub fn logs(&self) -> usize {
        self.logs
    }
    /// Everything the owner's publisher holds, including the write in flight.
    pub fn bytes(&self) -> usize {
        self.bytes
    }
    /// Largest serialized fact; a larger one is `Oversized`.
    pub fn fact_bytes(&self) -> usize {
        self.fact_bytes
    }
    /// Largest charged progress record, the per-call final-gauge reservation.
    pub fn progress_bytes(&self) -> usize {
        self.progress_bytes
    }
    /// Calls admitted at once; each holds one reservation.
    pub fn calls(&self) -> usize {
        self.calls
    }
    pub fn interval(&self) -> Duration {
        self.interval
    }
    /// `calls × progress_bytes`.
    pub fn reserve_bytes(&self) -> usize {
        self.calls * self.progress_bytes
    }
    /// Bytes of logs waiting for a write.
    pub fn log_bytes(&self) -> usize {
        (self.bytes - self.reserve_bytes()) / 2
    }
    /// Bytes of one write in flight.
    pub fn write_bytes(&self) -> usize {
        self.bytes - self.reserve_bytes() - self.log_bytes()
    }
    /// Every configured and derived bound, for operator diagnostics.
    pub fn snapshot(&self) -> ToolLivePolicySnapshot {
        ToolLivePolicySnapshot {
            events: self.events,
            logs: self.logs,
            bytes: self.bytes,
            fact_bytes: self.fact_bytes,
            progress_bytes: self.progress_bytes,
            calls: self.calls,
            interval: self.interval,
            reserve_bytes: self.reserve_bytes(),
            log_bytes: self.log_bytes(),
            write_bytes: self.write_bytes(),
            envelope_bytes: TOOL_LIVE_ENVELOPE_BYTES,
            completion_linger: TOOL_LIVE_COMPLETION_LINGER,
            write_timeout: TOOL_LIVE_WRITE_TIMEOUT,
        }
    }
}

/// A readable copy of a policy and the capacities it implies.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolLivePolicySnapshot {
    pub events: usize,
    pub logs: usize,
    pub bytes: usize,
    pub fact_bytes: usize,
    pub progress_bytes: usize,
    pub calls: usize,
    pub interval: Duration,
    pub reserve_bytes: usize,
    pub log_bytes: usize,
    pub write_bytes: usize,
    pub envelope_bytes: usize,
    pub completion_linger: Duration,
    pub write_timeout: Duration,
}

/// The tool class's retention does not consume either owner history window.
/// Defaults: 2048 events, 8 MiB and 120 s per owner, 4096 resident owners.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToolLiveRetention {
    pub(super) events: usize,
    pub(super) bytes: usize,
    pub(super) age: Duration,
    pub(super) owners: usize,
}
impl Default for ToolLiveRetention {
    fn default() -> Self {
        Self {
            events: 2048,
            bytes: 8388608,
            age: Duration::from_secs(120),
            owners: 4096,
        }
    }
}
impl ToolLiveRetention {
    pub fn events(&self) -> usize {
        self.events
    }
    pub fn bytes(&self) -> usize {
        self.bytes
    }
    pub fn age(&self) -> Duration {
        self.age
    }
    pub fn owners(&self) -> usize {
        self.owners
    }
    /// Ranges: events 1..=65536, bytes 4 KiB..=64 MiB, age 10 ms..=1 h and
    /// owners 1..=65536.
    pub fn new(
        events: usize,
        bytes: usize,
        age: Duration,
        owners: usize,
    ) -> Result<Self, ToolLivePolicyError> {
        use ToolLivePolicyField as Field;
        count_in(Field::RetentionEvents, events, 1..=65536)?;
        count_in(Field::RetentionBytes, bytes, 4096..=67108864)?;
        duration_in(
            Field::RetentionAge,
            age,
            Duration::from_millis(10)..=Duration::from_secs(3600),
        )?;
        count_in(Field::RetentionOwners, owners, 1..=65536)?;
        Ok(Self {
            events,
            bytes,
            age,
            owners,
        })
    }
}
