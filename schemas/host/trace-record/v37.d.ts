// Derived from v37.schema.json by scripts/generate-workflow-schemas.py.
// JSON Schema remains authoritative for value constraints and validation.

/**
 * One durable trace record: the envelope fields plus the flattened
 * event, whose `type` tag names the variant.
 */
export type TraceRecord = (({
  "metadata"?: {
    [key: string]: unknown;
  };
  "type": "turn_started";
  [key: string]: unknown;
} | {
  "components"?: Array<TracePromptComponent>;
  "prompt_chars": number;
  "prompt_hash": string;
  "type": "prompt_built";
  [key: string]: unknown;
} | {
  "elapsed_ms": number;
  "error": unknown;
  "limits": unknown;
  "plan"?: unknown;
  "type": "prompt_composition_failed";
  [key: string]: unknown;
} | {
  "attachment_id"?: (string | null);
  /**
   * On the wire `attachment_position`: a record's own `position`
   * is its place in the trace.
   */
  "attachment_position": AttachmentPosition;
  "label"?: (string | null);
  "media_type"?: (string | null);
  "reason": AttachmentMaterializationReason;
  "type": "attachment_degraded";
  [key: string]: unknown;
} | {
  /**
   * SHA-256 of the rendered system prompt plus ordered fingerprints of
   * the model-facing tool contracts.
   */
  "fingerprint": string;
  "rendered_system_prompt": string;
  /**
   * Full model-facing tool contracts in request order. This is kept
   * even when empty so the event is a self-contained snapshot.
   */
  "tool_schemas": Array<TraceToolSpec>;
  "type": "composition_changed";
  [key: string]: unknown;
} | {
  "max_context_tokens": number;
  "threshold_tokens": number;
  "type": "compaction_needed";
  "used_tokens": number;
  [key: string]: unknown;
} | {
  "instructions_present": boolean;
  "source_messages": number;
  "type": "compaction_started";
  [key: string]: unknown;
} | {
  "summary_nodes": number;
  "type": "compaction_completed";
  [key: string]: unknown;
} | {
  "max_context_tokens": number;
  "pruned_attachments": number;
  "type": "prompt_view_attachments_pruned";
  "used_tokens": number;
  [key: string]: unknown;
} | {
  "request": TraceLlmRequest;
  "type": "llm_call_started";
  [key: string]: unknown;
} | {
  "attempts"?: (Array<TraceModelAttempt> | null);
  "model"?: (TraceModelCall | null);
  "provider_usage"?: unknown;
  "request": TraceLlmRequest;
  "response": TraceLlmResponse;
  "response_metadata"?: {
    [key: string]: unknown;
  };
  "stream_summary"?: unknown;
  "type": "llm_call_completed";
  "usage"?: (LlmUsage | null);
  [key: string]: unknown;
} | {
  "attempts"?: (Array<TraceModelAttempt> | null);
  "error": TraceError;
  "model"?: (TraceModelCall | null);
  "request": TraceLlmRequest;
  "stream_summary"?: unknown;
  "type": "llm_call_failed";
  [key: string]: unknown;
} | {
  "attempt": AttemptRecord;
  "observation": TraceAttemptObservation;
  "response_metadata"?: {
    [key: string]: unknown;
  };
  "type": "llm_attempt_completed";
  [key: string]: unknown;
} | {
  "completion": TraceDomainCompletion;
  "type": "domain_completed";
  [key: string]: unknown;
} | {
  "event": TraceProviderEvent;
  "type": "provider_event";
  [key: string]: unknown;
} | {
  "event": TraceProviderReplayDropEvent;
  "type": "provider_replay_dropped";
  [key: string]: unknown;
} | {
  "event": TraceEffectEnvelopeDiffEvent;
  "type": "effect_envelope_diff";
  [key: string]: unknown;
} | {
  "event": TraceRuntimeStreamEvent;
  "type": "runtime_stream_event";
  [key: string]: unknown;
} | {
  "args": unknown;
  /**
   * Lash's identity for the call (ADR 0117).
   */
  "call_id": string;
  "issuing_node_id"?: (string | null);
  "name": string;
  /**
   * The model provider's id for the call, when a model issued it.
   */
  "provider_call_id"?: (string | null);
  "type": "tool_call_started";
  [key: string]: unknown;
} | {
  /**
   * Exporters outside lash cannot read the store, so this diagnostic content travels with the trace.
   */
  "args": TraceToolArgs;
  /**
   * Exporters outside lash cannot read the journal, so the observed retry ladder travels with the trace.
   */
  "attempts"?: (Array<TraceRetryAttempt> | null);
  /**
   * Lash's identity for the call (ADR 0117).
   */
  "call_id": string;
  "duration_ms": number;
  /**
   * Exporters outside lash cannot read the store, so issuer correlation travels with the trace.
   */
  "issuing_node_id"?: (string | null);
  /**
   * Exporters outside lash cannot read the store, so this diagnostic content travels with the trace.
   */
  "name": TraceToolName;
  /**
   * Exporters outside lash cannot read the store, so this diagnostic content travels with the trace.
   */
  "output": TraceToolCallOutput;
  /**
   * The model provider's id for the call, when a model issued it.
   * Exporters outside lash cannot read the store, so provider correlation travels with the trace.
   */
  "provider_call_id"?: (string | null);
  /**
   * What the body of the attempt that settled the call set on its own
   * tool span: in memory only, stamped by the span's closer.
   */
  "span_attributes"?: ToolSpanAnnotations;
  "type": "tool_call_returned";
  [key: string]: unknown;
} | {
  /**
   * Exporters outside lash cannot read the store, so this diagnostic content travels with the trace.
   */
  "args": TraceToolArgs;
  /**
   * Exporters outside lash cannot read the journal, so the observed retry ladder travels with the trace.
   */
  "attempts"?: (Array<TraceRetryAttempt> | null);
  /**
   * Lash's identity for the call (ADR 0117).
   */
  "call_id": string;
  "duration_ms": number;
  /**
   * Exporters outside lash cannot read the store, so issuer correlation travels with the trace.
   */
  "issuing_node_id"?: (string | null);
  /**
   * Exporters outside lash cannot read the store, so this diagnostic content travels with the trace.
   */
  "name": TraceToolName;
  /**
   * Exporters outside lash cannot read the store, so this diagnostic content travels with the trace.
   */
  "output": TraceToolCallOutput;
  /**
   * The model provider's id for the call, when a model issued it.
   * Exporters outside lash cannot read the store, so provider correlation travels with the trace.
   */
  "provider_call_id"?: (string | null);
  /**
   * What the body of the attempt that settled the call set on its own
   * tool span: in memory only, stamped by the span's closer.
   */
  "span_attributes"?: ToolSpanAnnotations;
  "type": "tool_call_completed";
  [key: string]: unknown;
} | {
  "code_chars": number;
  "language": string;
  "type": "exec_code_started";
  [key: string]: unknown;
} | {
  "code": string;
  "consumption": Array<CellConsumption>;
  "duration_ms": number;
  "language": string;
  "observation_count": number;
  "observation_projections": Array<TextProjectionMetadata>;
  "output": string;
  "output_chars": number;
  "result": CellOutcome;
  "started_at_ms": number;
  "tool_calls": Array<TraceExecToolCall>;
  /**
   * Observed bytes discarded after the model reply's first cell.
   */
  "trailing_discarded"?: number;
  "type": "exec_code_completed";
  [key: string]: unknown;
} | {
  "code": string;
  /**
   * The observed window, absent when no invocation existed.
   */
  "duration_ms"?: (number | null);
  /**
   * Human-readable detail. Not a stable classification surface.
   */
  "error": string;
  "language": string;
  /**
   * Closed failure classification; offline analysis keys on this rather
   * than matching `error` prose.
   */
  "reason": ExecCodeFailureReason;
  "started_at_ms": number;
  /**
   * Observed bytes discarded after the model reply's first cell.
   */
  "trailing_discarded"?: number;
  "type": "exec_code_failed";
  [key: string]: unknown;
} | {
  "code": string;
  "duration_ms": number;
  "language": string;
  "started_at_ms": number;
  /**
   * Observed bytes discarded after the model reply's first cell.
   */
  "trailing_discarded"?: number;
  "type": "exec_code_discarded";
  [key: string]: unknown;
} | {
  "projections": Array<TextProjectionMetadata>;
  "type": "observation_projection";
  [key: string]: unknown;
} | {
  "error_class": TraceStoreErrorClass;
  "message": string;
  "operation": string;
  "type": "store_error_observed";
  [key: string]: unknown;
} | (({
  "outcome": "ok";
  [key: string]: unknown;
} | {
  /**
   * Bounded compiler or linker feedback, using the raw-error trace limit.
   */
  "diagnostic": string;
  "outcome": "failure";
  [key: string]: unknown;
}) & {
  /**
   * Protocol iteration of the submitted program within its turn.
   */
  "step_index": number;
  "type": "program_step";
  [key: string]: unknown;
}) | {
  "payload": unknown;
  "plugin_id": string;
  "type": "protocol_step";
  [key: string]: unknown;
} | {
  "event": TraceLanguageExecution;
  /**
   * The source dialect, when the execution records one. Graph artifacts
   * are dialect-free and leave it absent.
   */
  "language"?: (string | null);
  "type": "language_execution";
  [key: string]: unknown;
} | {
  "step": StepBodyStarted;
  "type": "step_body_started";
  [key: string]: unknown;
} | {
  /**
   * Model calls admitted by this turn, including failed calls; absent
   * when the terminal owner cannot recover that fact.
   */
  "inference_calls"?: (number | null);
  /**
   * Admitted input messages, captured by the terminal owner from durable input.
   */
  "input": Array<TraceLlmMessage>;
  "inputs": number;
  "outcome": TraceTurnOutcome;
  /**
   * The assistant answer this turn settled with, governed as OutputMessages.
   */
  "output"?: Array<TraceLlmMessage>;
  /**
   * Physical work in the terminal owner attempt; prior owners are not replayed.
   */
  "profile"?: (TurnProfile | null);
  /**
   * Tool calls admitted by the turn and its cells, including failures.
   * The terminal owner reads the stored count plus acknowledged appends;
   * absent only for a record with no known execution counts.
   */
  "tool_calls"?: (number | null);
  "type": "turn_completed";
  [key: string]: unknown;
} | (({
  "outcome": "selected";
  [key: string]: unknown;
} | {
  "outcome": "reused";
  [key: string]: unknown;
} | {
  "error_type": string;
  "outcome": "refused";
  [key: string]: unknown;
} | {
  "outcome": "abandoned";
  [key: string]: unknown;
}) & {
  /**
   * A process start's time per phase it reached, in phase order.
   */
  "phases"?: Array<ProcessStartPhaseTime>;
  "producer": ProducerKind;
  "type": "producer_settled";
  [key: string]: unknown;
}) | {
  "conflict": ToolCheckConflict;
  /**
   * The plugin whose reply won.
   */
  "plugin_id": string;
  "type": "tool_check_conflict";
  [key: string]: unknown;
} | ExtensionSpanRecord | TraceOmissionDiagnostic | {
  "frame_node_id": string;
  "hook_id": string;
  "plugin_id": string;
  "task": string;
  "type": "context_pressure";
  [key: string]: unknown;
}) & {
  "content": ContentMarks;
  "context": TraceContext;
  "id": string;
  "omissions"?: TraceOmissionCounts;
  "position"?: (TracePosition | null);
  "schema_version": (37);
  "timestamp": TraceTimestamp;
  [key: string]: unknown;
});

