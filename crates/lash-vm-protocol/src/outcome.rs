//! Typed infrastructure outcomes: what a worker's failure is, kept apart from
//! anything the guest did.
//!
//! A guest error is the program's own failure and travels as
//! [`crate::WorkerMessage::GuestError`]. Everything here is the worker's
//! failure: the parent fences the lease, settles the operations it already
//! admitted, and retries transient worker failures. A deterministic run limit is
//! recorded as the run's terminal outcome. A worker that produced one is
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

pub use lash_sansio::worker_limit::{WorkerFrameKind, WorkerLimit};

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
    /// itself exhausted fails the same way on every attempt; a host's
    /// deadline or budget does not ([`WorkerLimit::is_host_verdict`]).
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::WorkerLimitExceeded { limit } => limit.is_host_verdict(),
            Self::PayloadTooLarge { .. } => false,
            Self::WorkerCrashed { .. }
            | Self::WorkerUnresponsive { .. }
            | Self::ProtocolViolation { .. } => true,
        }
    }
}

impl From<crate::OpaqueStateRefusal> for InfrastructureOutcome {
    fn from(refusal: crate::OpaqueStateRefusal) -> Self {
        match refusal {
            crate::OpaqueStateRefusal::TooLarge { limit, len } => Self::WorkerLimitExceeded {
                limit: WorkerLimit::VmState {
                    size: len,
                    bound: limit,
                },
            },
            refusal => Self::ProtocolViolation {
                reason: refusal.to_string(),
            },
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_deadline_is_the_hosts_verdict_and_retryable_where_a_run_limit_is_not() {
        for (limit, host_verdict) in [
            (WorkerLimit::Fuel, false),
            (WorkerLimit::Heap, false),
            (WorkerLimit::Depth, false),
            (WorkerLimit::Observations, false),
            (WorkerLimit::EffectValue { size: 2, bound: 1 }, false),
            (WorkerLimit::VmState { size: 2, bound: 1 }, false),
            (
                WorkerLimit::Frame {
                    kind: WorkerFrameKind::Complete,
                    size: 2,
                    bound: 1,
                },
                false,
            ),
            (WorkerLimit::Deadline, true),
        ] {
            assert_eq!(limit.is_host_verdict(), host_verdict, "{limit:?}");
            assert_eq!(
                InfrastructureOutcome::WorkerLimitExceeded { limit }.is_retryable(),
                host_verdict,
                "{limit:?}: only a limit the run itself exhausted fails every attempt"
            );
        }
    }
}
