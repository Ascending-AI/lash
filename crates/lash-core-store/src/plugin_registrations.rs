//! Pure operation requirements and composition preflight refusals.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
pub enum RequiredPort {
    ProtocolSession,
    ProtocolDriver,
}
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
pub enum ServiceKind {
    TurnScope,
    CurrentSessionSnapshot,
    NamedSessionSnapshot,
    RecordedToolCatalog,
    CommittedTurnRead,
    SessionCreate,
    SessionGraphAppend,
    ProcessList,
    ProcessStart,
    ProcessRecordedStart,
    ProcessBoundStart,
    ProcessBoundCancel,
    ProcessAwait,
    ProcessCancel,
    ProcessTransfer,
    ProcessReleaseHold,
    ProcessAbandonHold,
    GrantedSessionReads,
    ProcessListForAttempt,
    ProcessStartFromRequest,
    ProcessValidateVisible,
    ProcessRecordedCancel,
}
#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    thiserror::Error,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
pub enum PreflightIssue {
    #[error("plugin `{owner}` requires unavailable service {service:?}")]
    MissingService { owner: String, service: ServiceKind },
    #[error("missing required port {port:?}")]
    MissingPort { port: RequiredPort },
    #[error("duplicate port {port:?} supplied by {owners:?}")]
    DuplicatePort {
        port: RequiredPort,
        owners: Vec<String>,
    },
}

pub use lash_vocabulary::providers::{ToolProviderHandle, ToolProviderKey, ToolProviderKeyError};