/**
 * Why a request omitted an attachment.
 */
export type AttachmentMaterializationReason = ("target_model_does_not_accept_mime_and_position");

export type AttachmentPosition = ("message" | "tool_result");

export type AttachmentRef = {
  "byte_len": number;
  "id": string;
  "label"?: (string | null);
  "media_type": string;
  "type_metadata"?: (AttachmentTypeMetadata | null);
  [key: string]: unknown;
};

export type AttachmentTypeMetadata = ({
  "height"?: (number | null);
  "type": "image";
  "width"?: (number | null);
});

export type AttemptOutcome = ("completed" | "failed" | "aborted" | "interrupted");

export type AttemptRecord = {
  "error"?: (NormalizedError | null);
  "evidence"?: (ExecutionEvidence | null);
  /**
   * Which of the caller's generation options this attempt's request
   * carried, as reported by the adapter that built it.
   */
  "generation_disposition"?: (GenerationReceipt | null);
  "ordinal": number;
  "outcome": AttemptOutcome;
  "protocol_position": ProtocolPosition;
  "retry_budget_consumed": boolean;
  "retry_decision"?: (RetryDecision | null);
  /**
   * Provider-reported usage only. Absence is not zero usage.
   */
  "usage"?: (LlmUsage | null);
  [key: string]: unknown;
};

/**
 * A function and its operation context. Authored callbacks retain their defining
 * Link; build helpers carry the caller's real Link and an exact library ID.
 */
export type CallableId = {
  "function": FunctionId;
  "link": LinkId;
};

/**
 * The runtime or actor decision that requested cancellation of a process
 * or of a tool call's attempt. `RunClosing` is a call's owning tool Run
 * discharging its cancellation at Closing while the turn itself runs on.
 */
export type CancelOrigin = ("turn_stopped" | "parent_ended" | "operator_requested" | "model_requested" | "start_failed" | "run_closing");

/**
 * Stable semantic reference to the runtime fact that caused another fact.
 */
export type CausalRef = ({
  "session_id": string;
  "turn_id": string;
  "type": "turn";
} | {
  "address": EffectAddress;
  "type": "effect";
} | {
  "call_id": string;
  "session_id": string;
  "type": "tool_call";
} | {
  "process_id": string;
  "type": "process";
} | {
  "process_id": string;
  "sequence": number;
  "type": "process_event";
} | {
  "node_id": string;
  "session_id": string;
  "type": "session_node";
});

/**
 * Consumption at a cell decision, from existing kernel and supervisor meters.
 */
export type CellConsumption = {
  /**
   * Budget consumed. Charges saturate at the budget; `observed` retains
   * the raw kernel charge, including the refused crossing unit.
   * None means Lash has no meter for this bound, or no receipt arrived.
   */
  "consumed"?: (number | null);
  "limit": number;
  /**
   * The raw meter reading, which can pass the bound.
   */
  "observed"?: (number | null);
  "resource": CellResource;
};

/**
 * The control that ended a cell, as history records it.
 */
export type CellControl = ({
  "type": "finish";
  "value": CellFinishValue;
  [key: string]: unknown;
} | {
  "frame_key": FrameKey;
  "type": "switch_agent_frame";
  [key: string]: unknown;
});

/**
 * A rule of a session's cells that a cell's program broke.
 */
export type CellDefect = ({
  "kind": "namespace_shadowed_by_local";
  "shadowing": NamespaceShadowing;
  "span"?: ([number, number] | null);
  [key: string]: unknown;
} | {
  "kind": "internal_operation";
  "operation": FunctionName;
  "unit": Unit;
  [key: string]: unknown;
} | {
  "binding": string;
  "kind": "binding_not_carried";
  [key: string]: unknown;
} | {
  "kind": "tasks_outstanding";
  "unfinished": Array<string>;
  "unobserved": Array<string>;
  [key: string]: unknown;
});

/**
 * Structured failure returned by a code executor.
 */
export type CellFailure = {
  /**
   * The rule of a session's cells the program broke, when that is why
   * the cell failed, kept typed through the plugin and host result.
   */
  "defect"?: (CellDefect | null);
  /**
   * Why the `exec_code` effect failed before the executor produced a
   * response, when that is the failure: the closed reason, kept typed
   * through the protocol driver and the trajectory it records.
   */
  "exec_failure"?: (ExecCodeFailureReason | null);
  /**
   * Each pure repair callback's identity and answer, in registration order,
   * including empty answers. Retained with the failure for replay and attribution.
   */
  "feedback"?: Array<CellFailureRepair>;
  "kind": CellFailureKind;
  "message": string;
  "schema_admission"?: (SchemaAdmissionError | null);
  /**
   * The session's `max_tool_calls` refusal, when that is why the cell
   * failed (FIG-4546), kept typed through the plugin and host result.
   */
  "tool_call_limit"?: (ToolCallLimitExceeded | null);
  /**
   * The measured run limit, kept typed through the plugin and host result.
   */
  "worker_limit"?: (WorkerLimit | null);
  [key: string]: unknown;
};

/**
 * Why an executed code cell failed.
 */
export type CellFailureKind = ("policy" | "program" | "host");

/**
 * One plugin callback's recorded repair decision for a failed cell.
 */
export type CellFailureRepair = {
  /**
   * The complete callback key, including its slot prefix.
   */
  "callback": string;
  "plugin_id": string;
  "repairs": Array<string>;
};

export type CellFinishValue = ({
  "inline": unknown;
} | {
  "retained": RetainedValue;
});

/**
 * What an executed cell resolved to. One of three: a cell never carries a
 * failure and a control at once, and a cell that finished with `null` is
 * distinct from one that ran to its end without a control call.
 */
export type CellOutcome = ({
  "kind": "completed";
  [key: string]: unknown;
} | {
  "kind": "failed";
  "value": CellFailure;
  [key: string]: unknown;
} | {
  "kind": "controlled";
  "value": {
    "call_id": string;
    "control": CellControl;
    "tool_name": string;
    [key: string]: unknown;
  };
  [key: string]: unknown;
});

/**
 * A cell bound in its native units. Time values use nanoseconds.
 */
export type CellResource = ("charges" | "memory_bytes" | "live_tasks" | "call_depth" | "requests_per_park" | "join_members" | "worker_cpu_nanos" | "held_wall_nanos" | "compute_nanos" | "serialization_nanos" | "library_work" | "host_read_nanos" | "worker_memory_bytes" | "effects" | "checkpoint_bytes" | "tool_calls" | "effect_value_bytes" | "vm_state_bytes" | "frame_bytes");

/**
 * Why host charge-safety policy denied an otherwise transport-retryable
 * generation.
 *
 * # Integrator class
 *
 * Runtime and reporting implementors exhaustively render these host-policy
 * outcomes; provider adapters never construct them.
 */
export type ChargeSafetyDenialReason = ("guarantee_required" | "unsafe_retry_limit_exceeded" | "duplicate_cost_limit_exceeded");

/**
 * Every content field belongs to one of these categories. Reasoning also
 * requires consent to its message's direction.
 */
export type ContentCategory = ("system_instructions" | "input_messages" | "output_messages" | "reasoning" | "tool_definitions" | "output_schema" | "tool_arguments" | "tool_results" | "code" | "code_output" | "provider_payloads" | "response_metadata" | "diagnostics_text" | "extension");

/**
 * How a reader distinguishes omitted, empty, cut and oversized content.
 */
export type ContentMarks = {
  "captured": Array<ContentCategory>;
  "dropped_messages": number;
  "hook_failed": boolean;
  "omitted": Array<string>;
  "oversize": Array<string>;
  /**
   * References committed by a normally returning content hook.
   */
  "references": Array<[string, TraceValue]>;
  "truncated": Array<string>;
  [key: string]: unknown;
};

export type Duration = {
  "nanos": number;
  "secs": number;
  [key: string]: unknown;
};

/**
 * Canonical address of one admitted runtime effect.
 */
export type EffectAddress = {
  "execution_scope": ExecutionScope;
  "replay_key": string;
  [key: string]: unknown;
};

export type EffectId = string;

/**
 * The refused operation and the controller's own answer (FIG-7426).
 * This is controller diagnostic text, independent of provider error bodies.
 */
export type EffectRefusal = {
  /**
   * The controller's code in its own namespace.
   */
  "controller_code": FailureCode;
  /**
   * The controller's diagnostic, bounded by the runtime's error output cut.
   */
  "detail": string;
  /**
   * Stable runtime-effect kind, including kinds authored by newer builds.
   */
  "effect_kind": string;
  [key: string]: unknown;
};

