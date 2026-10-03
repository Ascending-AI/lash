use super::*;
use std::io::{self, Write};

/// Charge serialized payload bytes without allocating an encoded copy. Inline
/// event/reservation descriptors are charged separately. Shared read views are
/// charged in full for each event, rather than relying on other Arc owners.
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
    if let SessionObservationEventPayload::Committed { rows, .. } = &event.payload {
        counter.count(rows)?;
    }
    match &event.payload {
        SessionObservationEventPayload::TurnActivity(activity) => counter.count(activity)?,
        SessionObservationEventPayload::Committed { read_view, .. }
        | SessionObservationEventPayload::ResidentChanged { read_view } => {
            counter.count(&(
                read_view.session_id(),
                read_view.session_graph(),
                read_view.policy(),
                read_view.protocol_turn_options(),
                read_view.token_usage(),
                read_view.last_prompt_usage(),
                read_view.durable_relation(),
                read_view.messages(),
                read_view.active_events(),
            ))?;
        }
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
