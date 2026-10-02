//! The exported instrumentation contract. Changes require a release note.
#[cfg(feature = "otel")]
use opentelemetry::{KeyValue, Value};

pub const LASH_INSTRUMENTATION_NAME: &str = "lash";
pub const LASH_INSTRUMENTATION_CONTRACT: &str = "1.0";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ownership {
    Transition,
    Live,
    Gauge,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttributeType {
    String,
    Integer,
}

#[derive(Clone, Copy, Debug)]
pub struct AttributeDefinition {
    pub key: &'static str,
    pub value_type: AttributeType,
}

macro_rules! attributes {
    ($($id:ident => ($key:literal, $ty:ident)),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum AttributeKey { $($id),+ }
        impl AttributeKey {
            pub const fn definition(self) -> AttributeDefinition { match self {
                $(Self::$id => AttributeDefinition { key: $key, value_type: AttributeType::$ty }),+
            }}
            #[cfg(feature = "otel")]
            pub fn value(self, value: impl Into<Value>) -> KeyValue { KeyValue::new(self.definition().key, value) }
        }
        pub const ATTRIBUTES: &[AttributeDefinition] = &[$(AttributeDefinition { key: $key, value_type: AttributeType::$ty }),+];
    }
}
attributes! {
    ToolIntentKind => ("lash.tool_intent.kind", String),
    ToolIntentRefusal => ("lash.tool_intent.refusal_reason", String),
    Provider => ("lash.provider", String),
    ProviderRetryKind => ("lash.provider.retry.kind", String),
    LaneWaitOutcome => ("lash.session_execution_lane.wait.outcome", String),
    LaneGiveUp => ("lash.session_execution_lane.give_up", String),
    PoolAcquireOutcome => ("lash.postgres.pool.acquire.outcome", String),
    CommitBudgetOutcome => ("lash.runtime_commit.budget.outcome", String),
    ParkKind => ("lash.parked_work.kind", String),
    ParkReason => ("lash.parked_work.reason", String),
    ObligationKind => ("lash.obligation.kind", String),
    ObligationOutcome => ("lash.obligation.outcome", String),
    RecoveryLease => ("lash.recovery_leader.name", String),
    DrainGeneration => ("lash.generation_drain.generation", String),
    DrainKind => ("lash.generation_drain.kind", String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetricKind {
    Counter,
    Histogram,
    Gauge,
}
#[derive(Clone, Copy, Debug)]
pub struct MetricDefinition {
    pub name: &'static str,
    pub kind: MetricKind,
    pub unit: &'static str,
    pub ownership: Ownership,
}
macro_rules! metrics {
    ($($id:ident => ($name:literal, $kind:ident, $unit:literal, $owner:ident)),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum Metric { $($id),+ }
        impl Metric { pub const fn definition(self) -> MetricDefinition { match self {
            $(Self::$id => MetricDefinition { name: $name, kind: MetricKind::$kind, unit: $unit, ownership: Ownership::$owner }),+
        }}}
        pub const METRICS: &[MetricDefinition] = &[$(MetricDefinition { name: $name, kind: MetricKind::$kind, unit: $unit, ownership: Ownership::$owner }),+];
    }
}
metrics! {
    ProviderRetries => ("lash.provider.retries", Counter, "", Live),
    ProviderThrottleWait => ("lash.provider.throttle_wait.duration", Histogram, "ms", Live),
    LaneContentionWait => ("lash.session_execution_lane.contention_wait.duration", Histogram, "ms", Live),
    LaneGiveUps => ("lash.session_execution_lane.give_ups", Counter, "", Live),
    WakeRetries => ("lash.queued_work.wake_retries", Counter, "", Live),
    PoolAcquireWait => ("lash.postgres.pool.acquire_wait.duration", Histogram, "ms", Live),
    CommitBudgetedSize => ("lash.runtime_commit.budgeted_size", Histogram, "By", Live),
    Parks => ("lash.parked_work.parks", Counter, "", Transition),
    ParkCount => ("lash.parked_work.count", Gauge, "", Gauge),
    ParkOldestAge => ("lash.parked_work.oldest_age", Gauge, "ms", Gauge),
    IntentExecuted => ("lash.tool_intent.executed", Counter, "", Transition),
    IntentRefused => ("lash.tool_intent.refused", Counter, "", Transition),
    ObligationAttempts => ("lash.obligation.attempts", Counter, "", Transition),
    ObligationsStalled => ("lash.obligations.stalled", Gauge, "", Gauge),
    RecoveryLeader => ("lash.recovery_leader", Gauge, "", Gauge),
    RecoveryTerm => ("lash.recovery_leader.term", Gauge, "", Gauge),
    DrainWork => ("lash.generation_drain.work", Gauge, "", Gauge),
}

/// The scope used for every instrument created from an injected provider.
#[cfg(feature = "otel")]
pub fn instrumentation_scope() -> opentelemetry::InstrumentationScope {
    opentelemetry::InstrumentationScope::builder(LASH_INSTRUMENTATION_NAME)
        .with_version(LASH_INSTRUMENTATION_CONTRACT)
        .build()
}