/**
 * Why an `exec_code` effect failed before the executor produced a response.
 *
 * This is the closed classification offline trace analysis keys on; the human
 * detail lives in [`ExecCodeFailure::message`]. Serialized snake_case on the
 * durable journal and trace surfaces.
 */
export type ExecCodeFailureReason = ("executor_unavailable" | "runtime_stopped" | "session");

/**
 * Facts reported by the provider about the execution that produced a response.
 *
 * These fields must never be filled from request intent. In particular,
 * `reasoning_output_tokens: Some(0)` is distinct from an unreported value.
 */
export type ExecutionEvidence = {
  /**
   * Why provider evidence is partial even though the Lash response is
   * accepted. Present only when Lash deliberately preempted collection.
   */
  "collection_interruption"?: (ExecutionEvidenceCollectionInterruption | null);
  /**
   * Provider-reported terminal reason in that provider's own vocabulary.
   * Compatible gateways prefer their native reason over a normalized
   * OpenAI-compatible `finish_reason` when both are present.
   */
  "provider_finish_reason"?: (string | null);
  /**
   * Transport request identifier reported by the provider (for example,
   * OpenRouter's `x-request-id`). This is distinct from the response's
   * protocol-level identifier and may be present on failed attempts.
   */
  "provider_request_id"?: (string | null);
  "provider_response_id"?: (string | null);
  "reasoning_output_tokens"?: (number | null);
  "served_model"?: (string | null);
  [key: string]: unknown;
};

export type ExecutionEvidenceCollectionInterruption = ("protocol_abort");

/**
 * Stable semantic identity for one effectful runtime operation.
 *
 * This is the scope admitted by the host boundary before nondeterministic
 * work begins. Its journal encoding is an existing durable contract; effect
 * addresses compose it with a replay key without changing those bytes.
 */
export type ExecutionScope = ({
  "session_id": string;
  "turn_id": string;
  "type": "turn";
  [key: string]: unknown;
} | {
  "process_id": string;
  "type": "process";
  [key: string]: unknown;
} | {
  "operation_id": string;
  "session_id": string;
  "type": "session_operation";
  [key: string]: unknown;
} | {
  "session_id": string;
  "type": "session_delete";
  [key: string]: unknown;
} | {
  "operation_id": string;
  "type": "runtime_operation";
  [key: string]: unknown;
});

/**
 * One contributed attribute, as the bounded writer admitted it.
 */
export type ExtensionAttribute = {
  "key": string;
  "value": TraceValue;
};

/**
 * One content value an extension built under the `extension` category.
 */
export type ExtensionContent = {
  "key": string;
  "value": unknown;
};

/**
 * The GenAI operation a typed [`SpanSpec`] preset declares. Lash sets its
 * semconv attributes; a plugin cannot set them through `attr`.
 */
export type ExtensionOperation = ({
  "operation": "embeddings";
  "provider": string;
  "request_model": string;
} | {
  "data_source_id": string;
  "operation": "retrieval";
});

/**
 * Why an extension span ended.
 */
export type ExtensionSpanEnd = ("end" | "drop" | "body_exit" | "cancelled" | "panic" | "hold_pressure");

/**
 * One event an extension recorded on its span.
 */
export type ExtensionSpanEvent = {
  "at_ms": number;
  "attributes"?: Array<ExtensionAttribute>;
  "name": string;
};

/**
 * The OpenTelemetry kind of an extension span.
 */
export type ExtensionSpanKind = ("internal" | "client" | "producer" | "consumer");

/**
 * The one record of an extension span (or point): emitted when it ends,
 * whole, with everything it buffered.
 *
 * `plugin_id` and `callback` are the registered callback the span's
 * capability was bound from, never a value the plugin supplied.
 */
export type ExtensionSpanRecord = {
  "attributes"?: Array<ExtensionAttribute>;
  "callback": string;
  /**
   * Serialized as `contents`: the record envelope's `content` is its
   * content marks, and the record flattens this one beside them.
   */
  "contents"?: Array<ExtensionContent>;
  "ended_at_ms": number;
  /**
   * `None` for a point.
   */
  "ended_by"?: (ExtensionSpanEnd | null);
  /**
   * Set when the plugin called `fail`: the span's `error.type`.
   */
  "error_type"?: (string | null);
  "events"?: Array<ExtensionSpanEvent>;
  "kind": ExtensionSpanKind;
  "name": string;
  "operation"?: (ExtensionOperation | null);
  "plugin_id": string;
  /**
   * A zero-duration point recorded with no open span.
   */
  "point"?: boolean;
  "started_at_ms": number;
  [key: string]: unknown;
};

export type FailureCode = string;

/**
 * One key of an object's shape.
 */
export type FieldShape = {
  "key": string;
  /**
   * Some object merged at this position did not hold the key.
   */
  "optional"?: boolean;
  "shape": ValueShape;
};

export type FrameKey = string;

export type FunctionId = string;

export type FunctionName = string;

/**
 * What became of one host-set generation setting on one request.
 *
 * A setting a host sets is sent, or the call is refused before any I/O with a
 * typed failure; nothing is silently dropped. The receipt therefore names only
 * the two deliberate non-send dispositions besides `Applied`: a cap reduced to
 * the model's capacity, and stop sequences a protocol owns. `Applied` means
 * lash put the value on the wire, never that the provider complied.
 */
export type GenerationOptionOutcome = ("not_requested" | "applied" | "suppressed_protocol_owned" | "omitted_unsupported" | "clamped_to_capacity");

export type GenerationOptions = {
  /**
   * Upper bound on output tokens. Unset means lash sends no cap on wires
   * where one is optional; a wire that requires one takes the provider
   * options' `max_output_tokens` or refuses the call.
   */
  "output_token_cap"?: (number | null);
  /**
   * Whether the model may call several tools in one response. Unset sends
   * nothing; a wire with no such control refuses a set value.
   */
  "parallel_tool_calls"?: (boolean | null);
  /**
   * Sampling seed, a best-effort repeatability request. Carried by the
   * OpenAI-compatible Chat Completions dialect and Google's
   * `generationConfig`; every other wire refuses a set seed.
   */
  "seed"?: (number | null);
  /**
   * Literal sequences that terminate generation. Wires without a native
   * stop-sequence field refuse a non-empty list. A protocol whose grammar
   * owns the response boundary may suppress this entire list; the resulting
   * disposition is `SuppressedProtocolOwned`, not `Applied`.
   */
  "stop_sequences"?: Array<string>;
  /**
   * Sampling temperature. A wire that has no temperature field, or a model
   * that pins sampling, refuses a set temperature before any I/O; `None`
   * leaves the endpoint default in place.
   */
  "temperature"?: (NonNegativeFiniteF64 | null);
};

/**
 * Per-call account of every host generation setting: resolution supplies
 * what was requested, the adapter supplies what it emitted.
 *
 * This is request-side, adapter-owned bookkeeping and deliberately separate
 * from [`ExecutionEvidence`], which carries only facts the provider reported
 * about the execution.
 */
export type GenerationReceipt = {
  /**
   * Fate of explicit prompt-cache breakpoints in the request.
   */
  "cache"?: GenerationOptionOutcome;
  /**
   * The effective cap: the request's, else the provider options'.
   */
  "output_token_cap"?: GenerationOptionOutcome;
  "parallel_tool_calls"?: GenerationOptionOutcome;
  "passthrough"?: GenerationOptionOutcome;
  /**
   * The model spec's reasoning selection; `ProviderDefault` is not a request.
   */
  "reasoning"?: GenerationOptionOutcome;
  /**
   * The model spec's reasoning-history retention selection.
   */
  "reasoning_retention"?: GenerationOptionOutcome;
  "seed"?: GenerationOptionOutcome;
  "stop_sequences"?: GenerationOptionOutcome;
  "temperature"?: GenerationOptionOutcome;
  /**
   * `expose_thinking`'s wire half: a reasoning summary requested from the
   * provider.
   */
  "thinking_summary"?: GenerationOptionOutcome;
  /**
   * `expose_thinking`'s local half: reasoning output published to the host.
   */
  "thinking_visibility"?: GenerationOptionOutcome;
  [key: string]: unknown;
};

export type InvalidSchemaKind = ("null" | "number" | "string" | "array");

export type LimitCause = ("execution_slice" | "execution_total" | "wait_deadline");

export type LinkId = string;

/**
 * The immutable Link and selected start that determine process definition identity.
 */
export type LinkedStart = {
  "link": LinkId;
  "selector": StartSelector;
};

export type LlmOutputPart = ({
  "Text": {
    "response_meta"?: (ResponseTextMeta | null);
    "text": string;
    [key: string]: unknown;
  };
} | {
  "Reasoning": {
    "replay"?: (ProviderReasoningReplay | null);
    "text": string;
    [key: string]: unknown;
  };
} | {
  "ToolCall": {
    "call_id": string;
    "input_json": string;
    /**
     * Opaque provider replay state. Core may use `item_id` for stable
     * correlation, but provider crates own the wire semantics.
     */
    "replay"?: (ProviderReplayMeta | null);
    "tool_name": string;
    [key: string]: unknown;
  };
});

/**
 * Which way a raw provider observation travelled, with the name its side
 * of the wire gives it.
 */
export type LlmProviderTraceDirection = ({
  "direction": "request";
  "endpoint": string;
  [key: string]: unknown;
} | {
  "direction": "response";
  "event_name": string;
  [key: string]: unknown;
});

export type LlmTerminalReason = ("stop" | "tool_use" | "output_limit" | "context_overflow" | "content_filter" | "provider_error" | "cancelled" | "unknown");

export type LlmUsage = {
  "cache_read_input_tokens": number;
  "cache_write_input_tokens": number;
  "input_tokens": number;
  "output_tokens": number;
  "reasoning_output_tokens": number;
  /**
   * Model calls whose provider usage never reached lash, so none of their
   * tokens are in the counters above (ADR 0031): a sum with a nonzero count
   * is a lower bound, never a total. A single call's usage counts itself.
   */
  "unknown_calls"?: number;
  [key: string]: unknown;
};

