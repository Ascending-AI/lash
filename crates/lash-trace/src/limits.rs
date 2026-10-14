//! Host-selected working-memory and diagnostic cuts.

/// Hard maximum for individually recorded effect occurrences per process node.
pub const MAX_PROCESS_EFFECT_OCCURRENCES: u64 = 4096;

/// A host selected an evidence cut outside the supported range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TraceLimitsError {
    #[error("process effect occurrence limit {actual} exceeds maximum {maximum}")]
    ProcessEffectOccurrencesExceeded { actual: u64, maximum: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TraceLimits {
    /// Observations held before the journal frontier; zero drops all held records.
    pub held_observations: usize,
    /// Recorded payload bytes of one tool attempt's stream.
    pub attempt_stream_bytes: usize,
    /// Largest whole JSON value retained in an effect diff.
    pub diff_value_json_bytes: usize,
    /// Divergent paths included in a mismatch's error summary.
    pub diff_summary_paths: usize,
    /// Failed-generation visible output, including its truncation marker.
    pub failure_partial_output_bytes: usize,
    /// Largest whole provider request body retained in an extended trace.
    pub provider_request_body_json_bytes: usize,
    /// Cached tool-composition fingerprint generations; zero disables caching.
    pub composition_fingerprint_generations: usize,
    /// Sessions whose last reported composition is remembered; zero
    /// remembers none, so every model call reports its composition.
    pub composition_sessions: usize,
    /// Per-node effect occurrences recorded individually. Values above the
    /// hard maximum of [`MAX_PROCESS_EFFECT_OCCURRENCES`] (4096) are refused
    /// at configuration; zero summarizes every occurrence. The default is 64.
    pub process_effect_occurrences: u64,
    /// Characters retained from a trace diagnostic before its omission annotation.
    pub diagnostic_error_chars: usize,
    /// Threshold for a plugin session's state-size warning; the configured publication
    /// rejection budget remains independent. `usize::MAX` disables this warning.
    pub plugin_state_warn_bytes: usize,
}

impl TraceLimits {
    /// Refuse unsupported evidence cuts before the runtime accepts them.
    pub fn validate(self) -> Result<(), TraceLimitsError> {
        if self.process_effect_occurrences > MAX_PROCESS_EFFECT_OCCURRENCES {
            return Err(TraceLimitsError::ProcessEffectOccurrencesExceeded {
                actual: self.process_effect_occurrences,
                maximum: MAX_PROCESS_EFFECT_OCCURRENCES,
            });
        }
        Ok(())
    }

    /// Standard preset: 256 held observations, 256 KiB attempt streams,
    /// 2048-byte diff values, eight summary paths, 64 KiB failure output,
    /// 2048-byte provider bodies, eight fingerprint generations, 1024
    /// remembered session compositions, 64 process
    /// occurrences, 4000 diagnostic characters and a 4 MiB plugin-state warning. These are existing
    /// operational choices; no workload measurements justify their exact values.
    pub const fn standard() -> Self {
        Self {
            held_observations: 256,
            attempt_stream_bytes: 256 * 1024,
            diff_value_json_bytes: 2048,
            diff_summary_paths: 8,
            failure_partial_output_bytes: 64 * 1024,
            provider_request_body_json_bytes: 2048,
            composition_fingerprint_generations: 8,
            composition_sessions: 1024,
            process_effect_occurrences: 64,
            diagnostic_error_chars: 4000,
            plugin_state_warn_bytes: 4 * 1024 * 1024,
        }
    }
    /// Retain a diagnostic's head and tail, counting original Unicode characters.
    pub fn diagnostic_error(self, text: &str) -> String {
        let chars = text.chars().count();
        if chars <= self.diagnostic_error_chars {
            return text.to_owned();
        }
        let keep = self.diagnostic_error_chars / 2;
        let head = text.chars().take(keep).collect::<String>();
        let tail = text
            .chars()
            .rev()
            .take(keep)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<String>();
        let omitted = chars - keep * 2;
        format!("{head}\n\n... ({omitted} chars omitted) ...\n\n{tail}")
    }
}

impl Default for TraceLimits {
    fn default() -> Self {
        Self::standard()
    }
}

/// Optional observation work budgets, independent of retained data lifetimes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObservationWorkLimits {
    /// Provisional language publications queued in the session class.
    pub language_session_events: usize,
    /// Bytes each language publication class admits, including in-flight work.
    pub language_bytes: usize,
    /// Process publication rounds queued, including the round in flight.
    pub language_process_rounds: usize,
    pub replay_expiry_batch: std::num::NonZeroUsize,
    pub publisher_batch: std::num::NonZeroUsize,
    pub process_snapshot_event_tail: usize,
    pub session_dedup_ids: usize,
    /// Delivered event identities one process feed remembers, to drop a
    /// redelivery.
    pub process_dedup_ids: usize,
    /// Durable event pages one process observation snapshot may read.
    pub process_effect_fold_pages: usize,
    /// Events per durable page that read asks for.
    pub process_effect_fold_page_size: std::num::NonZeroUsize,
    /// Committed facts one process feed republishes to bridge its consumer
    /// to the durable process. A consumer further behind rebuilds from the
    /// durable read view instead, and the feed publishes nothing. Keep it a
    /// small fraction of the replay window: what a feed republishes pushes
    /// other observers' provisional evidence out of that window.
    pub process_reconcile_bridge_events: usize,
}
impl ObservationWorkLimits {
    /// Standard preset: expire 64 sessions per store call, publish 32 records
    /// per task poll, read 32 process tail events, remember 4096 session
    /// event IDs and 4096 process event IDs, fold a process's effects from
    /// at most 64 pages of 256 events and bridge a process feed across at
    /// most 256 committed facts. These exact values have no supporting
    /// workload measurements.
    pub const fn standard() -> Self {
        Self {
            language_session_events: 256,
            language_bytes: 4 * 1024 * 1024,
            language_process_rounds: 2,
            replay_expiry_batch: std::num::NonZeroUsize::MIN.saturating_add(63),
            publisher_batch: std::num::NonZeroUsize::MIN.saturating_add(31),
            process_snapshot_event_tail: 32,
            session_dedup_ids: 4096,
            process_dedup_ids: 4096,
            process_effect_fold_pages: 64,
            process_effect_fold_page_size: std::num::NonZeroUsize::MIN.saturating_add(255),
            process_reconcile_bridge_events: 256,
        }
    }
}
impl Default for ObservationWorkLimits {
    fn default() -> Self {
        Self::standard()
    }
}
