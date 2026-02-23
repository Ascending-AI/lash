use std::sync::OnceLock;
use std::time::Duration;

const DEFAULT_STREAM_CHUNK_TIMEOUT: Duration = Duration::from_secs(120);

pub fn stream_chunk_timeout() -> Duration {
    static CACHED: OnceLock<Duration> = OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var("LASH_LLM_STREAM_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(Duration::from_secs)
            .filter(|d| !d.is_zero())
            .unwrap_or(DEFAULT_STREAM_CHUNK_TIMEOUT)
    })
}
