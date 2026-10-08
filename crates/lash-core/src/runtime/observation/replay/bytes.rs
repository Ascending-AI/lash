use super::*;
use std::io::{self, Write};

/// Charge serialized payload bytes without allocating an encoded copy, plus
/// the event's inline descriptors. An event is charged for its own payload
/// only: a commit carries its rows delta, never the session's read view.
pub(super) fn event_bytes(
    event: &SessionObservationEvent,
    limit: usize,
) -> Result<usize, LiveReplayStoreError> {
    let mut counter = ByteCounter(
        std::mem::size_of::<StoredObservationEvent>()
            + std::mem::size_of::<SessionObservationEvent>()
            + 128,
        limit,
    );
    counter
        .write_all(event.cursor.as_str().as_bytes())
        .map_err(byte_error)?;
    if let Some(turn_id) = &event.turn_id {
        counter.write_all(turn_id.as_bytes()).map_err(byte_error)?;
    }
    match &event.payload {
        SessionObservationEventPayload::TurnActivity(activity) => counter.count(activity)?,
        SessionObservationEventPayload::Committed {
            base_revision,
            entries,
        } => counter.count(&(base_revision, entries))?,
        SessionObservationEventPayload::ResidentChanged => {}
        SessionObservationEventPayload::AgentFrameSwitched { frame_id } => {
            counter.count(frame_id)?
        }
        SessionObservationEventPayload::QueueChanged { batch_ids, .. } => {
            counter.count(batch_ids)?
        }
        SessionObservationEventPayload::ProcessChanged { process_ids, .. } => {
            counter.count(process_ids)?
        }
    }
    Ok(counter.0)
}

fn byte_error(error: impl fmt::Display) -> LiveReplayStoreError {
    LiveReplayStoreError::Store(format!("live replay byte accounting failed: {error}"))
}

struct ByteCounter(usize, usize);

impl ByteCounter {
    fn count(&mut self, value: &impl serde::Serialize) -> Result<(), LiveReplayStoreError> {
        serde_json::to_writer(self, value).map_err(byte_error)
    }
}

impl Write for ByteCounter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len())
            .ok_or_else(|| io::Error::other("byte count overflow"))?;
        if self.0 > self.1 {
            return Err(io::Error::other("store retention capacity exceeded"));
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
