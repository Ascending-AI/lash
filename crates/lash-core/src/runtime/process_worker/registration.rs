use crate::{ProcessRecord, ProcessRegistration};

/// Rebuild a runnable registration from a persisted row, preserving its
/// declared recovery contract.
pub(super) fn registration_from_record(record: ProcessRecord) -> ProcessRegistration {
    ProcessRegistration {
        id: record.id,
        input: record.input,
        disposition: record.disposition,
        max_attempts: record.max_attempts,
        identity: record.identity,
        event_types: record.event_types,
        provenance: record.provenance,
        env_ref: record.env_ref,
        wake_target: record.wake_target,
    }
}
