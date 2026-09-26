use crate::ProcessId;
use crate::SessionId;

/// One refusal for a durable identity re-presented with different content.
///
/// The stores fence a re-submitted identity at the point it mutates: a process
/// registration fingerprint, a process-event replay key, a trigger occurrence
/// idempotency key. Matching content replays the first writer's result;
/// differing content cannot, because the identity is already bound. Every such
/// site returns this one error so the tool-intent front door can map all three
/// to a single typed host-facing refusal instead of matching prose (FIG-1489).
///
/// It is spelled as a [`RuntimeError`](crate::RuntimeError) carrying
/// [`RuntimeErrorCode::DurableIdentityConflict`](crate::RuntimeErrorCode::DurableIdentityConflict)
/// rather than a new `PluginError` variant, because `PluginError` is journaled
/// through `ProcessEffectOutcome::CancelRefused`: a new code string is data an
/// existing variant already carries, while a new variant would be a shape an
/// older build could not read.
pub fn durable_identity_conflict(message: impl Into<String>) -> PluginError {
    PluginError::Runtime(crate::RuntimeError::new(
        crate::RuntimeErrorCode::DurableIdentityConflict,
        message,
    ))
}

/// Whether `error` is the durable-identity refusal minted by
/// [`durable_identity_conflict`], however many conversions it has crossed.
pub fn is_durable_identity_conflict(error: &PluginError) -> bool {
    match error {
        PluginError::Runtime(error) => {
            error.code == crate::RuntimeErrorCode::DurableIdentityConflict
        }
        PluginError::RuntimeEffectController(error) => {
            error.code == crate::RuntimeErrorCode::DurableIdentityConflict
        }
        _ => false,
    }
}
#[derive(Debug, thiserror::Error, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", content = "message", rename_all = "snake_case")]
#[non_exhaustive]
pub enum PluginError {
    /// The process already accepted a different cancellation request.
    #[error(
        "process `{process_id}` already accepted cancellation {existing:?}; refused {requested:?}"
    )]
    ProcessCancelConflict {
        process_id: ProcessId,
        existing: Box<crate::CancelRequest>,
        requested: Box<crate::CancelRequest>,
    },
    /// A child requested cancellation on end of an already-ended parent. The
    /// start is refused before an id is minted, so the refusal names the
    /// start by its key.
    #[error("cannot register process start {start_key:?}: parent scope {parent:?} has ended")]
    ParentEnded {
        start_key: Option<crate::StartKey>,
        parent: crate::ParentScope,
    },
    /// Discovery must itself be an inline member of the tool catalogue.
    #[error("discovery operation `{operation}` must be an inline catalogue member")]
    InvalidToolDiscovery { operation: String },
    /// An effective resident catalog member could not supply its immutable definition.
    #[error("resident tool `{name}` ({tool_id}) has no contract")]
    ResidentToolContractUnavailable {
        tool_id: crate::ToolId,
        name: String,
    },
    #[error("resident catalog repeats tool id `{tool_id}`")]
    ResidentToolDuplicateId { tool_id: crate::ToolId },
    #[error("resident catalog repeats tool name `{name}`")]
    ResidentToolDuplicateName { name: String },
    /// An effective resident catalog member has no executable route in the pinned registry.
    #[error("resident tool `{name}` ({tool_id}) has no pinned execution route: {reason}")]
    ResidentToolRouteUnavailable {
        tool_id: crate::ToolId,
        name: String,
        reason: String,
    },
    #[error("plugin registration error: {0}")]
    Registration(String),
    #[error("plugin invoke error: {0}")]
    Invoke(String),
    /// A bounded before-tool-call reinspection attempted to replace arguments again.
    #[error(
        "before_tool_call replacement from `{replacing_plugin_id}` was replaced again by `{repeated_plugin_id}` during bounded reinspection"
    )]
    BeforeToolCallReplacementConflict {
        /// Plugin whose replacement caused earlier hooks to be reinspected.
        replacing_plugin_id: String,
        /// Earlier plugin that attempted another replacement during reinspection.
        repeated_plugin_id: String,
    },
    /// A bounded after-tool-call reinspection attempted to replace the result again.
    #[error(
        "after_tool_call replacement from `{replacing_plugin_id}` was replaced again by `{repeated_plugin_id}` during bounded reinspection"
    )]
    AfterToolCallReplacementConflict {
        /// Plugin whose replacement caused earlier hooks to be reinspected.
        replacing_plugin_id: String,
        /// Earlier plugin that attempted another replacement during reinspection.
        repeated_plugin_id: String,
    },
    #[error("plugin session error: {0}")]
    Session(String),
    /// A `ParentFork` creation request carried no captured init payload.
    /// The capture is taken once at spawn; materialization never reads a live
    /// parent session, so there is nothing to fall back to.
    #[error(
        "session `{session_id}` requested a parent fork but carries no captured plugin init payload"
    )]
    MissingSessionInit { session_id: SessionId },
    /// A captured plugin init payload exceeded the durable-request bound.
    #[error("captured session init payload is {bytes} bytes, exceeding the {limit}-byte bound")]
    SessionInitTooLarge { bytes: usize, limit: usize },
    /// An existing plugin session cannot be reconstructed because a required
    /// protocol-owned field is absent from its durable record.
    #[error("recorded session config for plugin `{plugin_id}` is missing required field `{field}`")]
    MissingRecordedSessionConfig { plugin_id: String, field: String },
    /// A host attempted to substitute a durably pinned protocol selection.
    #[error(
        "recorded session config for plugin `{plugin_id}` pins `{field}` to {recorded}, refusing {requested}"
    )]
    RecordedSessionConfigConflict {
        plugin_id: String,
        field: String,
        recorded: String,
        requested: String,
    },
    #[error(transparent)]
    Runtime(crate::RuntimeError),
    /// A turn-scoped plugin write presented a lapsed or superseded borrowed
    /// session-execution guard.
    #[error("session execution lease for `{session_id}` was lost before plugin commit")]
    SessionExecutionLeaseLost { session_id: SessionId },
    /// A session append operation id was reused for different semantic request content.
    #[error(
        "append operation `{operation_key}` for session `{session_id}` was reused with different request content"
    )]
    AppendOperationIdentityConflict {
        /// Session whose append operation identity conflicted.
        session_id: SessionId,
        /// Canonical durable operation key that was reused incorrectly.
        operation_key: String,
    },
    /// Durable append receipt metadata contradicts the retry's requested-node
    /// count. This is store corruption, not a caller-recoverable conflict.
    #[error(
        "append receipt `{operation_key}` for session `{session_id}` has contradictory requested-node counts (stored {stored}, attempted {attempted})"
    )]
    AppendReceiptRequestedNodeCountCorrupt {
        /// Session whose append receipt is corrupt.
        session_id: SessionId,
        /// Canonical durable operation key of the corrupt receipt.
        operation_key: String,
        stored: u64,
        /// Count carried by the retry.
        attempted: u64,
    },
    /// A durable plugin-owned record contained a value outside its declared
    /// representation. Retrying cannot repair the stored bytes.
    #[error("stored {record_kind} data is corrupt: {message}")]
    StoredDataCorrupt {
        /// Stable name of the durable record whose payload was unreadable.
        record_kind: String,
        /// Backend diagnostic describing the malformed field or payload.
        message: String,
    },
    /// A store response confirmed usage identities outside the set staged by
    /// this operation. Applying it would discard unrelated usage.
    #[error(
        "store confirmed {confirmed_count} usage identities, but only {staged_count} were staged"
    )]
    UnstagedUsageConfirmation {
        confirmed_count: usize,
        staged_count: usize,
    },
    /// A backend-owned authoritative clock produced a value before the Unix
    /// epoch, outside the runtime clock contract.
    #[error("{clock} returned a pre-Unix-epoch millisecond value: {epoch_ms}")]
    ClockBeforeUnixEpoch { clock: String, epoch_ms: i64 },
    #[error("process handle `{process_id}` is not live or visible in this session")]
    ProcessNotVisible { process_id: ProcessId },
    /// An operation referenced a process id that the registry never knew.
    #[error("unknown process `{process_id}`")]
    ProcessUnknown { process_id: ProcessId },
    /// A Process Change Feed cursor predates deletion history removed by
    /// Tombstone Compaction. The consumer must perform a full relist before
    /// resuming from the reported horizon.
    #[error(
        "process change cursor {requested_cursor:?} is below tombstone-compaction horizon {tombstone_compaction_horizon:?}; a full relist is required"
    )]
    ProcessChangeCursorPruned {
        requested_cursor: crate::ProcessChangeCursor,
        tombstone_compaction_horizon: crate::ProcessChangeCursor,
    },
    /// A process park feed cursor predates history
    /// `compact_process_park_feed` removed. The consumer must relist parked
    /// processes before resuming from the reported horizon.
    #[error(
        "process park feed cursor is below the compaction horizon {horizon:?}; a full relist is required"
    )]
    ProcessParkFeedCursorCompacted {
        /// The lowest feed position the store still serves.
        horizon: crate::store::ParkFeedCursor,
    },
    #[error(transparent)]
    RuntimeEffectController(#[from] crate::RuntimeEffectControllerError),
    #[error("process `{process_id}` execution was already started by {by:?}")]
    ProcessAlreadyStarted {
        process_id: ProcessId,
        by: Box<crate::LeaseOwnerIdentity>,
    },
    #[error("process `{process_id}` exhausted its execution attempts ({attempts}/{max_attempts})")]
    ProcessAttemptsExhausted {
        process_id: ProcessId,
        attempts: u32,
        max_attempts: u32,
    },
    #[error("process lease for `{process_id}` is missing or expired (superseded)")]
    ProcessLeaseSuperseded { process_id: ProcessId },
    #[error("monotonic counter `{counter}` cannot advance past {current}")]
    MonotonicCounterOverflow { counter: String, current: u64 },
    #[error(
        "process outcome is no longer retained (terminal state `{terminal_label}`, pruned at {pruned_at_ms}ms)"
    )]
    ProcessNoLongerRetained {
        terminal_label: String,
        pruned_at_ms: u64,
    },
    /// A wait was requested on a row whose registering caller departed before
    /// any outcome could be recorded (FIG-1383).
    ///
    /// The wait is refused rather than parked: the row is non-terminal, no
    /// actor is left to terminalize it, and lash may never invent an outcome
    /// for it. Closure comes from external reconciliation writing the observed
    /// truth, or from retention reclaiming the row.
    #[error(
        "process `{process_id}` recorded a caller departure before any outcome; awaiting it would never resolve"
    )]
    ProcessCallerDeparted { process_id: ProcessId },
    /// A recovery would end a process that a later segment already carries
    /// (FIG-3820).
    #[error("process `{process_id}` is carried by its segment {segment_ordinal}")]
    ProcessHandedOver {
        process_id: ProcessId,
        segment_ordinal: u64,
    },
    #[error("process `{process_id}` is already terminal in state `{status:?}`")]
    ProcessAlreadyTerminal {
        process_id: ProcessId,
        status: crate::ProcessStatus,
    },
    #[error(
        "terminal process status `{declared_status:?}` contradicts outcome status `{outcome_status:?}`"
    )]
    ProcessTerminalOutcomeMismatch {
        declared_status: crate::ProcessStatus,
        outcome_status: Option<crate::ProcessStatus>,
    },
    #[error("process event type `{event_type}` is reserved for its dedicated registry mutation")]
    ReservedProcessEvent { event_type: String },
    #[error("process wake delivery carries an invalid wake identity `{wake_id}`")]
    InvalidProcessWakeIdentity { wake_id: String },
    #[error(
        "process wake delivery format version {found} is incompatible with version {expected}; drain in-flight sessions on the old build before deploying this build, or recreate development/test stores"
    )]
    ProcessWakeDeliveryFormatVersionMismatch { expected: u32, found: u32 },
    /// A worklist continuation was passed to a registry backend other than the
    /// backend that issued it.
    #[error("process worklist cursor belongs to backend `{actual}`, not `{expected}`")]
    ProcessWorklistCursorBackendMismatch { expected: String, actual: String },
}

