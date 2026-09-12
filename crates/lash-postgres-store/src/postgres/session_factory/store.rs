use super::*;

impl PostgresSessionStoreFactory {
    pub(super) fn store_for(&self, session_id: SessionId) -> PostgresSessionStore {
        PostgresSessionStore {
            pool: self.pool.clone(),
            await_event_signing_secret: Arc::clone(&self.await_event_signing_secret),
            clock: Arc::clone(&self.clock),
            session_id,
            #[cfg(any(test, feature = "testing"))]
            lease_clock_for_testing: self.lease_clock_for_testing.clone(),
            #[cfg(test)]
            checkpoint_probe_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            #[cfg(test)]
            checkpoint_write_transaction_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }
}