/**
 * The number of tool calls one execution may make: the host's required
 * decision, with no default and no built-in ceiling (FIG-4546).
 *
 * What it counts depends on the execution. A cell — one code cell of a turn,
 * or one tool step of a protocol without cells — may make at most this many
 * tool calls in total. A process may hold at most this many at once: a call
 * counts from its acceptance until the process no longer depends on it, so
 * a long-running process is bounded in what it retains, never in what it
 * does over its life. The call that passes the limit fails with
 * [`ToolCallLimitExceeded`]; every call under it runs untouched. The limit
 * paces nothing: rate limits and throttling are the host's own.
 */
export type MaxToolCalls = number;

/**
 * A variable, parameter or declared-function name.
 *
 * Any text is a name; the kernel gives none of them meaning. Structural
 * validation refuses the empty name.
 */
export type Name = string;

/**
 * The requested operation hidden by a lexical binding, independent of its type.
 */
export type NamespaceShadowing = {
  "binding_name": string;
  /**
   * The originating cell, when the resolution environment retains it.
   * Name-only session environments leave this absent.
   */
  "binding_origin"?: (string | null);
  "call_path": string;
  "namespace": string;
};

export type NonNegativeFiniteF64 = number;

/**
 * A journal-safe projection of a provider/transport failure.
 */
export type NormalizedError = {
  "class": ProviderFailureKind;
  /**
   * The failure code, namespaced by who authored the spelling: `lash:` for
   * workspace-authored codes, `provider:` for provider wire vocabulary,
   * and a host's own namespace for host-authored codes.
   */
  "code"?: (FailureCode | null);
  "http_status"?: (number | null);
  "provider_request_id"?: (string | null);
  "retry_after"?: (Duration | null);
  "transport"?: (TransportFailure | null);
  [key: string]: unknown;
};

/**
 * A phase of a host process start, in the order a start passes them. Each
 * phase a start reached carries its duration on the start's producer.
 */
export type ProcessStartPhase = ("dispatch" | "admission" | "environment" | "definition_load" | "definition_resolve" | "definition_acquire" | "definition_verify" | "definition" | "admit" | "artifacts" | "link_check" | "input" | "prepare" | "commit" | "adopt" | "observe");

/**
 * How long a process start spent in one phase, in microseconds.
 */
export type ProcessStartPhaseTime = {
  "elapsed_us": number;
  "phase": ProcessStartPhase;
};

/**
 * The host call a producer span stands for.
 */
export type ProducerKind = ("send" | "send_batch" | "process_start" | "tool_intent_submit" | "plugin_invoke");

export type ProtocolPosition = (("no_response" | "response_observed" | "output_started" | "terminal_observed") | "unobserved");

/**
 * Classification of a provider/transport failure.
 *
 * This is the single canonical failure-kind vocabulary: provider transports
 * classify failures into it (`lash-core` re-exports it from
 * `llm::transport`), the turn machine carries it on
 * [`ErrorEnvelope`](crate::session_model::ErrorEnvelope), and hosts read it
 * back from `TurnIssue`s without scraping traces.
 *
 * `Unknown` is an explicit classification, never a decoder fallback.
 * Persisted failure records reject unrecognized kind literals.
 */
export type ProviderFailureKind = ("transport" | "timeout" | "http" | "stream" | "auth" | "validation" | "quota" | "unsupported" | "unknown");

export type ProviderReasoningReplay = {
  "encrypted_content"?: (string | null);
  "item_id"?: (string | null);
  /**
   * Exact LLM Provider route that minted this reasoning replay state.
   * Missing on sessions persisted before replay provenance was introduced.
   */
  "origin"?: (ProviderRouteIdentity | null);
  "redacted"?: boolean;
  "signature"?: (string | null);
  "summary"?: Array<string>;
  [key: string]: unknown;
};

export type ProviderReplayMeta = {
  "item_id"?: (string | null);
  "opaque"?: (string | null);
  /**
   * Exact LLM Provider route that minted this opaque replay state. Missing
   * on sessions persisted before replay provenance was introduced.
   */
  "origin"?: (ProviderRouteIdentity | null);
  [key: string]: unknown;
};

/**
 * Stable identity of one configured LLM Provider route.
 *
 * `endpoint` is either a normalized base URL or a host-supplied opaque route
 * id. Together with the LLM Provider kind and requested model it is the replay
 * contract. Model aliases behind one endpoint are intentionally out of scope:
 * hosts must use the exact model string they want included in route equality.
 * HTTP(S) endpoints reject userinfo because credentials are neither route
 * identity nor safe trace metadata. Explicit default ports remain distinct
 * from implicit ports. Scheme and host case, plus an empty path versus `/`,
 * normalize; path case and query strings remain identity-significant.
 */
export type ProviderRouteIdentity = {
  "endpoint": string;
  "model": string;
  "provider": string;
  [key: string]: unknown;
};

/**
 * Provider replay phase tag: the two-value vocabulary the kernel interprets
 * (`"commentary"`/`"final_answer"` on the wire). An unknown spelling fails
 * decoding rather than silently meaning "neither".
 */
export type ResponsePhase = ("commentary" | "final_answer");

export type ResponseTextMeta = {
  "id"?: (string | null);
  /**
   * Exact LLM Provider route that minted this provider-owned response
   * metadata. Missing on sessions persisted before replay provenance was
   * introduced.
   */
  "origin"?: (ProviderRouteIdentity | null);
  /**
   * Provider replay phase tag; see [`ResponsePhase`].
   */
  "phase"?: (ResponsePhase | null);
  /**
   * Provider-owned payload needed to replay this text part on a future
   * request. The kernel stores it opaquely and providers decide whether it
   * is valid for their next wire request.
   */
  "provider_payload"?: (string | null);
  "status"?: (string | null);
  [key: string]: unknown;
};

/**
 * An output history does not hold: the attachment that holds its complete
 * bytes, and the bounded witness history keeps in its place.
 *
 * The witness is what a reader of history — the model, a transcript — sees
 * of the output. It is never expanded automatically: the complete bytes are
 * read only by resolving `reference` through the session's attachment store.
 */
export type RetainedOutput = {
  /**
   * The session attachment holding the complete output.
   */
  "reference": AttachmentRef;
  /**
   * The bounded text history keeps in the output's place.
   */
  "witness": string;
};

/**
 * A code mode value retained out of history (FIG-1439): the retention of
 * its JSON encoding, and the shape of the value, which history keeps for a
 * program to read in its place.
 */
export type RetainedValue = {
  "output": RetainedOutput;
  "shape": ValueShape;
};

/**
 * Evidence that permits another provider attempt.
 */
export type RetryClass = ({
  "class": "no_response";
} | {
  "class": "rejected_http_response";
} | {
  "class": "empty_stream_partial";
} | {
  "class": "provider_idempotency";
} | {
  "class": "provider_resume";
} | {
  "attempt_number": number;
  "class": "charge_authorized";
  "tokens_at_stake": number;
});

/**
 * The sealed decision for a prospective provider retry.
 */
export type RetryDecision = ({
  "class": RetryClass;
  "delay": Duration;
  "outcome": "scheduled";
  "wait": RetryWait;
} | (({
  "cause": "not_retryable";
  [key: string]: unknown;
} | {
  "cause": "retry_budget_exhausted";
  [key: string]: unknown;
} | {
  "cause": "retry_after_exceeds_cap";
  [key: string]: unknown;
} | {
  "cause": "timed_out";
  "limit": LimitCause;
  [key: string]: unknown;
} | {
  "attempt_number": number;
  "cause": "charge_safety";
  "reason": ChargeSafetyDenialReason;
  "tokens_at_stake": number;
  [key: string]: unknown;
}) & {
  "outcome": "declined";
  [key: string]: unknown;
}));

export type RetryWait = ("throttle" | "backoff");

/**
 * Aggregate wake rule. Run-owned aggregates preserve these semantics;
 * the rule does not require a durable group dispatcher.
 *
 * `Promise.all` and `Promise.allSettled` both use [`All`](Self::All): the
 * caller's early exit on rejection is not a host wake rule. The wire shape
 * has no default, so a missing rule cannot be replayed under a guess.
 */
export type RunAggregateWakePolicy = ("first" | "first_success" | "all");

/**
 * Per-run observation. Off is unobserved, never a zero count.
 */
export type RunObservation = ("off" | "counts");

/**
 * Who a runtime runs for: a session, or a process named by its minted id.
 *
 * A session runtime and a process runtime cross the same interfaces — the
 * plugin session, the tool catalog, the attachment facade, the authority a
 * tool intent is declared under — and those interfaces take a
 * `RuntimeOwner`. A process runtime is never handed a session id as a
 * stand-in for its own identity.
 */
export type RuntimeOwner = ({
  "session": string;
} | {
  "process": string;
});

export type SchemaAdmissionError = ({
  "actual": InvalidSchemaKind;
  "kind": "invalid_kind";
  [key: string]: unknown;
} | {
  "kind": "compilation";
  "message": string;
  "schema_path": string;
  [key: string]: unknown;
} | {
  "kind": "non_local_reference";
  "reference": string;
  "schema_path": string;
  [key: string]: unknown;
} | {
  "kind": "unprojectable";
  "message": string;
  "schema_path": string;
  [key: string]: unknown;
});

/**
 * A node's address: the code it belongs to and the chain of
 * [`Node::children`] indexes that reaches it from that code's body.
 *
 * A site is derived from the document and is never written by a host
 * (`K-ID-004`). An empty chain addresses the body itself.
 */
export type Site = {
  "path"?: Array<number>;
  "unit": Unit;
};

/**
 * What a record is to the span it names.
 */
export type SpanRole = ("opens" | "closes" | "whole" | "event");

/**
 * Main requires a main block; Export names a top-level function and never runs main.
 */
export type StartSelector = ({
  "kind": "main";
} | {
  "kind": "export";
  "value": Name;
});

