use lash_vm_protocol::{
    CodecRefusal, Detail, InfrastructureOutcome, PayloadKind, PoolFault, ProtocolBreach,
    RunRefusal, SupervisorEvidence, WorkerLimit,
};
use thiserror::Error;

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum PoolError {
    #[error(transparent)]
    ProtocolVersion(#[from] lash_vm_protocol::ProtocolVersionRefusal),
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
    /// Preserve the worker's cause and retry class at the plugin/host boundary.
    pub fn into_runtime_error(self) -> lash_core_execution::RuntimeError {
        let message = self.to_string();
        let code = if matches!(&self, Self::Infrastructure(outcome) if outcome.deployment_fault().is_some())
        {
            lash_core_execution::RuntimeErrorCode::VmWorkerUnavailable
        } else if matches!(self, Self::CheckoutTimedOut) {
            lash_core_execution::RuntimeErrorCode::WorkerCheckoutTimedOut
        } else {
            lash_core_execution::RuntimeErrorCode::VmWorkerFailed
        };
        let mut error = lash_core_execution::RuntimeError::new(code, message);
        error.cause = Some(lash_core_execution::RuntimeErrorCause::VmWorker {
            outcome: Box::new(self.into_outcome()),
        });
        error
    }

    /// Whether this is a verdict of the host and the attempt that met it:
    /// its worker failure, worker budget (a deadline, cumulative CPU or
    /// replacement attempts), pool capacity, or recovery store. It is read
    /// live, outside any recorded step, and a replay or another host with
    /// capacity answers it differently, so it fails the attempt and is never
    /// an execution's recorded outcome (FIG-4451, FIG-4459). Infrastructure
    /// faults use the same retryability classification during setup and
    /// execution. A deployment fault also aborts uncommitted work, but parks
    /// instead of retrying. Only a limit of the run itself remains a recorded outcome.
    pub fn is_host_verdict(&self) -> bool {
        match self {
            Self::Infrastructure(outcome) => {
                outcome.is_retryable() || outcome.deployment_fault().is_some()
            }
            Self::QueueFull { .. }
            | Self::CheckoutTimedOut
            | Self::RestartStorm
            | Self::RetryLimitExceeded => true,
            Self::ProtocolVersion(_)
            | Self::InvalidConfiguration
            | Self::UnsupportedPlatform
            | Self::Io { .. } => false,
        }
    }
    /// A broken exchange: retried on a fresh worker.
    pub fn breach(breach: impl Into<ProtocolBreach>) -> Self {
        InfrastructureOutcome::from(breach.into()).into()
    }
    /// A typed payload that did not encode or decode.
    pub fn payload(payload: PayloadKind, error: impl std::fmt::Display) -> Self {
        Self::breach(ProtocolBreach::Payload {
            payload,
            detail: Detail::new(error),
        })
    }
    /// A refusal of the run's own inputs: terminal.
    pub fn refused(refusal: RunRefusal) -> Self {
        InfrastructureOutcome::from(refusal).into()
    }
    /// The typed outcome this failure is to the run it met: the one mapping
    /// a transport read, a refused checkout and a worker's own refusal share.
    pub fn into_outcome(self) -> InfrastructureOutcome {
        let fault = match self {
            Self::Infrastructure(outcome) => return outcome,
            Self::ProtocolVersion(refusal) => return ProtocolBreach::Version { refusal }.into(),
            // The attempt budget is the host's clock, like a deadline.
            Self::RetryLimitExceeded => {
                return InfrastructureOutcome::WorkerLimitExceeded {
                    limit: WorkerLimit::Deadline,
                };
            }
            Self::QueueFull { bytes } => PoolFault::QueueFull {
                bytes: bytes as u64,
            },
            Self::CheckoutTimedOut => PoolFault::CheckoutTimedOut,
            Self::RestartStorm => PoolFault::RestartStorm,
            Self::InvalidConfiguration => PoolFault::InvalidConfiguration,
            Self::UnsupportedPlatform => PoolFault::UnsupportedPlatform,
            Self::Io { code, .. } => PoolFault::Io { code },
        };
        ProtocolBreach::Pool { fault }.into()
    }
    pub fn eof() -> Self {
        InfrastructureOutcome::WorkerCrashed {
            evidence: SupervisorEvidence::EndOfStream,
        }
        .into()
    }
    /// Classify failures of the configured executable at the shared spawn seam.
    pub(crate) fn spawn(error: std::io::Error, executable: &std::path::Path) -> Self {
        let fault = match error.kind() {
            std::io::ErrorKind::NotFound => lash_vm_protocol::WorkerDeploymentFault::NotFound,
            std::io::ErrorKind::PermissionDenied => {
                lash_vm_protocol::WorkerDeploymentFault::NotExecutable
            }
            _ if error.raw_os_error() == Some(libc::ENOEXEC) => {
                lash_vm_protocol::WorkerDeploymentFault::NotExecutable
            }
            _ => return Self::io(error),
        };
        InfrastructureOutcome::WorkerDeployment {
            executable: executable.to_owned(),
            fault,
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

    #[test]
    fn retryable_worker_faults_are_host_verdicts() {
        assert!(
            !PoolError::refused(RunRefusal::PayloadTooLarge { limit: 1, size: 2 })
                .is_host_verdict()
        );
        for outcome in [
            InfrastructureOutcome::WorkerCrashed {
                evidence: SupervisorEvidence::EndOfStream,
            },
            InfrastructureOutcome::WorkerUnresponsive { silent_ms: 10 },
            ProtocolBreach::from(lash_vm_protocol::SequenceFault::UnexpectedServiceResponse).into(),
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
