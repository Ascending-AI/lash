//! Typed infrastructure outcomes: what a worker's failure is, kept apart from
//! anything the guest did.
//!
//! A guest error is the program's own failure and travels as
//! [`crate::WorkerMessage::GuestError`]. Everything here is the worker's
//! failure: the parent fences the lease, settles the operations it already
//! admitted, and retries transient worker failures. A deterministic run limit
//! or a refusal of the run's own inputs is recorded as the run's terminal
//! outcome. A worker that produced one is discarded, never reset.
//!
//! Every cause is a variant. A [`Detail`] beside one is a bounded diagnostic
//! and never decides a class.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::codec::CodecRefusal;
use crate::message::HeaderRefusal;
use crate::state::{OpaqueStateRefusal, VmStateKind};
use crate::version::ProtocolVersionRefusal;

/// What the supervisor observed of a worker's end. Evidence, never testimony:
/// a worker cannot claim how it ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
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

/// A diagnostic beside a typed cause, cut to [`Self::MAX_BYTES`]. The variant
/// that carries it decides the class; the bound keeps a refusal within any
/// frame whatever its text.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(from = "String", into = "String")]
pub struct Detail(String);

impl Detail {
    pub const MAX_BYTES: usize = 512;

    pub fn new(text: impl std::fmt::Display) -> Self {
        Self::from(text.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for Detail {
    fn from(mut text: String) -> Self {
        if text.len() > Self::MAX_BYTES {
            let mut end = Self::MAX_BYTES;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
        }
        Self(text)
    }
}

impl From<Detail> for String {
    fn from(detail: Detail) -> Self {
        detail.0
    }
}

impl std::fmt::Display for Detail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The exchange a worker answered with a message it does not admit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Exchange {
    /// The first message of a checkout, which is the handshake.
    Handshake,
    /// A step of the run.
    Run,
    /// The parent was performing the run's request.
    RequestInFlight,
    Park,
    /// The run declined a park, and owed its request again.
    DeclinedPark,
    Prepare,
    Reset,
}

impl std::fmt::Display for Exchange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Handshake => "as its handshake",
            Self::Run => "mid-run",
            Self::RequestInFlight => "while its request was performed",
            Self::Park => "in answer to a park",
            Self::DeclinedPark => "after a declined park",
            Self::Prepare => "in answer to pure work",
            Self::Reset => "in answer to a reset",
        })
    }
}

/// A message that arrived out of order, or an exchange whose accounting does
/// not add up.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Error, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SequenceFault {
    #[error("an idle worker expects Start or lifecycle control")]
    IdleWithoutStart,
    #[error("the exchange runs under no lease")]
    MissingLease,
    #[error("the exchange has no owner")]
    MissingOwner,
    #[error("Start before reset")]
    StartBeforeReset,
    #[error("Prepare before reset")]
    PrepareBeforeReset,
    #[error("the checkout already started")]
    CheckoutAlreadyStarted,
    #[error("a result answers no pending request")]
    NoPendingRequest,
    #[error("a result names another request than the pending one")]
    WrongRequestId,
    #[error("a request repeats an id the run already used")]
    RepeatedRequestId,
    #[error("a control request was answered with another outcome")]
    WrongControlResult,
    #[error("a checkpoint result answered an effect")]
    CheckpointAnsweredEffect,
    #[error("park answers no parkable request")]
    ParkWithoutParkableRequest,
    #[error("the resumed run issued another request than the one it parked on")]
    ResumedRequestChanged,
    #[error("a projection read was answered with another control")]
    ProjectionOtherControl,
    #[error("a worker phase is out of order")]
    InvalidPhase,
    #[error("worker CPU accounting regressed")]
    CpuAccountingRegressed,
    #[error("exchange timing arrived unasked")]
    UnexpectedExchangeTiming,
    #[error("a failed worker cannot reset")]
    FailedWorkerReset,
    #[error("the stream ended inside a frame")]
    EndInPartialFrame,
    #[error("a frame header is incomplete")]
    IncompleteFrameHeader,
    #[error("lease space is exhausted")]
    LeaseSpaceExhausted,
    #[error("pure work was answered with another response")]
    UnexpectedServiceResponse,
    #[error("a completion names other definitions than its snapshot")]
    CompletionDefinitionsMismatch,
    #[error("the worker receipt probe overflowed")]
    ReceiptProbeOverflow,
}

