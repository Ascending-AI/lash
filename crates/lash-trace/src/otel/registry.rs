//! The exported instrumentation contract. Changes require a release note.
#[cfg(feature = "otel")]
use opentelemetry::trace::SpanKind;
#[cfg(feature = "otel")]
use opentelemetry::{KeyValue, Value};

pub const LASH_INSTRUMENTATION_NAME: &str = "lash";
pub const LASH_INSTRUMENTATION_CONTRACT: &str = "1.0";
pub const GEN_AI_SEMCONV_SNAPSHOT: &str =
    "open-telemetry/semantic-conventions-genai@07794021bfe46e1c241408b6f55cb4354bd28318";

/// The semantic-convention discriminator for an adapter's wire protocol.
pub(crate) fn gen_ai_provider_name(kind: &str) -> &str {
    match kind {
        "openai" | "codex" | "openai-compatible" => "openai",
        "google_oauth" => "gcp.gen_ai",
        kind => kind,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ownership {
    Transition,
    Live,
    Gauge,
    Physical,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttributeType {
    String,
    StringArray,
    Integer,
    Boolean,
    Double,
}

/// Who may change an attribute lash sets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttributeMutability {
    /// A fact lash derived: ids, times, outcomes, measurements and every
    /// `lash.*` key. Nothing replaces it.
    Provenance,
    /// Lash's best default for a fact the host may know better, which the
    /// host's enricher can replace through [`RefinableKey`].
    Refinable,
}

#[derive(Clone, Copy, Debug)]
pub struct AttributeDefinition {
    pub key: &'static str,
    pub value_type: AttributeType,
    pub mutability: AttributeMutability,
    /// The span kinds lash sets the attribute on. Empty for a metric label.
    pub on: TraceSpanKindSet,
}

use crate::{TraceSpanKind as K, TraceSpanKindSet};

/// Every kind lash exports a span of.
const EVERY: TraceSpanKindSet = TraceSpanKindSet::ALL;
/// No span: a metric label.
const NONE: TraceSpanKindSet = TraceSpanKindSet::EMPTY;
/// Internal domain completions that carry intent correlation.
const DOMAIN: TraceSpanKindSet = TraceSpanKindSet::of(&[K::Process, K::ToolIntent]);
const MODEL: TraceSpanKindSet = TraceSpanKindSet::of(&[K::Model]);
const ATTEMPT: TraceSpanKindSet = TraceSpanKindSet::of(&[K::ModelAttempt]);

macro_rules! attributes {
    (
        provenance: { $($id:ident => ($key:literal, $ty:ident, $on:expr)),+ $(,)? }
        refinable: { $($rid:ident => ($rkey:literal, $rty:ident, $ron:expr)),+ $(,)? }
    ) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum AttributeKey { $($id,)+ $($rid),+ }
        impl AttributeKey {
            pub const fn definition(self) -> AttributeDefinition { match self {
                $(Self::$id => AttributeDefinition {
                    key: $key,
                    value_type: AttributeType::$ty,
                    mutability: AttributeMutability::Provenance,
                    on: $on,
                },)+
                $(Self::$rid => AttributeDefinition {
                    key: $rkey,
                    value_type: AttributeType::$rty,
                    mutability: AttributeMutability::Refinable,
                    on: $ron,
                }),+
            }}
            #[cfg(feature = "otel")]
            pub fn value(self, value: impl Into<Value>) -> KeyValue { KeyValue::new(self.definition().key, value) }
        }
        /// Every registry attribute, in table order.
        pub const ATTRIBUTE_KEYS: &[AttributeKey] = &[$(AttributeKey::$id,)+ $(AttributeKey::$rid),+];
        pub const ATTRIBUTES: &[AttributeDefinition] = &[
            $(AttributeKey::$id.definition(),)+
            $(AttributeKey::$rid.definition()),+
        ];

        /// A semantic default lash sets that the host's enricher can
        /// replace: the registry's refinable attributes.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        #[non_exhaustive]
        pub enum RefinableKey { $($rid),+ }
        impl RefinableKey {
            /// The registry attribute the key replaces.
            pub(crate) const fn attribute(self) -> AttributeKey {
                match self { $(Self::$rid => AttributeKey::$rid),+ }
            }
            pub const fn key(self) -> &'static str {
                self.attribute().definition().key
            }
        }
    }
}
attributes! {
    provenance: {
        ScopeKind => ("lash.scope.kind", String, EVERY),
        ScopeBoundary => ("lash.scope.boundary", Integer, EVERY),
        ProducerOutcome => ("lash.producer.outcome", String, TraceSpanKindSet::of(&[K::Producer])),
        LinksOmitted => ("lash.links.omitted", Integer, EVERY),
        RecordId => ("lash.record.id", String, EVERY),
        EventType => ("lash.event.type", String, EVERY),
        ConversationId => ("gen_ai.conversation.id", String, EVERY),
        RunId => ("lash.run.id", String, EVERY),
        ProcessId => ("lash.process.id", String, EVERY),
        LlmCallId => ("lash.llm_call.id", String, EVERY),
        SendId => ("lash.send.id", String, EVERY),
        HttpResponseStatusCode => ("http.response.status_code", Integer, ATTEMPT),
        SessionId => ("lash.session.id", String, EVERY),
        TurnId => ("lash.turn.id", String, EVERY),
        AttemptInvocationId => ("lash.attempt.invocation_id", String, EVERY),
        OperationName => ("gen_ai.operation.name", String,
            TraceSpanKindSet::of(&[K::Agent, K::Model, K::Tool, K::Cell, K::Workflow])),
        RequestModel => ("gen_ai.request.model", String, MODEL),
        ResponseModel => ("gen_ai.response.model", String, TraceSpanKindSet::of(&[K::Model])),
        ToolName => ("gen_ai.tool.name", String,
            TraceSpanKindSet::of(&[K::Tool, K::Cell, K::Process, K::ToolIntent])),
        ToolCallId => ("gen_ai.tool.call.id", String,
            TraceSpanKindSet::of(&[K::Tool, K::Process, K::ToolIntent])),
        CellId => ("lash.cell.id", String, TraceSpanKindSet::of(&[K::Cell])),
        CellLanguage => ("lash.cell.language", String, TraceSpanKindSet::of(&[K::Cell])),
        CellToolCalls => ("lash.cell.tool_calls", Integer, TraceSpanKindSet::of(&[K::Cell])),
        InputTokens => ("gen_ai.usage.input_tokens", Integer, MODEL),
        OutputTokens => ("gen_ai.usage.output_tokens", Integer, MODEL),
        CacheReadTokens => ("gen_ai.usage.cache_read.input_tokens", Integer, MODEL),
        CacheWriteTokens => ("gen_ai.usage.cache_write.input_tokens", Integer, MODEL),
        ReasoningTokens => ("gen_ai.usage.reasoning.output_tokens", Integer, MODEL),
        ModelAttemptOrdinal => ("lash.model.attempt.ordinal", Integer,
            TraceSpanKindSet::of(&[K::ModelAttempt])),
        ModelAttempts => ("lash.model.attempts", Integer, TraceSpanKindSet::of(&[K::Model])),
        RequestStream => ("gen_ai.request.stream", Boolean, MODEL),
        ReasoningLevel => ("gen_ai.request.reasoning.level", String, MODEL),
        ReasoningDisabled => ("lash.request.reasoning", String, MODEL),
        OutputType => ("gen_ai.output.type", String, MODEL),
        RequestMaxTokens => ("gen_ai.request.max_tokens", Integer, MODEL),
        RequestSeed => ("gen_ai.request.seed", Integer, MODEL),
        RequestTemperature => ("gen_ai.request.temperature", Double, MODEL),
        RequestStopSequences => ("gen_ai.request.stop_sequences", StringArray, MODEL),
        ResponseId => ("gen_ai.response.id", String, MODEL),
        ResponseFinishReasons => ("gen_ai.response.finish_reasons", StringArray, MODEL),
        UsageOverflow => ("lash.usage.overflow", Boolean, MODEL),
        AttemptInputTokens => ("lash.usage.input_tokens", Integer, ATTEMPT),
        AttemptOutputTokens => ("lash.usage.output_tokens", Integer, ATTEMPT),
        AttemptCacheReadTokens => ("lash.usage.cache_read_tokens", Integer, ATTEMPT),
        AttemptCacheWriteTokens => ("lash.usage.cache_write_tokens", Integer, ATTEMPT),
        AttemptReasoningTokens => ("lash.usage.reasoning_tokens", Integer, ATTEMPT),
        ProviderRequestId => ("lash.provider.request_id", String, ATTEMPT),
        ProcessKind => ("lash.process.kind", String, TraceSpanKindSet::of(&[K::Process, K::Workflow])),
        ProcessLabel => ("lash.process.label", String, TraceSpanKindSet::of(&[K::Process, K::Workflow])),
        WorkflowName => ("gen_ai.workflow.name", String, TraceSpanKindSet::of(&[K::Workflow])),
        ToolId => ("lash.tool.id", String, TraceSpanKindSet::of(&[K::Tool])),
        ToolAttempts => ("lash.tool.attempts", Integer, TraceSpanKindSet::of(&[K::Tool])),
        ProviderCallId => ("lash.tool.provider_call_id", String, TraceSpanKindSet::of(&[K::Tool])),
        Outcome => ("lash.outcome", String, TraceSpanKindSet::of(&[
            K::Agent, K::Model, K::ModelAttempt, K::Tool, K::Cell, K::Process, K::Workflow,
            K::WorkflowNode, K::ToolIntent,
        ])),
        TurnInputs => ("lash.turn.inputs", Integer, TraceSpanKindSet::of(&[K::Agent])),
        CancelOrigin => ("lash.cancel.origin", String, TraceSpanKindSet::of(&[K::Agent])),
        SystemInstructions => ("gen_ai.system_instructions", String, TraceSpanKindSet::of(&[K::Model])),
        InputMessages => ("gen_ai.input.messages", String, TraceSpanKindSet::of(&[K::Model, K::Agent])),
        OutputMessages => ("gen_ai.output.messages", String, TraceSpanKindSet::of(&[K::Model, K::Agent])),
        ToolDefinitions => ("gen_ai.tool.definitions", String, TraceSpanKindSet::of(&[K::Model])),
        ToolArguments => ("gen_ai.tool.call.arguments", String, TraceSpanKindSet::of(&[K::Tool,K::Cell])),
        ToolResult => ("gen_ai.tool.call.result", String, TraceSpanKindSet::of(&[K::Tool,K::Cell])),
        ToolArgumentsWrapped => ("lash.tool.arguments.wrapped", Boolean, TraceSpanKindSet::of(&[K::Tool])),
        ToolResultWrapped => ("lash.tool.result.wrapped", Boolean, TraceSpanKindSet::of(&[K::Tool])),
        ContentTruncated => ("lash.content.truncated", StringArray, EVERY),
        ContentOversize => ("lash.content.oversize", StringArray, EVERY),
        ContentDroppedMessages => ("lash.content.dropped_messages", Integer, EVERY),
        ToolIntentKind => ("lash.tool_intent.kind", String, DOMAIN),
        ToolIntentRefusal => ("lash.tool_intent.refusal_reason", String, DOMAIN),
        TraceOmitted => ("lash.trace.omitted", StringArray, EVERY),
        TraceOmittedCount => ("lash.trace.omitted.count", Integer, EVERY),
        Provider => ("lash.provider", String, NONE),
        ProviderRetryKind => ("lash.provider.retry.kind", String, NONE),
        PoolAcquireOutcome => ("lash.store.pool.acquire.outcome", String, NONE),
        CommitLabel => ("lash.durable.commit.label", String, NONE),
        CommitBudgetOutcome => ("lash.runtime_commit.budget.outcome", String, NONE),
        ParkKind => ("lash.parked_work.kind", String, NONE),
        ParkReason => ("lash.parked_work.reason", String, NONE),
        ObligationKind => ("lash.obligation.kind", String, NONE),
        ObligationOutcome => ("lash.obligation.outcome", String, NONE),
        RecoveryLease => ("lash.recovery_leader.name", String, NONE),
    }
    refinable: {
        ToolType => ("gen_ai.tool.type", String, TraceSpanKindSet::of(&[K::Tool, K::Cell])),
        GenAiProviderName => ("gen_ai.provider.name", String, MODEL),
        ErrorType => ("error.type", String, EVERY),
    }
}

