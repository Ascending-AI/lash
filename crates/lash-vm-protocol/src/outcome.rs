//! Typed infrastructure outcomes: what a worker's failure is, kept apart from
//! anything the guest did.
//!
//! A guest error is the program's own failure and travels as
//! [`crate::WorkerMessage::GuestError`]. Everything here is the worker's
//! failure: the parent fences the lease, settles the operations it already
//! admitted, and reports the outcome as retryable infrastructure, so the
//! owning substrate invocation is re-driven. A worker that produced one is
//! discarded, never reset.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::codec::CodecRefusal;

/// What the supervisor observed of a worker's end. Evidence, never testimony:
/// a worker cannot claim how it ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SupervisorEvidence {
    /// The pipe closed with no exit status observed yet.
    EndOfStream,
    Exited {
        code: i32,
    },
    Signalled {
        signal: i32,
    },
}

/// The VM or worker limit a run exhausted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerLimit {
    Fuel,
    Heap,
    Depth,
    Deadline,
}

#[derive(Clone, Debug, PartialEq, Eq, Error, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InfrastructureOutcome {
    #[error("the worker crashed ({evidence:?})")]
    WorkerCrashed { evidence: SupervisorEvidence },
    #[error("the worker sent nothing for {silent_ms} ms")]
    WorkerUnresponsive { silent_ms: u64 },
    #[error("the worker broke the protocol: {reason}")]
    ProtocolViolation { reason: String },
    #[error("a frame of {size} bytes exceeds the {limit}-byte bound")]
    PayloadTooLarge { limit: u64, size: u64 },
    #[error("the run exhausted its {limit:?} limit")]
    WorkerLimitExceeded { limit: WorkerLimit },
}

impl InfrastructureOutcome {
    /// Whether re-driving the owning invocation can succeed. A limit the run
    /// itself exhausted fails the same way on every attempt.
    pub fn is_retryable(&self) -> bool {
        !matches!(self, Self::WorkerLimitExceeded { .. })
    }
}

impl From<CodecRefusal> for InfrastructureOutcome {
    fn from(refusal: CodecRefusal) -> Self {
        match refusal {
            CodecRefusal::FrameTooLarge { limit, declared } => Self::PayloadTooLarge {
                limit,
                size: declared,
            },
            CodecRefusal::AllocationExceeded { limit, requested } => Self::PayloadTooLarge {
                limit,
                size: requested,
            },
            refusal => Self::ProtocolViolation {
                reason: refusal.to_string(),
            },
        }
    }
}