/// A typed payload inside a frame that did not encode or decode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PayloadKind {
    Bootstrap,
    EffectRequest,
    EffectOutcome,
    CancelCheckpoint,
    ParkDecline,
    ProjectionRead,
    ProjectionResponse,
    ProjectedValue,
    Observation,
    ParkedRun,
    Continuation,
    Snapshot,
    Completion,
    GuestError,
    ServiceRequest,
    ServiceResponse,
}

/// Why a freshly exec'd worker could not take its place on the pipe.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Error, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum BootstrapFault {
    #[error("the worker environment is not empty")]
    EnvironmentNotEmpty,
    #[error("the IPC descriptor is missing")]
    MissingDescriptor,
    #[error("the IPC descriptor is not a stream socket above stdio")]
    InvalidDescriptor,
    #[error("inherited descriptors cannot be enumerated")]
    DescriptorEnumeration,
    #[error("the IPC bounds are missing")]
    MissingBounds,
    #[error("the IPC bounds do not decode")]
    InvalidBounds,
}

/// A failure of the supervising pool itself, met while it drove a worker.
#[derive(Clone, Debug, PartialEq, Eq, Error, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PoolFault {
    #[error("the worker recovery store failed")]
    Recovery,
    #[error("the worker queue refuses {bytes} bytes")]
    QueueFull { bytes: u64 },
    #[error("worker checkout exceeded its bounded wait")]
    CheckoutTimedOut,
    #[error("repeated worker failures exhausted the restart window")]
    RestartStorm,
    #[error("worker pool configuration is invalid")]
    InvalidConfiguration,
    #[error("this platform has no worker descriptor adapter")]
    UnsupportedPlatform,
    #[error("worker I/O failed with OS error {code:?}")]
    Io { code: Option<i32> },
}

/// A parent, its worker or the pool between them broke the protocol. A fresh
/// worker may keep it, so the owning invocation is re-driven.
#[derive(Clone, Debug, PartialEq, Eq, Error, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolBreach {
    #[error("{refusal}")]
    Frame { refusal: CodecRefusal },
    #[error("{refusal}")]
    Header { refusal: HeaderRefusal },
    #[error("{refusal}")]
    Version { refusal: ProtocolVersionRefusal },
    #[error("{fault}")]
    Sequence { fault: SequenceFault },
    #[error("the worker sent {found:?} {exchange}")]
    Unexpected {
        exchange: Exchange,
        found: WorkerFrameKind,
    },
    #[error("{payload:?} payload is refused: {detail}")]
    Payload {
        payload: PayloadKind,
        detail: Detail,
    },
    /// State the worker itself produced fails the parent's structural check.
    #[error("the worker's state is refused: {refusal}")]
    State { refusal: OpaqueStateRefusal },
    #[error("the VM refused a step the protocol drove: {detail}")]
    Vm { detail: Detail },
    #[error("{fault}")]
    Bootstrap { fault: BootstrapFault },
    /// Worker code, or the parent task driving it, panicked.
    #[error("worker code panicked: {detail}")]
    Panicked { detail: Detail },
    #[error("{fault}")]
    Pool { fault: PoolFault },
}

/// An input of the run that the worker reads when it starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunInput {
    Context,
    Artifact,
    State { kind: VmStateKind },
}

impl std::fmt::Display for RunInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Context => "context",
            Self::Artifact => "artifact",
            Self::State {
                kind: VmStateKind::Continuation,
            } => "continuation",
            Self::State {
                kind: VmStateKind::Snapshot,
            } => "snapshot",
        })
    }
}

