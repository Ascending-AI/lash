use lash_vm_protocol::{CodecRefusal, InfrastructureOutcome, SupervisorEvidence};
use thiserror::Error;

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum PoolError {
    #[error(transparent)]
    ProtocolVersion(#[from] lash_vm_protocol::ProtocolVersionRefusal),
    #[error("worker recovery store failed: {message}")]
    Recovery { message: String },
    #[error(transparent)]
    Infrastructure(#[from] InfrastructureOutcome),
    #[error("the worker queue refuses {bytes} bytes at its item or byte bound")]
    QueueFull { bytes: usize },
    #[error("worker checkout exceeded its bounded wait")]
    CheckoutTimedOut,
    #[error("repeated worker failures exhausted the restart window")]
    RestartStorm,
    #[error("the execution exhausted its replacement attempt budget")]
    RetryLimitExceeded,
    #[error("worker pool configuration is invalid")]
    InvalidConfiguration,
    #[error("this platform has no worker descriptor adapter")]
    UnsupportedPlatform,
    #[error("worker I/O failed with OS error {code:?}: {message}")]
    Io { code: Option<i32>, message: String },
}

impl PoolError {
    /// Whether this is a verdict of the host and the attempt that met it:
    /// its worker failure, worker budget (a deadline, cumulative CPU or
    /// replacement attempts), pool capacity, or recovery store. It is read
    /// live, outside any recorded step, and a replay or another host with
    /// capacity answers it differently, so it fails the attempt and is never
    /// an execution's recorded outcome (FIG-4451, FIG-4459). Infrastructure
    /// faults use the same retryability classification during setup and
    /// execution; only a limit of the run itself remains a recorded outcome.
    pub fn is_host_verdict(&self) -> bool {
        match self {
            Self::Infrastructure(outcome) => outcome.is_retryable(),
            Self::Recovery { .. }
            | Self::QueueFull { .. }
            | Self::CheckoutTimedOut
            | Self::RestartStorm
            | Self::RetryLimitExceeded => true,
            Self::ProtocolVersion(_)
            | Self::InvalidConfiguration
            | Self::UnsupportedPlatform
            | Self::Io { .. } => false,
        }
    }
    pub fn protocol(error: impl std::fmt::Display) -> Self {
        InfrastructureOutcome::ProtocolViolation {
            reason: error.to_string(),
        }
        .into()
    }
    pub fn eof() -> Self {
        InfrastructureOutcome::WorkerCrashed {
            evidence: SupervisorEvidence::EndOfStream,
        }
        .into()
    }
    pub fn io(error: std::io::Error) -> Self {
        match error.kind() {
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => {
                InfrastructureOutcome::WorkerUnresponsive { silent_ms: 0 }.into()
            }
            std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::UnexpectedEof => Self::eof(),
            _ => Self::Io {
                code: error.raw_os_error(),
                message: error.to_string(),
            },
        }
    }
}
impl From<CodecRefusal> for PoolError {
    fn from(error: CodecRefusal) -> Self {
        InfrastructureOutcome::from(error).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_vm_protocol::WorkerLimit;

    #[test]
    fn retryable_worker_faults_are_host_verdicts() {
        assert!(
            !PoolError::Infrastructure(InfrastructureOutcome::PayloadTooLarge {
                limit: 1,
                size: 2,
            })
            .is_host_verdict()
        );
        for outcome in [
            InfrastructureOutcome::WorkerCrashed {
                evidence: SupervisorEvidence::EndOfStream,
            },
            InfrastructureOutcome::WorkerUnresponsive { silent_ms: 10 },
            InfrastructureOutcome::ProtocolViolation {
                reason: "lost worker response".into(),
            },
            InfrastructureOutcome::WorkerLimitExceeded {
                limit: WorkerLimit::Deadline,
            },
        ] {
            assert!(
                PoolError::Infrastructure(outcome.clone()).is_host_verdict(),
                "{outcome:?}: a retryable worker fault must fail the attempt"
            );
        }
        for limit in [
            WorkerLimit::Fuel,
            WorkerLimit::Heap,
            WorkerLimit::Depth,
            WorkerLimit::Observations,
            WorkerLimit::EffectValue { size: 2, bound: 1 },
            WorkerLimit::VmState { size: 2, bound: 1 },
            WorkerLimit::Frame {
                kind: lash_vm_protocol::WorkerFrameKind::Complete,
                size: 2,
                bound: 1,
            },
        ] {
            assert!(
                !PoolError::Infrastructure(InfrastructureOutcome::WorkerLimitExceeded { limit })
                    .is_host_verdict(),
                "{limit:?}: the run's own limit remains a recorded outcome"
            );
        }
    }
}
