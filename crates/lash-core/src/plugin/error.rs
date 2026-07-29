#[derive(Debug, thiserror::Error, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", content = "message", rename_all = "snake_case")]
pub enum PluginError {
    #[error("plugin registration error: {0}")]
    Registration(String),
    #[error("plugin snapshot error: {0}")]
    Snapshot(String),
    #[error("plugin invoke error: {0}")]
    Invoke(String),
    #[error("plugin session error: {0}")]
    Session(String),
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
    #[error(
        "process outcome is no longer retained (terminal state `{terminal_label}`, pruned at {pruned_at_ms}ms)"
    )]
    ProcessNoLongerRetained {
        terminal_label: String,
        pruned_at_ms: u64,
    },
    #[error("process event type `{event_type}` is reserved for its dedicated registry mutation")]
    ReservedProcessEvent { event_type: String },
    #[error("wake delivery `{delivery_id}` is already terminal in state `{state:?}`")]
    WakeDeliveryNotPending {
        delivery_id: String,
        state: crate::WakeDeliveryState,
    },
}