/// The run's own inputs are refused. They are the same on every attempt, so
/// re-driving the invocation refuses them again: the refusal is terminal.
#[derive(Clone, Debug, PartialEq, Eq, Error, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunRefusal {
    #[error("unusable payload schema: {source}")]
    UnusableSchema {
        source: Box<lash_sansio::SchemaAdmissionError>,
    },
    #[error("{refusal}")]
    State { refusal: OpaqueStateRefusal },
    #[error("the run's {input} does not decode: {detail}")]
    Undecodable { input: RunInput, detail: Detail },
    #[error("a payload of {size} bytes exceeds the {limit}-byte bound")]
    PayloadTooLarge { limit: u64, size: u64 },
    #[error("the run names a context this worker does not know")]
    UnknownContext,
    #[error("the source is in another dialect than the worker's")]
    SourceDialect,
    #[error("the source does not parse: {detail}")]
    Parse { detail: Detail },
    #[error("the program does not compile: {detail}")]
    Compile { detail: Detail },
    #[error("the artifact is not the module the run names")]
    ArtifactIdentityMismatch,
    #[error("the artifact is inconsistent: {detail}")]
    Artifact { detail: Detail },
    #[error("a run limit is zero")]
    ZeroLimit,
    #[error("a projected binding is refused: {detail}")]
    ProjectedBinding { detail: Detail },
    #[error("a global is refused: {detail}")]
    Global { detail: Detail },
    #[error("a protected global cannot be patched")]
    ProtectedGlobal,
    #[error("the VM refuses to start the run: {detail}")]
    Start { detail: Detail },
    #[error("the resumed run stands on another request than the one it parked on")]
    ParkedRequestChanged,
    #[error("the terminal value nests deeper than {limit}")]
    ValueTooDeep { limit: u32 },
}

/// What a worker may say of its own failure: that the protocol was broken, or
/// that it refuses the run. How it ended is the supervisor's evidence, and a
/// limit travels as [`crate::WorkerMessage::LimitExceeded`].
#[derive(Clone, Debug, PartialEq, Eq, Error, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerRefusal {
    #[error("{0}")]
    Breach(ProtocolBreach),
    #[error("{0}")]
    Run(RunRefusal),
}

/// Why this deployment cannot launch its configured worker executable.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Error, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum WorkerDeploymentFault {
    #[error("the configured executable was not found")]
    NotFound,
    #[error("the configured file cannot be executed")]
    NotExecutable,
}

#[derive(Clone, Debug, PartialEq, Eq, Error, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InfrastructureOutcome {
    #[error("the worker executable {executable:?} is unavailable: {fault}")]
    WorkerDeployment {
        executable: std::path::PathBuf,
        fault: WorkerDeploymentFault,
    },
    #[error("the worker crashed ({evidence:?})")]
    WorkerCrashed { evidence: SupervisorEvidence },
    #[error("the worker sent nothing for {silent_ms} ms")]
    WorkerUnresponsive { silent_ms: u64 },
    #[error("the worker protocol was broken: {breach}")]
    ProtocolViolation { breach: ProtocolBreach },
    #[error("the run is refused: {refusal}")]
    RunRefused { refusal: RunRefusal },
    #[error("the run exhausted its {limit:?} limit")]
    WorkerLimitExceeded { limit: WorkerLimit },
}

impl InfrastructureOutcome {
    /// Whether re-driving the owning invocation can succeed. A refusal of the
    /// run's inputs and a limit the run itself exhausted fail the same way on
    /// every attempt; a host's deadline or budget does not
    /// ([`WorkerLimit::is_host_verdict`]).
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::WorkerLimitExceeded { limit } => limit.is_host_verdict(),
            Self::RunRefused { .. } | Self::WorkerDeployment { .. } => false,
            Self::WorkerCrashed { .. }
            | Self::WorkerUnresponsive { .. }
            | Self::ProtocolViolation { .. } => true,
        }
    }

    /// The deployment fault, whose repair lets an operator redrive retained work.
    pub fn deployment_fault(&self) -> Option<(&std::path::Path, WorkerDeploymentFault)> {
        match self {
            Self::WorkerDeployment { executable, fault } => Some((executable, *fault)),
            _ => None,
        }
    }

    /// State a run was handed fails its structural check. It is one of the
    /// run's inputs: over its bound it is the run's limit, and any other
    /// refusal is the run's, on every path that reads it.
    pub fn input_state(refusal: OpaqueStateRefusal) -> Self {
        match refusal {
            OpaqueStateRefusal::TooLarge { limit, len } => Self::state_limit(limit, len),
            refusal @ (OpaqueStateRefusal::WrongKind { .. }
            | OpaqueStateRefusal::WrongOwner { .. }
            | OpaqueStateRefusal::ComponentOutsideReadRange { .. }
            | OpaqueStateRefusal::HashMismatch) => RunRefusal::State { refusal }.into(),
        }
    }

    /// State a worker produced fails the parent's structural check. Its size
    /// is the run's limit; anything else is the worker's breach.
    pub fn output_state(refusal: OpaqueStateRefusal) -> Self {
        match refusal {
            OpaqueStateRefusal::TooLarge { limit, len } => Self::state_limit(limit, len),
            refusal @ (OpaqueStateRefusal::WrongKind { .. }
            | OpaqueStateRefusal::WrongOwner { .. }
            | OpaqueStateRefusal::ComponentOutsideReadRange { .. }
            | OpaqueStateRefusal::HashMismatch) => ProtocolBreach::State { refusal }.into(),
        }
    }

    fn state_limit(limit: u64, len: u64) -> Self {
        Self::WorkerLimitExceeded {
            limit: WorkerLimit::VmState {
                size: len,
                bound: limit,
            },
        }
    }
}

