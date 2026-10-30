// Derived from v6.schema.json by scripts/generate-workflow-schemas.py.
// JSON Schema remains authoritative for value constraints and validation.

/**
 * App-facing semantic activity emitted during a turn.
 *
 * `id` is unique per emitted activity event. `correlation_id` groups related
 * events in the same logical activity, such as code start/completion, tool
 * start/completion, or text deltas from one output block.
 */
export type TurnActivity = (({
  /**
   * The committed admission identity hosts use to follow the run.
   */
  "run_id": RunId;
  "turn_id": string;
  "type": "turn_started";
  [key: string]: unknown;
} | {
  "protocol_iteration": number;
  "type": "model_request_started";
  [key: string]: unknown;
} | {
  "protocol_iteration": number;
  "type": "checkpoint_recorded";
  [key: string]: unknown;
} | StreamBlockEvent | {
  "assistant_prose_correlation_ids": Array<TurnActivityId>;
  "reasoning_correlation_ids": Array<TurnActivityId>;
  "type": "model_attempt_reset";
  [key: string]: unknown;
} | {
  "call_id": LlmCallId;
  "type": "model_call_recorded";
  [key: string]: unknown;
} | {
  "code": string;
  "graph_key"?: (string | null);
  "language": string;
  "type": "code_block_started";
  [key: string]: unknown;
} | {
  "consumption": Array<CellConsumption>;
  /**
   * The observed invocation window, absent when unmeasured.
   */
  "duration_ms"?: (number | null);
  "graph_key"?: (string | null);
  /**
   * Provisional activity, not yet committed and never committed after an Immediate stop (ADR 0122).
   */
  "language": string;
  /**
   * Provisional activity, not yet committed and never committed after an Immediate stop (ADR 0122); absent when unavailable.
   */
  "prints"?: (Array<CellPrint> | null);
  /**
   * Provisional activity, not yet committed and never committed after an Immediate stop (ADR 0122).
   */
  "prints_retained"?: (RetainedValue | null);
  /**
   * Provisional activity, not yet committed and never committed after an Immediate stop (ADR 0122).
   */
  "result": CellOutcome;
  /**
   * Provisional activity, not yet committed and never committed after an Immediate stop (ADR 0122); absent when unavailable.
   */
  "tool_call_ids"?: (Array<string> | null);
  "type": "code_block_returned";
  [key: string]: unknown;
} | {
  "consumption": Array<CellConsumption>;
  /**
   * The observed invocation window, absent when unmeasured.
   */
  "duration_ms"?: (number | null);
  "graph_key"?: (string | null);
  "language": string;
  "prints"?: (Array<CellPrint> | null);
  "prints_retained"?: (RetainedValue | null);
  "result": CellOutcome;
  "tool_call_ids"?: (Array<string> | null);
  "type": "code_block_completed";
  [key: string]: unknown;
} | {
  "args": unknown;
  /**
   * Lash's identity for the call (ADR 0117).
   */
  "call_id": string;
  "execution"?: ToolCallExecution;
  /**
   * Graph key of the enclosing code block, when this tool call ran
   * inside one. `None` when the call did not run inside a code block.
   */
  "graph_key"?: (string | null);
  "name": string;
  /**
   * The model provider's id for the call, when a model issued it.
   */
  "provider_call_id"?: (string | null);
  "read"?: (ToolReadMetadata | null);
  "type": "tool_call_started";
  [key: string]: unknown;
} | {
  "args": unknown;
  /**
   * Lash's identity for the call (ADR 0117).
   */
  "call_id": string;
  /**
   * The observed call window, absent when unmeasured.
   */
  "duration_ms"?: (number | null);
  "execution"?: ToolCallExecution;
  /**
   * Graph key of the enclosing code block, when this tool call ran
   * inside one. `None` when the call did not run inside a code block.
   */
  "graph_key"?: (string | null);
  "name": string;
  "output": ToolCallOutput;
  /**
   * The model provider's id for the call, when a model issued it.
   */
  "provider_call_id"?: (string | null);
  "read"?: (ToolReadMetadata | null);
  "type": "tool_call_returned";
  [key: string]: unknown;
} | {
  "args": unknown;
  /**
   * Lash's identity for the call (ADR 0117).
   */
  "call_id": string;
  /**
   * The observed call window, absent when unmeasured.
   */
  "duration_ms"?: (number | null);
  /**
   * Graph key of the enclosing code block, when this tool call ran
   * inside one. `None` when the call did not run inside a code block.
   */
  "graph_key"?: (string | null);
  "name": string;
  "output": ToolCallOutput;
  /**
   * The model provider's id for the call, when a model issued it.
   */
  "provider_call_id"?: (string | null);
  "type": "tool_call_completed";
  [key: string]: unknown;
} | {
  "call_id": string;
  "outcome": ToolIntentActivityOutcome;
  "type": "tool_intent_outcome";
  [key: string]: unknown;
} | {
  "tool_name": string;
  "type": "finished";
  "value": unknown;
  [key: string]: unknown;
} | RetryProgress | {
  "event": PluginRuntimeEvent;
  "plugin_id": string;
  "type": "plugin_runtime";
  [key: string]: unknown;
} | {
  "applications": Array<TurnInputApplication>;
  "type": "queued_input_accepted";
  [key: string]: unknown;
} | ReportedFailure) & {
  "correlation_id": TurnActivityId;
  "id": TurnActivityId;
  [key: string]: unknown;
});

/**
 * Typed deterministic admission refusal retained across the plugin/host boundary.
 */
export type AdmissionRefusal = ({
  "kind": "document_validation";
  "value": Invalid;
} | {
  "kind": "document_check";
  "value": CheckRefusal;
} | {
  "kind": "helper_unavailable";
  "value": {
    "function": FunctionId;
    "reason": HelperUnavailableReason;
  };
} | {
  "kind": "callable_identity";
  "value": {
    "callable": CallableId;
  };
} | {
  "kind": "entry_annotation";
  "value": {
    "source": ENTRY_ANNOTATION_REFUSED;
  };
} | {
  "kind": "signature_required";
  "value": {
    "source": SIGNATURE_REQUIRED;
  };
} | {
  "kind": "callable_too_large";
  "value": {
    "bytes": number;
    "limit": number;
  };
} | {
  "kind": "unpublished_origin";
  "value": {
    "link": LinkId;
  };
} | {
  "kind": "missing_import";
  "value": {
    "name": QualifiedName;
    "slot": ImportSlot;
  };
} | {
  "kind": "kind_mismatch";
  "value": {
    "name": QualifiedName;
    "slot": ImportSlot;
  };
} | {
  "kind": "signature_mismatch";
  "value": {
    "mismatch"?: (SignatureMismatch | null);
    "name": QualifiedName;
    "slot": ImportSlot;
  };
} | {
  "kind": "convention_mismatch";
  "value": {
    "name": QualifiedName;
    "slot": ImportSlot;
  };
} | {
  "kind": "not_visible";
  "value": {
    "name": QualifiedName;
  };
} | {
  "kind": "path_conflict";
  "value": {
    "path": PackagePath;
  };
} | {
  "kind": "not_owner";
  "value": {
    "path": PackagePath;
  };
} | {
  "kind": "undeclared_deferred";
  "value": {
    "path": PackagePath;
  };
} | {
  "kind": "provider_not_hostable";
  "value": {
    "provider": ProviderName;
    "registry": RegistryKind;
  };
} | {
  "kind": "missing_route";
  "value": {
    "use_": OperationUse;
  };
} | {
  "kind": "contract_mismatch";
  "value": {
    "offered": ContractId;
    "pinned": ContractId;
    "use_": OperationUse;
  };
} | {
  "kind": "missing_main";
} | {
  "kind": "not_function";
  "value": {
    "export": ExportName;
  };
} | {
  "kind": "not_data_only";
  "value": {
    "export": ExportName;
  };
} | {
  "kind": "invalid_document";
  "value": {
    "message": string;
  };
} | {
  "kind": "incompatible_succession";
  "value": {
    "message": string;
  };
});

/**
 * Why a round was refused. One refused member admits no member.
 */
export type AdmissionRefusal2 = ({
  "call_id": string;
  "refusal": "duplicate_call";
} | {
  "member": number;
  "refusal": "operand_out_of_range";
  "slot": number;
} | {
  "member": number;
  "refusal": "unreferenced_member";
} | {
  "member": number;
  "refusal": "request_not_owned";
} | {
  "cause": DeclarationRefusal;
  "member": number;
  "refusal": "declaration";
} | {
  "member": number;
  "refusal": "unsupported_isolation";
} | {
  "member": number;
  "refusal": "unreduced_checks";
} | {
  "member": number;
  "refusal": "cached_not_owned";
});

/**
 * Why a written entry annotation cannot describe boundary data.
 */
export type AnnotationResolutionReason = ({
  "kind": "unresolved";
  "name": string;
} | {
  "kind": "cycle";
  "name": string;
} | {
  "construct": string;
  "kind": "unsupported";
} | {
  "kind": "limit";
});

/**
 * One artifact, by the store that holds it and its reference there.
 */
export type ArtifactName = {
  "artifact_ref": string;
  "store": ArtifactStoreId;
  [key: string]: unknown;
};

export type ArtifactReferrer = {
  "id": string;
  "kind": ArtifactReferrerKind;
  [key: string]: unknown;
};

/**
 * The referrer kinds, as the `referrer_kind` column stores them.
 */
export type ArtifactReferrerKind = ("environment_version" | "frame_environment" | "process_record" | "start" | "start_input" | "execution" | "host_pin" | "session" | "upload" | "source");

/**
 * The store that holds an artifact.
 */
export type ArtifactStoreId = ({
  "store": "process_env";
  [key: string]: unknown;
} | {
  "store": "kernel_document";
  [key: string]: unknown;
} | {
  "kind": string;
  "store": "engine";
  [key: string]: unknown;
} | {
  "store": "process_definition";
  [key: string]: unknown;
} | {
  "store": "tool_material";
  [key: string]: unknown;
} | {
  "store": "turn_prelude";
  [key: string]: unknown;
});

/**
 * Which claim of a reference its stored content failed.
 */
export type AttachmentContentMismatch = ({
  "actual": number;
  "claim": "length";
  "expected": number;
  [key: string]: unknown;
} | {
  "claim": "digest";
  [key: string]: unknown;
});

export type AttachmentRef = {
  "byte_len": number;
  "id": string;
  "label"?: (string | null);
  "media_type": string;
  "type_metadata"?: (AttachmentTypeMetadata | null);
  [key: string]: unknown;
};

/**
 * Structured attachment failure evidence. Backend diagnostics remain on the
 * runtime error's message; classification and refusal data remain typed.
 */
export type AttachmentRetentionFailure = ({
  "attachment_id": string;
  "kind": "not_found";
  [key: string]: unknown;
} | {
  "byte_len": number;
  "kind": "size_limit_exceeded";
  "max_bytes": number;
  [key: string]: unknown;
} | {
  "byte_len": number;
  "kind": "read_limit_exceeded";
  "max_bytes": number;
  [key: string]: unknown;
} | {
  "kind": "request_budget_exceeded";
  "max_bytes": number;
  [key: string]: unknown;
} | {
  "attachment_id": string;
  "kind": "delivery_unsupported";
  "media_type": string;
  [key: string]: unknown;
} | {
  "attachment_id": string;
  "detail": AttachmentContentMismatch;
  "kind": "content_mismatch";
  [key: string]: unknown;
} | {
  "kind": "io";
  "path": string;
  "raw_os_error"?: (number | null);
  [key: string]: unknown;
} | {
  "attachment_id": string;
  "kind": "referrers_operation_failed";
  "operation": string;
  "source": AttachmentRetentionStoreFailure;
  [key: string]: unknown;
} | {
  "abort_error": AttachmentRetentionStoreFailure;
  "attachment_id": string;
  "kind": "write_rollback_failed";
  "write_error": AttachmentRetentionFailure;
  [key: string]: unknown;
} | {
  "class": AttachmentStoreFailureClass;
  "kind": "backend";
  "operation": string;
  [key: string]: unknown;
} | {
  "kind": "contract";
  [key: string]: unknown;
} | {
  "kind": "root_set_enumeration_failed";
  "source": AttachmentRetentionStoreFailure;
  [key: string]: unknown;
} | {
  "kind": "root_set_operation_failed";
  "operation": string;
  "source": AttachmentRetentionStoreFailure;
  [key: string]: unknown;
} | {
  "attachment_id": string;
  "attempts": number;
  "kind": "reclamation_in_flight";
  [key: string]: unknown;
});

/**
 * A nested store cause with the store's authoritative retry class and its
 * runtime code and structured refusal. The diagnostic is carried separately.
 */
export type AttachmentRetentionStoreFailure = ({
  "cause"?: (RuntimeErrorCause | null);
  "code": string;
  "kind": "transient";
  [key: string]: unknown;
} | {
  "cause"?: (RuntimeErrorCause | null);
  "code": string;
  "kind": "refused";
  [key: string]: unknown;
});

/**
 * Why an attachment-store backend operation failed, as a property a caller can
 * act on without parsing the message.
 *
 * The retry and operator verdicts are derived from the class, never stored
 * separately, so a class and its verdicts cannot contradict each other.
 */
export type AttachmentStoreFailureClass = ("transient" | "credentials" | "terminal");

export type AttachmentTypeMetadata = ({
  "height"?: (number | null);
  "type": "image";
  "width"?: (number | null);
});

