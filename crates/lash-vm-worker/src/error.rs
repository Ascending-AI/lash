use lash_vm_protocol::{CodecRefusal, InfrastructureOutcome, SupervisorEvidence};
use thiserror::Error;

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum PoolError {
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
    #[error("releasing this slot needs the broker's pending-effect checkpoint")]
    PendingEffectParkingRequired,
    #[error("worker pool configuration is invalid")]
    InvalidConfiguration,
    #[error("this platform has no worker descriptor adapter")]
    UnsupportedPlatform,
    #[error("worker I/O failed with OS error {code:?}: {message}")]
    Io { code: Option<i32>, message: String },
}

impl PoolError {
    pub(crate) fn protocol(error: impl std::fmt::Display) -> Self {
        InfrastructureOutcome::ProtocolViolation {
            reason: error.to_string(),
        }
        .into()
    }
    pub(crate) fn eof() -> Self {
        InfrastructureOutcome::WorkerCrashed {
            evidence: SupervisorEvidence::EndOfStream,
        }
        .into()
    }
    pub(crate) fn io(error: std::io::Error) -> Self {
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
