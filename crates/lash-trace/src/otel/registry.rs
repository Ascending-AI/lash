//! The exported instrumentation contract. Changes require a release note.
#[cfg(feature = "otel")]
use opentelemetry::trace::SpanKind;
#[cfg(feature = "otel")]
use opentelemetry::{KeyValue, Value};

pub const LASH_INSTRUMENTATION_NAME: &str = "lash";
pub const LASH_INSTRUMENTATION_CONTRACT: &str = "1.0";
pub const GEN_AI_SEMCONV_SNAPSHOT: &str =
    "open-telemetry/semantic-conventions-genai@07794021bfe46e1c241408b6f55cb4354bd28318";

// Advisory ExplicitBucketBoundaries from GEN_AI_SEMCONV_SNAPSHOT:
// docs/gen-ai/client-inference.md (durations) and gen-ai-token-metrics.md
// (operation tokens). Apply the count preset to agent call counts too.
#[cfg(feature = "otel")]
pub(crate) const DURATION_BOUNDARIES: [f64; 14] = [
    0.01, 0.02, 0.04, 0.08, 0.16, 0.32, 0.64, 1.28, 2.56, 5.12, 10.24, 20.48, 40.96, 81.92,
];
#[cfg(feature = "otel")]
pub(crate) const COUNT_BOUNDARIES: [f64; 14] = [
    1.0, 4.0, 16.0, 64.0, 256.0, 1024.0, 4096.0, 16384.0, 65536.0, 262144.0, 1048576.0, 4194304.0,
    16777216.0, 67108864.0,
];

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
const PRODUCER: TraceSpanKindSet = TraceSpanKindSet::of(&[K::Producer]);
/// Extension spans and points.
const EXTENSION: TraceSpanKindSet = TraceSpanKindSet::of(&[K::Extension, K::Point]);

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
            /// Every refinable key, in table order.
            pub const ALL: &[Self] = &[$(Self::$rid),+];

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
        ProducerOutcome => ("lash.producer.outcome", String, PRODUCER),
        LinksOmitted => ("lash.links.omitted", Integer, EVERY),
        RecordId => ("lash.record.id", String, EVERY),
        EventType => ("lash.event.type", String, EVERY),
        ConversationId => ("gen_ai.conversation.id", String, EVERY),
        RunId => ("lash.run.id", String, EVERY),
        ProcessId => ("lash.process.id", String, EVERY),
        LlmCallId => ("lash.llm_call.id", String, EVERY),
        SendId => ("lash.send.id", String, EVERY),
        ServerAddress => ("server.address", String, ATTEMPT),
        TransportErrorKind => ("lash.transport.error.kind", String, ATTEMPT),
        TransportErrorElapsedMs => ("lash.transport.error.elapsed_ms", Integer, ATTEMPT),
        ProviderErrorCode => ("lash.provider.error.code", String, ATTEMPT),
        ModelAttemptRetryDelayMs => ("lash.model.attempt.retry_delay_ms", Integer, ATTEMPT),
        HttpResponseStatusCode => ("http.response.status_code", Integer, ATTEMPT),
        SessionId => ("lash.session.id", String, EVERY),
        TurnId => ("lash.turn.id", String, EVERY),
        AttemptInvocationId => ("lash.attempt.invocation_id", String, EVERY),
        OperationName => ("gen_ai.operation.name", String,
            TraceSpanKindSet::of(&[K::Agent, K::Model, K::Tool, K::Cell, K::Workflow, K::Extension])),
        RequestModel => ("gen_ai.request.model", String, MODEL.with(K::Extension)),
        DataSourceId => ("gen_ai.data_source.id", String, TraceSpanKindSet::of(&[K::Extension])),
        PluginId => ("lash.plugin.id", String, EXTENSION),
        PluginCallback => ("lash.plugin.callback", String, EXTENSION),
        SpanEndedBy => ("lash.span.ended_by", String, TraceSpanKindSet::of(&[K::Extension])),
        SpanPoint => ("lash.span.point", Boolean, TraceSpanKindSet::of(&[K::Point])),
        ResponseModel => ("gen_ai.response.model", String, TraceSpanKindSet::of(&[K::Model])),
        ToolName => ("gen_ai.tool.name", String,
            TraceSpanKindSet::of(&[K::Tool, K::Cell, K::Process, K::ToolIntent])),
        ToolCallId => ("gen_ai.tool.call.id", String,
            TraceSpanKindSet::of(&[K::Tool, K::Process, K::ToolIntent])),
        CellId => ("lash.cell.id", String, TraceSpanKindSet::of(&[K::Cell])),
        CellChargesConsumed => ("lash.cell.charges.consumed", Integer, TraceSpanKindSet::of(&[K::Cell])),
        CellChargesLimit => ("lash.cell.charges.limit", Integer, TraceSpanKindSet::of(&[K::Cell])),
        CellChargesObserved => ("lash.cell.charges.observed", Integer, TraceSpanKindSet::of(&[K::Cell])),
        CellWorkerCpuNanosConsumed => ("lash.cell.worker_cpu_nanos.consumed", Integer, TraceSpanKindSet::of(&[K::Cell])),
        CellWorkerCpuNanosLimit => ("lash.cell.worker_cpu_nanos.limit", Integer, TraceSpanKindSet::of(&[K::Cell])),
        CellWorkerCpuNanosObserved => ("lash.cell.worker_cpu_nanos.observed", Integer, TraceSpanKindSet::of(&[K::Cell])),
        CellHeldWallNanosConsumed => ("lash.cell.held_wall_nanos.consumed", Integer, TraceSpanKindSet::of(&[K::Cell])),
        CellHeldWallNanosLimit => ("lash.cell.held_wall_nanos.limit", Integer, TraceSpanKindSet::of(&[K::Cell])),
        CellHeldWallNanosObserved => ("lash.cell.held_wall_nanos.observed", Integer, TraceSpanKindSet::of(&[K::Cell])),
        CellMemoryBytesConsumed => ("lash.cell.memory_bytes.consumed", Integer, TraceSpanKindSet::of(&[K::Cell])),
        CellMemoryBytesLimit => ("lash.cell.memory_bytes.limit", Integer, TraceSpanKindSet::of(&[K::Cell])),
        CellMemoryBytesObserved => ("lash.cell.memory_bytes.observed", Integer, TraceSpanKindSet::of(&[K::Cell])),
        CellConsumption => ("lash.cell.consumption", String, TraceSpanKindSet::of(&[K::Cell])),
        CellLanguage => ("lash.cell.language", String, TraceSpanKindSet::of(&[K::Cell])),
        CellTrailingDiscarded => ("lash.cell.trailing_discarded", Integer, TraceSpanKindSet::of(&[K::Cell])),
        CellToolCalls => ("lash.cell.tool_calls", Integer, TraceSpanKindSet::of(&[K::Cell])),
        InputTokens => ("gen_ai.usage.input_tokens", Integer, MODEL),
        OutputTokens => ("gen_ai.usage.output_tokens", Integer, MODEL),
        CacheReadTokens => ("gen_ai.usage.cache_read.input_tokens", Integer, MODEL),
        CacheWriteTokens => ("gen_ai.usage.cache_write.input_tokens", Integer, MODEL),
        ReasoningTokens => ("gen_ai.usage.reasoning.output_tokens", Integer, MODEL),
        ModelAttemptOrdinal => ("lash.model.attempt.ordinal", Integer,
            TraceSpanKindSet::of(&[K::ModelAttempt])),
        ProviderTerminalDeliveryTimedOut => ("lash.provider.terminal_delivery_timed_out", Boolean, MODEL.with(K::ModelAttempt)),
        ModelAttemptResponseMetadataKeys => ("lash.model.attempt.response_metadata.keys", StringArray, ATTEMPT),
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
        UsageUnknown => ("lash.usage.unknown", Boolean, MODEL),
        AttemptInputTokens => ("lash.usage.input_tokens", Integer, ATTEMPT),
        AttemptOutputTokens => ("lash.usage.output_tokens", Integer, ATTEMPT),
        AttemptCacheReadTokens => ("lash.usage.cache_read_tokens", Integer, ATTEMPT),
        AttemptCacheWriteTokens => ("lash.usage.cache_write_tokens", Integer, ATTEMPT),
        AttemptReasoningTokens => ("lash.usage.reasoning_tokens", Integer, ATTEMPT),
        ProviderRequestId => ("lash.provider.request_id", String, ATTEMPT),
        ProcessKind => ("lash.process.kind", String, TraceSpanKindSet::of(&[K::Process, K::Workflow])),
        StartDispatchUs => ("lash.process.start.dispatch_us", Integer, PRODUCER),
        StartAdmissionUs => ("lash.process.start.admission_us", Integer, PRODUCER),
        StartEnvironmentUs => ("lash.process.start.environment_us", Integer, PRODUCER),
        StartDefinitionLoadUs => ("lash.process.start.definition_load_us", Integer, PRODUCER),
        StartDefinitionResolveUs => ("lash.process.start.definition_resolve_us", Integer, PRODUCER),
        StartDefinitionAcquireUs => ("lash.process.start.definition_acquire_us", Integer, PRODUCER),
        StartDefinitionVerifyUs => ("lash.process.start.definition_verify_us", Integer, PRODUCER),
        StartDefinitionUs => ("lash.process.start.definition_us", Integer, PRODUCER),
        StartAdmitUs => ("lash.process.start.admit_us", Integer, PRODUCER),
        StartArtifactsUs => ("lash.process.start.artifacts_us", Integer, PRODUCER),
        StartLinkCheckUs => ("lash.process.start.link_check_us", Integer, PRODUCER),
        StartInputUs => ("lash.process.start.input_us", Integer, PRODUCER),
        StartPrepareUs => ("lash.process.start.prepare_us", Integer, PRODUCER),
        StartCommitUs => ("lash.process.start.commit_us", Integer, PRODUCER),
        StartAdoptUs => ("lash.process.start.adopt_us", Integer, PRODUCER),
        StartObserveUs => ("lash.process.start.observe_us", Integer, PRODUCER),
        ProcessLabel => ("lash.process.label", String, TraceSpanKindSet::of(&[K::Process, K::Workflow])),
        WorkflowName => ("gen_ai.workflow.name", String, TraceSpanKindSet::of(&[K::Workflow])),
        WorkflowNodeSite => ("lash.workflow.node.site", String, TraceSpanKindSet::of(&[K::WorkflowNode])),
        WorkflowNodeOccurrence => ("lash.workflow.node.occurrence", String,
            TraceSpanKindSet::of(&[K::WorkflowNode])),
        ToolId => ("lash.tool.id", String, TraceSpanKindSet::of(&[K::Tool])),
        ToolAttempts => ("lash.tool.attempts", Integer, TraceSpanKindSet::of(&[K::Tool])),
        ProviderCallId => ("lash.tool.provider_call_id", String, TraceSpanKindSet::of(&[K::Tool])),
        Outcome => ("lash.outcome", String, TraceSpanKindSet::of(&[
            K::Agent, K::Model, K::ModelAttempt, K::Tool, K::Cell, K::Process, K::Workflow,
            K::WorkflowNode, K::ToolIntent,
        ])),
        TurnStatements => ("lash.turn.sql.statements", Integer, TraceSpanKindSet::of(&[K::Agent])),
        TurnDml => ("lash.turn.sql.dml", Integer, TraceSpanKindSet::of(&[K::Agent])),
        TurnTransactions => ("lash.turn.sql.transactions", Integer, TraceSpanKindSet::of(&[K::Agent])),
        TurnStoreBytes => ("lash.turn.store.bytes_written", Integer, TraceSpanKindSet::of(&[K::Agent])),
        TurnSnapshots => ("lash.turn.snapshots", Integer, TraceSpanKindSet::of(&[K::Agent])),
        TurnManifests => ("lash.turn.manifests", Integer, TraceSpanKindSet::of(&[K::Agent])),
        TurnLinkPayloads => ("lash.turn.link_payloads", Integer, TraceSpanKindSet::of(&[K::Agent])),
        TurnOmittedIntervals => ("lash.turn.profile.omitted_intervals", Integer, TraceSpanKindSet::of(&[K::Agent])),
        TurnOmittedSqlKeys => ("lash.turn.profile.omitted_sql_keys", Integer, TraceSpanKindSet::of(&[K::Agent])),
        TurnInputs => ("lash.turn.inputs", Integer, TraceSpanKindSet::of(&[K::Agent])),
        RefusedEffectKind => ("lash.effect.refused.kind", String, TraceSpanKindSet::of(&[K::Agent])),
        EffectControllerCode => ("lash.effect.refused.controller_code", String, TraceSpanKindSet::of(&[K::Agent])),
        EffectControllerDetail => ("lash.effect.refused.detail", String, TraceSpanKindSet::of(&[K::Agent])),
        ProviderValidation => ("lash.provider.validation", String, TraceSpanKindSet::of(&[K::Agent])),
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
    }
    refinable: {
        ToolType => ("gen_ai.tool.type", String, TraceSpanKindSet::of(&[K::Tool, K::Cell])),
        GenAiProviderName => ("gen_ai.provider.name", String, MODEL.with(K::Extension)),
        ErrorType => ("error.type", String, EVERY),
    }
}