export type AwaitEventKey = {
  "key_id": string;
  "scope": ExecutionScope;
  "signature": string;
  "wait": AwaitEventWaitIdentity;
  [key: string]: unknown;
};

/**
 * The wait a Deferred source resolves: the deferring tool call it parked
 * on, by its stable call id (ADR 0117).
 */
export type AwaitEventWaitIdentity = ({
  "call_id": string;
  "type": "tool_call";
  [key: string]: unknown;
});

/**
 * Why a freshly exec'd worker could not take its place on the pipe.
 */
export type BootstrapFault = ("environment_not_empty" | "missing_descriptor" | "invalid_descriptor" | "descriptor_enumeration" | "missing_bounds" | "invalid_bounds" | "confinement");

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
 * A process cancellation fact. Its first timestamp survives retries.
 */
export type CancelRequest = {
  "origin": CancelOrigin;
  "requested_at_ms": number;
  "requester": string;
};

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
  "bound"?: (ExecutionBoundFact | null);
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
  "read_panic"?: (ReadPanic | null);
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
 * One print of an executed cell: the text the model read, the value
 * printed, and how the text was projected from it. The one shape a print
 * has in the executor's response, in a cell's committed record, in a step's
 * attachment archive and in the completion activity.
 */
export type CellPrint = {
  "projection": TextProjectionMetadata;
  "text": string;
  "value": unknown;
};

/**
 * A cell bound in its native units. Time values use nanoseconds.
 */
export type CellResource = (("charges" | "memory_bytes" | "live_tasks" | "call_depth" | "requests_per_park" | "join_members" | "worker_cpu_nanos" | "held_wall_nanos" | "compute_nanos" | "serialization_nanos" | "library_work" | "host_read_nanos" | "worker_memory_bytes" | "effects" | "checkpoint_bytes" | "tool_calls" | "effect_value_bytes" | "vm_state_bytes" | "frame_bytes") | "worker_deadline_nanos");

/**
 * A cgroup v2 controller a worker's limits need.
 */
export type CgroupController = ("memory" | "cpu");

/**
 * Why a pool cannot place its workers in the cgroup v2 subtree its host
 * named (FIG-6787). The host's deployment, never the run's.
 */
export type CgroupRefusal = ("not_cgroup_v2" | {
  "controller_not_delegated": {
    "controller": CgroupController;
    [key: string]: unknown;
  };
} | {
  "refused": {
    "code"?: (number | null);
    [key: string]: unknown;
  };
});

/**
 * One reason, and the node at fault when the fault is in a node.
 */
export type CheckFailure = {
  "reason": RefusalReason;
  "site"?: (Site | null);
};

/**
 * Everything that keeps a document from being admitted. Admission does not
 * stop at the first fault of the environment: every missing effect, every
 * mismatched signature and every missing function identity is named.
 */
export type CheckRefusal = {
  "errors": Array<CheckFailure>;
};

export type CheckpointKind = ("after_work" | "before_completion");

/**
 * Why a checkpoint root's header failed the parent's or the worker's
 * structural check.
 */
export type CheckpointRefusal = ({
  "kernel_outside_read_range": {
    "found": number;
    "reads": VersionRange;
    [key: string]: unknown;
  };
} | {
  "wrong_document": {
    "expected": string;
    "found": string;
    [key: string]: unknown;
  };
} | {
  "wrong_root_kind": {
    "found": number;
    [key: string]: unknown;
  };
} | {
  "wrong_owner": {
    "expected": string;
    "found": string;
    [key: string]: unknown;
  };
} | {
  "wrong_closure": {
    "expected": string;
    "found": string;
    [key: string]: unknown;
  };
} | {
  "malformed": {
    "problem": string;
    [key: string]: unknown;
  };
});

export type CodecRefusal = (("bad_magic") | {
  "truncated": {
    "available": number;
    "needed": number;
    [key: string]: unknown;
  };
} | {
  "frame_too_large": {
    "declared": number;
    "limit": number;
    [key: string]: unknown;
  };
} | {
  "depth_exceeded": {
    "limit": number;
    [key: string]: unknown;
  };
} | {
  "node_limit_exceeded": {
    "limit": number;
    "observed": number;
    [key: string]: unknown;
  };
} | {
  "allocation_exceeded": {
    "limit": number;
    "requested": number;
    [key: string]: unknown;
  };
} | {
  "malformed": {
    "reason": Detail;
    [key: string]: unknown;
  };
} | {
  "trailing_bytes": {
    "extra": number;
    [key: string]: unknown;
  };
} | {
  "page_batch": {
    "bytes": number;
    "pages": number;
    [key: string]: unknown;
  };
});

/**
 * Why a store, an epoch or a stored label is refused.
 *
 * Each message names its remedy with a `lashctl` command. The JSON shape is
 * what `lashctl --json` reports and what a newer build reads from an older
 * one, so a variant's fields change only in place under the version freeze.
 */
export type CompatRefusal = ({
  "component": string;
  "refusal": "unstamped";
  "writing_release"?: (string | null);
  [key: string]: unknown;
} | {
  "component": string;
  "detail": string;
  "refusal": "malformed_stamp";
  "writing_release"?: (string | null);
  [key: string]: unknown;
} | {
  "component": string;
  "found": number;
  "reads": VersionRange;
  "refusal": "too_old";
  "writing_release"?: (string | null);
  [key: string]: unknown;
} | {
  "component": string;
  "found": number;
  "refusal": "migration_pending";
  "target": number;
  "writing_release"?: (string | null);
  [key: string]: unknown;
} | {
  "component": string;
  "found": number;
  "min_reader": number;
  "reads": VersionRange;
  "refusal": "reader_floor_above";
  "writing_release"?: (string | null);
  [key: string]: unknown;
} | {
  "component": string;
  "findings": Array<string>;
  "refusal": "shape_refused";
  "writing_release"?: (string | null);
  [key: string]: unknown;
} | {
  "recorded": number;
  "refusal": "fleet_outside_writable";
  "writable": VersionRange;
  "writing_release"?: (string | null);
  [key: string]: unknown;
} | {
  "component": string;
  "refusal": "fleet_unrecorded";
  "writing_release"?: (string | null);
  [key: string]: unknown;
} | {
  /**
   * The retired layout's database files the directory holds.
   */
  "files": Array<string>;
  "location": string;
  "refusal": "retired_sqlite_layout";
  [key: string]: unknown;
} | {
  "component": string;
  "refusal": "pre_release";
  "writing_release"?: (string | null);
  [key: string]: unknown;
} | {
  "label": string;
  "refusal": "unknown_vocabulary";
  "surface": string;
  [key: string]: unknown;
} | {
  "namespace": FormatNamespace;
  "permitted": VersionRange;
  "plugin": string;
  "refusal": "plugin_writer_outside_range";
  "writer": number;
  [key: string]: unknown;
} | {
  "plugin": string;
  "refusal": "plugin_writer_unprovisioned";
  [key: string]: unknown;
} | {
  "detail": string;
  "plugin": string;
  "refusal": "plugin_writer_range_malformed";
  [key: string]: unknown;
} | {
  "permitted": VersionRange;
  "plugin": string;
  "refusal": "plugin_writer_unwritable";
  "writable": Array<number>;
  [key: string]: unknown;
});

/**
 * A refused config change: the owner it names, where it was refused and
 * why. Nothing of a refused change is published.
 */
export type ConfigRefusal = {
  "at": RefusalSite;
  "owner": string;
  "reason": ConfigRefusalReason;
};

/**
 * Why a config change was refused. Only [`Self::Owner`] carries data in the
 * owner's registered refusal type; every other reason is the framework's.
 */
export type ConfigRefusalReason = ({
  "kind": "format";
  "refusal": FormatRefusal;
} | {
  "kind": "owner";
  "message": string;
  "refusal": unknown;
} | {
  "kind": "unknown_owner";
} | {
  "kind": "unknown_command";
} | {
  "kind": "unrecorded_namespace";
} | {
  "kind": "unreadable";
  "message": string;
  "role": ConfigValueRole;
});

/**
 * A config registration that cannot stand.
 */
export type ConfigRegistrationError = ({
  "id": string;
  "kind": "duplicate_plugin_id";
  [key: string]: unknown;
} | {
  "kind": "duplicate_owner";
  "plugin_id": string;
  [key: string]: unknown;
} | {
  "command": string;
  "kind": "duplicate_command";
  "plugin_id": string;
  [key: string]: unknown;
} | {
  "command": string;
  "kind": "foreign_command";
  "plugin_id": string;
  [key: string]: unknown;
} | {
  "kind": "reserved_owner";
  "owner": string;
  [key: string]: unknown;
} | {
  "key": string;
  "kind": "foreign_run_options_key";
  "plugin_id": string;
  [key: string]: unknown;
});

/**
 * Which value of a config judgment could not be read or written as its
 * owner's type.
 */
export type ConfigValueRole = ("creation_input" | "arguments" | "run_options" | "candidate" | "output" | "refusal");

/**
 * What pins an operation's meaning under its signature: opaque text,
 * compared for equality and never read for anything else.
 */
export type ContractId = string;

/**
 * An unanswered callback at its host-configured aggregate phase deadline.
 */
export type ControlPhaseExpired = {
  "bound_ms": number;
  "callback": string;
  "phase": string;
  "plugin": string;
};

/**
 * Why a declaration, or an outcome under it, is refused.
 */
export type DeclarationRefusal = ({
  "refusal": "unarmed_source";
} | {
  "kind": ToolIntentKind;
  "refusal": "duplicate_intent";
} | {
  "refusal": "intent_order";
} | {
  "refusal": "isolated_inline_capability";
} | {
  "refusal": "undeclared_deferral";
} | {
  "kind": ToolIntentKind;
  "refusal": "undeclared_intent";
} | {
  "control": TurnControlKind;
  "refusal": "undeclared_control";
} | {
  "refusal": "undeclared_output";
} | {
  "refusal": "inline_outcome_from_isolated";
} | {
  "mismatch": ValueMismatch;
  "refusal": "finish_value_mismatch";
  /**
   * The schema the value was checked against: the admitted
   * declaration's.
   */
  "value_schema": JsonSchema;
} | {
  "mismatch": ValueMismatch;
  /**
   * The schema the value was checked against: the contract's the
   * call was admitted under.
   */
  "output_schema": JsonSchema;
  "refusal": "output_value_mismatch";
});

/**
 * Why a registration cannot be a declared start.
 */
export type DeclaredStartObligationRefusal = ("keyless" | "no_consumer_hold");

/**
 * An immutable run definition name and revision a deployment registers.
 *
 * Resolution looks up the exact revision; a worker that does not serve it
 * retries and parks, and never falls back to another revision.
 */
export type DefinitionRef = {
  "name": string;
  "revision": number;
  [key: string]: unknown;
};

/**
 * A diagnostic beside a typed cause, cut to [`Self::MAX_BYTES`]. The variant
 * that carries it decides the class; the bound keeps a refusal within any
 * frame whatever its text.
 */
export type Detail = string;

export type DocumentId = string;

export type ENTRY_ANNOTATION_REFUSED = {
  "function": FunctionLocation;
  "position": SignaturePosition;
  "reason": AnnotationResolutionReason;
  "span"?: (Span | null);
};

/**
 * The exact logical opener that durable work binds (ADR 0099 §1).
 *
 * Run admission records it, recovery validates it, and language host bridges
 * mint their identities under it. Every path shares the same owner value.
 */
export type EffectOpener = ({
  "kind": "turn";
  /**
   * The session the turn belongs to.
   */
  "session_id": string;
  /**
   * The turn's durable identity.
   */
  "turn_id": string;
  [key: string]: unknown;
} | {
  "kind": "session_operation";
  /**
   * The operation's durable identity. A plain string because no one
   * typed id names every operation; empty is refused, as
   * `ExecutionScope::validate` refuses an empty scope id.
   */
  "operation_id": string;
  /**
   * The session the operation is on.
   */
  "session_id": string;
  [key: string]: unknown;
} | {
  "kind": "process";
  /**
   * The minted id of the process that owns this work.
   */
  "process_id": string;
  [key: string]: unknown;
});

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
 * Typed refusal, artifact, fence, unsupported or infrastructure error. Infrastructure is never a guest terminal.
 */
export type EnginePortError = ({
  "kind": "package_refused";
  "value": PackageRefusal;
} | {
  "kind": "refused";
  "value": AdmissionRefusal;
} | {
  "kind": "missing";
  "value": {
    "artifact": ArtifactName;
  };
} | {
  "kind": "corrupt";
  "value": {
    "artifact": ArtifactName;
    "message": string;
  };
} | {
  "kind": "fenced";
} | {
  "kind": "unsupported";
  "value": {
    "operation": string;
  };
} | {
  "kind": "store_unavailable";
  "value": {
    "fault": StoreFault;
  };
} | {
  "kind": "store_refused";
  "value": StoreRefusal;
} | {
  "kind": "unavailable";
  "value": {
    "message": string;
  };
});

/**
 * A public entry's input or settled result violated its declared contract.
 */
export type EntryTypeMismatch = {
  "callable": CallableId;
  "mismatch": TypeMismatch;
};

