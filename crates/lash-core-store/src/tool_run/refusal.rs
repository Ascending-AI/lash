//! Typed refusals carried intact from the logical Run to its host.

use serde::{Deserialize, Serialize};

/// A process binding that cannot honor admission.
#[derive(
    Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, thiserror::Error,
)]
pub enum IsolatedStartRefusal {
    #[error("an isolated start must name a registered engine input")]
    NotEngine,
    #[error("process implementation `{kind}` is unavailable")]
    Unavailable { kind: String },
    #[error("the isolated start is refused: {cause}")]
    Start {
        cause: DeclaredStartObligationRefusal,
    },
}

/// Why a registration cannot be a declared start.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
    thiserror::Error,
)]
#[serde(rename_all = "snake_case")]
pub enum DeclaredStartObligationRefusal {
    #[error("a declared start needs a stable start key")]
    Keyless,
    #[error("a declared start lash executes needs its captured execution environment")]
    NoEnvironment,
    #[error("a declared start needs the consuming call's hold")]
    NoConsumerHold,
}

/// What a recorded admission names differently from the call replaying it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum SingletonDrift {
    CallId,
    ToolName,
    Arguments,
    IsolationBinding,
}

/// Why a Run's frame cannot proceed: an earlier frame of its invocation
/// failed.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
    thiserror::Error,
)]
pub enum RunCutRefusal {
    #[error("the invocation failed; its owner recovers the Run from its records")]
    InvocationFailed,
}