/**
 * The admitted body of a process step started: the step's actor committed
 * its admission, bound it to a call, and is about to run it. A step that is
 * refused never produces one, and a retried body produces one more with the
 * same site, occurrence and call and the next attempt.
 *
 * It names no language and no document: the process's own definition scopes
 * the site.
 */
export type StepBodyStarted = {
  /**
   * The occurrence of the site in the process's workflow document the
   * step runs for.
   */
  "at": EffectId;
  /**
   * The one-based attempt of the admitted body.
   */
  "attempt": number;
  /**
   * The call the admission bound the step to.
   */
  "call_id": string;
  "origin": Site;
  "process_id": string;
};

/**
 * One step of a streamed output block's lifecycle: the single payload every
 * lane carries for it — the provider stream, the session stream, turn
 * activity and the trace. A lane adds its own envelope (observation
 * identity, correlation, sequence, elapsed time) around this payload and
 * never redefines it.
 */
export type StreamBlockEvent = ({
  "block": StreamBlockIdentity;
  "kind": StreamBlockKind;
  "phase": "started";
  [key: string]: unknown;
} | {
  "block": StreamBlockIdentity;
  "kind": StreamBlockKind;
  "phase": "delta";
  "text": string;
  [key: string]: unknown;
} | {
  "block": StreamBlockIdentity;
  "kind": StreamBlockKind;
  "phase": "completed";
  "text": string;
  [key: string]: unknown;
});

/**
 * Provider-minted identity of one streamed output block.
 *
 * Minted by the provider adapter from provider facts: OpenAI
 * `(item_id, summary_index)`, Anthropic content-block index, or a
 * deterministic per-attempt ordinal for providers with no native notion.
 * The id is opaque to hosts; ordering comes from `ordinal`, never from
 * parsing `id`. Live and replayed streams emit identical identities.
 */
export type StreamBlockIdentity = {
  /**
   * Opaque provider-minted block key, stable within one response attempt.
   */
  "id": string;
  /**
   * The reasoning or message item this block belongs to, when the provider
   * has one. N blocks may share one item's replay material.
   */
  "item_id"?: (string | null);
  /**
   * Per-attempt block ordinal assigned by the provider adapter.
   */
  "ordinal": number;
  [key: string]: unknown;
};

/**
 * Which render lane a streamed output block belongs to.
 *
 * A *block* is the unit a host renders: one OpenAI reasoning summary part,
 * one Anthropic `thinking`/`text` content block, or one ordinal run of
 * thought/text for providers with no native notion. Blocks name their owning
 * reasoning item or message item when the provider has one; several blocks
 * may share one item's replay material.
 */
export type StreamBlockKind = ("assistant_text" | "reasoning");

export type TextProjectionMetadata = {
  "limit_chars": number;
  "original_chars": number;
  "projected_chars": number;
  "truncated": boolean;
  [key: string]: unknown;
};

/**
 * A tool call refused by the session's [`MaxToolCalls`]: the program asked
 * for more tool calls than its recorded limit admits.
 *
 * It is the program's failure, never the host's: the limit is recorded
 * config and the count is a fact of the program's own history, so a replay
 * or a redrive refuses the same call. Nothing of the refused calls is
 * journaled or dispatched.
 */
export type ToolCallLimitExceeded = {
  /**
   * The calls counted against the limit before the refused ones: made so
   * far by a cell, held now by a process.
   */
  "counted": number;
  /**
   * The recorded `max_tool_calls` the execution runs under.
   */
  "limit": MaxToolCalls;
  /**
   * The calls refused together: one for a single call, an aggregate's
   * unique tool calls for an aggregate.
   */
  "requested": number;
  "scope": ToolCallLimitScope;
};

/**
 * Which execution a [`MaxToolCalls`] limit refused.
 */
export type ToolCallLimitScope = ("cell" | "process");

/**
 * The terminal reply a tool check's reduction selected and every terminal
 * reply it displaced, in reduction order.
 */
export type ToolCheckConflict = {
  "displaced": Array<ToolCheckReply>;
  "phase": ToolCheckPhase;
  "winner": ToolCheckReply;
  [key: string]: unknown;
};

/**
 * The built-in tool check phase a reply was given in.
 */
export type ToolCheckPhase = ("tool_args_check" | "tool_result_check");

/**
 * One callback's reply to a tool check, attributed to its plugin.
 */
export type ToolCheckReply = {
  "callback": string;
  "plugin_id": string;
  "verdict": ToolCheckVerdictKind;
  [key: string]: unknown;
};

/**
 * What a tool check callback decided, without the decision's payload.
 */
export type ToolCheckVerdictKind = ("allow" | "cached" | "deny" | "cancel" | "abort_run");

export type ToolFailureClass = ("invalid_request" | "io" | "unavailable" | "permission_denied" | "timeout" | "execution" | "external" | "resource_limit" | "internal");

export type ToolFailureSource = ("runtime" | "tool" | "plugin" | "policy" | "cancellation");

/**
 * Generates the canonical tool-intent kind set from the vocabulary in
 * [`crate::tool_intents`].
 */
export type ToolIntentIdentity = {
  /**
   * The enclosing execution-scope id: a turn id for turn scope and a
   * process id for process scope.
   */
  "execution_scope_id": string;
  "intent_index": number;
  /**
   * Replay key of the durable invocation that minted this declaration,
   * retained so `replay_key` stays re-derivable from the record alone.
   * `None` for host-submitted intents, whose identity binds no emission.
   */
  "minting_emission_replay_key"?: (string | null);
  /**
   * The runtime whose authority declared the intent: a session, or a
   * process runtime.
   */
  "owner": RuntimeOwner;
  "replay_key": string;
  "tool_call_id": string;
  [key: string]: unknown;
};

/**
 * Literal command kind of one recorded declaration.
 *
 * Generated from [`tool_intent_variants!`]; never hand-edited.
 */
export type ToolIntentKind = ("start_process" | "cancel_process" | "get_definition" | "publish_definition");

/**
 * The attributes a tool body set on its own `execute_tool` span, through
 * its bounded writer. Held in memory with the attempt that set them and
 * stamped by the span's closer; never persisted.
 */
export type ToolSpanAnnotations = Array<ExtensionAttribute>;

export type TraceAgentFrameSwitch = {
  "frame_key": string;
  [key: string]: unknown;
};

export type TraceAttachment = {
  "byte_len": number;
  /**
   * The delivery form name. Absent on a logical summary taken before send.
   */
  "delivery_form"?: (string | null);
  "id": string;
  "media_type": string;
  /**
   * `message` or `tool_result`.
   */
  "position": string;
  [key: string]: unknown;
};

/**
 * The durable-substrate attempt a handler is executing under, as that
 * substrate's transport delivered it.
 *
 * It is an optional capability of the substrate seam: a substrate with no
 * attempt notion reports none. A record it makes carries it in its
 * position, and its span links the attempt's context; it never becomes an
 * operation's parent and is never stored.
 */
export type TraceAttemptLink = {
  /**
   * The attempt span the substrate injected, absent when its tracing is
   * off or the header was malformed.
   */
  "context"?: (TraceCarrier | null);
  /**
   * The substrate's invocation id, when the delivery names one.
   */
  "invocation_id"?: (string | null);
};

/**
 * What the provider seam observed around one dispatched attempt. The
 * attempt's facts are its sealed `AttemptRecord`, reported beside this.
 */
export type TraceAttemptObservation = {
  /**
   * Wall-clock epoch milliseconds the attempt was sealed, when the
   * provider seam timed it.
   */
  "ended_at_ms"?: (number | null);
  /**
   * The provider that served the attempt, when the provider seam named it.
   */
  "provider"?: (string | null);
  /**
   * The model the request named.
   */
  "request_model": string;
  /**
   * Wall-clock epoch milliseconds the attempt was dispatched, when the
   * provider seam timed it.
   */
  "started_at_ms"?: (number | null);
  [key: string]: unknown;
};

export type TraceBranchSelection = ("then" | "else");

export type TraceCarrier = {
  /**
   * W3C `traceparent`, version 00.
   */
  "traceparent": string;
  /**
   * W3C `tracestate`, absent when empty.
   */
  "tracestate"?: (string | null);
};

export type TraceContentBlock = ({
  "cache_breakpoint"?: boolean;
  "kind": "text";
  "text": string;
  [key: string]: unknown;
} | {
  "kind": "attachment";
  "reference": TraceAttachment;
  [key: string]: unknown;
} | {
  "call_id"?: (string | null);
  "has_signature": boolean;
  "input_json": unknown;
  "item_id"?: (string | null);
  "kind": "tool_call";
  "tool_name": string;
  [key: string]: unknown;
} | {
  "call_id"?: (string | null);
  "content": Array<TraceToolResultBlock>;
  "kind": "tool_result";
  "tool_name"?: (string | null);
  [key: string]: unknown;
} | {
  "has_encrypted": boolean;
  "item_id"?: (string | null);
  "kind": "reasoning";
  "redacted": boolean;
  "summary": Array<string>;
  "text": string;
  [key: string]: unknown;
});

/**
 * A record's correlation: the stable ids it belongs to and the effect that
 * made it.
 */
export type TraceContext = {
  "batch_id"?: (string | null);
  "call_id"?: (string | null);
  /**
   * The fact that caused the effect invocation that made the record.
   */
  "caused_by"?: (CausalRef | null);
  /**
   * A code cell's graph key.
   */
  "cell"?: (string | null);
  "effect_id"?: (string | null);
  /**
   * The workflow-graph join key of a language-execution record: the
   * observed node's structural id, equal to the `WorkflowGraph` node id
   * (ADR 0100 R0). A child start carries its parent node's id, and
   * execution start and finish leave it unset.
   */
  "graph_node_id"?: (string | null);
  "llm_call_id"?: (string | null);
  "process_id"?: (string | null);
  "protocol_iteration"?: (number | null);
  /**
   * The replay key of the effect invocation that made the record.
   */
  "replay_key"?: (string | null);
  /**
   * The turn's head input, or a producer's accepted input.
   */
  "send_id"?: (string | null);
  "session_id"?: (string | null);
  "tool_intent"?: (ToolIntentIdentity | null);
  "turn_id"?: (string | null);
  "turn_index"?: (number | null);
  [key: string]: unknown;
};

