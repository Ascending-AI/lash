//! The turn capture segment (ADR 0114 §3).
//!
//! A physical turn's emitted prose, tool lifecycle facts and argument
//! fragments are staged as capture frames while the turn runs. A stop fences
//! every writer, seals the cutoff and materializes the stopped partial with
//! [`reduce_capture`](crate::capture::reduce_capture); the turn's commit then
//! publishes it. Frames and partials are session rows: they live outside
//! graph nodes and history records, and no artifact referrer names them.

use lash_sansio::llm::types::StreamBlockIdentity;
use lash_sansio::{
    CaptureBase, StopReason, StoppedPartial, ToolCallOutput, ToolInputIdentity, ToolOutputChunk,
};

use super::StoreError;
use crate::{SessionId, TurnAddress, TurnId};

/// The capture key: which physical turn, which checkpoint base, which effect
/// invocation, which attempt epoch, which position.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CaptureFrameKey {
    pub turn: TurnAddress,
    pub base: CaptureBase,
    pub invocation: CaptureInvocationKey,
    pub attempt_epoch: u32,
    /// Store-assigned, dense and monotone per physical turn.
    pub sequence: u64,
}

/// The invocation's replay key (`RuntimeEffectInvocation::replay_key`).
/// Opaque to the store.
#[derive(
    Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct CaptureInvocationKey(pub String);

impl CaptureInvocationKey {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One captured fact. Each frame's content is exactly what the host was sent,
/// or will be sent once the frame is acknowledged.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "frame", rename_all = "snake_case")]
// justification: frames are transient batch DTOs matched by stores and the reducer; the settled output stays inline so the shape is the ADR's.
#[allow(clippy::large_enum_variant)]
pub enum CaptureFrame {
    TextStart {
        block: StreamBlockIdentity,
    },
    TextDelta {
        block: StreamBlockIdentity,
        text: String,
    },
    TextEnd {
        block: StreamBlockIdentity,
        text: String,
    },
    ReasoningStart {
        block: StreamBlockIdentity,
    },
    ReasoningDelta {
        block: StreamBlockIdentity,
        text: String,
    },
    ReasoningEnd {
        block: StreamBlockIdentity,
        text: String,
    },
    ToolInputStart {
        call: ToolInputIdentity,
    },
    ToolInputDelta {
        call: ToolInputIdentity,
        text: String,
    },
    ToolInputEnd {
        call: ToolInputIdentity,
        raw_arguments: String,
    },
    /// Written by the stream loop after `ToolInputEnd`, from the protocol's
    /// parse.
    ToolCallParsed {
        call: ToolInputIdentity,
        call_id: String,
        tool_name: String,
        arguments: serde_json::Value,
    },
    ToolCallUnparseable {
        call: ToolInputIdentity,
        parse_error: String,
    },
    ToolExecutionStarted {
        call_id: String,
    },
    ToolOutputProgress {
        call_id: String,
        chunk: ToolOutputChunk,
    },
    ToolSettled {
        call_id: String,
        output: ToolCallOutput,
    },
}

/// The segment's operations. Every operation is session-scoped: each request
/// carries its [`TurnAddress`], and so its session. None has a default.
#[async_trait::async_trait]
pub trait TurnCaptureStore: Send + Sync {
    /// Starts, or restarts after a worker loss, the writer of one invocation.
    /// Mints `attempt_epoch = previous + 1` and fences every earlier epoch of
    /// that invocation. The lease names the base current at open. When an
    /// earlier epoch had acknowledged frames and was never retracted, the
    /// turn is marked recovered, and the lease reports the prefix it inherits.
    async fn open_capture_writer(
        &self,
        request: &OpenCaptureWriter,
    ) -> Result<CaptureWriterLease, StoreError>;

    /// Appends one bounded batch under the writer's lease, in one
    /// transaction. It assigns sequences. It is idempotent by
    /// `(turn, invocation, attempt_epoch, batch_ordinal)`: an identical retry
    /// returns the first ack, and a different body under the same ordinal is
    /// `CaptureBatchConflict`.
    async fn append_capture_batch(&self, batch: &CaptureBatch) -> Result<CaptureAck, StoreError>;

    /// Retracts the lease's current epoch and mints the next one. The reducer
    /// then excludes every frame of the retracted epoch. Idempotent by
    /// `(turn, invocation, retracted_epoch)`.
    async fn persist_attempt_reset(
        &self,
        reset: &CaptureAttemptReset,
    ) -> Result<CaptureWriterLease, StoreError>;

    /// Moves the turn's base to `to` and deletes its frames under `to`.
    /// Idempotent at the current base. Any other value is
    /// `CaptureBaseStale`.
    async fn advance_capture_base(&self, advance: &CaptureBaseAdvance) -> Result<(), StoreError>;

    /// In one transaction: fence every writer of the turn, seal the cutoff at
    /// the highest acknowledged sequence, and materialize the partial with
    /// `reduce_capture`. First writer wins: a second call returns the
    /// existing seal whatever its request says, so a crash after sealing
    /// reuses it. Returns the committed partial once the turn has committed.
    async fn seal_turn_capture(
        &self,
        request: &SealTurnCapture,
    ) -> Result<SealedCapture, StoreError>;