/**
 * Structured error payload carried on [`SessionStreamEvent::Error`] (and
 * [`SessionStreamEvent::RetryStatus`]).
 *
 * Durability: this type appears inside persisted session snapshots and turn
 * checkpoints, so every field added after the initial shape must stay
 * additive — `#[serde(default)]` on decode and
 * `#[serde(skip_serializing_if = "Option::is_none")]` on encode — to keep
 * old snapshots decodable and new snapshots readable by older readers.
 * Transient runtime-stream signal for live consumers.
 *
 * These events may be partial, duplicated, or display-only and are not proof
 * of durable session history. Persisted semantic history uses
 * [`SessionHistoryRecord`].
 */
export type ErrorEnvelope = {
  /**
   * Namespaced failure code: `lash:` codes are this workspace's
   * [`TurnFailureCode`] vocabulary, everything else is a foreign spelling
   * carried verbatim and never reinterpreted. A bare spelling decodes as
   * the pre-cutover form — `lash` when it parses as a `TurnFailureCode`
   * arm, `provider` otherwise — so old snapshots keep decoding.
   */
  "code"?: (FailureCode | null);
  /**
   * The operation and controller answer behind an effect refusal.
   */
  "effect_refusal"?: (EffectRefusal | null);
  /**
   * Typed origin of the failure. Serializes as the same snake_case string
   * the field carried when it was a bare `String`, and an unrecognized
   * spelling decodes as [`TurnFailureKind::Unknown`] rather than failing.
   */
  "kind": TurnFailureKind;
  /**
   * Typed provider-failure classification, set only when the error came
   * from an LLM provider/transport failure whose kind was classified
   * (an unclassified `Unknown` kind is surfaced as `None`).
   */
  "provider_failure_kind"?: (ProviderFailureKind | null);
  "raw"?: (string | null);
  /**
   * Whether the failing operation is safe to retry, when the source
   * carries a typed signal (provider transports classify retryability).
   * `None` means the source did not know.
   */
  "retryable"?: (boolean | null);
  "terminal_reason"?: (LlmTerminalReason | null);
  "user_message": string;
  [key: string]: unknown;
};

/**
 * The exchange a worker answered with a message it does not admit.
 */
export type Exchange = (("deliver" | "seal" | "prepare" | "reset") | "handshake" | "run" | "host_read" | "pages");

/**
 * Why an `exec_code` effect failed before the executor produced a response.
 *
 * This is the closed classification offline trace analysis keys on; the human
 * detail lives in [`ExecCodeFailure::message`]. Serialized snake_case on the
 * durable journal and trace surfaces.
 */
export type ExecCodeFailureReason = ("executor_unavailable" | "runtime_stopped" | "session");

/**
 * The bound that ended a kernel execution, with only the receipts received.
 */
export type ExecutionBoundFact = {
  "consumed"?: (number | null);
  "function"?: (FunctionName | null);
  "limit": number;
  "observed"?: (number | null);
  "resource": CellResource;
  "site"?: (Site | null);
  [key: string]: unknown;
};

/**
 * The lease one checkout of a worker runs under. The parent mints it and
 * fences it when the worker is lost, so a late message under an old lease is
 * refused rather than applied.
 */
export type ExecutionLease = number;

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
 * A document Export, optionally a member of its TypeDef. Type members are not start selectors.
 */