impl AttributeKey {
    /// The registry attribute named `key`, if lash declares one.
    pub fn of_key(key: &str) -> Option<Self> {
        ATTRIBUTE_KEYS
            .iter()
            .copied()
            .find(|attribute| attribute.definition().key == key)
    }
}

/// The number of registered attributes lash may set on a span of `kind`:
/// the reserve every host or plugin contribution to that span is admitted
/// after. Generated from the registry's `on` column, never hand-counted.
pub const fn attribute_reserve(kind: crate::TraceSpanKind) -> u32 {
    let mut count = 0_u32;
    let mut index = 0;
    while index < ATTRIBUTES.len() {
        if ATTRIBUTES[index].on.contains(kind) {
            count += 1;
        }
        index += 1;
    }
    count
}

#[cfg(feature = "otel")]
#[derive(Clone, Debug)]
pub struct SpanDefinition {
    pub name: &'static str,
    pub kind: SpanKind,
    pub ownership: Ownership,
    pub semconv_type: Option<&'static str>,
}
#[cfg(feature = "otel")]
macro_rules! spans {
    ($($id:ident => ($name:literal, $kind:ident, $owner:ident, $semconv:expr)),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum DomainSpan { $($id),+ }
        impl DomainSpan { pub const fn definition(self) -> SpanDefinition { match self {
            $(Self::$id => SpanDefinition { name: $name, kind: SpanKind::$kind, ownership: Ownership::$owner, semconv_type: $semconv }),+
        }}}
        pub const SPANS: &[SpanDefinition] = &[$(SpanDefinition { name: $name, kind: SpanKind::$kind, ownership: Ownership::$owner, semconv_type: $semconv }),+];
    }
}
#[cfg(feature = "otel")]
spans! {
    Send => ("lash.send", Producer, Live, None),
    SendBatch => ("lash.send.batch", Producer, Live, None),
    ProcessStart => ("lash.process.start", Producer, Live, None),
    IntentSubmit => ("lash.tool_intent.submit", Producer, Live, None),
    PluginInvoke => ("lash.plugin.invoke", Producer, Live, None),
    Turn => ("invoke_agent", Internal, Transition, Some("gen_ai.invoke_agent.internal")),
    Model => ("chat", Client, Live, Some("gen_ai.client.inference")),
    ModelAttempt => ("lash.model.attempt", Client, Live, None),
    Tool => ("execute_tool", Internal, Transition, Some("gen_ai.execute_tool.internal")),
    Intent => ("lash.tool_intent", Internal, Transition, None),
    Process => ("lash.process", Internal, Transition, None),
    Workflow => ("invoke_workflow", Internal, Transition, Some("gen_ai.invoke_workflow.internal")),
    ExecCode => ("execute_tool", Internal, Transition, Some("gen_ai.execute_tool.internal")),
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
    InferenceDuration => ("gen_ai.client.inference.duration", Histogram, "s", Live),
    FirstChunk => ("gen_ai.client.inference.time_to_first_chunk", Histogram, "s", Live),
    InputTokens => ("gen_ai.client.inference.usage.input_tokens", Counter, "{token}", Live),
    OutputTokens => ("gen_ai.client.inference.usage.output_tokens", Counter, "{token}", Live),
    CacheReadTokens => ("gen_ai.client.inference.usage.cache_read.input_tokens", Counter, "{token}", Live),
    CacheWriteTokens => ("gen_ai.client.inference.usage.cache_write.input_tokens", Counter, "{token}", Live),
    ReasoningTokens => ("gen_ai.client.inference.usage.reasoning.output_tokens", Counter, "{token}", Live),
    OperationInputTokens => ("gen_ai.client.inference.operation.input_tokens", Histogram, "{token}", Live),
    OperationOutputTokens => ("gen_ai.client.inference.operation.output_tokens", Histogram, "{token}", Live),
    AgentDuration => ("gen_ai.invoke_agent.duration", Histogram, "s", Transition),
    AgentInferenceCalls => ("gen_ai.invoke_agent.inference_calls", Histogram, "{inference_call}", Transition),
    AgentToolCalls => ("gen_ai.invoke_agent.tool_calls", Histogram, "{tool_call}", Transition),
    ToolDuration => ("gen_ai.execute_tool.duration", Histogram, "s", Transition),
    WorkflowDuration => ("gen_ai.invoke_workflow.duration", Histogram, "s", Transition),
    TraceOmissions => ("lash.trace.omissions", Counter, "", Live),
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
        "# Lash instrumentation contract\n\nScope `{LASH_INSTRUMENTATION_NAME}`, version `{LASH_INSTRUMENTATION_CONTRACT}`. GenAI snapshot `{GEN_AI_SEMCONV_SNAPSHOT}`. Exported changes require a release note. No schema URL is claimed.\n\n`TurnCompleted.output` carries the settled assistant answer under `OutputMessages` consent; its closer alone supplies `gen_ai.output.messages` on `invoke_agent`. Content is omitted by default and passes through the content hook and bounds. Model decorators read the logical chat span from `LlmRequestScope.trace_carrier` (absent when no model span is built). Content hooks identify tool records through `ContentView.tool_name`.\n\n| Span name or prefix | Kind | Ownership | Semconv type |\n|---|---|---|---|\n"
    );
    for span in SPANS {
        let _ = writeln!(
            out,
            "| `{}` | {:?} | {:?} | {} |",
            span.name,
            span.kind,
            span.ownership,
            span.semconv_type.unwrap_or("—")
        );
    }
    out.push_str("\n| Attribute | Type | Mutability | Set on |\n|---|---|---|---|\n");
    for attr in ATTRIBUTES {
        let on = attr
            .on
            .iter()
            .map(crate::TraceSpanKind::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(
            out,
            "| `{}` | {:?} | {:?} | {} |",
            attr.key,
            attr.value_type,
            attr.mutability,
            if on.is_empty() { "metrics only" } else { &on }
        );
    }
    out.push_str("\n| Span kind | Attribute reserve |\n|---|---|\n");
    for kind in crate::TraceSpanKind::ALL {
        let _ = writeln!(out, "| `{kind}` | {} |", attribute_reserve(kind));
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