/**
 * A count observation owns the subject and coordinates that determine how
 * its counts accumulate. No optional field selects the arithmetic.
 */
export type TraceCountObservation = ({
  "delta": Array<[number, number]>;
  "frame": number;
  "process_id": string;
  "segment": string;
  "type": "process";
} | {
  "address": EffectAddress;
  "effect_id": string;
  "frame": number;
  "segment": string;
  "totals": Array<[number, number]>;
  "type": "cell";
});

/**
 * The terminal of one durable domain operation.
 */
export type TraceDomainCompletion = {
  /**
   * The typed failure code of a failed operation.
   */
  "error_code"?: (FailureCode | null);
  /**
   * Wall-clock epoch milliseconds the operation's scope retained as its
   * start.
   */
  "started_at_ms": number;
  "status": TraceDomainStatus;
  "subject": TraceDomainSubject;
  "tool_call_id"?: (string | null);
  "tool_name"?: (string | null);
  [key: string]: unknown;
};

/**
 * How a durable domain operation ended.
 */
export type TraceDomainStatus = ("completed" | "failed" | "cancelled");

/**
 * The durable domain operation a [`TraceEvent::DomainCompleted`] ends.
 */
export type TraceDomainSubject = ({
  "kind": string;
  "label"?: (string | null);
  "type": "process";
} | {
  "kind": string;
  "name"?: (string | null);
  "type": "workflow";
} | {
  "at": EffectId;
  "type": "workflow_node";
} | {
  "kind": ToolIntentKind;
  "type": "tool_intent";
});

export type TraceEffectEnvelopeDiffEntry = {
  "path": string;
  "reconstructed": TraceEffectEnvelopeDiffValue;
  "recorded": TraceEffectEnvelopeDiffValue;
  [key: string]: unknown;
};

/**
 * Structural differences between the canonical envelopes on a failed durable
 * replay validation. This event may contain prompt and tool-result values and
 * must therefore only be emitted through the extended-trace gate.
 */
export type TraceEffectEnvelopeDiffEvent = {
  "divergent_paths": Array<TraceEffectEnvelopeDiffEntry>;
  "reconstructed_envelope_hash": string;
  "recorded_envelope_hash": string;
  [key: string]: unknown;
};

/**
 * One side of a divergent canonical-envelope value.
 *
 * Large values are omitted whole. Their exact serialized length and digest
 * remain available, but no prefix is retained.
 */
export type TraceEffectEnvelopeDiffValue = ({
  "state": "missing";
  [key: string]: unknown;
} | {
  "json_len": number;
  "json_sha256": string;
  "state": "present";
  "value_json"?: unknown;
  "value_json_omitted_reason"?: (string | null);
  [key: string]: unknown;
});

export type TraceError = {
  "code"?: (FailureCode | null);
  "failure_kind": ProviderFailureKind;
  "retryable": boolean;
  "terminal_reason": LlmTerminalReason;
  [key: string]: unknown;
};

export type TraceExecToolCall = {
  "call_id": string;
  "name": string;
  "status": TraceToolCallStatus;
  [key: string]: unknown;
};

/**
 * Recorded code identity for diagnostics. Captures remain in kernel input.
 */
export type TraceExecutionCode = ({
  "kind": "start";
  "value": LinkedStart;
} | {
  "kind": "callable";
  "value": CallableId;
});

export type TraceLanguageChildExecution = {
  "attempt"?: (number | null);
  "document"?: (LinkedStart | null);
  "process_id": string;
  "scope": TraceRuntimeScope;
  [key: string]: unknown;
};

export type TraceLanguageExecution = (({
  "kind": "execution_started";
  "observation": RunObservation;
  "subject": TraceRuntimeSubject;
  [key: string]: unknown;
} | {
  "error"?: (string | null);
  "kind": "execution_finished";
  "status": TraceLanguageExecutionStatus;
  "subject": TraceRuntimeSubject;
  [key: string]: unknown;
} | {
  "at": EffectId;
  "fact": TraceNodeFact;
  "kind": "node";
  "subject": TraceRuntimeSubject;
  [key: string]: unknown;
} | {
  "counts": TraceCountObservation;
  "kind": "counts";
  [key: string]: unknown;
}) & {
  "event_key": string;
  "identity": TraceLanguageExecutionIdentity;
});

/**
 * The observed cause of a failed language node.
 */
export type TraceLanguageExecutionFailure = ({
  "class": ToolFailureClass;
  "code": string;
  "kind": "effect";
  "message": string;
  "replay_key": string;
  "source": ToolFailureSource;
  "suggested_delay_ms"?: (number | null);
  [key: string]: unknown;
} | {
  "code": string;
  "kind": "runtime";
  "message": string;
  [key: string]: unknown;
});

export type TraceLanguageExecutionIdentity = {
  "attempt"?: number;
  "document": TraceExecutionCode;
  "engine_execution_id"?: (string | null);
  "entry_name": string;
  "scope": TraceRuntimeScope;
};

export type TraceLanguageExecutionStatus = ("running" | "completed" | "failed" | "cancelled");

export type TraceLinks = {
  "contexts": Array<TraceCarrier>;
  /**
   * Contexts dropped past the retained limit.
   */
  "omitted"?: number;
};

export type TraceLlmMessage = {
  "blocks": Array<TraceContentBlock>;
  "role": string;
  [key: string]: unknown;
};

export type TraceLlmRequest = {
  "composition_fingerprint": string;
  "messages": Array<TraceLlmMessage>;
  "model": string;
  "model_variant"?: (string | null);
  "output_spec"?: unknown;
  "stream": boolean;
  "system_instructions"?: (string | null);
  "tool_choice": string;
  "tools"?: Array<TraceToolSpec>;
  [key: string]: unknown;
};

export type TraceLlmResponse = {
  "duration_ms": number;
  /**
   * Which of the caller's generation options the adapter put on the wire.
   * Absent when the adapter does not report, which is not the same as
   * reporting that nothing was requested.
   */
  "generation_disposition"?: (GenerationReceipt | null);
  /**
   * The model's output parts. Opaque provider replay payloads (encrypted
   * reasoning, signatures, provider-owned replay blobs) are not captured;
   * every other field is the part as the runtime holds it.
   */
  "parts"?: (Array<LlmOutputPart> | null);
  /**
   * The model identifier requested for the corresponding LLM call.
   */
  "request_model": string;
  "terminal_reason"?: (LlmTerminalReason | null);
  "text": string;
  [key: string]: unknown;
};

/**
 * The execution attempt's facts, with trace metadata only for an attempt
 * that reached provider dispatch. A synthetic cut between attempts has no span.
 */
export type TraceModelAttempt = {
  "error"?: (NormalizedError | null);
  "evidence"?: (ExecutionEvidence | null);
  /**
   * Which of the caller's generation options this attempt's request
   * carried, as reported by the adapter that built it.
   */
  "generation_disposition"?: (GenerationReceipt | null);
  "ordinal": number;
  "outcome": AttemptOutcome;
  "protocol_position": ProtocolPosition;
  "retry_budget_consumed": boolean;
  "retry_decision"?: (RetryDecision | null);
  "span"?: (TraceModelAttemptSpan | null);
  /**
   * Provider-reported usage only. Absence is not zero usage.
   */
  "usage"?: (LlmUsage | null);
  [key: string]: unknown;
};

/**
 * One real provider attempt's trace identity and provider-seam clocks.
 * Execution-local: none of these facts enters the stored execution ledger.
 */
export type TraceModelAttemptSpan = {
  "ended_at_ms": number;
  "provider": string;
  /**
   * The keys of the response metadata the provider leaf captured for
   * this attempt (e.g. `header:x-opper-cost`), never their values.
   */
  "response_metadata_keys"?: Array<string>;
  "span_id": W3cSpanId;
  "started_at_ms": number;
  /**
   * Lash stopped awaiting the host's terminal callback before it returned.
   */
  "terminal_delivery_timed_out": boolean;
  [key: string]: unknown;
};

/**
 * Operational request facts retained until a logical model call closes.
 * These contain no captured prompt, tool schema or host telemetry label.
 */
export type TraceModelCall = {
  "generation": GenerationOptions;
  "output_type"?: (string | null);
  "provider"?: (string | null);
  "reasoning"?: (string | null);
  "request_model": string;
  "stream": boolean;
  [key: string]: unknown;
};

export type TraceNodeAwaited = ({
  "deadline_ms"?: (number | null);
  "type": "sleep";
  [key: string]: unknown;
} | {
  "key": string;
  "name": string;
  "type": "signal";
  [key: string]: unknown;
} | {
  "process_ids": Array<string>;
  "type": "child_processes";
  [key: string]: unknown;
} | {
  "batch_id": string;
  "position": number;
  "type": "tool_batch";
  [key: string]: unknown;
} | {
  "group_key": string;
  "position": number;
  "type": "run_aggregate";
  "wake": RunAggregateWakePolicy;
  [key: string]: unknown;
});

/**
 * What happened to the occurrence a
 * [`TraceLanguageExecutionPayload::Node`] names.
 */
export type TraceNodeFact = ({
  "kind": "origin";
  "origin": Site;
  [key: string]: unknown;
} | {
  "call_id"?: (string | null);
  "kind": "started";
  [key: string]: unknown;
} | {
  "awaited": TraceNodeAwaited;
  "kind": "waiting";
  [key: string]: unknown;
} | {
  "kind": "resumed";
  "resolution": TraceNodeWaitResolution;
  [key: string]: unknown;
} | {
  "kind": "cancelled";
  [key: string]: unknown;
} | {
  "call_id"?: (string | null);
  "kind": "completed";
  [key: string]: unknown;
} | {
  "call_id"?: (string | null);
  "failure": TraceLanguageExecutionFailure;
  "kind": "failed";
  [key: string]: unknown;
} | {
  "kind": "branch_selected";
  /**
   * The arm of the branch this occurrence took.
   */
  "selected": TraceBranchSelection;
  [key: string]: unknown;
} | {
  "child": TraceLanguageChildExecution;
  "kind": "child_started";
  [key: string]: unknown;
});

