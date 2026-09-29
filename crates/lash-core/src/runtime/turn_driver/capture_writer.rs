//! One step body's writer of the turn capture (ADR 0114 §3, §4).
//!
//! A step body that emits host-visible content stages it as capture frames
//! under its own writer lease. The frames persist in bounded batches, and the
//! observations they back publish only once their batch is acknowledged, so
//! everything a host was ever sent survives the worker that sent it. At most
//! one append is in flight: frames that arrive while it runs form the next
//! batch.

use std::collections::BTreeSet;
use std::sync::Arc;

use crate::store::{
    CAPTURE_BATCH_MAX_BYTES, CAPTURE_BATCH_MAX_FRAMES, CaptureAttemptReset, CaptureBatch,
    CaptureFrame, CaptureInvocationKey, CaptureWriterLeaseRef, OpenCaptureWriter, StoreError,
};

pub(in crate::runtime) struct CaptureWriter {
    store: Arc<dyn crate::RuntimePersistence>,
    lease: CaptureWriterLeaseRef,
    /// The next batch ordinal under the current epoch: an append's
    /// idempotency key with the lease.
    next_batch: u64,
    pending: Vec<CaptureFrame>,
    acknowledged_through: Option<u64>,
}

impl CaptureWriter {
    /// Opens the invocation's writer. A successor that inherits frames an
    /// earlier worker acknowledged restarts the step's work, so it retracts
    /// every inherited epoch before it writes anything new (ADR 0114 §4.4).
    pub(in crate::runtime) async fn open(
        store: Arc<dyn crate::RuntimePersistence>,
        turn: crate::TurnAddress,
        root: crate::TurnId,
        invocation: CaptureInvocationKey,
    ) -> Result<Self, StoreError> {
        let lease = store
            .open_capture_writer(&OpenCaptureWriter {
                turn,
                root,
                invocation,
            })
            .await?;
        let inherited = lease
            .inherited
            .iter()
            .map(|(key, _)| key.attempt_epoch)
            .collect::<BTreeSet<_>>();
        let mut writer = Self {
            acknowledged_through: lease.inherited.iter().map(|(key, _)| key.sequence).max(),
            lease: lease.lease_ref(),
            store,
            next_batch: 0,
            pending: Vec::new(),
        };
        for attempt_epoch in inherited {
            let reset = CaptureAttemptReset {
                lease: CaptureWriterLeaseRef {
                    attempt_epoch,
                    ..writer.lease.clone()
                },
            };
            writer.lease = writer
                .store
                .persist_attempt_reset(&reset)
                .await?
                .lease_ref();
        }
        Ok(writer)
    }

    /// Stages one frame for the next flush.
    pub(in crate::runtime) fn push(&mut self, frame: CaptureFrame) {
        self.pending.push(frame);
    }

    /// Persists every staged frame, one bounded batch at a time.
    pub(in crate::runtime) async fn flush(&mut self) -> Result<(), StoreError> {
        while !self.pending.is_empty() {
            let frames = take_batch(&mut self.pending)?;
            let batch = CaptureBatch {
                lease: self.lease.clone(),
                batch_ordinal: self.next_batch,
                frames,
            };
            let ack = self.store.append_capture_batch(&batch).await?;
            self.next_batch += 1;
            self.acknowledged_through = Some(ack.last_sequence);
        }
        Ok(())
    }

    /// Persists the staged frames, then retracts the current attempt epoch
    /// and moves to the next one (ADR 0114 §4.2).
    pub(in crate::runtime) async fn reset_attempt(&mut self) -> Result<(), StoreError> {
        self.flush().await?;
        let lease = self
            .store
            .persist_attempt_reset(&CaptureAttemptReset {
                lease: self.lease.clone(),
            })
            .await?;
        self.lease = lease.lease_ref();
        self.next_batch = 0;
        Ok(())
    }

    /// Where this writer stood: the reference the step's recorded outcome
    /// carries. `None` until anything was acknowledged.
    pub(in crate::runtime) fn watermark(
        &self,
    ) -> Option<lash_core_execution::runtime::CaptureWatermark> {
        self.acknowledged_through.map(|acknowledged_through| {
            lash_core_execution::runtime::CaptureWatermark {
                invocation: self.lease.invocation.clone(),
                attempt_epoch: self.lease.attempt_epoch,
                acknowledged_through,
            }
        })
    }
}

/// Takes the longest prefix of `pending` within both batch bounds. A frame
/// larger than the byte bound travels alone.
fn take_batch(pending: &mut Vec<CaptureFrame>) -> Result<Vec<CaptureFrame>, StoreError> {
    let mut bytes = 0u64;
    let mut count = 0usize;
    for frame in pending.iter() {
        let size = serde_json::to_vec(frame)
            .map_err(|error| StoreError::RecordEncodingFailed {
                record_kind: "capture frame".to_string(),
                message: error.to_string(),
            })?
            .len() as u64;
        if count > 0
            && (count == CAPTURE_BATCH_MAX_FRAMES
                || bytes.saturating_add(size) > CAPTURE_BATCH_MAX_BYTES)
        {
            break;
        }
        bytes = bytes.saturating_add(size);
        count += 1;
    }
    Ok(pending.drain(..count).collect())
}

/// The live fault a failed capture write ends its step with: retried, never
/// recorded, never a stop or an empty partial. A deleted session stays the
/// settled refusal it is.
pub(in crate::runtime) fn capture_write_fault(
    error: StoreError,
) -> crate::RuntimeEffectControllerError {
    if matches!(error, StoreError::SessionDeleted { .. }) {
        return error.into();
    }
    crate::RuntimeEffectControllerError::turn_capture_write_failed(format!(
        "turn capture write failed: {error}"
    ))
}
