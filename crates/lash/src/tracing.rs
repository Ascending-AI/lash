// The vocabulary this module's signatures name (the facade-completeness rule).
/// Where engine code stands when it observes, and the journaled-step
/// boundary that grants the right to.
pub use lash_core::facade_support::{
    JournalFrontier, LiveStep, OpenSpan, StepIssue, TraceStanding,
};
pub use lash_core::facade_support::{ProviderCompletionSideband, StoreObserver};
/// Physical SQL windows and the receipt vocabulary their store sinks receive.
pub use lash_core::operational_metrics::sql;
pub use lash_sansio::AttachmentMaterializationReason;
/// The scope, cause, permit and identity vocabulary the trace runtime's
/// signatures name.
pub use lash_trace::telemetry::metrics::{
    DurableCommitCost, ObligationMetrics, ParkedWorkMetrics, RuntimeTuningMetrics,
    TelemetryMetrics, ToolIntentMetrics,
};
/// Every tracing knob's sealed policy, its typed refusal, and the bounded
/// writer and omission vocabulary every host contribution goes through.
pub use lash_trace::{
    AttrKey, ContentLimits, PolicyOperand, PolicyRelation, PolicyValue, PublicationHold,
    RefinableKey, TraceDetail, TraceDetailPolicy, TraceExportPolicy, TraceLimits, TraceMetrics,
    TraceOmission, TraceOmissionCounts, TraceOmissionDiagnostic, TraceOmissionLedger,
    TraceOmissionReport, TraceOmissionSite, TraceOmissionSiteKind, TracePolicyError,
    TracePolicyField, TracePolicySelection, TracePolicySnapshot, TraceSpanKindSet, TraceValue,
    WriterRemaining, attribute_reserve,
    policy::{FieldBounds, TraceDetailPolicyBuilder, TraceExportPolicyBuilder, TraceLimitsBuilder},
};
pub use lash_trace::{
    DurableTraceScope, EmissionPermit, EmissionSource, InvalidTraceCarrier, InvalidTraceLinks,
    LashIdentity, ProducerKind, ProducerOutcome, ProducerSpan, ProducerSpec,
    ProductObservationSink, SpanKey, SpanRole, TraceAttemptId, TraceAttemptLink,
    TraceAttemptObservation, TraceCall, TraceCaller, TraceCarrier, TraceCause,
    TraceDomainCompletion, TraceDomainStatus, TraceDomainSubject, TraceEventKind, TraceIdentity,
    TraceIds, TraceLinkMode, TraceLinks, TraceModelAttempt, TraceModelAttemptSpan, TraceModelCall,
    TraceParent, TracePosition, TraceRecordIdentity, TraceScopeAdmission, TraceScopeId,
    TraceScopeKind, TraceScopeOffer, TraceScopeOwner, TraceSpanKind, TraceStateLimits,
    TraceTimestamp, TraceToolOwner, TraceTransitionKind, W3cSpanId, W3cTraceFlags, W3cTraceId,
    W3cTraceState, derive_span,
};
pub use lash_trace::{TRACESTATE_CHAR_LIMIT, TRACESTATE_MEMBER_LIMIT};

pub use lash_core::{
    TraceAttachment, TraceContentBlock, TraceEffectEnvelopeDiffEntry, TraceEffectEnvelopeDiffEvent,
    TraceEffectEnvelopeDiffValue, TraceError, TraceEvent, TraceLlmMessage, TraceLlmRequest,
    TraceLlmResponse, TracePromptComponent, TraceProviderBodyOmission, TraceProviderEvent,
    TraceProviderReplayDropEvent, TraceProviderReplayDropReason, TraceProviderReplayKind,
    TraceProviderRouteIdentity, TraceRuntimeStreamEvent, TraceRuntimeStreamPayload,
    TraceToolResultBlock, TraceToolSpec, facade_support::TraceBranchSelection,
    facade_support::TraceRecord, facade_support::TraceRuntimeScope,
    facade_support::TraceRuntimeSubject, facade_support::TraceSinkError,
};
pub use lash_sansio::ExecutionNodeKind;
#[cfg(feature = "otel-trace")]
pub use lash_trace::otel::api as otel;
#[cfg(feature = "otel-trace")]
pub use lash_trace::otel::registry::{
    GEN_AI_SEMCONV_SNAPSHOT, LASH_INSTRUMENTATION_CONTRACT, LASH_INSTRUMENTATION_NAME,
    contract_markdown, recommended_latency_histogram_boundaries,
};
#[cfg(feature = "otel-trace")]
pub use lash_trace::otel::{
    LashTracerProvider, OtelOptions, OtelTelemetry, SpanEnricher, SpanEnrichment, SpanView,
};
pub use lash_trace::{
    CONTENT_POLICY_OMISSION, FlightRecorderOutcome, FlightRecorderRecord, FlightRecorderSettings,
    FlightRecorderSink, FlightRecorderSnapshot, ObservationWorkLimits, TeeTraceSink,
    TelemetryContent, TraceContext, TraceSink, TraceToolCallOutcome, TraceToolCallOutput,
};
/// Every type reachable from a [`TraceEvent`] payload, so a facade consumer
/// can name — match on, take in a signature, or build in a test — what a
/// `TurnCompleted` or tool-call variant carries. The `LanguageExecution`
/// variant exists in every build, so its payload types are unconditional
/// `lash-trace` re-exports rather than `rlm`-gated.
pub use lash_trace::{
    ExecCodeFailureReason, StepBodyStarted, TRACE_SCHEMA_VERSION, TextProjectionMetadata,
    TraceAgentFrameSwitch, TraceExecToolCall, TraceFailureCode, TraceLanguageChildExecution,
    TraceLanguageExecution, TraceLanguageExecutionFailure, TraceLanguageExecutionGeneration,
    TraceLanguageExecutionIdentity, TraceLanguageExecutionPayload, TraceLanguageExecutionStatus,
    TraceLlmTerminalReason, TraceNodeAwaited, TraceNodeFact, TraceNodeWaitKind,
    TraceNodeWaitResolution, TraceNormalizedError, TraceProgramStepOutcome,
    TraceProviderFailureKind, TraceRetryAttempt, TraceRetryAttemptDetail, TraceRetryClass,
    TraceRetryDeclineCause, TraceRetryWait, TraceStoreErrorClass, TraceToolAttemptOutcome,
    TraceToolCallStatus, TraceTurnCancellationEvidence, TraceTurnCompletionReason,
    TraceTurnFailureReason, TraceTurnOutcome,
};

/// How a producer's host call that failed with `error` settles: refused,
/// its `error.type` the runtime error code when the failure carries one,
/// else `_OTHER`.
pub(crate) fn refused(error: &crate::EmbedError) -> ProducerOutcome {
    let code = match error {
        crate::EmbedError::Runtime(error)
        | crate::EmbedError::Plugin(lash_core::PluginError::Runtime(error)) => {
            serde_json::to_value(&error.code)
                .ok()
                .and_then(|code| code.as_str().map(str::to_owned))
        }
        _ => None,
    };
    ProducerOutcome::Refused {
        error_type: code.map_or(
            std::borrow::Cow::Borrowed("_OTHER"),
            std::borrow::Cow::Owned,
        ),
    }
}