export type ExportName = {
  "member"?: (Segment | null);
  "name": Name;
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

export type FormatNamespace = ("state" | "config");

/**
 * A plugin cannot interpret or encode this namespace's requested format.
 */
export type FormatRefusal = {
  "namespace": FormatNamespace;
  "plugin": string;
  "readable": number;
  "stored": number;
  [key: string]: unknown;
};

/**
 * The context frame's epoch: advances when a frame opens (F5), which fences
 * every response from the frame before it.
 */
export type FrameEpoch = number;

export type FrameKey = string;

/**
 * Why a frontier refuses a resolution.
 */
export type FrontierRefusal = ({
  "found": number;
  "owner": number;
  "refusal": "stale_publisher";
} | {
  "found": number;
  "refusal": "out_of_order";
} | {
  "found": number;
  "refusal": "receipt_mismatch";
});

export type FunctionId = string;

/**
 * Source identity before a callable has been linked.
 */
export type FunctionLocation = {
  "name"?: (string | null);
  "site"?: (Site | null);
};

export type FunctionName = string;

/**
 * Why a receiver refused a header.
 */
export type HeaderRefusal = ({
  "stale_lease": {
    "expected": ExecutionLease;
    "found": ExecutionLease;
    [key: string]: unknown;
  };
} | {
  "stale_owner_epoch": {
    "expected": OwnerEpoch;
    "found": OwnerEpoch;
    [key: string]: unknown;
  };
} | {
  "stale_frame_epoch": {
    "expected": FrameEpoch;
    "found": FrameEpoch;
    [key: string]: unknown;
  };
} | {
  "out_of_sequence": {
    "expected": TransportSequence;
    "found": TransportSequence;
    [key: string]: unknown;
  };
});

/**
 * A heap fault the worker met itself (FIG-6422), as it crosses to the
 * parent: infrastructure, never a guest outcome. The wire mirrors the
 * kernel's `lash_kernel_doc::heap::HeapFault`, which this crate cannot
 * name: the worker carries each fault's `Display`, which names the page,
 * as its `detail`.
 */
export type HeapFault = (("lease_lost") | {
  "missing": {
    "detail": Detail;
    [key: string]: unknown;
  };
} | {
  "corrupt": {
    "detail": Detail;
    [key: string]: unknown;
  };
} | {
  "unauthorized": {
    "detail": Detail;
    [key: string]: unknown;
  };
} | {
  "exhausted": {
    "resource": HostResource;
    [key: string]: unknown;
  };
} | {
  "transport": {
    "detail": Detail;
    [key: string]: unknown;
  };
} | {
  "malformed": {
    "detail": Detail;
    [key: string]: unknown;
  };
});

export type HelperOperationId = string;

/**
 * Why an exact helper cannot execute on this node.
 */
export type HelperUnavailableReason = ("retired" | "unknown" | "corrupt");

/**
 * A recorded typed hook cause: a plugin's declared error type, its version
 * and payload. Unknown payloads survive replay unchanged.
 */
export type HookCause = {
  "error_type": string;
  "error_version": number;
  "payload": unknown;
};

/**
 * A host item refused before durable enqueue or execution.
 */
export type HostPayloadTooLarge = {
  "bytes": number;
  "kind": string;
  "limit": number;
  [key: string]: unknown;
};

/**
 * The host resources a heap draws on beside guest memory, as they cross.
 */
export type HostResource = ("trace_budget" | "clean_residency" | "dirty_residency" | "pinned_pages" | "staging_bytes" | "read_batch" | "host_work");

/**
 * A stable document-local Import coordinate. Names are diagnostic, never dispatch keys.
 */
export type ImportSlot = number;

export type InfrastructureOutcome = ({
  "worker_deployment": {
    "executable": string;
    "fault": WorkerDeploymentFault;
    [key: string]: unknown;
  };
} | {
  "worker_crashed": {
    "evidence": SupervisorEvidence;
    [key: string]: unknown;
  };
} | {
  "run_crashed_worker": {
    "evidence": SupervisorEvidence;
    [key: string]: unknown;
  };
} | {
  "worker_unresponsive": {
    "silent_ms": number;
    [key: string]: unknown;
  };
} | {
  "protocol_violation": {
    "breach": ProtocolBreach;
    [key: string]: unknown;
  };
} | {
  "run_refused": {
    "refusal": RunRefusal;
    [key: string]: unknown;
  };
} | {
  "worker_limit_exceeded": {
    "limit": WorkerLimit;
    [key: string]: unknown;
  };
} | {
  "page_refused": {
    "refusal": PageRefusal;
    [key: string]: unknown;
  };
} | {
  "heap_fault": {
    "fault": HeapFault;
    [key: string]: unknown;
  };
});

/**
 * Why a document or a definition is refused, and the node at fault.
 */
export type Invalid = {
  "reason": InvalidReason;
  /**
   * The node at fault. `None` when the fault is in no node: the
   * manifest, an entry, a signature.
   */
  "site"?: (Site | null);
};

export type InvalidReason = (("empty_name" | "limit_measures_result" | "empty_formula" | "guard_without_native" | "empty_guard_unit") | {
  "document": {
    "message": string;
  };
} | {
  "import": {
    "slot": ImportSlot;
  };
} | {
  "kernel_version": {
    "found": number;
    "supported": number;
  };
} | {
  "too_deep": {
    "limit": number;
  };
} | {
  "duplicate_param": {
    "name": Name;
  };
} | {
  "duplicate_field": {
    "field": string;
  };
} | {
  "loop_control_outside_loop": {
    "keyword": string;
  };
} | {
  "call_not_native": {
    "function": FunctionId;
    "name": FunctionName;
  };
} | {
  "function_of_another_version": {
    "function": FunctionId;
    "name": FunctionName;
    "required": number;
    "written_for": number;
  };
} | {
  "function_not_listed": {
    "function": FunctionId;
  };
} | {
  "function_not_resolved": {
    "function": FunctionId;
  };
} | {
  "helper_unavailable": {
    "function": FunctionId;
    "reason": HelperUnavailableReason;
  };
} | {
  "call_arity": {
    "accepted": number;
    "given": number;
    "name": string;
    "required": number;
  };
} | {
  "effect_not_listed": {
    "slot": ImportSlot;
  };
} | {
  "function_not_declared": {
    "name": Name;
  };
} | {
  "declared_function_in_library": {
    "name": Name;
  };
} | {
  "effect_not_declared": {
    "slot": ImportSlot;
  };
} | {
  "entry_not_declared": {
    "entry": Name;
  };
} | {
  "entry_arity": {
    "declared": number;
    "entry": Name;
    "signature": number;
  };
} | {
  "invalid_type": {
    "problem": string;
  };
} | {
  "required_after_optional": {
    "name": Name;
  };
} | {
  "formula_unknown_param": {
    "name": Name;
  };
} | {
  "native_version_without_native": {
    "version": number;
  };
} | "native_version_zero" | {
  "native_takes_function": {
    "name": Name;
  };
} | {
  "native_body_may_wait": {
    "form": StatementForm;
  };
} | {
  "place_first_assignment": {
    "problem": string;
  };
} | "action_as_expression" | {
  "start_arity": {
    "declared": number;
    "signature": number;
  };
} | "start_token");

export type InvalidSchemaKind = ("null" | "number" | "string" | "array");

/**
 * A process binding that cannot honor admission.
 */
export type IsolatedStartRefusal = (("NotEngine") | {
  "Unavailable": {
    "kind": string;
    [key: string]: unknown;
  };
} | {
  "Start": {
    "cause": DeclaredStartObligationRefusal;
    [key: string]: unknown;
  };
});

export type JsonSchema = ({
  [key: string]: unknown;
} | boolean);

/**
 * The kernel's precise refusal of a checkpoint it will not resume, carried
 * typed from the worker to the parent and into the store's graph refusals.
 */
export type KernelRestoreRefusal = ({
  "helper_unavailable": {
    "function": FunctionId;
    "reason": HelperUnavailableReason;
    [key: string]: unknown;
  };
} | {
  "not_admitted": {
    "reason": string;
    [key: string]: unknown;
  };
} | {
  "document_mismatch": {
    "given": string;
    "parked": string;
    [key: string]: unknown;
  };
} | {
  "kernel_version": {
    "parked": number;
    "supported": number;
    [key: string]: unknown;
  };
} | {
  "admission_unresolved": {
    "effect": string;
    [key: string]: unknown;
  };
} | {
  "malformed": {
    "problem": string;
    [key: string]: unknown;
  };
} | {
  "missing_native": {
    "function": string;
    [key: string]: unknown;
  };
} | {
  "definition_refused": {
    "function": string;
    "problem": string;
    [key: string]: unknown;
  };
} | {
  "admission_mismatch": {
    "effect": string;
    [key: string]: unknown;
  };
});

/**
 * A deterministic rejection of a plugin-state key.
 */
export type KeyRejection = (("Empty") | {
  "TooLong": {
    "limit": number;
    [key: string]: unknown;
  };
} | {
  "IllegalCharacter": {
    "at": number;
    "byte": number;
    [key: string]: unknown;
  };
});

export type LimitCause = ("execution_slice" | "execution_total" | "wait_deadline");

export type LinkId = string;

/**
 * Lash-owned identity for one logical LLM call, spanning all transport
 * attempts made by the retry owner.
 */
export type LlmCallId = string;

/**
 * Deterministic taxonomy of effort-validation failures. The serde snake_case
 * codes are a stable contract: downstream consumers match on them.
 */
export type LlmProfileEffortValidationCategory = ("unsupported_effort" | "effort_not_configurable" | "effort_required" | "malformed_capability");

/**
 * The host's opaque name for one registered model.
 *
 * Lash never parses it: `glm-5.3-flash@tensorx` names a registration, not a
 * provider and a model. A session records the key with the metadata the
 * host's registry minted for it ([`RecordedLlmProfile`]), so the key alone never
 * decides how a recorded root runs.
 */
export type LlmProfileKey = string;

export type LlmTerminalReason = ("stop" | "tool_use" | "output_limit" | "context_overflow" | "content_filter" | "provider_error" | "cancelled" | "unknown");

export type MapType = {
  "key": Type;
  "value": Type;
};

/**
 * The integrity digest of the material's canonical bytes: 64 lowercase
 * hexadecimal digits of a BLAKE3 hash. The hashing domain belongs to the
 * material codec (FIG-4876), not to this reference.
 */
export type MaterialDigest = string;

/**
 * Where the bytes live.
 */
export type MaterialLocation = ({
  "location": "journal_local";
} | {
  "artifact": ArtifactName;
  "location": "retained_artifact";
});

/**
 * Who owns a payload: the logical Run that admitted it, the process that
 * produced it, or the Deferred source whose seal resolved to it.
 */
export type MaterialOwner = ({
  "opener": EffectOpener;
  "owner": "run";
} | {
  "owner": "process";
  "process_id": string;
} | {
  "owner": "source";
  "source": AwaitEventKey;
});

/**
 * A typed reference to one canonical payload.
 */
export type MaterialRef = {
  "digest": MaterialDigest;
  "location": MaterialLocation;
  "owner": MaterialOwner;
  "role": MaterialRole;
};

/**
 * Why recorded material cannot be served: a typed retained-result failure.
 * None of these re-executes the body that produced the material. FIG-4876
 * carries it through the runtime error framework.
 */
export type MaterialRefusal = ({
  "reference": MaterialRef;
  "refusal": "missing";
} | {
  "reference": MaterialRef;
  "refusal": "retired";
} | {
  "found": MaterialDigest;
  "reference": MaterialRef;
  "refusal": "corrupt";
} | {
  "expected": MaterialOwner;
  "reference": MaterialRef;
  "refusal": "wrong_owner";
} | {
  "expected": MaterialRole;
  "found": MaterialRole;
  "reference": MaterialRef;
  "refusal": "role_mismatch";
} | {
  "found": number;
  "reference": MaterialRef;
  "refusal": "format_mismatch";
});

/**
 * The record that produced a payload, which is also its sole canonical
 * owner record: A owns the prepared request, X the attempt output and its
 * captures, V only presentation bytes distinct from the output.
 */
export type MaterialRole = ("prepared_request" | "plugin_state_snapshot" | "attempt_output" | "check_contributions" | "presentation");

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

export type MessageOrigin = ({
  "kind": "plugin";
  "plugin_id": string;
  "transient"?: boolean;
  [key: string]: unknown;
} | {
  /**
   * The durable turn input this message was materialized from, present
   * when the input arrived through queued ingress and absent when the
   * turn was executed with its input in hand.
   */
  "input_id"?: (string | null);
  "kind": "turn_input";
  /**
   * The turn whose input this message carries.
   */
  "turn_id": string;
  [key: string]: unknown;
} | {
  /**
   * The executed cell this output is the assistant context of: the
   * [`crate::CellRecord::id`] of the cell the reply ran. `None` for
   * output that ran no cell.
   */
  "cell_id"?: (string | null);
  "kind": "turn_output";
  /**
   * The runtime or plugin that authored the output.
   */
  "source": TurnOutputSource;
  /**
   * The turn that produced this output.
   */
  "turn_id": string;
  [key: string]: unknown;
});

export type MessageRole = ("User" | "Assistant" | "System" | "Event");

/**
 * One ordered block of a tool's model-facing result: text, an attachment
 * at the position the tool's value placed it, or output retained out of
 * history (FIG-1643). The same blocks travel unchanged into the
 * transcript's one [`crate::Part::ToolResult`] and the provider request's
 * one tool-result block.
 */
export type ModelToolReturnPart = ({
  "text": string;
  "type": "text";
  [key: string]: unknown;
} | AttachmentRef | {
  /**
   * The session attachment holding the complete output.
   */
  "reference": AttachmentRef;
  "type": "retained";
  /**
   * The bounded text history keeps in the output's place.
   */
  "witness": string;
});

export type ModuleArtifactCorruption = ({
  "invalid_reference": {
    "record_kind": string;
    "reference": string;
  };
} | {
  "storage": {
    "message": string;
    "record_kind": string;
  };
});

export type ModuleArtifactRefusal = ({
  "corrupt": ModuleArtifactCorruption;
});

/**
 * A variable, parameter or declared-function name.
 *
 * Any text is a name; the kernel gives none of them meaning. Structural
 * validation refuses the empty name.
 */
export type Name = string;

/**
 * A frontier's refusal of a recorded resolution of one plugin's namespace.
 */
export type NamespaceFrontierRefusal = {
  "plugin": string;
  "refusal": FrontierRefusal;
};

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

/**
 * The run spec a frame switch's follow-on runs under (FIG-6021), in its
 * serialized form. The spec type belongs to the store vocabulary above
 * this crate, which converts it both ways; the sans-io core carries it
 * unread. The producer of the switch states it whole: inheriting the
 * switching run's shape is passing that run's recorded spec back, and a
 * different spec is exactly that spec. The switch's commit mails the
 * follow-on under it, so every retry of that commit mails the same one.
 */
export type NextRun = unknown;

/**
 * Disjoint operation namespaces. Helper bodies retain their own local import
 * slots, but execution identifies their declarations by content.
 */
export type OperationSlot = ({
  "kind": "import";
  "value": ImportSlot;
} | {
  "kind": "helper";
  "value": HelperOperationId;
});

/**
 * One authored import or helper operation declaration in one immutable Link.
 * All dispatch, modes, recorded reads and waited outcomes use this identity.
 */
export type OperationUse = {
  "link": LinkId;
  "slot": OperationSlot;
};

/**
 * The owner's epoch: advances when ownership of the session or durable
 * process moves, so a message from a superseded owner is refused.
 */
export type OwnerEpoch = number;

/**
 * The planning revision refused under a still-current owner epoch.
 */
export type OwnerRevisionConflict = {
  "actor": string;
  "actual": number;
  "epoch": number;
  "expected": number;
  [key: string]: unknown;
};

export type PackagePath = string;

/**
 * Typed immutable-content and admission refusals shared by both backends.
 */
export type PackageRefusal = ({
  "kind": "session_not_live";
  "value": string;
} | {
  "kind": "identity";
  "value": string;
} | {
  "kind": "immutable";
  "value": string;
} | {
  "kind": "missing";
  "value": string;
} | {
  "kind": "carry_missing";
  "value": {
    "artifact_ref": string;
    "to": ArtifactReferrer;
  };
} | {
  "kind": "publication";
} | {
  "kind": "cycle";
} | {
  "kind": "execution_use";
} | {
  "kind": "resolution_target";
} | {
  "kind": "referrer_ended";
  "value": ArtifactReferrer;
});

/**
 * Why the parent refused a page exchange. Every refusal ends the
 * activation as infrastructure; none is a guest outcome.
 */
export type PageRefusal = (("unauthorized" | "lease_lost" | "missing" | "corrupt" | "deadline" | "store_unavailable") | {
  "batch_too_large": {
    "bytes": number;
    "pages": number;
    [key: string]: unknown;
  };
} | {
  "staging_exhausted": {
    "bytes": number;
    [key: string]: unknown;
  };
});

export type Param = {
  "name": Name;
  /**
   * The argument may be omitted, and is then absent (`K-FN-004`). No
   * required parameter follows an optional one.
   */
  "optional"?: boolean;
  "ty": Type;
};

/**
 * An ordered message part whose variant owns its kind-specific fields.
 * `id` identifies the part within its message; `content` is rendered text,
 * except on a tool result, whose content is its ordered blocks.
 * Serialization uses a flat object and rejects unknown fields and invalid
 * kind/field combinations through [`InvalidPartCombination`].
 */
export type Part = {
  "attachment"?: (PartAttachment | null);
  "blocks"?: (Array<ModelToolReturnPart> | null);
  "call_id"?: (string | null);
  "content"?: (string | null);
  "id": string;
  "kind": string;
  "provider_call_id"?: (string | null);
  "reasoning_meta"?: (ProviderReasoningReplay | null);
  "response_meta"?: (ResponseTextMeta | null);
  "status"?: (ToolCallStatus | null);
  "tool_name"?: (string | null);
  "tool_replay"?: (ProviderReplayMeta | null);
};

export type PartAttachment = {
  "reference": AttachmentRef;
};

/**
 * A typed payload inside a frame that did not encode or decode.
 */
export type PayloadKind = (("bootstrap" | "document" | "start" | "park" | "end" | "outcome" | "host_read" | "host_answer" | "printed" | "checkpoint" | "service_request" | "service_response") | "sites");

/**
 * A callback slot within one plugin, selected by its stable key.
 */
export type PluginCallbackIdentity = {
  "key": string;
  "owner": string;
};

/**
 * Why a factory's [`PluginDeclaration`] is refused.
 */
export type PluginDeclarationError = ({
  "id": string;
  "reason": "duplicate_plugin_id";
} | {
  "declared": string;
  "factory": string;
  "reason": "id_mismatch";
} | {
  "format_version": number;
  "plugin": string;
  "reason": "native_stream_state_not_readable";
} | {
  "format_version": number;
  "plugin": string;
  "reason": "native_format_not_writable";
});

/**
 * How a declared plugin failure settles, independent of its display text.
 */
export type PluginFailureClass = ("retryable" | "redrivable" | "terminal" | "parked");

/**
 * The declared code that produced a failure. Lash stamps this at dispatch.
 */
export type PluginFailureOrigin = {
  "operation": string;
  "plugin_id": string;
};

/**
 * One failed callback in registration order, including its original typed payload.
 */
export type PluginHookFailure = {
  "failure": PluginOperationFailure;
  "origin": PluginFailureOrigin;
  [key: string]: unknown;
};

export type PluginMessage = {
  "id"?: (string | null);
  "origin"?: (MessageOrigin | null);
  "parts": Array<Part>;
  "role": MessageRole;
};

/**
 * A failure whose payload remains intact when its codec is unavailable.
 */
export type PluginOperationFailure = {
  "class": PluginFailureClass;
  "code": FailureCode;
  "error_type": string;
  "error_version": number;
  "message": string;
  "origin"?: (PluginFailureOrigin | null);
  "payload": unknown;
  [key: string]: unknown;
};

export type PluginRuntimeEvent = ({
  "detail"?: (string | null);
  "key": string;
  "kind": "status";
  "label": string;
  [key: string]: unknown;
} | ToolCheckConflict | {
  "kind": "custom";
  "name": string;
  "payload": unknown;
  [key: string]: unknown;
});

/**
 * A typed plugin-state failure.
 */
export type PluginStateError = ({
  "type": "effect_owner_mismatch";
  [key: string]: unknown;
} | {
  "plugin": string;
  "refusal": FrontierRefusal;
  "type": "frontier";
  [key: string]: unknown;
} | {
  "plugin": string;
  "type": "publication_fenced";
  [key: string]: unknown;
} | {
  "plugin": string;
  "type": "unrecorded";
  [key: string]: unknown;
} | {
  "key": string;
  "reason": KeyRejection;
  "type": "invalid_key";
  [key: string]: unknown;
} | {
  "bytes": number;
  "key": string;
  "limit": number;
  "type": "value_too_large";
  [key: string]: unknown;
} | {
  "bytes": number;
  "limit": number;
  "type": "store_too_large";
  [key: string]: unknown;
} | {
  "key": string;
  "message": string;
  "type": "encode";
  [key: string]: unknown;
} | {
  "key": string;
  "message": string;
  "type": "decode";
  [key: string]: unknown;
});

/**
 * A failure of the supervising pool itself, met while it drove a worker.
 */
export type PoolFault = (("recovery" | "checkout_timed_out" | "invalid_configuration" | "unsupported_platform") | {
  "library": {
    "refusal": WorkerLibraryRefusal;
    [key: string]: unknown;
  };
} | {
  "queue_full": {
    "bytes": number;
    [key: string]: unknown;
  };
} | {
  "restart_storm": {
    "first": InfrastructureOutcome;
    [key: string]: unknown;
  };
} | {
  "io": {
    "code"?: (number | null);
    [key: string]: unknown;
  };
} | {
  "cgroup": {
    "refusal": CgroupRefusal;
    [key: string]: unknown;
  };
});

export type PreflightIssue = ({
  "MissingService": {
    "owner": string;
    "service": ServiceKind;
    [key: string]: unknown;
  };
} | {
  "MissingPort": {
    "port": RequiredPort;
    [key: string]: unknown;
  };
} | {
  "DuplicatePort": {
    "owners": Array<string>;
    "port": RequiredPort;
    [key: string]: unknown;
  };
});

/**
 * Opaque position in a store's Process Change Feed.
 *
 * The wrapped sequence is meaningful only to the registry backend that issued
 * it. Backends expose constructors/accessors so external store implementations
 * can persist and bind the position, but consumers should treat values as
 * cursors, not comparable timestamps.
 */
export type ProcessChangeCursor = number;

/**
 * One immutable definition as a holder sees it: its id and its signature.
 *
 * Returned by create, get and publish with the derived signature. When a
 * value carries one back in, its signature is a claim that
 * `ProcessEngineRegistry::verify_definition_claim` checks.
 */
export type ProcessDefinition = {
  "id": ProcessDefinitionId;
  "signature": ProcessSignature;
};

/**
 * The content-derived id of one immutable process definition.
 *
 * Serializes as `{"$lash_definition_id": "<spelling>"}` and refuses any other
 * field on the way in. Ordering is over the spelling.
 */
export type ProcessDefinitionId = {
  "$lash_definition_id": string;
};

/**
 * Why a process environment is refused at admission (ADR 0004; D7, D10).
 * A refused environment stores nothing.
 */
export type ProcessEnvRefusal = ({
  "issue": PreflightIssue;
  "kind": "plugin_preflight";
  [key: string]: unknown;
} | {
  "id": string;
  "kind": "duplicate_component";
  [key: string]: unknown;
} | {
  "id": string;
  "kind": "component_not_installed";
  [key: string]: unknown;
} | {
  "id": string;
  "kind": "component_retired";
  [key: string]: unknown;
} | {
  "effect": string;
  "id": string;
  "kind": "retired_component_serves";
  [key: string]: unknown;
} | {
  "id": string;
  "kind": "retired_component_has_state";
  [key: string]: unknown;
} | {
  "format": number;
  "id": string;
  "kind": "config_format_unreadable";
  "readable": Array<number>;
  [key: string]: unknown;
} | {
  "id": string;
  "kind": "config_refused";
  "reason": string;
  [key: string]: unknown;
} | {
  "id": string;
  "kind": "component_build_failed";
  "reason": string;
  [key: string]: unknown;
} | {
  "first": string;
  "kind": "duplicate_presenter";
  "second": string;
  [key: string]: unknown;
} | {
  "effect": string;
  "kind": "effect_not_linked";
  [key: string]: unknown;
} | {
  "effect": string;
  "kind": "effect_not_performed";
  [key: string]: unknown;
} | {
  "effect": string;
  "kind": "effect_unfit";
  "reason": string;
  "tool": string;
  [key: string]: unknown;
} | {
  "effect": string;
  "kind": "tool_not_served";
  "provider": string;
  "tool": string;
  [key: string]: unknown;
} | {
  "effect": string;
  "kind": "runner_not_packaged";
  "package": string;
  "reason": string;
  "tool": string;
  [key: string]: unknown;
} | {
  "effect": string;
  "kind": "contract_changed";
  "live": ContractId;
  "pinned": ContractId;
  "tool": string;
  [key: string]: unknown;
} | {
  "effect": string;
  "kind": "capture_missing";
  "tool": string;
  [key: string]: unknown;
} | {
  "codec": string;
  "effect": string;
  "kind": "binding_unreadable";
  "reason": string;
  "tool": string;
  [key: string]: unknown;
} | {
  "kind": "uncertified";
  "reason": string;
  [key: string]: unknown;
} | {
  "env_ref": string;
  "kind": "unpublished";
  [key: string]: unknown;
} | {
  "format": number;
  "id": string;
  "kind": "config_writer_outside_fleet_range";
  [key: string]: unknown;
} | {
  "kind": "policy";
  "reason": string;
  [key: string]: unknown;
} | {
  "engine": string;
  "kind": "settings_refused";
  "reason": string;
  [key: string]: unknown;
});

/**
 * A process signature as it travels on a value: a claim, never authority.
 *
 * The engine's stored artifact decides what a definition's real signature is;
 * see [`ProcessEngine::resolve`](super::engine::ProcessEngine::resolve). Core
 * compares claim against authority and refuses a disagreement before any
 * durable row exists.
 *
 * [`ProcessSignature::Unknown`] is how a holder says it asserts nothing
 * (ADR 0090's unknown process type). It is not a wildcard that matches: it is
 * the absence of a claim, so resolution adopts the authority outright. A
 * [`ProcessSignature::Known`] claim must equal the authority exactly.
 */
export type ProcessSignature = ({
  "signature": "unknown";
} | {
  "encoding": unknown;
  "signature": "known";
});

/**
 * Durable lifecycle status of a process row.
 *
 * This enum has no `#[serde(other)]` fallback arm, by design: a store that
 * silently folded an unknown status into a known one would corrupt the very
 * fold the registry exists to keep honest.
 */
export type ProcessStatus = ("running" | "waiting" | "completed" | "failed" | "cancelled" | "abandoned");

/**
 * Why a successor request is refused. A refusal records nothing, and the
 * process goes on as written.
 */
export type ProcessSuccessorRefusal = ({
  "refused": "unknown_process";
} | {
  "cause": EnginePortError;
  "refused": "admission";
} | {
  "refused": "ended";
} | {
  "refused": "not_engine";
} | {
  "kind": string;
  "refused": "unsupported";
} | {
  "refusal": ProcessEnvRefusal;
  "refused": "link";
} | {
  "effect": string;
  "refused": "admitted_step_relinked";
} | {
  "refused": "cancelling";
} | {
  "message": string;
  "refusal": unknown;
  "refused": "engine";
});

/**
 * A parent, its worker or the pool between them broke the protocol. A fresh
 * worker may keep it, so the owning invocation is redriven.
 */
export type ProtocolBreach = ({
  "frame": {
    "refusal": CodecRefusal;
    [key: string]: unknown;
  };
} | {
  "header": {
    "refusal": HeaderRefusal;
    [key: string]: unknown;
  };
} | {
  "version": {
    "refusal": ProtocolVersionRefusal;
    [key: string]: unknown;
  };
} | {
  "sequence": {
    "fault": SequenceFault;
    [key: string]: unknown;
  };
} | {
  "unexpected": {
    "exchange": Exchange;
    "found": WorkerFrameKind;
    [key: string]: unknown;
  };
} | {
  "payload": {
    "detail": Detail;
    "payload": PayloadKind;
    [key: string]: unknown;
  };
} | {
  "checkpoint": {
    "refusal": CheckpointRefusal;
    [key: string]: unknown;
  };
} | {
  "machine": {
    "detail": Detail;
    [key: string]: unknown;
  };
} | {
  "bootstrap": {
    "fault": BootstrapFault;
    [key: string]: unknown;
  };
} | {
  "panicked": {
    "detail": Detail;
    [key: string]: unknown;
  };
} | {
  "pool": {
    "fault": PoolFault;
    [key: string]: unknown;
  };
});

/**
 * Durable protocol payload stored in session history. Its payload is
 * shared by every clone of the event (see [`ProtocolPayload`]).
 */
export type ProtocolEvent = {
  "payload": unknown;
  "plugin_id": string;
  [key: string]: unknown;
};

export type ProtocolVersionRefusal = {
  "minimum_supported_version": number;
  "parent_crate_version": string;
  "parent_version": number;
  "worker_crate_version": string;
  "worker_version": number;
  [key: string]: unknown;
};

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

/**
 * Logical owner/local provider identity, independent of the session/process hosting registry.
 */
export type ProviderName = {
  "key": string;
  "owner": string;
};

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
 * An Import name parsed by segment, optionally addressing a type member.
 */
export type QualifiedName = {
  "member": Segment;
  "package": PackagePath;
  "type_member"?: (Segment | null);
};

/**
 * A provider panic has no admitted call id or attempt ordinal.
 */
export type ReadPanic = {
  "message": string;
  "tool_name": string;
  [key: string]: unknown;
};

/**
 * A reasoning selection the recorded capability of the model under `key`
 * refuses.
 */
export type ReasoningRefused = {
  "category": LlmProfileEffortValidationCategory;
  "key": LlmProfileKey;
  "message": string;
  "reasoning": ReasoningSelection;
  [key: string]: unknown;
};

/**
 * The host-resolved reasoning choice carried from model selection to providers.
 */
export type ReasoningSelection = ("provider_default" | "disabled" | {
  "effort": string;
});

/**
 * A record either refuses, discards, or retains undeclared fields.
 */
export type RecordRest = (("closed" | "drop") | {
  "keep": Type;
});

export type RecordType = {
  "fields"?: Array<RecordTypeField>;
  /**
   * What happens to fields not named in `fields` at a boundary.
   */
  "rest"?: RecordRest;
};

export type RecordTypeField = {
  "name": string;
  /**
   * The field may be missing.
   */
  "optional"?: boolean;
  "ty": Type;
};

/**
 * A recorded callback key the running build no longer registers.
 */
export type RecordedCallbackUnavailable = {
  "available": Array<PluginCallbackIdentity>;
  "recorded": PluginCallbackIdentity;
};

/**
 * The byte family a referrer claims.
 */
export type ReferrerStore = ("artifact" | "attachment");

export type RefusalReason = ({
  "invalid": InvalidReason;
} | {
  "missing_effect": {
    "slot": ImportSlot;
  };
} | {
  "missing_function": {
    "function": FunctionId;
    "name": FunctionName;
  };
} | {
  "unbound_variable": {
    "name": Name;
  };
} | {
  "argument_type": {
    "callee": string;
    "expected": Type;
    "found": Type;
    "param": Name;
  };
} | {
  "perform_result": {
    "declared": Type;
    "slot": ImportSlot;
    "stated": Type;
  };
});

/**
 * Where a config refusal was raised.
 */
export type RefusalSite = ({
  "command": string;
  "index": number;
  "kind": "command";
} | {
  "kind": "candidate";
} | {
  "kind": "creation";
});

/**
 * Why registration refused a tool: why its manifest cannot be built, or why
 * a catalog does not take it.
 */
export type RegistrationRefused = ({
  "bound": ToolBound;
  "refusal": "missing_bound";
  "tool": string;
} | {
  "refusal": "park_without_deferral";
  "tool": string;
} | {
  "cause": DeclarationRefusal;
  "refusal": "declaration";
  "tool": string;
} | {
  "refusal": "isolated_without_engine";
  "tool": string;
} | {
  "refusal": "engine_without_isolation";
  "tool": string;
} | {
  "engine": string;
  "refusal": "unregistered_isolation_engine";
  "tool": string;
} | {
  "refusal": "control_declares_output";
  "tool": string;
} | {
  "capability": string;
  "refusal": "read_declares";
  "tool": string;
} | {
  "provider": string;
  "refusal": "read_without_reader";
  "tool": string;
});

/**
 * The registry a recorded callback resolves through.
 */
export type RegistryKind = ("session" | "process");

/**
 * Why a protocol refused to resolve the render a run's results present
 * with. `refusal` is the protocol's own refusal type, serialized, and
 * `message` its display text.
 */
export type RenderRefusal = {
  "message": string;
  "refusal": unknown;
};

/**
 * A failure reported to hosts: the single payload both host lanes carry for
 * it (`SessionStreamEvent::Error` and the turn activity's `Error`).
 */
export type ReportedFailure = {
  /**
   * The failure's typed kind, code, terminal reason, retryability and
   * provider classification, when its source classified it.
   */
  "envelope"?: (ErrorEnvelope | null);
  "message": string;
  [key: string]: unknown;
};

export type RequiredPort = ("ProtocolSession" | "ProtocolDriver");

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
 * The status a process row had when retention reclaimed it: the retired
 * subset of [`ProcessStatus`].
 *
 * A tombstone and every "no longer retained" answer carry one, so a pruned
 * process still says how it ended: a `failed` one is never reported as
 * completed.
 */
export type RetiredProcessStatus = ("completed" | "failed" | "cancelled" | "abandoned");

/**
 * A registry retirement refuses new admission or a recorded dependency.
 */
export type RetirementRefusal = ({
  "id": string;
  "reason": "admission";
  "registry": RegistryKind;
} | {
  "effect": string;
  "id": string;
  "reason": "serves";
  "registry": RegistryKind;
} | {
  "id": string;
  "reason": "has_state";
  "registry": RegistryKind;
});

/**
 * A retry that is about to wait: the single payload the provider stream and
 * both host lanes carry for it (`LlmStreamEvent::RetryStatus`,
 * `SessionStreamEvent::RetryStatus` and the turn activity's `RetryStatus`).
 */
export type RetryProgress = {
  "attempt": number;
  /**
   * The failure being retried, when its source classified it.
   */
  "envelope"?: (ErrorEnvelope | null);
  "max_attempts": number;
  "reason": string;
  "wait_seconds": number;
  [key: string]: unknown;
};

/**
 * One of the bounds a run states and its worker pool admits up to: a
 * field of [`crate::RunBounds`].
 */
export type RunBound = ("charge" | "memory" | "call_depth" | "live_tasks" | "requests_per_park" | "join_members");

/**
 * Why a Run's frame cannot proceed: an earlier frame of its invocation
 * failed.
 */
export type RunCutRefusal = ("InvocationFailed");

/**
 * Why a registered definition refused to shape a run: deterministic, so it
 * is recorded as the run's failure. `refusal` is the definition's own
 * refusal type, serialized, and `message` its display text.
 */
export type RunDefinitionRefusal = {
  "definition": DefinitionRef;
  "message": string;
  "refusal": unknown;
};

/**
 * One logical Run: a turn or operation and every physical segment it owns.
 * Its spelling is retained across restart and redrive.
 */
export type RunId = string;

/**
 * An input of the run that the worker reads when it starts.
 */
export type RunInput = ("document" | "start" | "checkpoint");

/**
 * The run's own inputs are refused. They are the same on every attempt, so
 * redriving the invocation refuses them again: the refusal is terminal.
 */
export type RunRefusal = ({
  "document_not_supplied": {
    "document": DocumentId;
    [key: string]: unknown;
  };
} | {
  "image_document_missing": {
    "document": DocumentId;
    [key: string]: unknown;
  };
} | {
  "unusable_schema": {
    "source": SchemaAdmissionError;
    [key: string]: unknown;
  };
} | {
  "checkpoint": {
    "refusal": CheckpointRefusal;
    [key: string]: unknown;
  };
} | {
  "dialect_not_installed": {
    "dialect": string;
    [key: string]: unknown;
  };
} | {
  "restore": {
    "refusal": KernelRestoreRefusal;
    [key: string]: unknown;
  };
} | {
  "undecodable": {
    "detail": Detail;
    "input": RunInput;
    [key: string]: unknown;
  };
} | {
  "payload_too_large": {
    "limit": number;
    "size": number;
    [key: string]: unknown;
  };
} | {
  "start": {
    "detail": Detail;
    [key: string]: unknown;
  };
} | {
  "resume": {
    "detail": Detail;
    [key: string]: unknown;
  };
} | {
  "above_ceiling": {
    "bound": RunBound;
    "ceiling": number;
    "requested": number;
    [key: string]: unknown;
  };
});

/**
 * Why a run's shape was refused: deterministic, so it is the run's
 * recorded failure, carried typed as
 * [`RuntimeErrorCause::RunShapeRefused`](crate::RuntimeErrorCause::RunShapeRefused).
 */
export type RunShapeRefusal = ({
  "kind": "definition";
  "refusal": RunDefinitionRefusal;
  [key: string]: unknown;
} | {
  "kind": "owner";
  "refusal": ConfigRefusal;
  [key: string]: unknown;
} | {
  "kind": "reasoning";
  "refusal": ReasoningRefused;
  [key: string]: unknown;
} | {
  "kind": "reasoning_without_llm_profile";
  [key: string]: unknown;
} | {
  "kind": "render";
  "refusal": RenderRefusal;
  [key: string]: unknown;
});

/**
 * Compact, content-free mismatch evidence retained on the controller error.
 */
export type RuntimeEffectReplayMismatchReport = {
  "divergent_path_count": number;
  /**
   * The kind of the recorded effect whose envelope diverged (its command
   * `type`, e.g. `llm_call`, `tool_invocation`), so an operator can tell a
   * model-call drift from a tool or cell drift (FIG-3587).
   */
  "effect_kind"?: (string | null);
  "first_divergent_paths": Array<string>;
  [key: string]: unknown;
};

/**
 * Typed cause retained when a controller-owned runtime effect must abort
 * through the generic runtime error boundary.
 *
 * Attachment retention and worker causes keep their source's retry class.
 * Other causes are terminal except for an unavailable recorded model.
 */
export type RuntimeErrorCause = ({
  "kind": "host_payload_too_large";
  "refusal": HostPayloadTooLarge;
  [key: string]: unknown;
} | OwnerRevisionConflict | {
  "kind": "package_store_refused";
  "refusal": PackageRefusal;
  [key: string]: unknown;
} | {
  "kind": "plugin_retired";
  "refusal": RetirementRefusal;
  [key: string]: unknown;
} | {
  "expiry": ControlPhaseExpired;
  "kind": "control_phase_expired";
  [key: string]: unknown;
} | {
  "kind": "tool_run_cut_refused";
  "refusal": RunCutRefusal;
  [key: string]: unknown;
} | {
  "kind": "tool_run_isolation_refused";
  "refusal": IsolatedStartRefusal;
  [key: string]: unknown;
} | {
  "aborted": boolean;
  "call_id": string;
  "cause"?: (HookCause | null);
  "kind": "tool_run_control";
  [key: string]: unknown;
} | {
  "kind": "tool_run_admission_refused";
  "refusal": AdmissionRefusal2;
  [key: string]: unknown;
} | {
  "kind": "material_refused";
  "refusal": MaterialRefusal;
  [key: string]: unknown;
} | {
  "kind": "plugin_state_unrecorded";
  "plugin": string;
  [key: string]: unknown;
} | {
  "kind": "plugin_state_effect_owner_mismatch";
  [key: string]: unknown;
} | {
  "kind": "plugin_state_frontier";
  "refusal": NamespaceFrontierRefusal;
  [key: string]: unknown;
} | {
  "kind": "plugin_state_publication_fenced";
  "plugin": string;
  [key: string]: unknown;
} | {
  "kind": "plugin_state";
  "refusal": PluginStateError;
  [key: string]: unknown;
} | {
  "code"?: (FailureCode | null);
  "failure_kind": ProviderFailureKind;
  "kind": "provider_failure";
  "retryable": boolean;
  "terminal_reason": LlmTerminalReason;
  [key: string]: unknown;
} | {
  "failure": PluginOperationFailure;
  "kind": "plugin_operation";
  [key: string]: unknown;
} | {
  "causes": Array<PluginHookFailure>;
  "kind": "plugin_hooks";
  [key: string]: unknown;
} | {
  "kind": "vm_worker";
  "outcome": InfrastructureOutcome;
  [key: string]: unknown;
} | {
  "kind": "cell_snapshot_undecodable";
  "refusal": RunRefusal;
  [key: string]: unknown;
} | {
  "kind": "recorded_callback_unavailable";
  "refusal": RecordedCallbackUnavailable;
  [key: string]: unknown;
} | {
  "kind": "stream_state_format_unreadable";
  "refusal": StreamStateFormatRefusal;
  [key: string]: unknown;
} | {
  "kind": "plugin_format";
  "refusal": FormatRefusal;
  [key: string]: unknown;
} | {
  "kind": "process_parent_ended";
  "parent": ScopeId;
  "start_key"?: (StartKey | null);
  [key: string]: unknown;
} | {
  "kind": "process_env_refused";
  "refusal": ProcessEnvRefusal;
  [key: string]: unknown;
} | {
  "kind": "process_start_key_conflict";
  "start_key": StartKey;
  [key: string]: unknown;
} | {
  "failure": AttachmentRetentionFailure;
  "kind": "attachment_retention";
  [key: string]: unknown;
} | {
  "kind": "module_artifact_refused";
  "refusal": ModuleArtifactRefusal;
  [key: string]: unknown;
} | {
  "kind": "schema_refused";
  "source": SchemaAdmissionError;
  [key: string]: unknown;
} | {
  "kind": "tool_schema_refused";
  "source": ToolCatalogBuildError;
  [key: string]: unknown;
} | {
  "context": string;
  "kind": "value_mismatch";
  "source": ValueMismatch;
  [key: string]: unknown;
} | {
  "ingress_kind": string;
  "kind": "ingress_reserved_source_key";
  "session_id": string;
  "source_key": string;
  [key: string]: unknown;
} | {
  "input_id": string;
  "kind": "turn_input_forsaken";
  [key: string]: unknown;
} | {
  "kind": "store_refusal";
  "refusal": StoreRefusal;
  [key: string]: unknown;
} | {
  "kind": "stored_data_corrupt";
  "message": string;
  "record_kind": string;
  [key: string]: unknown;
} | {
  "engine_kind": string;
  "kind": "missing_recorded_process_config";
  [key: string]: unknown;
} | {
  "kind": "compat";
  "refusal": CompatRefusal;
  [key: string]: unknown;
} | {
  "kind": "session_deleted";
  "session_id": string;
  [key: string]: unknown;
} | {
  "kind": "artifact_referrer_ended";
  "referrer": ArtifactReferrer;
  [key: string]: unknown;
} | {
  "kind": "llm_profile_unavailable";
  "profile_key": LlmProfileKey;
  [key: string]: unknown;
} | {
  "exceeded": ToolCallLimitExceeded;
  "kind": "max_tool_calls_exceeded";
  [key: string]: unknown;
} | {
  "kind": "run_shape_refused";
  "refusal": RunShapeRefusal;
  [key: string]: unknown;
} | {
  "kind": "config_refused";
  "refusal": ConfigRefusal;
  [key: string]: unknown;
});

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

/**
 * Only entry promotion requires a written parameter and settled-result type.
 * Producers resolve explicit any/unknown to Json before calling this seam.
 */
export type SIGNATURE_REQUIRED = {
  "function": FunctionLocation;
  "position": SignaturePosition;
  "span"?: (Span | null);
};

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

export type SchemaPurpose = ("tool_input" | "tool_output" | "structured_output" | "prompt_schema");

/**
 * A scope a process may live until.
 *
 * An effect opener (a logical turn run, a session operation, one process)
 * or a session. A session is never a tool Run opener:
 * it owns lifetimes, not effects.
 */
export type ScopeId = ({
  "kind": "opener";
  "scope": EffectOpener;
  [key: string]: unknown;
} | {
  "kind": "session";
  "scope": string;
  [key: string]: unknown;
});

export type Segment = string;

/**
 * A message that arrived out of order, or an exchange whose accounting does
 * not add up.
 */
export type SequenceFault = ("idle_without_start" | "missing_lease" | "missing_owner" | "start_before_reset" | "prepare_before_reset" | "checkout_already_started" | "no_pending_read" | "wrong_read_id" | "repeated_read_id" | "read_other_control" | "no_pending_pages" | "wrong_page_exchange" | "no_run" | "run_ended" | "invalid_phase" | "cpu_accounting_regressed" | "failed_worker_reset" | "end_in_partial_frame" | "incomplete_frame_header" | "lease_space_exhausted" | "unexpected_service_response" | "receipt_probe_overflow");

export type ServiceKind = ("TurnScope" | "CurrentSessionSnapshot" | "NamedSessionSnapshot" | "RecordedToolCatalog" | "CommittedTurnRead" | "SessionCreate" | "SessionGraphAppend" | "ProcessList" | "ProcessStart" | "ProcessRecordedStart" | "ProcessBoundStart" | "ProcessBoundCancel" | "ProcessAwait" | "ProcessCancel" | "ProcessTransfer" | "ProcessReleaseHold" | "ProcessAbandonHold" | "GrantedSessionReads" | "ProcessListForAttempt" | "ProcessStartFromRequest" | "ProcessValidateVisible" | "ProcessRecordedCancel");

/**
 * Typed node accepted at session-graph append boundaries.
 *
 * Its semantic fields are projected by Lash's versioned append-request
 * identity encoder. Adding or changing a variant or nested semantic field
 * requires an identity encoding version bump and replacement golden corpus;
 * serde representation itself is deliberately not the identity format.
 */
export type SessionAppendNode = ({
  "kind": "message";
  "message": PluginMessage;
  [key: string]: unknown;
} | {
  "event": ProtocolEvent;
  "kind": "protocol_event";
  [key: string]: unknown;
} | {
  "body"?: unknown;
  "kind": "plugin";
  "plugin_type": string;
  [key: string]: unknown;
});

/**
 * What owns a session's head, refusing a head write outside every run.
 */
export type SessionHeadOwner = ({
  "owner": "run";
  "run": string;
  [key: string]: unknown;
} | {
  "epoch": number;
  "owner": "actor";
  [key: string]: unknown;
} | {
  "enqueue_seq": number;
  "owner": "command_lane";
  [key: string]: unknown;
});

/**
 * What a function, an effect or an entry takes and returns.
 */
export type Signature = {
  "params"?: Array<Param>;
  "result": Type;
};

/**
 * The first incompatible part found by the signature's serving relation.
 * `path` uses JSON pointer escaping in the projected type structure:
 * `/input`, `/output`, and their properties, items or additional properties.
 * The types are the kernel's projections, not the original JSON schemas.
 */
export type SignatureMismatch = {
  "expected": Type;
  "path": string;
  "provided": Type;
  "side": SignatureSide;
};

export type SignaturePosition = ({
  "index": number;
  "kind": "parameter";
  "name": Name;
} | {
  "kind": "result";
});

/**
 * The side of an effect signature that cannot serve its declaration.
 */
export type SignatureSide = ("input" | "output");

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
 * UTF-8 byte offsets in authored or printed source.
 */
export type Span = {
  "end": number;
  "start": number;
};

/**
 * The idempotency key of one process start (ADR 0107).
 *
 * A start key is never a reference: nothing resolves a key to a process. The
 * registrar maps a key to the process it minted for it while that process is
 * retained, so a retried start returns the retained process instead of
 * starting a second one, and after the process is pruned the same key starts
 * a new process with a new id.
 *
 * A key lash derives from an admitted operation (a tool intent, an isolated
 * call) is trusted: a retry under it returns the retained process
 * whatever it submitted. A host's key (one it supplied, or its keyless
 * start's derived key) fences its start: a retry under it returns the
 * retained process only if it presents the same start, and is otherwise a
 * [`StartKeyConflict`](crate::runtime_error::RuntimeErrorCode::ProcessStartKeyConflict)
 * that names nothing but the key.
 *
 * Every key is a framed digest in one family, with each start path in its own
 * namespace, so a host-supplied key can never collide with one lash derives
 * for a tool intent or isolated call. The digest is over admitted
 * operation identity only — never over submitted content, source or compiler
 * identity, or the minted result. Only [`StartKey::for_host`] is open to a
 * host; every other family is derived under a [`StartKeyDerivation`].
 */
export type StartKey = string;

/**
 * An action form, as a refusal names one the body of a function with a
 * native implementation may not run (`K-LIB-004`).
 */
export type StatementForm = ("call" | "perform" | "join" | "yield" | "spawn" | "cancel" | "cancel_effect" | "await");

/**
 * A native database error code, independent of diagnostic message text.
 */
export type StorageCode = ({
  "backend": "postgres";
  "class": string;
  "sqlstate": string;
  [key: string]: unknown;
} | {
  "backend": "sqlite";
  "extended_code": number;
  [key: string]: unknown;
});

/**
 * A fault of the storage substrate that stays typed past the store, with
 * the same fields as its store error. A plugin error is cloned and
 * journaled, so it carries this rather than the store error: the identical
 * operation may succeed when it is made again, and nothing about it is the
 * operation's answer.
 */
export type StoreFault = ({
  "type": "contended";
  [key: string]: unknown;
} | {
  "database": string;
  "location": string;
  "type": "migration_open_elsewhere";
  [key: string]: unknown;
} | {
  "backend": string;
  /**
   * Native database code; absent for driver, pool, I/O or decode failures.
   */
  "code"?: (StorageCode | null);
  "message": string;
  "type": "storage_failure";
  [key: string]: unknown;
} | {
  "message": string;
  "type": "backend";
  [key: string]: unknown;
});

/**
 * A store refusal that stays typed past the store, with the same fields as
 * its store error: a compatibility, session identity or cancellation
 * authority refusal. Plugin errors are cloned and journaled, so
 * this representation carries only the terminal refusals rather than backend
 * failures and sources.
 */
export type StoreRefusal = ({
  "revision": number;
  "type": "turn_base_not_retained";
  [key: string]: unknown;
} | {
  "kind": ArtifactReferrerKind;
  "store": ReferrerStore;
  "type": "referrer_kind_refused";
  [key: string]: unknown;
} | {
  "refusal": CompatRefusal;
  "type": "incompatible";
  [key: string]: unknown;
} | {
  "recorded": number;
  "type": "writer_fenced";
  "writable": VersionRange;
  [key: string]: unknown;
} | {
  "loaded": string;
  "requested": string;
  "type": "store_session_mismatch";
  [key: string]: unknown;
} | {
  "current": number;
  "found": number;
  "type": "session_state_version_unsupported";
  [key: string]: unknown;
} | {
  "current": number;
  "found": number;
  "type": "session_state_version_newer_than_runtime";
  [key: string]: unknown;
});

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

/**
 * Opaque stream-hook state whose format the receiving plugin cannot read.
 */
export type StreamStateFormatRefusal = {
  "callback": PluginCallbackIdentity;
  "readable": Array<number>;
  "stored": number;
};

/**
 * What the supervisor observed of a worker's end. Evidence, never testimony:
 * a worker cannot claim how it ended.
 */
export type SupervisorEvidence = ("end_of_stream" | {
  "exited": {
    "code": number;
    [key: string]: unknown;
  };
} | {
  "signalled": {
    "signal": number;
    [key: string]: unknown;
  };
} | "forbidden_syscall");

/**
 * Two tools a surface would offer as one operation: refused, naming both
 * tools and both providers. The host fixes it by reconfiguring one
 * provider.
 */
export type SurfaceCollision = {
  "first_provider": ToolProviderHandle;
  "first_tool": string;
  "operation": string;
  "second_provider": ToolProviderHandle;
  "second_tool": string;
  [key: string]: unknown;
};

/**
 * A surface entry whose tool id another entry already offers.
 */
export type SurfaceDuplicateTool = {
  "first_provider": ToolProviderHandle;
  "second_provider": ToolProviderHandle;
  "tool": string;
  [key: string]: unknown;
};

/**
 * Why a surface could not be built.
 */
export type SurfaceRefusal = (SurfaceCollision | SurfaceDuplicateTool);

export type TextProjectionMetadata = {
  "limit_chars": number;
  "original_chars": number;
  "projected_chars": number;
  "truncated": boolean;
  [key: string]: unknown;
};

/**
 * The bound a tool's manifest must declare, named by
 * [`RegistrationRefused::MissingBound`].
 */
export type ToolBound = ("execution" | "park");

/**
 * Whether tool-call activity describes a recorded call or an unrecorded read.
 */
export type ToolCallExecution = ("recorded" | "read");

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

export type ToolCallOutcome = ({
  "payload": unknown;
  "status": "success";
  [key: string]: unknown;
} | {
  "payload": ToolFailure;
  "status": "failure";
  [key: string]: unknown;
} | {
  "payload": ToolCancellation;
  "status": "cancelled";
  [key: string]: unknown;
});

export type ToolCallOutput = {
  "control"?: (ToolControl | null);
  "outcome": ToolCallOutcome;
  /**
   * Optional display projection for a successful result; `outcome` remains the code value.
   */
  "projection_value"?: unknown;
  "view"?: (ToolView | null);
  [key: string]: unknown;
};

export type ToolCallStatus = ("success" | "failure" | "cancelled");

export type ToolCancellation = {
  /**
   * Whether lash forced the end: a cancelled process whose engine did not
   * end it within its cancel grace (ADR 0132 §10).
   */
  "forced"?: boolean;
  "message": string;
  "origin"?: (CancelOrigin | null);
  "raw"?: unknown;
  "source": ToolFailureSource;
  [key: string]: unknown;
};

export type ToolCatalogBuildError = ({
  "InvalidOperation": {
    "message": string;
    "tool_id": string;
    [key: string]: unknown;
  };
} | {
  "UnusableSchema": {
    "operation": string;
    "purpose": SchemaPurpose;
    "source": SchemaAdmissionError;
    "tool_id": string;
    [key: string]: unknown;
  };
} | {
  "MissingOutputDeclaration": {
    "operation": string;
    "tool_id": string;
    [key: string]: unknown;
  };
} | {
  "MissingContract": {
    "operation": string;
    "tool_id": string;
    [key: string]: unknown;
  };
} | {
  "DuplicateId": {
    "tool_id": string;
    [key: string]: unknown;
  };
} | {
  "DuplicateOperation": {
    "first_tool": string;
    "operation": string;
    "second_tool": string;
    [key: string]: unknown;
  };
} | {
  "RegistrationRefused": {
    "refusal": RegistrationRefused;
    [key: string]: unknown;
  };
});

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

export type ToolControl = ({
  "control": TurnControl;
  "type": "turn";
  [key: string]: unknown;
} | {
  "failure": ToolFailure;
  "type": "fail";
  [key: string]: unknown;
} | {
  "code": FailureCode;
  "message": string;
  "type": "abort_run";
  [key: string]: unknown;
});

export type ToolFailure = {
  "cause"?: (ToolFailureCause | null);
  "class": ToolFailureClass;
  "code": string;
  "message": string;
  "raw"?: unknown;
  "source": ToolFailureSource;
  "suggested_delay_ms"?: (number | null);
  [key: string]: unknown;
};

export type ToolFailureCause = ({
  "kind": "entry_type_mismatch";
  "source": EntryTypeMismatch;
  [key: string]: unknown;
} | {
  "kind": "type_mismatch";
  "source": TypeMismatch;
  [key: string]: unknown;
} | {
  "expiry": ControlPhaseExpired;
  "kind": "control_phase_expired";
  [key: string]: unknown;
} | {
  "kind": "interrupted";
  [key: string]: unknown;
} | {
  "kind": "read_panicked";
  "panic": ReadPanic;
  [key: string]: unknown;
} | {
  "fact": ExecutionBoundFact;
  "kind": "execution_bound";
  [key: string]: unknown;
} | {
  "call_id": string;
  "kind": "panicked";
  "message": string;
  "tool_name": string;
  [key: string]: unknown;
} | {
  "engine": string;
  "kind": "engine_step_registration_unavailable";
  "step": string;
  "step_kind": string;
  [key: string]: unknown;
} | {
  "kind": "engine_step_without_engine";
  "step": string;
  "step_kind": string;
  [key: string]: unknown;
} | {
  "cause": LimitCause;
  "kind": "execution_limit";
  [key: string]: unknown;
} | {
  "kind": "plugin_state_unrecorded";
  "plugin": string;
  [key: string]: unknown;
} | {
  "kind": "tool_schema_admission";
  "source": ToolCatalogBuildError;
  [key: string]: unknown;
} | {
  "kind": "schema_admission";
  "source": SchemaAdmissionError;
  [key: string]: unknown;
} | {
  "kind": "value_mismatch";
  "source": ValueMismatch;
  [key: string]: unknown;
} | {
  "kind": "declaration";
  "refusal": DeclarationRefusal;
  [key: string]: unknown;
} | {
  "env_ref": string;
  "kind": "launch_not_held";
  [key: string]: unknown;
} | {
  "effect": string;
  "kind": "linked_tool_missing";
  "tool": string;
  [key: string]: unknown;
} | {
  "effect": string;
  "kind": "operation_contract_changed";
  "live": ContractId;
  "pinned": ContractId;
  "tool": string;
  [key: string]: unknown;
} | {
  "kind": "dispatch_input_refused";
  "reason": string;
  "tool": string;
  [key: string]: unknown;
} | {
  "kind": "package_namespace";
  "path": string;
  [key: string]: unknown;
} | {
  "effect": string;
  "kind": "dispatch_not_granted";
  "tool": string;
  [key: string]: unknown;
} | {
  "committed": string;
  "kind": "dispatch_diverged";
  "recomputed": string;
  "tool": string;
  [key: string]: unknown;
} | {
  "kind": "dynamic_call_refused";
  "path": string;
  "reason": string;
  [key: string]: unknown;
} | {
  "kind": "dynamic_call_unavailable";
  "note"?: (string | null);
  "path": string;
  [key: string]: unknown;
} | {
  "kind": "control_from_spawned_task";
  [key: string]: unknown;
} | {
  "kind": "control_with_outstanding_tasks";
  [key: string]: unknown;
} | {
  "kind": "control_attempt_spent";
  [key: string]: unknown;
} | {
  "kind": "control_sibling_failed";
  [key: string]: unknown;
} | {
  /**
   * The error's kind as the body raised it (`type_error`,
   * `tool_failed`), or `None` when the body threw a value that is
   * not an error.
   */
  "error_kind"?: (string | null);
  "kind": "uncaught_error";
  /**
   * Where the run's main task raised it, in the document the process
   * runs: the `perform`, `sleep` or `join` whose outcome raised, else
   * the statement whose evaluation raised, a library call's included.
   * An error out of library code, an `await` of a failed task's among
   * them, names the first document statement it unwinds into, and one
   * a `finally` held names its `try` statement. `None` when a run-root
   * cleanup held it (a Python entry that started tasks), or no
   * document statement was left to name it.
   */
  "site"?: (Site | null);
  [key: string]: unknown;
});

export type ToolFailureClass = ("invalid_request" | "io" | "unavailable" | "permission_denied" | "timeout" | "execution" | "external" | "resource_limit" | "internal");

export type ToolFailureSource = ("runtime" | "tool" | "plugin" | "policy" | "cancellation");

/**
 * The new fact a tool intent produced, without copying its committed process record.
 */
export type ToolIntentActivityOutcome = ({
  "intent_index": number;
  "kind": ToolIntentKind;
  "realized": ToolIntentRealizedRef;
  "status": "executed";
} | {
  "intent_index": number;
  "kind": ToolIntentKind;
  "refusal": ToolIntentRefusalReason;
  "status": "refused";
} | {
  "refusal": ToolIntentRefusalReason;
  "status": "batch_refused";
});

/**
 * The typed command cause retained by a tool intent and carried to the host.
 */
export type ToolIntentCommandFailure = ({
  "message": EnginePortError;
  "type": "package";
} | {
  "message": {
    "code"?: (FailureCode | null);
    "kind": ProviderFailureKind;
    "message": string;
    "retryable": boolean;
    "terminal_reason": LlmTerminalReason;
  };
  "type": "provider_failure";
} | {
  "message": PluginOperationFailure;
  "type": "operation";
} | {
  "message": {
    "causes": Array<PluginHookFailure>;
  };
  "type": "hook_failures";
} | {
  "message": {
    "engine_kind": string;
  };
  "type": "missing_recorded_process_config";
} | {
  "message": {
    "existing": CancelRequest;
    "process_id": string;
    "requested": CancelRequest;
  };
  "type": "process_cancel_conflict";
} | {
  "message": {
    "parent": ScopeId;
    "start_key"?: (StartKey | null);
  };
  "type": "parent_ended";
} | {
  "message": {
    "start_key": StartKey;
  };
  "type": "start_key_conflict";
} | {
  "message": {
    "refusal": ProcessEnvRefusal;
  };
  "type": "process_env_refused";
} | {
  "message": {
    "process": string;
    "refusal": ProcessSuccessorRefusal;
  };
  "type": "process_successor_refused";
} | {
  "message": {
    "path": string;
  };
  "type": "package_namespace";
} | {
  "message": {
    "operation": string;
  };
  "type": "internal_operation";
} | {
  "message": {
    "operation": string;
  };
  "type": "invalid_tool_discovery";
} | {
  "message": {
    "ceiling": number;
    "requested": number;
  };
  "type": "invalid_batch_maximum";
} | {
  "message": {
    "code": string;
    "message": string;
    "plugin_id": string;
    "presenter": string;
  };
  "type": "tool_presentation_refused";
} | {
  "message": {
    "source": SchemaAdmissionError;
  };
  "type": "unusable_schema";
} | {
  "message": {
    "source": ToolCatalogBuildError;
  };
  "type": "unusable_tool_schema";
} | {
  "message": {
    "context": string;
    "source": ValueMismatch;
  };
  "type": "value_mismatch";
} | {
  "message": {
    "operation": string;
    "tool_id": string;
  };
  "type": "resident_tool_contract_unavailable";
} | {
  "message": {
    "tool_id": string;
  };
  "type": "resident_tool_duplicate_id";
} | {
  "message": {
    "message": string;
    "tool_id": string;
  };
  "type": "resident_tool_invalid_operation";
} | {
  "message": {
    "first_tool": string;
    "operation": string;
    "second_tool": string;
  };
  "type": "resident_tool_duplicate_operation";
} | {
  "message": {
    "source": RegistrationRefused;
  };
  "type": "tool_registration_refused";
} | {
  "message": {
    "refusal": SurfaceRefusal;
  };
  "type": "tool_surface_refused";
} | {
  "message": {
    "session_id": string;
  };
  "type": "session_already_exists";
} | {
  "message": {
    "owner": SessionHeadOwner;
    "session_id": string;
  };
  "type": "session_head_owned";
} | {
  "message": string;
  "type": "registration";
} | {
  "message": PreflightIssue;
  "type": "preflight";
} | {
  "message": {
    "message": string;
    "owner": string;
  };
  "type": "provider_config";
} | {
  "message": {
    "expected": RegistryKind;
    "provider": ToolProviderHandle;
  };
  "type": "provider_registry_mismatch";
} | {
  "message": ConfigRegistrationError;
  "type": "config_registration";
} | {
  "message": string;
  "type": "invoke";
} | {
  "message": string;
  "type": "session";
} | {
  "message": PluginStateError;
  "type": "state";
} | {
  "message": PluginDeclarationError;
  "type": "declaration";
} | {
  "message": FormatRefusal;
  "type": "format";
} | {
  "message": PackageRefusal;
  "type": "package_store_refused";
} | {
  "message": StoreRefusal;
  "type": "store_refusal";
} | {
  "message": {
    "fault": StoreFault;
  };
  "type": "store_unavailable";
} | {
  "message": {
    "field": string;
    "plugin_id": string;
  };
  "type": "missing_recorded_session_config";
} | {
  "message": {
    "field": string;
    "plugin_id": string;
    "recorded": string;
    "requested": string;
  };
  "type": "recorded_session_config_conflict";
} | {
  "message": ToolIntentRuntimeFailure;
  "type": "runtime";
} | {
  "message": {
    "session_id": string;
  };
  "type": "session_execution_lease_lost";
} | {
  "message": {
    /**
     * Canonical durable operation key that was reused incorrectly.
     */
    "operation_key": string;
    /**
     * Session whose append operation identity conflicted.
     */
    "session_id": string;
  };
  "type": "append_operation_identity_conflict";
} | {
  "message": {
    /**
     * Count carried by the retry.
     */
    "attempted": number;
    /**
     * Canonical durable operation key of the corrupt receipt.
     */
    "operation_key": string;
    /**
     * Session whose append receipt is corrupt.
     */
    "session_id": string;
    "stored": number;
  };
  "type": "append_receipt_requested_node_count_corrupt";
} | {
  "message": {
    /**
     * Backend diagnostic describing the malformed field or payload.
     */
    "message": string;
    /**
     * Stable name of the durable record whose payload was unreadable.
     */
    "record_kind": string;
  };
  "type": "stored_data_corrupt";
} | {
  "message": {
    "clock": string;
    "epoch_ms": number;
  };
  "type": "clock_before_unix_epoch";
} | {
  "message": {
    "process_id": string;
  };
  "type": "process_not_visible";
} | {
  "message": {
    "operation": string;
    "process_id": string;
  };
  "type": "not_a_session_runtime";
} | {
  "message": {
    "digest": string;
  };
  "type": "process_output_attachment_unavailable";
} | {
  "message": {
    "process_id": string;
  };
  "type": "process_unknown";
} | {
  "message": Record<string, never>;
  "type": "process_roster_filter_mismatch";
} | {
  "message": {
    "requested_cursor": ProcessChangeCursor;
    "tombstone_compaction_horizon": ProcessChangeCursor;
  };
  "type": "process_change_cursor_pruned";
} | {
  "message": {
    "process_id": string;
    /**
     * The highest released sequence: reads resume strictly after it.
     */
    "released_through": number;
  };
  "type": "process_events_released";
} | {
  "message": ToolIntentRuntimeFailure;
  "type": "runtime_effect_controller";
} | {
  "message": {
    "process_id": string;
  };
  "type": "process_execution_superseded";
} | {
  "message": {
    "counter": string;
    "current": number;
  };
  "type": "monotonic_counter_overflow";
} | {
  "message": {
    "pruned_at_ms": number;
    "terminal_label": RetiredProcessStatus;
  };
  "type": "process_no_longer_retained";
} | {
  "message": {
    "process_id": string;
    "segment_ordinal": number;
  };
  "type": "process_handed_over";
} | {
  "message": {
    "process_id": string;
    "status": ProcessStatus;
  };
  "type": "process_already_terminal";
} | {
  "message": {
    "declared_status": ProcessStatus;
    "outcome_status"?: (ProcessStatus | null);
  };
  "type": "process_terminal_outcome_mismatch";
} | {
  "message": {
    "event_type": string;
  };
  "type": "unknown_process_event_kind";
} | {
  "message": {
    "actual": string;
    "expected": string;
  };
  "type": "process_registry_cursor_backend_mismatch";
});

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
 * The typed identity and admission outcome of a realized intent.
 */
export type ToolIntentRealizedRef = ({
  "kind": "process";
  "result": string;
} | {
  "kind": "definition";
  "result": ProcessDefinition;
} | {
  "kind": "cancel";
  "result": {
    "process_id": string;
    "status": ProcessStatus;
  };
});

export type ToolIntentRefusalReason = ({
  "reason": "unsupported_protocol_version";
  "recorded": number;
  [key: string]: unknown;
} | {
  "reason": "intent_index_overflow";
  [key: string]: unknown;
} | {
  "actual": number;
  "maximum": number;
  "reason": "count_budget_exceeded";
  [key: string]: unknown;
} | {
  "actual": number;
  "maximum": number;
  "reason": "canonical_byte_budget_exceeded";
  [key: string]: unknown;
} | {
  "actual": number;
  "kind": ToolIntentKind;
  "maximum": number;
  "reason": "per_kind_budget_exceeded";
  [key: string]: unknown;
} | {
  "expected": RuntimeOwner;
  "reason": "owner_mismatch";
  "recorded": RuntimeOwner;
  [key: string]: unknown;
} | {
  "cause": ToolIntentCommandFailure;
  "reason": "command_failed";
  [key: string]: unknown;
} | {
  "reason": "minting_run_cancelled";
  [key: string]: unknown;
} | {
  "expected": ToolIntentIdentity;
  "reason": "declared_start_identity_mismatch";
  "recorded": ToolIntentIdentity;
  [key: string]: unknown;
});

/**
 * The durable fields of a runtime command failure. Invocation-local retry
 * markers are not recorded command facts.
 */
export type ToolIntentRuntimeFailure = {
  "cause"?: (RuntimeErrorCause | null);
  "code": string;
  "message": string;
  "summary"?: (RuntimeEffectReplayMismatchReport | null);
  "turn_input_acceptance"?: (TurnInputAcceptanceReceipt | null);
};

/**
 * The scoped address of a declared provider. Only the callback codec spells its seam prefix.
 */
export type ToolProviderHandle = {
  "key": string;
  "owner": string;
  "registry": RegistryKind;
};

/**
 * The caller of an unrecorded tool read. This is observation identity only.
 */
export type ToolReadCaller = ({
  "graph_key": string;
  "kind": "code_block";
  [key: string]: unknown;
} | {
  "kind": "process";
  "process_id": string;
  [key: string]: unknown;
} | {
  "kind": "model_round";
  "round_key": string;
  [key: string]: unknown;
});

/**
 * Read observation identity and live measurements. The caller saves the read
 * fact, while elapsed time and byte counts remain provisional activity.
 */
export type ToolReadMetadata = {
  "answer_bytes": number;
  /**
   * JSON byte sizes; zero when input or answer was unavailable.
   */
  "args_bytes": number;
  "caller": ToolReadCaller;
  "elapsed_us": number;
  /**
   * One-based ordinal among the caller's attempted reads, retained at saves.
   */
  "ordinal": number;
  "origin": string;
  "provider": ToolProviderHandle;
  "site": string;
  [key: string]: unknown;
};

export type ToolView = {
  "blocks": Array<ToolViewBlock>;
};

export type ToolViewBlock = ({
  "meta"?: ToolViewMeta;
  "text": string;
  "type": "text";
} | {
  "meta"?: ToolViewMeta;
  "reference": AttachmentRef;
  "type": "attachment";
} | {
  "description"?: (string | null);
  "meta"?: ToolViewMeta;
  "mime_type"?: (string | null);
  "name": string;
  "title"?: (string | null);
  "type": "resource_link";
  "uri": string;
});

export type ToolViewMeta = {
  "last_modified"?: (string | null);
  "priority"?: (ToolViewPriority | null);
};

/**
 * A tool view's finite display priority. Construct it before admitting a view.
 */
export type ToolViewPriority = number;

/**
 * The transport sequence: each direction numbers its messages from zero,
 * one apart, so a dropped, duplicated or reordered message is refused.
 */
export type TransportSequence = number;

/**
 * Stable identifier for a semantic turn activity.
 *
 * An activity a shift observation yields is named by the observation's
 * replay key and ordinal, `{key}#{ordinal}`; a frame of coalesced stream
 * deltas is named by the ordinal range it covers, `{key}#{first}..{last}`
 * (FIG-5098). Both re-derive identically on a redrive, which is what lets a
 * live replay store drop what it already delivered. Any other id is opaque.
 */
export type TurnActivityId = string;

export type TurnControl = ({
  "type": "finish";
  "value": unknown;
  /**
   * The schema `value` was checked against when its call settled: the
   * admitted declaration's ([`FinishDeclaration`](lash_kernel_value::tool_declaration::FinishDeclaration)).
   * A body's own Finish states none; settlement stamps it, and every
   * later reader — a completion candidate, a replay, a batch that
   * folds the call — keeps the schema the value settled under rather
   * than reading a live one.
   */
  "value_schema"?: (JsonSchema | null);
  [key: string]: unknown;
} | {
  "frame_key": FrameKey;
  "initial_nodes"?: Array<SessionAppendNode>;
  "next_run": NextRun;
  "task": string;
  "type": "switch_agent_frame";
  [key: string]: unknown;
});

/**
 * A way a tool's call may end its caller's turn.
 */
export type TurnControlKind = ("finish" | "switch_agent_frame");

export type TurnFailureKind = string;

/**
 * Durable acceptance evidence returned to an ingress caller.
 *
 * The receipt deliberately carries only stable routing and idempotency
 * identity. Queue dispatch and the pending row's mutable lifecycle state are
 * observed separately through the pending-input reconciliation surface.
 */
export type TurnInputAcceptanceReceipt = {
  "ingress": TurnInputIngress;
  "input_id": string;
  "session_id": string;
  "source_key"?: (string | null);
  [key: string]: unknown;
};

/**
 * Durable evidence that an admitted input became canonical conversation input.
 *
 * This is the application stage between [`TurnInputAcceptanceReceipt`]
 * (admission) and the terminal turn commit (settlement). It deliberately
 * carries identity only: hosts correlate an input to its canonical turn and
 * committed message without parsing or retaining display text.
 */
export type TurnInputApplication = {
  /**
   * Present for active-turn checkpoint application and absent when the
   * input formed the initial canonical input of an idle queued turn.
   */
  "checkpoint"?: (CheckpointKind | null);
  "committed_message_id": string;
  "input_id": string;
  "source_key"?: (string | null);
  "turn_id": string;
  [key: string]: unknown;
};

export type TurnInputCheckpointBoundary = ("after_work" | "before_completion");

export type TurnInputIngress = ({
  "min_boundary"?: TurnInputCheckpointBoundary;
  "scope": "active_turn";
  "turn_id": string;
  [key: string]: unknown;
} | {
  "scope": "next_turn";
  [key: string]: unknown;
});

export type TurnOutputSource = ({
  "kind": "runtime";
  [key: string]: unknown;
} | {
  "kind": "plugin";
  "plugin_id": string;
  [key: string]: unknown;
});

export type Type = (("null" | "absent" | "bool" | "int" | "float" | "text" | "bytes" | "timestamp" | "error") | "any" | "json" | "number" | {
  "tuple": Array<Type>;
} | {
  "list": Type;
} | {
  "map": MapType;
} | {
  "set": Type;
} | {
  "record": RecordType;
} | {
  "enum": Array<string>;
} | {
  "function": Signature;
} | {
  "task": Type;
} | {
  "handle": string;
} | {
  "union": Array<Type>;
});

/**
 * The first incompatible value at a public boundary. `path` is an RFC 6901
 * pointer into the value, never a path through the type grammar.
 */
export type TypeMismatch = {
  /**
   * A bounded kind witness; no user value is included.
   */
  "actual": Type;
  "expected": Type;
  "path": string;
};

/**
 * The code a node belongs to.
 */
export type Unit = ("main" | {
  "function": Name;
} | {
  "library": FunctionId;
});

export type ValueMismatch = {
  "instance_path": string;
  "message": string;
  [key: string]: unknown;
};

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
 * The wire form of [`VersionRange`], before validation.
 */
export type VersionRange = {
  "max": number;
  "min": number;
  [key: string]: unknown;
};

/**
 * Why this deployment cannot launch its configured worker executable.
 */
export type WorkerDeploymentFault = ("not_found" | "not_executable");

/**
 * The worker message an encoder refused, without its guest data.
 */
export type WorkerFrameKind = ("documents" | "refused" | "progress" | "limit_exceeded" | "ready" | "started" | "host_read" | "printed" | "parked" | "slice" | "ended" | "delivered" | "sealed" | "cancelled" | "reset_done" | "prepared");

/**
 * The native library's typed validation refusal, independent of VM transport.
 */
export type WorkerLibraryRefusal = ({
  "malformed": {
    "problem": string;
    [key: string]: unknown;
  };
} | {
  "native_missing": {
    "function": FunctionId;
    "name": FunctionName;
    [key: string]: unknown;
  };
} | {
  "plan_missing": {
    "function": FunctionId;
    "name": FunctionName;
    [key: string]: unknown;
  };
} | {
  "native_unused": {
    "function": FunctionId;
    [key: string]: unknown;
  };
});

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
