/// Controllable epoch clock shared by store and runtime conformance tests.
#[derive(Debug)]
pub struct TestClock(std::sync::atomic::AtomicU64);

impl TestClock {
    pub fn new(timestamp_ms: u64) -> Self {
        Self(std::sync::atomic::AtomicU64::new(timestamp_ms))
    }

    pub fn advance(&self, duration_ms: u64) {
        self.0
            .fetch_add(duration_ms, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn set(&self, timestamp_ms: u64) {
        self.0
            .store(timestamp_ms, std::sync::atomic::Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl crate::clock::Clock for TestClock {
    fn now(&self) -> std::time::Instant {
        std::time::Instant::now()
    }

    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        let timestamp_ms = self.0.load(std::sync::atomic::Ordering::SeqCst);
        chrono::DateTime::from(
            std::time::UNIX_EPOCH + std::time::Duration::from_millis(timestamp_ms),
        )
    }

    async fn sleep(&self, duration: std::time::Duration) {
        tokio::time::sleep(duration).await;
    }

    async fn sleep_until(&self, deadline: std::time::Instant) {
        tokio::time::sleep_until(deadline.into()).await;
    }
}