impl WorkerRefusal {
    /// The parent's reading of a worker's refusal: the one mapping every
    /// reader of the pipe shares.
    pub fn into_outcome(self) -> InfrastructureOutcome {
        match self {
            Self::Breach(breach) => InfrastructureOutcome::ProtocolViolation { breach },
            Self::Run(refusal) => InfrastructureOutcome::RunRefused { refusal },
        }
    }

    /// What a worker sends its parent for `outcome`: a refusal, a limit, or
    /// nothing for an end only the supervisor can witness.
    pub fn testimony(outcome: InfrastructureOutcome) -> Option<crate::WorkerMessage> {
        match outcome {
            InfrastructureOutcome::WorkerLimitExceeded { limit } => {
                Some(crate::WorkerMessage::LimitExceeded { limit })
            }
            InfrastructureOutcome::ProtocolViolation { breach } => {
                Some(crate::WorkerMessage::Refused {
                    refusal: Self::Breach(breach),
                })
            }
            InfrastructureOutcome::RunRefused { refusal } => Some(crate::WorkerMessage::Refused {
                refusal: Self::Run(refusal),
            }),
            InfrastructureOutcome::WorkerDeployment { .. }
            | InfrastructureOutcome::WorkerCrashed { .. }
            | InfrastructureOutcome::WorkerUnresponsive { .. } => None,
        }
    }
}

impl From<ProtocolBreach> for InfrastructureOutcome {
    fn from(breach: ProtocolBreach) -> Self {
        Self::ProtocolViolation { breach }
    }
}

impl From<RunRefusal> for InfrastructureOutcome {
    fn from(refusal: RunRefusal) -> Self {
        Self::RunRefused { refusal }
    }
}

impl From<SequenceFault> for ProtocolBreach {
    fn from(fault: SequenceFault) -> Self {
        Self::Sequence { fault }
    }
}

impl From<HeaderRefusal> for ProtocolBreach {
    fn from(refusal: HeaderRefusal) -> Self {
        Self::Header { refusal }
    }
}

impl From<BootstrapFault> for ProtocolBreach {
    fn from(fault: BootstrapFault) -> Self {
        Self::Bootstrap { fault }
    }
}

