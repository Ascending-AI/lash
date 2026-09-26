use crate::{ProcessRecord, ProcessRegistration};

/// Rebuild a runnable registration from a persisted row, preserving its
/// declared recovery contract. Reconstruction resumes the record's minted id
/// and never registers anything (ADR 0107).
pub(super) fn registration_from_record(record: ProcessRecord) -> ProcessRegistration {
    ProcessRegistration {
        start_key: record.start_key,
        input: record.input,
        disposition: record.disposition,
        lifecycle: record.lifecycle,
        max_attempts: record.max_attempts,
        identity: record.identity,
        event_types: record.event_types,
        provenance: record.provenance,
        env_ref: record.env_ref,
        wake_session_id: None,
    }
}
