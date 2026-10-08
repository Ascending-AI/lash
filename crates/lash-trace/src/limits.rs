//! Optional working-memory and diagnostic cuts, independent of wire ceilings.

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
    /// Per-node effect occurrences recorded individually. Values above the
    /// fixed wire ceiling (eight) resolve to eight; zero summarizes every occurrence.
    pub process_effect_occurrences: u64,
    /// Characters retained from a trace diagnostic before its omission annotation.
    pub diagnostic_error_chars: usize,
    /// Threshold for a plugin session's state-size warning; the fixed rejection
    /// ceiling remains independent. `usize::MAX` disables this warning.
    pub plugin_state_warn_bytes: usize,
}

impl TraceLimits {
    /// Standard preset: 256 held observations, 256 KiB attempt streams,
    /// 2048-byte diff values, eight summary paths, 64 KiB failure output,
    /// 2048-byte provider bodies, eight fingerprint generations, eight process
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
            process_effect_occurrences: 8,
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
    pub replay_expiry_batch: std::num::NonZeroUsize,
    pub publisher_batch: std::num::NonZeroUsize,
    pub process_snapshot_event_tail: usize,
    pub session_dedup_ids: usize,
}
impl ObservationWorkLimits {
    /// Standard preset: expire 64 sessions per store call, publish 32 records
    /// per task poll, read 32 process tail events and remember 4096 session
    /// event IDs. These exact values have no supporting workload measurements.
    pub const fn standard() -> Self {
        Self {
            replay_expiry_batch: std::num::NonZeroUsize::MIN.saturating_add(63),
            publisher_batch: std::num::NonZeroUsize::MIN.saturating_add(31),
            process_snapshot_event_tail: 32,
            session_dedup_ids: 4096,
        }
    }
}
impl Default for ObservationWorkLimits {
    fn default() -> Self {
        Self::standard()
    }
}