/// A size bound refuses the same payload on every attempt; any other codec
/// refusal is a broken frame.
impl From<CodecRefusal> for InfrastructureOutcome {
    fn from(refusal: CodecRefusal) -> Self {
        match refusal {
            CodecRefusal::FrameTooLarge { limit, declared } => RunRefusal::PayloadTooLarge {
                limit,
                size: declared,
            }
            .into(),
            CodecRefusal::AllocationExceeded { limit, requested } => RunRefusal::PayloadTooLarge {
                limit,
                size: requested,
            }
            .into(),
            refusal @ (CodecRefusal::Truncated { .. }
            | CodecRefusal::BadMagic
            | CodecRefusal::DepthExceeded { .. }
            | CodecRefusal::NodeLimitExceeded { .. }
            | CodecRefusal::Malformed { .. }
            | CodecRefusal::TrailingBytes { .. }) => ProtocolBreach::Frame { refusal }.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ExecutionLease, FrameCodec, FrameEpoch, MessageFence, OwnerEpoch, VmOwner, WorkerFrame,
        WorkerMessage,
    };

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

    /// FIG-4645: a state the run was handed fails its check the same way on
    /// every attempt, so the refusal is terminal wherever it is met.
    #[test]
    fn a_refused_input_state_is_terminal_and_a_broken_frame_is_retryable() {
        for refusal in [
            OpaqueStateRefusal::WrongKind {
                expected: VmStateKind::Snapshot,
                found: VmStateKind::Continuation,
            },
            OpaqueStateRefusal::WrongOwner {
                expected: VmOwner::new("a"),
                found: VmOwner::new("b"),
            },
            OpaqueStateRefusal::HashMismatch,
        ] {
            let outcome = InfrastructureOutcome::input_state(refusal.clone());
            assert_eq!(
                outcome,
                InfrastructureOutcome::RunRefused {
                    refusal: RunRefusal::State {
                        refusal: refusal.clone()
                    }
                }
            );
            assert!(!outcome.is_retryable());
            assert!(InfrastructureOutcome::output_state(refusal).is_retryable());
        }
        assert!(
            !InfrastructureOutcome::from(CodecRefusal::FrameTooLarge {
                limit: 1,
                declared: 2
            })
            .is_retryable()
        );
        assert!(InfrastructureOutcome::from(CodecRefusal::BadMagic).is_retryable());
    }

    /// FIG-4645: a worker testifies only to a breach, a refusal or a limit.
    /// How it ended is the supervisor's to say.
    #[test]
    fn a_worker_cannot_assert_supervisor_evidence() {
        assert_eq!(
            WorkerRefusal::testimony(InfrastructureOutcome::WorkerCrashed {
                evidence: SupervisorEvidence::Exited { code: 0 }
            }),
            None
        );
        assert_eq!(
            WorkerRefusal::testimony(InfrastructureOutcome::WorkerUnresponsive { silent_ms: 1 }),
            None
        );
        for outcome in [
            InfrastructureOutcome::from(ProtocolBreach::from(SequenceFault::WrongRequestId)),
            InfrastructureOutcome::from(RunRefusal::UnknownContext),
        ] {
            let Some(WorkerMessage::Refused { refusal }) =
                WorkerRefusal::testimony(outcome.clone())
            else {
                panic!("{outcome:?} is the worker's to refuse")
            };
            assert_eq!(refusal.into_outcome(), outcome);
        }
        for json in [
            serde_json::json!({"refused": {"refusal": {"worker_crashed": {"evidence": "end_of_stream"}}}}),
            serde_json::json!({"refused": {"refusal": {"worker_unresponsive": {"silent_ms": 1}}}}),
            serde_json::json!({"refused": {"refusal": {"worker_limit_exceeded": {"limit": "fuel"}}}}),
            serde_json::json!({"refused": {"outcome": {"worker_crashed": {"evidence": "end_of_stream"}}}}),
        ] {
            assert!(
                serde_json::from_value::<WorkerMessage>(json.clone()).is_err(),
                "{json} is not a refusal a worker can send"
            );
        }
    }

    /// FIG-4645: the text beside a cause is cut to a bound, so a refusal
    /// crosses the pipe as itself whatever its length, and keeps its class.
    #[test]
    fn a_refusals_text_length_cannot_change_its_class() {
        let codec = FrameCodec::new(crate::DecodeLimits {
            max_frame_bytes: 1024,
            ..crate::DecodeLimits::standard()
        });
        let mut fence = MessageFence::new(ExecutionLease(1), OwnerEpoch(1), FrameEpoch(1));
        for text in ["short".to_owned(), "é".repeat(1 << 20)] {
            for (outcome, retryable) in [
                (
                    InfrastructureOutcome::from(RunRefusal::Parse {
                        detail: Detail::new(&text),
                    }),
                    false,
                ),
                (
                    InfrastructureOutcome::from(ProtocolBreach::Vm {
                        detail: Detail::new(&text),
                    }),
                    true,
                ),
            ] {
                let message =
                    WorkerRefusal::testimony(outcome.clone()).expect("a worker's refusal");
                let bytes = codec
                    .encode_worker(&WorkerFrame {
                        header: fence.next_header(),
                        message,
                    })
                    .expect("a refusal fits any frame");
                let WorkerMessage::Refused { refusal } =
                    codec.decode_worker(&bytes).expect("frame").message
                else {
                    panic!("a refusal crosses as a refusal")
                };
                assert_eq!(refusal.into_outcome(), outcome);
                assert_eq!(outcome.is_retryable(), retryable);
            }
        }
        assert!(Detail::new("é".repeat(1 << 20)).as_str().len() <= Detail::MAX_BYTES);
    }
}
