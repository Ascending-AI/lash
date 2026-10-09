//! The exported instrumentation contract. Changes require a release note.
#[cfg(feature = "otel")]
use opentelemetry::trace::SpanKind;
#[cfg(feature = "otel")]
use opentelemetry::{KeyValue, Value};

pub const LASH_INSTRUMENTATION_NAME: &str = "lash";
pub const LASH_INSTRUMENTATION_CONTRACT: &str = "1.0";
pub const GEN_AI_SEMCONV_SNAPSHOT: &str = "b31e9e8ea26ac1c086d3313d474e31d7c3f391ae";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ownership {
    Candidate,
    Transition,
    Live,
    Gauge,
    Physical,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttributeType {
    String,
    Integer,
    Boolean,
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
    ScopeKind => ("lash.scope.kind", String),
    ScopeBoundary => ("lash.scope.boundary", Integer),
    AdmissionOutcome => ("lash.admission.outcome", String),
    LinksOmitted => ("lash.links.omitted", Integer),
    RecordId => ("lash.record.id", String),
    EventType => ("lash.event.type", String),
    ConversationId => ("gen_ai.conversation.id", String),
    RunId => ("lash.run.id", String),
    ProcessId => ("lash.process.id", String),
    LlmCallId => ("lash.llm_call.id", String),
    HostRunId => ("lash.context.run.id", String),
    HttpResponseStatusCode => ("http.response.status_code", Integer),
    SessionId => ("lash.session.id", String),
    TurnId => ("lash.turn.id", String),
    AttemptInvocationId => ("lash.attempt.invocation_id", String),
    OperationName => ("gen_ai.operation.name", String),
    ProviderName => ("gen_ai.provider.name", String),
    RequestModel => ("gen_ai.request.model", String),
    ResponseModel => ("gen_ai.response.model", String),
    ToolName => ("gen_ai.tool.name", String),
    ToolCallId => ("gen_ai.tool.call.id", String),
    InputTokens => ("gen_ai.usage.input_tokens", Integer),
    OutputTokens => ("gen_ai.usage.output_tokens", Integer),
    CacheReadTokens => ("gen_ai.usage.cache_read.input_tokens", Integer),
    CacheWriteTokens => ("gen_ai.usage.cache_write.input_tokens", Integer),
    ReasoningTokens => ("lash.usage.reasoning_output_tokens", Integer),
    ModelAttemptOrdinal => ("lash.model.attempt.ordinal", Integer),
    ModelVariant => ("lash.request.model_variant", String),
    ResponseTextChars => ("lash.response.text_chars", Integer),
    Outcome => ("lash.outcome", String),
    ErrorType => ("error.type", String),
    ContextMetadata => ("lash.context.metadata", String),
    Payload => ("lash.payload.json", String),
    PayloadTruncated => ("lash.payload.truncated", Boolean),
    PayloadsTruncated => ("lash.payload.truncated_fields", Integer),
    ContentOmitted => ("lash.content.omitted", Boolean),
    ToolIntentKind => ("lash.tool_intent.kind", String),
    ToolIntentRefusal => ("lash.tool_intent.refusal_reason", String),
    Provider => ("lash.provider", String),
    ProviderRetryKind => ("lash.provider.retry.kind", String),
    PoolAcquireOutcome => ("lash.store.pool.acquire.outcome", String),
    CommitLabel => ("lash.durable.commit.label", String),
    CommitBudgetOutcome => ("lash.runtime_commit.budget.outcome", String),
    ParkKind => ("lash.parked_work.kind", String),
    ParkReason => ("lash.parked_work.reason", String),
    ObligationKind => ("lash.obligation.kind", String),
    ObligationOutcome => ("lash.obligation.outcome", String),
    RecoveryLease => ("lash.recovery_leader.name", String),
}

#[cfg(feature = "otel")]
#[derive(Clone, Debug)]
pub struct SpanDefinition {
    pub name: &'static str,
    pub kind: SpanKind,
    pub ownership: Ownership,
}
#[cfg(feature = "otel")]
macro_rules! spans {
    ($($id:ident => ($name:literal, $kind:ident, $owner:ident)),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum DomainSpan { $($id),+ }
        impl DomainSpan { pub const fn definition(self) -> SpanDefinition { match self {
            $(Self::$id => SpanDefinition { name: $name, kind: SpanKind::$kind, ownership: Ownership::$owner }),+
        }}}
        pub const SPANS: &[SpanDefinition] = &[$(SpanDefinition { name: $name, kind: SpanKind::$kind, ownership: Ownership::$owner }),+];
    }
}
#[cfg(feature = "otel")]
spans! {
    AdmissionAttempt => ("lash.admission.attempt", Internal, Candidate),
    TurnAdmitted => ("lash.turn.admitted", Internal, Candidate),
    ToolAdmitted => ("lash.tool.admitted", Internal, Candidate),
    IntentAdmitted => ("lash.tool_intent.admitted", Internal, Candidate),
    ProcessAdmitted => ("lash.process.admitted", Internal, Candidate),
    Send => ("lash.send", Producer, Live),
    Turn => ("invoke_agent", Internal, Transition),
    Model => ("chat", Client, Live),
    Tool => ("execute_tool", Internal, Live),
    Intent => ("lash.tool_intent", Internal, Transition),
    Process => ("lash.process", Internal, Transition),
    ExecCode => ("lash.exec_code", Internal, Live),
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
    PoolAcquireWait => ("lash.store.pool.acquire_wait.duration", Histogram, "ms", Physical),
    DurableAcquireWait => ("lash.durable.commit.acquire_wait.duration", Histogram, "us", Physical),
    DurableTransactionDuration => ("lash.durable.commit.transaction.duration", Histogram, "us", Physical),
    DurableSqlStatements => ("lash.durable.commit.sql_statements", Histogram, "", Physical),
    DurableReturnedBytes => ("lash.durable.commit.returned_bytes", Histogram, "By", Physical),
    DurableLockStatementElapsed => ("lash.durable.commit.lock_statement_elapsed", Histogram, "us", Physical),
    DurableGroupCommitMembers => ("lash.durable.commit.group_commit.members", Histogram, "", Physical),
    CommitBudgetedSize => ("lash.runtime_commit.budgeted_size", Histogram, "By", Live),
    Parks => ("lash.parked_work.parks", Counter, "", Transition),
    IntentExecuted => ("lash.tool_intent.executed", Counter, "", Transition),
    IntentRefused => ("lash.tool_intent.refused", Counter, "", Transition),
    ObligationAttempts => ("lash.obligation.attempts", Counter, "", Transition),
    ObligationsStalled => ("lash.obligations.stalled", Gauge, "", Gauge),
    RecoveryLeader => ("lash.recovery_leader", Gauge, "", Physical),
    RecoveryTerm => ("lash.recovery_leader.term", Gauge, "", Physical),
}

/// L6 writes this generated contract into the public reference and checks equality.
#[cfg(feature = "otel")]
pub fn contract_markdown() -> String {
    use std::fmt::Write;
    let mut out = format!(
        "# Lash instrumentation contract\n\nScope `{LASH_INSTRUMENTATION_NAME}`, version `{LASH_INSTRUMENTATION_CONTRACT}`. GenAI snapshot `{GEN_AI_SEMCONV_SNAPSHOT}`. Exported changes require a release note. No schema URL is claimed.\n\n| Span name or prefix | Kind | Ownership |\n|---|---|---|\n"
    );
    for span in SPANS {
        let _ = writeln!(
            out,
            "| `{}` | {:?} | {:?} |",
            span.name, span.kind, span.ownership
        );
    }
    out.push_str("\n| Attribute | Type |\n|---|---|\n");
    for attr in ATTRIBUTES {
        let _ = writeln!(out, "| `{}` | {:?} |", attr.key, attr.value_type);
    }
    out.push_str("\n| Metric | Kind | Unit | Ownership |\n|---|---|---|---|\n");
    for metric in METRICS {
        let _ = writeln!(
            out,
            "| `{}` | {:?} | `{}` | {:?} |",
            metric.name, metric.kind, metric.unit, metric.ownership
        );
    }
    out
}

/// Recommended explicit boundaries for a Lash latency instrument, in its
/// declared unit. Non-latency instruments return `None`.
///
/// Instruments supply these as default hints. Hosts can use this helper in
/// their provider's views, or select a different aggregation there. The
/// millisecond preset has dense subsecond buckets and reaches 60 seconds;
/// microsecond instruments receive the same time boundaries scaled by 1000.
pub fn recommended_latency_histogram_boundaries(name: &str) -> Option<Vec<f64>> {
    let metric = METRICS.iter().find(|metric| metric.name == name)?;
    if metric.kind != MetricKind::Histogram {
        return None;
    }
    let scale = match metric.unit {
        "ms" => 1.0,
        "us" => 1000.0,
        _ => return None,
    };
    Some(
        [
            0.0, 1.0, 2.0, 5.0, 10.0, 25.0, 50.0, 75.0, 100.0, 250.0, 500.0, 750.0, 1000.0, 2500.0,
            5000.0, 10_000.0, 15_000.0, 30_000.0, 60_000.0,
        ]
        .into_iter()
        .map(|bound| bound * scale)
        .collect(),
    )
}

/// The scope shared by tracing and metric providers. Source identity is
/// separate from the instrumentation contract; the host owns `service.version`
/// on its providers' shared resource. A build revision is exported only when
/// supplied as a deterministic compile-time input.
#[cfg(feature = "otel")]
pub fn instrumentation_scope() -> opentelemetry::InstrumentationScope {
    let mut attributes = vec![KeyValue::new("lash.version", env!("CARGO_PKG_VERSION"))];
    if let Some(revision) = option_env!("LASH_BUILD_REVISION").filter(|value| !value.is_empty()) {
        attributes.push(KeyValue::new("lash.build.revision", revision));
    }
    opentelemetry::InstrumentationScope::builder(LASH_INSTRUMENTATION_NAME)
        .with_version(LASH_INSTRUMENTATION_CONTRACT)
        .with_attributes(attributes)
        .build()
}