impl RefinableKey {
    /// The refinable key named `key`, if the registry declares one.
    pub fn of_key(key: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|refinable| refinable.key() == key)
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
    WorkflowNode => ("lash.workflow.node", Internal, Transition, None),
    ExecCode => ("execute_tool", Internal, Transition, Some("gen_ai.execute_tool.internal")),
    Extension => ("lash.extension", Internal, Live, None),
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
    ProviderTerminalDeliveryTimeouts => ("lash.provider.terminal_delivery.timeouts", Counter, "", Live),
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
}

/// L6 writes this generated contract into the public reference and checks equality.
#[cfg(feature = "otel")]
pub fn contract_markdown() -> String {
    use std::fmt::Write;
    let mut out = format!(
        "# Lash instrumentation contract\n\nScope `{LASH_INSTRUMENTATION_NAME}`, version `{LASH_INSTRUMENTATION_CONTRACT}`. GenAI snapshot `{GEN_AI_SEMCONV_SNAPSHOT}`. Exported changes require a release note. No schema URL is claimed.\n\n`TurnCompleted.output` carries the settled assistant answer under `OutputMessages` consent; its closer alone supplies `gen_ai.output.messages` on `invoke_agent`. An assistant message supplies its text; a declared finish control supplies one assistant text part, with string values verbatim and other values as compact JSON. Content is omitted by default and passes through the content hook and bounds. Model decorators read the logical chat span from `LlmRequestScope.trace_carrier` (absent when no model span is built). Content hooks identify tool records through `ContentView.tool_name`.\n\nOpeners carry no content: `LlmCallStarted` always carries an omitted request; hosts read request content from `LlmCallCompleted` or `LlmCallFailed`. `ResponseMetadata` is a separate consent category for record sinks, not an OTel-exported content attribute.\n\n`ContentLimits::standard()` cuts parts at 16 KiB, which truncates typical agent system prompts. Hosts wanting the full prompt raise `max_part_bytes` (Figments uses 64 KiB parts and 256 KiB attributes).\n\n`lash.model.attempt.response_metadata.keys` names the response-metadata keys the provider leaf captured for the attempt, such as `header:x-opper-cost`, and never their values. A host process start's `lash.process.start` producer carries one `lash.process.start.<phase>_us` attribute per phase the start reached; the phases run back to back from the producer's opening. Definition starts split descriptor loading (`definition_load`), engine derivation (`definition_resolve`), closure acquisition and its store waits (`definition_acquire`), and a supplied signature claim's verification (`definition_verify`); `definition` holds the remaining payload construction or inline realization. Sum all five for the former coarse definition total.\n\n`lash.provider.validation` on `invoke_agent` retains a failed turn's Lash-owned provider-validation check even when the model call completed. `empty_response` means assistant output contained neither visible text nor tool calls. It carries no provider text and needs no content opt-in.\n\nFIG-7426: `lash.effect.refused.kind`, `lash.effect.refused.controller_code` and `lash.effect.refused.detail` on `invoke_agent` retain the refused operation and the controller’s namespaced code and bounded diagnostic. The same `effect_refusal` is carried by the turn settlement, failure envelope and terminal trace record. No new opt-in is required.\n\nProvider terminal delivery timeouts set `lash.provider.terminal_delivery_timed_out` on the logical call and attempt spans and increment `lash.provider.terminal_delivery.timeouts`. A bounded invocation does not promise completed host settlement.\n\nFIG-7452: Failed HTTP attempts retain a content-free transport kind in `error.type` and `lash.transport.error.kind`, monotonic elapsed milliseconds in `lash.transport.error.elapsed_ms`, and a parsed endpoint host of at most 253 ASCII bytes in `server.address`. Kinds are `dns`, `connect`, `tls`, `connect_timeout`, `request_timeout`, `reset`, `body_read`, and `other`. The 1-based attempt ordinal and `lash.model.attempt.retry_delay_ms` identify the attempt and selected next wait, with zero when no retry is scheduled. HTTP failures retain status and a typed provider code in `lash.provider.error.code` only when it is at most 128 ASCII letters, digits, underscores, dots or hyphens. No URL paths, queries, credentials, prompts or raw error text are exported. The host owns admission spans such as Figments `lash.llm_admission` and must consume the typed evidence there.\n\nVendor rendering: the pinned semconv 07794021 uses `tool_call_response.response` and `reasoning`. Laminar reads `result`/`thinking` and semconv messages only on LLM spans. Hosts map these themselves; for example, map an agent span's `gen_ai.output.messages` into the host agent-output representation and translate nested tool response/reasoning parts for that renderer.\n\n| Span name or prefix | Kind | Ownership | Semconv type |\n|---|---|---|---|\n"
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
    let _ = writeln!(
        out,
        "\nGenAI histograms supply advisory semconv boundaries. Durations in seconds: `{:?}`. Token and call counts: `{:?}`. Host meter-provider views take precedence.",
        DURATION_BOUNDARIES, COUNT_BOUNDARIES,
    );
    out.push_str(r#"
## Turn physical work

`TurnCompleted.profile` carries bounded content-free physical owner-attempt
measurements. `invoke_agent` exports these integer attributes:

| Attribute | Unit | Meaning |
|---|---|---|
| `lash.turn.sql.statements` | statements | attempted SQL executions |
| `lash.turn.sql.dml` | statements | INSERT, UPDATE, DELETE and REPLACE executions |
| `lash.turn.sql.transactions` | transactions | BEGIN executions |
| `lash.turn.store.bytes_written` | bytes | newly installed canonical graph bytes, committed only on success |
| `lash.turn.snapshots` | calls | provider environment snapshot entries |
| `lash.turn.manifests` | calls | manifest collection in the default provider snapshot |
| `lash.turn.link_payloads` | calls | link-payload entries in the default provider snapshot |
| `lash.turn.profile.omitted_intervals` | intervals | phase intervals beyond the capture bound |
| `lash.turn.profile.omitted_sql_keys` | observations | detail beyond the SQL/write key bound |

Phase children are `lash.turn.claim`, `lash.turn.readback`, `lash.turn.admission`, `lash.turn.owner_orchestration`,
`lash.turn.prompt_environment_build`, `lash.turn.model_call`,
`lash.turn.tool_calls`, `lash.turn.cell_execution`,
`lash.turn.checkpoint_encode`, `lash.turn.commit`,
`lash.turn.store_queue_wait`, `lash.turn.store_gate_wait`,
`lash.turn.store_pool_wait`, `lash.turn.worker_checkout_wait` and
`lash.turn.worker_IPC`, when reached. Each has `lash.turn.phase`; commits also
have `lash.owner.revision`. Each child exports exclusive SQL counters for its
exact interval: `lash.turn.sql.statements`, `lash.turn.sql.round_trips`,
`lash.turn.sql.rows_returned`, `lash.turn.sql.rows_affected`,
`lash.turn.sql.bound_bytes` and `lash.turn.sql.result_bytes`.
Round trips count observed PostgreSQL statement submissions, excluding pool
checks and protocol control; SQLite contributes zero. Bytes are parameter
and returned scalar payload, excluding framing, WAL and fsync.
`lash.turn.sql.top_shapes` is a JSON array of at most eight shape entries,
ranked by statement count, with literals removed and each shape capped at
256 characters. Entries combine all origins for the same backend and shape, and contain
statements, round trips, rows and payload bytes.
`lash.turn.sql.omitted_shapes` counts additional captured backend/shape keys;
`lash.turn.profile.omitted_sql_keys` on the parent reports capture overflow.
Repeated phase names have independent counters. They parent to `invoke_agent` and follow its
sampling decision. Their kind is `Agent`; the host's export policy and bounded
enricher apply to each phase. Sinks recover the same phase identities through
`TurnProfile::phase_positions` using the terminal's record id and position.
Phase spans carry correlation ids and omit terminal content and usage.
Nested spans overlap; exclusive shares require an interval
union with precedence for the deepest active phase. No additional metric
labels contain SQL shapes, origins, revisions, session ids or turn ids.

Cold owners carry their successful physical claim into the first pass. Shared
batch SQL is assigned once to the first claimed actor; each member retains its
shared wall. Warm resident turns have no claim.

The profile retains at most 512 intervals, 512 SQL shape/origin keys and 512
graph-write kind keys per attempt. SQL literals are redacted. Physical totals
are distinct from logical lifecycle counters and do not survive owner death.
The flight recorder can trigger on a slow turn terminal using its retained
logical duration and the configured threshold.
"#);
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
