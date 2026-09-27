use crate::{ProcessRecord, ProcessRegistration};

/// Rebuild a runnable registration from a persisted row. Reconstruction resumes the record's minted id
/// and never registers anything (ADR 0107).
pub(super) fn registration_from_record(record: ProcessRecord) -> ProcessRegistration {
    ProcessRegistration {
        start_key: record.start_key,
        input: record.input,
        lifetime: record.lifetime,
        ancestry: record.ancestry,
        session_capability: record.session_capability,
        identity: record.identity,
        event_types: record.event_types,
        provenance: record.provenance,
        env_ref: record.env_ref,
        wake_session_id: None,
    }
}
