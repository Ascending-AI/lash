// The vocabulary this module's signatures name (the facade-completeness rule).
/// Where engine code stands when it observes, and the journaled-step
/// boundary that grants the right to.
pub use lash_core::facade_support::{JournalFrontier, LiveStep, StepIssue, TraceStanding};
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
pub use lash_trace::{
    AttemptObservation, DurableTraceScope, EmissionPermit, EmissionSource, InvalidTraceCarrier,
    InvalidTraceLinks, TraceAdmissionCandidate, TraceAnchor, TraceAttemptId,
    TraceAttemptObservation, TraceCandidateOutcome, TraceCarrier, TraceCause,
    TraceDomainCompletion, TraceDomainOperation, TraceDomainProjector, TraceDomainStatus,
    TraceEventKind, TraceHostOperation, TraceLinks, TraceRecordIdentity, TraceScopeAdmission,
    TraceScopeFactory, TraceScopeId, TraceScopeKind, TraceScopeOffer, TraceScopeOwner,
    TraceToolOwner, TraceTransitionKind, UntracedScopes, W3cSpanId, W3cTraceFlags, W3cTraceId,
    W3cTraceState,
};
pub use lash_trace::{TRACE_LINK_LIMIT, TRACESTATE_CHAR_LIMIT, TRACESTATE_MEMBER_LIMIT};

pub use lash_core::{
    TraceAttachment, TraceContentBlock, TraceEffectEnvelopeDiffEntry, TraceEffectEnvelopeDiffEvent,
    TraceEffectEnvelopeDiffValue, TraceError, TraceEvent, TraceLlmMessage, TraceLlmRequest,
    TraceLlmResponse, TracePromptComponent, TraceProviderBodyOmission, TraceProviderEvent,
    TraceProviderReplayDropEvent, TraceProviderReplayDropReason, TraceProviderReplayKind,
    TraceProviderRouteIdentity, TraceRuntimeStreamEvent, TraceRuntimeStreamPayload,
    TraceToolResultBlock, TraceToolSpec, facade_support::JsonlTraceReadError,
    facade_support::JsonlTraceSink, facade_support::TraceBranchSelection,
    facade_support::TraceRecord, facade_support::TraceRuntimeScope,
    facade_support::TraceRuntimeSubject, facade_support::TraceSinkError,
    facade_support::parse_jsonl_records,
};
pub use lash_sansio::ExecutionNodeKind;
#[cfg(feature = "otel-trace")]
pub use lash_trace::otel::api as otel;
#[cfg(feature = "otel-trace")]
pub use lash_trace::otel::registry::{
    GEN_AI_SEMCONV_SNAPSHOT, LASH_INSTRUMENTATION_CONTRACT, LASH_INSTRUMENTATION_NAME,
    contract_markdown,
};
#[cfg(feature = "otel-trace")]
pub use lash_trace::otel::{OtelAdmissionLimits, OtelOptions, OtelSpanEnricher, OtelTelemetry};
pub use lash_trace::{
    CONTENT_POLICY_OMISSION, ObservationWorkLimits, StderrTraceSink, TeeTraceSink,
    TelemetryContent, TraceContext, TraceLevel, TraceLimits, TraceSink, TraceToolCallOutcome,
    TraceToolCallOutput,
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