export type TraceNodeWaitResolution = ("resumed" | "timed_out" | "cancelled");

/**
 * Omissions by reason: the non-zero reasons and their counts.
 */
export type TraceOmissionCounts = {
  "attributes"?: number;
  "body_bytes"?: number;
  "disallowed_key"?: number;
  "enricher_failed"?: number;
  "events"?: number;
  "held_observations"?: number;
  "hook_failed"?: number;
  "immutable"?: number;
  "key"?: number;
  "late"?: number;
  "links"?: number;
  "name"?: number;
  "non_finite"?: number;
  "publication_hold"?: number;
  "reference_bytes"?: number;
  "references"?: number;
  "reserved_key"?: number;
  "selector_failed"?: number;
  "span_bytes"?: number;
  "spans"?: number;
  "use_replace"?: number;
  "value_too_large"?: number;
  "value_truncated"?: number;
};

/**
 * One body's or one record's omissions with the address they happened at.
 * Bounded whatever the body did: one site and one fixed counter array.
 */
export type TraceOmissionDiagnostic = {
  "counts": TraceOmissionCounts;
  "site": TraceOmissionSite;
  [key: string]: unknown;
};

/**
 * Where an omission happened: a stable lash address, never a host value.
 * Two bodies or two records never share one.
 */
export type TraceOmissionSite = ({
  "at"?: (W3cSpanId | null);
  "attempt"?: (number | null);
  "call"?: (string | null);
  "key": string;
  "owner": string;
  "site": "body";
} | {
  "kind": TraceSpanKind;
  "record": string;
  "site": "record";
  "span"?: (W3cSpanId | null);
} | {
  "kind": TraceSpanKind;
  "site": "span";
  "span": W3cSpanId;
} | {
  "scope"?: (TraceScopeId | null);
  "site": "shift";
} | {
  "scope": TraceScopeId;
  "site": "admission";
} | {
  "graph_key": string;
  "site": "cell";
});

/**
 * A record's position: the span it opens, closes, is, or is an event of,
 * with that span's parent, links and real start.
 *
 * The span's flags are its trace's sampling decision. `parent` is `None`
 * only for a producer with no caller.
 */
export type TracePosition = {
  /**
   * The substrate attempt that made the record, when the substrate
   * reports one: a link, never a parent.
   */
  "attempt"?: (TraceAttemptLink | null);
  /**
   * Set on a closing or whole record: the span's exported end.
   */
  "ended_at"?: (TraceTimestamp | null);
  "kind": TraceSpanKind;
  "links"?: TraceLinks;
  "parent"?: (TraceCarrier | null);
  "role": SpanRole;
  /**
   * The span's logical owner, when a durable scope owns it.
   */
  "scope"?: (TraceScopeId | null);
  "span": TraceCarrier;
  /**
   * The span's real start; on a closing or whole record, its exported
   * start.
   */
  "started_at": TraceTimestamp;
};

export type TracePromptComponent = {
  "chars"?: (number | null);
  "hash": string;
  "id": string;
  "kind": string;
  [key: string]: unknown;
};

/**
 * Why a provider observation's parsed `raw_json` view is absent although the
 * raw bytes were observed.
 */
export type TraceProviderBodyOmission = (("size_limit" | "invalid_json") | "content_policy");

/**
 * One raw provider observation of a model call: the request Lash sent or
 * one event of the provider's response.
 */
export type TraceProviderEvent = {
  "direction": LlmProviderTraceDirection;
  "elapsed_ms": number;
  "item_id"?: (string | null);
  "output_index"?: (number | null);
  "provider": string;
  "raw_json"?: unknown;
  "raw_json_omitted_reason"?: (TraceProviderBodyOmission | null);
  "raw_len": number;
  /**
   * SHA-256 of the exact wire bytes. `raw_json` is a parsed structured
   * view whose re-serialization may not reproduce these bytes.
   */
  "raw_sha256": string;
  "sequence": number;
  [key: string]: unknown;
};

export type TraceProviderReplayDropEvent = {
  "minting_route"?: (TraceProviderRouteIdentity | null);
  "reason": TraceProviderReplayDropReason;
  "replay_kind": TraceProviderReplayKind;
  "serving_route": TraceProviderRouteIdentity;
  [key: string]: unknown;
};

export type TraceProviderReplayDropReason = ("unstamped" | "foreign_route");

/**
 * Opaque replay state rejected before an LLM Provider request was serialized.
 * A missing minting route identifies a session written before provenance was
 * introduced (or another unstamped producer); it is intentionally not
 * inferred from the currently selected route.
 */
export type TraceProviderReplayKind = ("response_text" | "reasoning" | "tool_call");

export type TraceProviderRouteIdentity = {
  "endpoint": string;
  "model": string;
  "provider": string;
  [key: string]: unknown;
};

/**
 * One tool attempt projected from its retry owner's sealed record. A model
 * call's attempts are its sealed `AttemptRecord`s, carried as they are.
 */
export type TraceRetryAttempt = {
  "delay_ms"?: (number | null);
  "detail": TraceRetryAttemptDetail;
  "ordinal": number;
};

export type TraceRetryAttemptDetail = ({
  "kind": "tool";
  "outcome": TraceToolAttemptOutcome;
});

export type TraceRuntimeScope = {
  "protocol_iteration"?: (number | null);
  "session_id"?: (string | null);
  "turn_id"?: (string | null);
  "turn_index"?: (number | null);
  [key: string]: unknown;
};

/**
 * One decoded provider stream event, in the order the runtime folded it.
 */
export type TraceRuntimeStreamEvent = {
  "elapsed_ms": number;
  "payload": TraceRuntimeStreamPayload;
  "sequence": number;
  [key: string]: unknown;
};

/**
 * What a [`TraceRuntimeStreamEvent`] reports.
 */
export type TraceRuntimeStreamPayload = ({
  "event": StreamBlockEvent;
  /**
   * The provider's text before stream transforms rewrote it, when it
   * was observed apart from the event's own text.
   */
  "raw_text"?: (string | null);
  "type": "block";
  [key: string]: unknown;
} | {
  "item_id"?: (string | null);
  "text": string;
  "type": "text_part";
  [key: string]: unknown;
} | {
  "item_id"?: (string | null);
  "text": string;
  "type": "reasoning_part";
  [key: string]: unknown;
} | {
  "call_id": string;
  "input_json": unknown;
  "item_id"?: (string | null);
  "tool_name": string;
  "type": "tool_call_part";
  [key: string]: unknown;
} | {
  "type": "usage";
  "usage": LlmUsage;
  [key: string]: unknown;
});

export type TraceRuntimeSubject = ({
  "address": EffectAddress;
  "effect_id": string;
  "type": "effect";
  [key: string]: unknown;
} | {
  "process_id": string;
  "type": "process";
  [key: string]: unknown;
});

/**
 * A trace scope's identity: its owner and the owner's boundary ordinal.
 *
 * Boundary 0 is the owner's admission. A later boundary of the same owner
 * (a process segment, a checkpoint that consumed new input) takes the next
 * ordinal the owner's own records already count. The id is separate from
 * every business identity preimage.
 */
export type TraceScopeId = {
  "boundary"?: number;
  "owner": TraceScopeOwner;
};

/**
 * The durable owner of a trace scope: the admitted thing whose work the
 * scope parents.
 */
export type TraceScopeOwner = ({
  "kind": "turn";
  "session_id": string;
  "turn_id": string;
} | {
  "call_id": string;
  "kind": "tool";
  "owner": TraceToolOwner;
} | {
  "kind": "tool_intent";
  "owner": RuntimeOwner;
  "replay_key": string;
} | {
  "kind": "process";
  "process_id": string;
});

/**
 * The kind of span a record names: one row of the span taxonomy.
 */
export type TraceSpanKind = ("producer" | "agent" | "model" | "model_attempt" | "tool" | "cell" | "process" | "workflow" | "workflow_node" | "tool_intent" | "extension" | "point");

export type TraceStoreErrorClass = ("stored_data_corrupt" | "monotonic_counter_overflow");

export type TraceTimestamp = string;

/**
 * Tool arguments observed at completion, distinct from arguments that are unavailable.
 */
export type TraceToolArgs = ({
  "availability": "value";
  "value": unknown;
  [key: string]: unknown;
} | {
  "availability": "unavailable";
  [key: string]: unknown;
});

export type TraceToolAttemptOutcome = ({
  "outcome": "completed";
} | {
  "class": ToolFailureClass;
  "code": string;
  "message": string;
  "outcome": "failed";
  "source": ToolFailureSource;
  "suggested_delay_ms"?: (number | null);
} | {
  "message": string;
  "origin"?: (CancelOrigin | null);
  "outcome": "cancelled";
  "source": ToolFailureSource;
});

export type TraceToolCallOutcome = ({
  "payload": unknown;
  "status": "success";
  [key: string]: unknown;
} | {
  "payload": TraceToolFailure;
  "status": "failure";
  [key: string]: unknown;
} | {
  "payload": TraceToolCancellation;
  "status": "cancelled";
  [key: string]: unknown;
} | {
  "status": "discarded";
  [key: string]: unknown;
});

export type TraceToolCallOutput = {
  "control"?: unknown;
  "outcome": TraceToolCallOutcome;
  [key: string]: unknown;
};

export type TraceToolCallStatus = ("success" | "failure" | "cancelled" | "discarded");

/**
 * A cancelled call as a trace carries it: where the cancellation came from
 * and whether lash forced it, which survive every content policy, hook and
 * bound, and its governed [diagnostics](TraceToolCancellation::diagnostics).
 */
export type TraceToolCancellation = {
  /**
   * `ToolResults` content: one JSON object holding the cancellation's
   * `message`, and its `raw` evidence when it has it. Governed as
   * [`TraceToolFailure::diagnostics`] is.
   */
  "diagnostics"?: unknown;
  "forced"?: boolean;
  "origin"?: (CancelOrigin | null);
  "source": ToolFailureSource;
  [key: string]: unknown;
};