impl PluginError {
    /// Settles a plugin hook's failure by its cause (FIG-3575).
    ///
    /// A live fault the hook ran into aborts the turn under a live code, so a
    /// redrive repairs it: a carried runtime or controller error keeps its own
    /// code, a lost session lease is `SessionExecutionLeaseLost`, and an opaque
    /// session-seam failure (store I/O behind a plugin service) or a lost
    /// process lease is `PluginSessionManager`. A carried session retirement
    /// keeps its own error and cause, so the caller aborts on it (FIG-3630).
    /// Every other variant is a
    /// deliberate refusal over the turn's inputs or durable state: an outcome
    /// spelled as `refusal`, recorded or settled once instead of retried.
    pub fn into_turn_failure(self, refusal: crate::RuntimeErrorCode) -> crate::RuntimeError {
        match self {
            Self::Runtime(error)
                if error.turn_failure_cause().aborts_invocation()
                    || error.is_session_retirement() =>
            {
                error
            }
            Self::RuntimeEffectController(error)
                if error.turn_failure_cause().aborts_invocation()
                    || error.is_session_retirement() =>
            {
                error.into_runtime_error()
            }
            error @ Self::SessionExecutionLeaseLost { .. } => crate::RuntimeError::new(
                crate::RuntimeErrorCode::SessionExecutionLeaseLost,
                error.to_string(),
            ),
            error @ (Self::Session(_) | Self::ProcessLeaseSuperseded { .. }) => {
                crate::RuntimeError::new(
                    crate::RuntimeErrorCode::PluginSessionManager,
                    error.to_string(),
                )
            }
            refused @ (Self::Runtime(_)
            | Self::RuntimeEffectController(_)
            | Self::ProcessCancelConflict { .. }
            | Self::ParentEnded { .. }
            | Self::InvalidToolDiscovery { .. }
            | Self::ResidentToolContractUnavailable { .. }
            | Self::ResidentToolDuplicateId { .. }
            | Self::ResidentToolDuplicateName { .. }
            | Self::ResidentToolRouteUnavailable { .. }
            | Self::Registration(_)
            | Self::Invoke(_)
            | Self::BeforeToolCallReplacementConflict { .. }
            | Self::AfterToolCallReplacementConflict { .. }
            | Self::MissingSessionInit { .. }
            | Self::SessionInitTooLarge { .. }
            | Self::MissingRecordedSessionConfig { .. }
            | Self::RecordedSessionConfigConflict { .. }
            | Self::AppendOperationIdentityConflict { .. }
            | Self::AppendReceiptRequestedNodeCountCorrupt { .. }
            | Self::StoredDataCorrupt { .. }
            | Self::UnstagedUsageConfirmation { .. }
            | Self::ClockBeforeUnixEpoch { .. }
            | Self::ProcessNotVisible { .. }
            | Self::ProcessUnknown { .. }
            | Self::ProcessChangeCursorPruned { .. }
            | Self::ProcessParkFeedCursorCompacted { .. }
            | Self::ProcessAlreadyStarted { .. }
            | Self::ProcessAttemptsExhausted { .. }
            | Self::MonotonicCounterOverflow { .. }
            | Self::ProcessNoLongerRetained { .. }
            | Self::ProcessCallerDeparted { .. }
            | Self::ProcessAlreadyTerminal { .. }
            | Self::ProcessHandedOver { .. }
            | Self::ProcessTerminalOutcomeMismatch { .. }
            | Self::ReservedProcessEvent { .. }
            | Self::InvalidProcessWakeIdentity { .. }
            | Self::ProcessWakeDeliveryFormatVersionMismatch { .. }
            | Self::ProcessWorklistCursorBackendMismatch { .. }) => {
                crate::RuntimeError::new(refusal, refused.to_string())
            }
        }
    }

