use super::*;

/// One expiry key per resident entry. Removal carries its position
/// floor away with the buffer. A scalar watermark fences removed history without
/// retaining an entry or tombstone for each evicted session.
#[derive(Debug, Default)]
pub(super) struct ReplayRetention {
    pub(super) buffers: HashMap<SessionId, LiveReplaySessionBuffer>,
    idle: BTreeMap<(Instant, SessionId), ()>,
    pub(super) retained_bytes: usize,
    high_watermark: u64,
}

impl ReplayRetention {
    pub(super) fn invalidate_all(&mut self) -> Result<(), LiveReplayStoreError> {
        // Advance even with no resident window: cursors issued for an unknown
        // subject must also be behind the invalidation fence.
        self.high_watermark = self
            .high_watermark
            .checked_add(1)
            .ok_or_else(|| LiveReplayStoreError::Store("live replay position overflow".into()))?;
        self.buffers.clear();
        self.idle.clear();
        self.retained_bytes = 0;
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn expiry_entry_count(&self) -> usize {
        self.idle.len()
    }

    pub(super) fn touch(&mut self, session_id: &SessionId, now: Instant) {
        if let Some(buffer) = self.buffers.get_mut(session_id) {
            self.idle.remove(&(buffer.last_access, session_id.clone()));
            buffer.last_access = now;
            self.idle.insert((now, session_id.clone()), ());
        }
    }

    pub(super) fn remove(&mut self, session_id: &SessionId) {
        if let Some(buffer) = self.buffers.remove(session_id) {
            self.idle.remove(&(buffer.last_access, session_id.clone()));
            self.retained_bytes -= buffer.retained_bytes;
            if self.buffers.is_empty() {
                self.buffers = HashMap::new();
            }
        }
    }

    /// Release the entries idle beyond `max_age` that nobody follows, doing
    /// at most `budget` units of work. An entry with a live subscriber
    /// counts as accessed instead: its follower is at the tail of a session
    /// that is only quiet, and releasing it would gap them over nothing.
    pub(super) fn expire(
        &mut self,
        config: &InMemoryLiveReplayStoreConfig,
        now: Instant,
        budget: usize,
    ) -> usize {
        let mut removed = 0;
        for _ in 0..budget {
            let Some((last_access, session_id)) = self.idle.first_key_value().map(|(key, ())| key)
            else {
                break;
            };
            if now.saturating_duration_since(*last_access) <= config.max_age {
                break;
            }
            let session_id = session_id.clone();
            if self
                .buffers
                .get(&session_id)
                .is_some_and(LiveReplaySessionBuffer::is_followed)
            {
                self.touch(&session_id, now);
            } else {
                self.remove(&session_id);
                removed += 1;
            }
        }
        removed
    }

    pub(super) fn ensure_session(
        &mut self,
        config: &InMemoryLiveReplayStoreConfig,
        session_id: &SessionId,
        now: Instant,
        replay_incarnation_id: &str,
    ) -> Result<(), LiveReplayStoreError> {
        if self.buffers.contains_key(session_id) {
            self.touch(session_id, now);
            return Ok(());
        }
        let metadata_bytes = session_id
            .len()
            .checked_mul(3)
            .and_then(|bytes| {
                bytes.checked_add(std::mem::size_of::<LiveReplaySessionBuffer>() + 256)
            })
            .ok_or_else(capacity_error)?;
        if config.max_sessions == 0 || metadata_bytes > config.max_retained_bytes {
            return Err(capacity_error());
        }
        while self.buffers.len() >= config.max_sessions
            || self.retained_bytes > config.max_retained_bytes - metadata_bytes
        {
            self.evict_oldest(None)?;
        }
        let first_position = self
            .high_watermark
            .checked_add(1)
            .ok_or_else(|| LiveReplayStoreError::Store("live replay position overflow".into()))?;
        self.high_watermark = first_position;
        let mut buffer = LiveReplaySessionBuffer::new(now, first_position, replay_incarnation_id);
        buffer.retained_bytes = metadata_bytes;
        self.retained_bytes += metadata_bytes;
        self.buffers.insert(session_id.clone(), buffer);
        self.idle.insert((now, session_id.clone()), ());
        Ok(())
    }

    pub(super) fn reserve_bytes(
        &mut self,
        config: &InMemoryLiveReplayStoreConfig,
        session_id: &SessionId,
        additional: usize,
    ) -> Result<(), LiveReplayStoreError> {
        let buffer_bytes = self
            .buffers
            .get(session_id)
            .map_or(0, |buffer| buffer.retained_bytes);
        if additional > config.max_retained_bytes
            || buffer_bytes > config.max_retained_bytes - additional
        {
            return Err(capacity_error());
        }
        while self.retained_bytes > config.max_retained_bytes - additional {
            self.evict_oldest(Some(session_id))?;
        }
        Ok(())
    }

    fn evict_oldest(&mut self, protected: Option<&SessionId>) -> Result<(), LiveReplayStoreError> {
        let victim = self
            .idle
            .keys()
            .find(|(_, id)| Some(id) != protected)
            .map(|(_, id)| id.clone())
            .ok_or_else(capacity_error)?;
        self.remove(&victim);
        Ok(())
    }

    pub(super) fn update<R>(
        &mut self,
        session_id: &SessionId,
        mutate: impl FnOnce(&mut LiveReplaySessionBuffer) -> R,
    ) -> Option<R> {
        let buffer = self.buffers.get_mut(session_id)?;
        let before = buffer.retained_bytes;
        let result = mutate(buffer);
        self.high_watermark = self.high_watermark.max(buffer.tail_position);
        self.retained_bytes = self.retained_bytes - before + buffer.retained_bytes;
        Some(result)
    }
}

fn capacity_error() -> LiveReplayStoreError {
    LiveReplayStoreError::Store(
        "live replay publication exceeds the store retention capacity".into(),
    )
}

pub(super) fn channel_bytes(capacity: usize) -> Result<usize, LiveReplayStoreError> {
    capacity
        .max(1)
        .checked_next_power_of_two()
        .and_then(|slots| slots.checked_mul(std::mem::size_of::<ReplayNotification>() + 64))
        .ok_or_else(capacity_error)
}