/**
 * A failed call as a trace carries it: the failure's classification, which
 * is structure and survives every content policy, hook and bound, and its
 * governed [diagnostics](TraceToolFailure::diagnostics).
 */
export type TraceToolFailure = {
  "class": ToolFailureClass;
  "code": string;
  /**
   * `ToolResults` content: one JSON object holding the failure's
   * `message`, and its `cause` and `raw` evidence when it has them. A
   * content hook sees it at the `tool_results` path, and may rewrite it
   * to any JSON value. Absent when that content is not captured, or was
   * omitted, offloaded or over its bound.
   */
  "diagnostics"?: unknown;
  "source": ToolFailureSource;
  "suggested_delay_ms"?: (number | null);
  [key: string]: unknown;
};

/**
 * A tool's recorded name, or its identity when no recorded name is available.
 * Exporters use an identity to name the span, but omit `gen_ai.tool.name`.
 */
export type TraceToolName = (string | {
  "tool_id": string;
  [key: string]: unknown;
});

/**
 * The logical owner of a tool call, independent of its execution route.
 */
export type TraceToolOwner = ({
  "kind": "turn";
  "session_id": string;
  "turn_id": string;
} | {
  "kind": "process";
  "process_id": string;
} | {
  "kind": "operation";
  "operation_id": string;
  "session_id": string;
});

/**
 * One ordered block of a traced tool result: text, or an attachment at the
 * position the tool's value placed it.
 */
export type TraceToolResultBlock = ({
  "kind": "text";
  "text": string;
  [key: string]: unknown;
} | {
  "kind": "attachment";
  "reference": TraceAttachment;
  [key: string]: unknown;
});

export type TraceToolSpec = {
  "description": string;
  "input_schema": unknown;
  "name": string;
  "output_schema": unknown;
  [key: string]: unknown;
};

/**
 * Durable evidence that the traced turn was cancelled. Mirrors the runtime's
 * `TurnCancellationEvidence`, which only [`TurnStop::Cancelled`] can carry, so
 * a cancelled trace outcome can never be stated without saying which request
 * produced it.
 *
 * [`TurnStop::Cancelled`]: lash_sansio::session_model::TurnStop::Cancelled
 */
export type TraceTurnCancellationEvidence = {
  /**
   * Opaque host-domain data. Lash records and returns it unchanged.
   */
  "origin"?: (string | null);
  "reason"?: (string | null);
  "request_id": string;
  [key: string]: unknown;
};

/**
 * Why a [`TraceTurnOutcome::Completed`] turn finished.
 */
export type TraceTurnCompletionReason = (("assistant_message") | "finished");

/**
 * Why a [`TraceTurnOutcome::Failed`] turn stopped. Mirrors the non-cancelled
 * `TurnStop` reasons.
 */
export type TraceTurnFailureReason = (("incomplete" | "invalid_input" | "max_turns" | "tool_failure" | "provider_error" | "context_overflow" | "plugin_abort" | "runtime_error" | "submitted_error" | "tool_error") | "agent_frame_switch_limit");

/**
 * The single terminal outcome of a traced turn.
 *
 * One closed shape replaces the former `status` / `done_reason` /
 * `agent_frame_switch` triple: each variant owns exactly the data its state
 * can carry, so an agent-frame switch cannot be reported without a frame key
 * and a cancellation cannot be reported without its evidence. Cancellation is
 * its own variant rather than a failure reason — [`Self::is_failed`] is
 * `false` for it.
 */
export type TraceTurnOutcome = ({
  "done_reason": TraceTurnCompletionReason;
  "status": "completed";
  [key: string]: unknown;
} | {
  "frame_switch": TraceAgentFrameSwitch;
  "status": "agent_frame_switch";
  [key: string]: unknown;
} | {
  "evidence": TraceTurnCancellationEvidence;
  "status": "cancelled";
  [key: string]: unknown;
} | {
  "done_reason": TraceTurnFailureReason;
  /**
   * The refused operation and the controller's answer, as settled.
   */
  "effect_refusal"?: (EffectRefusal | null);
  /**
   * The typed check retained by a provider-validation settlement.
   */
  "provider_validation"?: (TurnFailureCode | null);
  "status": "failed";
  [key: string]: unknown;
});

/**
 * A contributed value. `Json` is the declared JSON exception of the
 * tracing seam.
 */
export type TraceValue = ({
  "type": "str";
  "value": string;
  [key: string]: unknown;
} | {
  "type": "int";
  "value": number;
  [key: string]: unknown;
} | {
  "type": "float";
  "value": number;
  [key: string]: unknown;
} | {
  "type": "bool";
  "value": boolean;
  [key: string]: unknown;
} | {
  "type": "str_array";
  "value": Array<string>;
  [key: string]: unknown;
} | {
  "type": "json";
  "value": unknown;
  [key: string]: unknown;
});

/**
 * Content-free cause observed at the HTTP transport boundary.
 */
export type TransportErrorKind = ("dns" | "connect" | "tls" | "connect_timeout" | "request_timeout" | "reset" | "body_read" | "other");

/**
 * Transport diagnostics with no URL, headers, body or rendered error text.
 */
export type TransportFailure = {
  /**
   * Monotonic time since the HTTP send began, including response body reads.
   */
  "elapsed_ms": number;
  "kind": TransportErrorKind;
  /**
   * Parsed endpoint host only, at most 253 ASCII bytes. No port or userinfo.
   */
  "server_address"?: (string | null);
  [key: string]: unknown;
};

export type TurnFailureCode = string;

export type TurnPhaseInterval = {
  "bound_bytes": number;
  "depth": number;
  "end_ns": number;
  "phase": string;
  "result_bytes": number;
  "revision"?: (number | null);
  "round_trips": number;
  "rows_affected": number;
  "rows_returned": number;
  "start_ns": number;
  /**
   * SQL directly executed in this interval, excluding nested intervals.
   */
  "statements": number;
  [key: string]: unknown;
};

/**
 * Bounded phases. Intervals overlap when one phase calls another; consumers
 * must use the deepest active interval for an exclusive wall-time table.
 */
export type TurnProfile = {
  "dml": number;
  "elapsed_ns": number;
  "link_payloads": number;
  "manifests": number;
  "omitted_intervals": number;
  "omitted_sql_keys": number;
  "phases": Array<TurnPhaseInterval>;
  "snapshots": number;
  "sql": Array<TurnSqlWork>;
  "started_at_unix_ns": number;
  "statements": number;
  /**
   * Newly installed canonical graph bytes, not WAL or filesystem bytes.
   */
  "store_bytes_written": number;
  "transactions": number;
  "writes": Array<TurnStoreWrite>;
  [key: string]: unknown;
};

export type TurnSqlWork = {
  "backend": string;
  "bound_bytes": number;
  "origin": string;
  "owner_commit": boolean;
  "phase": string;
  /**
   * Index of the exact deepest interval, so repeated phase names stay distinct.
   */
  "phase_index"?: (number | null);
  "result_bytes": number;
  "revision"?: (number | null);
  "round_trips": number;
  "rows_affected": number;
  "rows_returned": number;
  "shape": string;
  "statements": number;
  [key: string]: unknown;
};

/**
 * Canonical graph nodes physically inserted in an attempt, committed on success.
 */
export type TurnStoreWrite = {
  "bytes": number;
  "kind": string;
  "nodes": number;
  "phase": string;
  "revision"?: (number | null);
  [key: string]: unknown;
};

/**
 * The code a node belongs to.
 */
export type Unit = ("main" | {
  "function": Name;
} | {
  "library": FunctionId;
});

/**
 * What a JSON value is made of. A position that held several values — the
 * elements of an array — is their merge: its sizes are the largest
 * observed, an object key not every one of them held is `optional`, and a
 * position whose values differ in type is [`ValueShape::OneOf`] them.
 */
export type ValueShape = ({
  "type": "null";
} | {
  "type": "boolean";
} | {
  "type": "number";
} | {
  /**
   * The string's length in UTF-8 bytes.
   */
  "bytes": number;
  "type": "string";
} | {
  /**
   * The shape of the elements; absent for an empty array and for one
   * whose contents the bound counts.
   */
  "items"?: (ValueShape | null);
  "length": number;
  "type": "array";
} | {
  /**
   * The described keys, in the order first observed.
   */
  "fields"?: Array<FieldShape>;
  /**
   * Keys the bound counted instead of describing.
   */
  "omitted"?: number;
  "type": "object";
} | {
  "shapes": Array<ValueShape>;
  "type": "one_of";
});

/**
 * A nonzero W3C span id: 16 lowercase hex digits.
 */
export type W3cSpanId = string;

/**
 * The worker message an encoder refused, without its guest data.
 */
export type WorkerFrameKind = ("documents" | "refused" | "progress" | "limit_exceeded" | "ready" | "started" | "host_read" | "printed" | "parked" | "slice" | "ended" | "delivered" | "sealed" | "cancelled" | "reset_done" | "prepared");

/**
 * The VM or worker limit a run exhausted.
 */
export type WorkerLimit = (("fuel" | "heap" | "depth") | "observations" | {
  "effect_value": {
    "bound": number;
    "size": number;
    [key: string]: unknown;
  };
} | {
  "vm_state": {
    "bound": number;
    "size": number;
    [key: string]: unknown;
  };
} | {
  "frame": {
    "bound": number;
    "kind": WorkerFrameKind;
    "size": number;
    [key: string]: unknown;
  };
} | {
  "cpu": {
    "bound_ms": number;
    [key: string]: unknown;
  };
} | {
  "deadline": {
    "bound_ms": number;
    [key: string]: unknown;
  };
} | {
  "memory": {
    "bound_bytes": number;
    [key: string]: unknown;
  };
} | {
  "wall": {
    "bound_ms": number;
    [key: string]: unknown;
  };
} | {
  "attempts": {
    "bound": number;
    [key: string]: unknown;
  };
} | {
  "host_read": {
    "bound_ms": number;
    [key: string]: unknown;
  };
});