    /// The park this failure puts a process in, when it is a refusal that
    /// parks ([`ParkReason::of_error`](crate::store::ParkReason::of_error)):
    /// the body refused to replay its journal with nothing dispatched.
    pub fn park_reason(&self) -> Option<crate::store::ParkReason> {
        match self {
            Self::Runtime(error) => crate::store::ParkReason::of_error(error),
            Self::RuntimeEffectController(error) => {
                crate::store::ParkReason::of_error(&error.clone().into_runtime_error())
            }
            _ => None,
        }
    }

    /// Whether retrying the identical plugin operation is explicitly safe.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Runtime(error) => error.is_retryable(),
            Self::RuntimeEffectController(error) => {
                error.cause.is_none() && error.code.is_retryable()
            }
            _ => false,
        }
    }

    /// Whether retrying the identical plugin operation cannot succeed without
    /// changing durable state, configuration, or wiring.
    pub fn is_terminal(&self) -> bool {
        match self {
            Self::Runtime(error) => error.is_terminal(),
            Self::RuntimeEffectController(error) => error.is_terminal(),
            Self::BeforeToolCallReplacementConflict { .. }
            | Self::AfterToolCallReplacementConflict { .. }
            | Self::MissingRecordedSessionConfig { .. }
            | Self::RecordedSessionConfigConflict { .. }
            | Self::AppendOperationIdentityConflict { .. }
            | Self::AppendReceiptRequestedNodeCountCorrupt { .. }
            | Self::StoredDataCorrupt { .. }
            | Self::UnstagedUsageConfirmation { .. }
            | Self::ClockBeforeUnixEpoch { .. }
            | Self::MonotonicCounterOverflow { .. }
            | Self::ProcessChangeCursorPruned { .. }
            | Self::ProcessParkFeedCursorCompacted { .. }
            | Self::ProcessNoLongerRetained { .. }
            | Self::ProcessCallerDeparted { .. }
            | Self::ProcessAlreadyTerminal { .. }
            | Self::ProcessHandedOver { .. }
            | Self::ParentEnded { .. }
            | Self::ProcessCancelConflict { .. }
            | Self::ProcessTerminalOutcomeMismatch { .. }
            | Self::ReservedProcessEvent { .. }
            | Self::InvalidProcessWakeIdentity { .. }
            | Self::ProcessWakeDeliveryFormatVersionMismatch { .. }
            | Self::ProcessWorklistCursorBackendMismatch { .. } => true,
            _ => false,
        }
    }
}

#[cfg(test)]
mod classification_tests {
    use super::*;

    #[test]
    fn deterministic_corrupt_state_is_terminal_but_opaque_infrastructure_is_unknown() {
        let corrupt = PluginError::StoredDataCorrupt {
            record_kind: "process_event".to_string(),
            message: "negative sequence".to_string(),
        };
        assert!(corrupt.is_terminal());
        assert!(!corrupt.is_retryable());

        let opaque = PluginError::Session("database connection closed".to_string());
        assert!(!opaque.is_terminal());
        assert!(!opaque.is_retryable());
    }
}
