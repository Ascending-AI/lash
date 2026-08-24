#[derive(Debug, thiserror::Error, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", content = "message", rename_all = "snake_case")]
pub enum PluginError {
    #[error("plugin registration error: {0}")]
    Registration(String),
    #[error("plugin snapshot error: {0}")]
    Snapshot(String),
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
    #[error(transparent)]
    Runtime(crate::RuntimeError),
    /// A turn-scoped plugin write presented a lapsed or superseded borrowed
    /// session-execution guard.
    #[error("session execution lease for `{session_id}` was lost before plugin commit")]
    SessionExecutionLeaseLost { session_id: String },
    /// A session append operation id was reused for different semantic request content.
    #[error(
        "append operation `{operation_key}` for session `{session_id}` was reused with different request content"
    )]
    AppendOperationIdentityConflict {
        /// Session whose append operation identity conflicted.
        session_id: String,
        /// Canonical durable operation key that was reused incorrectly.
        operation_key: String,
    },
    /// Durable append receipt metadata contradicts the retry's requested-node
    /// count. This is store corruption, not a caller-recoverable conflict.
    #[error(
        "append receipt `{operation_key}` for session `{session_id}` has contradictory requested-node counts (stored {stored:?}, attempted {attempted:?})"
    )]
    AppendReceiptRequestedNodeCountCorrupt {
        /// Session whose append receipt is corrupt.
        session_id: String,
        /// Canonical durable operation key of the corrupt receipt.
        operation_key: String,
        /// Count stored with the first attempt, when present.
        stored: Option<u64>,
        /// Count carried by the retry, when present.
        attempted: Option<u64>,
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
    ProcessNotVisible { process_id: String },
    /// An operation referenced a process id that the registry never knew.
    #[error("unknown process `{process_id}`")]
    ProcessUnknown { process_id: String },
    #[error(transparent)]
    RuntimeEffectController(#[from] crate::RuntimeEffectControllerError),
    #[error("process `{process_id}` execution was already started by {by:?}")]
    ProcessAlreadyStarted {
        process_id: String,
        by: Box<crate::LeaseOwnerIdentity>,
    },
    #[error("process `{process_id}` exhausted its execution attempts ({attempts}/{max_attempts})")]
    ProcessAttemptsExhausted {
        process_id: String,
        attempts: u32,
        max_attempts: u32,
    },
    #[error("process lease for `{process_id}` is missing or expired (superseded)")]
    ProcessLeaseSuperseded { process_id: String },
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
    ProcessCallerDeparted { process_id: String },
    /// One bounded transport attachment elapsed while the durable process wait
    /// remained live. Hosts must re-attach using the same process id.
    #[error(
        "process `{process_id}` terminal attach ceiling elapsed; re-attach to continue waiting"
    )]
    ProcessAttachCeilingElapsed { process_id: String },
    #[error("process `{process_id}` is already terminal in state `{status:?}`")]
    ProcessAlreadyTerminal {
        process_id: String,
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