    /// The authorized read. It answers only for turns the session itself
    /// owns. A fork's ancestor turns are `Unknown`.
    async fn read_stopped_partial(
        &self,
        request: &StoppedPartialReadRequest,
    ) -> Result<StoppedPartialRead, StoreError>;
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OpenCaptureWriter {
    pub turn: TurnAddress,
    pub root: TurnId,
    pub invocation: CaptureInvocationKey,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CaptureWriterLease {
    pub turn: TurnAddress,
    pub invocation: CaptureInvocationKey,
    pub attempt_epoch: u32,
    pub base: CaptureBase,
    /// Frames acknowledged under earlier epochs of this invocation and not
    /// yet retracted: the prefix a successor rebuilds.
    pub inherited: Vec<(CaptureFrameKey, CaptureFrame)>,
}

impl CaptureWriterLease {
    /// The reference a batch or reset names this lease by.
    pub fn lease_ref(&self) -> CaptureWriterLeaseRef {
        CaptureWriterLeaseRef {
            turn: self.turn.clone(),
            invocation: self.invocation.clone(),
            attempt_epoch: self.attempt_epoch,
            base: self.base,
        }
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CaptureBatch {
    pub lease: CaptureWriterLeaseRef,
    pub batch_ordinal: u64,
    /// At most `CAPTURE_BATCH_MAX_FRAMES` frames and
    /// `CAPTURE_BATCH_MAX_BYTES` of encoded frames. One frame larger than
    /// the byte bound travels alone: a tool's closed arguments or its settled
    /// output cannot be split without changing what they say.
    pub frames: Vec<CaptureFrame>,
}

pub const CAPTURE_BATCH_MAX_FRAMES: usize = 256;
pub const CAPTURE_BATCH_MAX_BYTES: u64 = 256 * 1024;

impl CaptureBatch {
    /// The encoded size the byte bound is measured in: the JSON of every
    /// frame, as stored in `frame_json`.
    pub fn encoded_bytes(&self) -> Result<u64, StoreError> {
        self.frames.iter().try_fold(0u64, |total, frame| {
            let bytes =
                serde_json::to_vec(frame).map_err(|error| StoreError::RecordEncodingFailed {
                    record_kind: "capture frame".to_string(),
                    message: error.to_string(),
                })?;
            Ok(total.saturating_add(bytes.len() as u64))
        })
    }

    /// Refuses an empty batch with `CaptureBatchEmpty`, and a batch over
    /// either bound with `CaptureBatchTooLarge`. A batch of one frame is
    /// within the byte bound whatever its size.
    pub fn validate_bounds(&self) -> Result<(), StoreError> {
        if self.frames.is_empty() {
            return Err(StoreError::CaptureBatchEmpty {
                batch_ordinal: self.batch_ordinal,
            });
        }
        let bytes = self.encoded_bytes()?;
        if self.frames.len() > CAPTURE_BATCH_MAX_FRAMES
            || (self.frames.len() > 1 && bytes > CAPTURE_BATCH_MAX_BYTES)
        {
            return Err(StoreError::CaptureBatchTooLarge {
                frames: self.frames.len(),
                bytes,
                max_frames: CAPTURE_BATCH_MAX_FRAMES,
                max_bytes: CAPTURE_BATCH_MAX_BYTES,
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CaptureWriterLeaseRef {
    pub turn: TurnAddress,
    pub invocation: CaptureInvocationKey,
    pub attempt_epoch: u32,
    pub base: CaptureBase,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CaptureAck {
    pub first_sequence: u64,
    pub last_sequence: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CaptureAttemptReset {
    pub lease: CaptureWriterLeaseRef,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CaptureBaseAdvance {
    pub turn: TurnAddress,
    pub to: CaptureBase,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SealTurnCapture {
    pub turn: TurnAddress,
    pub root: TurnId,
    pub reason: StopReason,
    /// The highest `CaptureAck::last_sequence` the drive's recorded outcomes
    /// reference. The seal must cover it, or the call fails
    /// `CaptureSealBelowWatermark`.
    pub recorded_watermark: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum SealedCapture {
    Sealed(StoppedPartial),
    Committed(StoppedPartial),
}

impl SealedCapture {
    pub fn partial(&self) -> &StoppedPartial {
        match self {
            Self::Sealed(partial) | Self::Committed(partial) => partial,
        }
    }

    pub fn into_partial(self) -> StoppedPartial {
        match self {
            Self::Sealed(partial) | Self::Committed(partial) => partial,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StoppedPartialReadRequest {
    pub session_id: SessionId,
    /// The physical turn id or the root id. Both are unique in a session,
    /// and a root has at most one stopped physical turn: only its last turn
    /// can stop.
    pub turn: TurnId,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum StoppedPartialRead {
    Available(StoppedPartial),
    /// The turn is still running, or it is sealed but not yet committed.
    Pending,
    /// The turn settled without a stop.
    NotStopped,
    /// The session owns no such turn.
    Unknown,
}
