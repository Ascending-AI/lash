//! The trace scope an occurrence retains from the fire that first ingested
//! it (FIG-4829).

use super::{TriggerOccurrenceRecord, TriggerOccurrenceRequest};

impl TriggerOccurrenceRequest {
    /// Sets what the fire offers the occurrence's trace scope
    /// ([`Self::trace`]).
    pub fn with_trace(mut self, trace: lash_trace::TraceScopeOffer) -> Self {
        self.trace = trace;
        self
    }

    /// The record the ingest that inserts this occurrence writes, under the
    /// id it derived and at the time it accepted the fire.
    pub fn into_record(
        self,
        occurrence_id: String,
        occurred_at_ms: u64,
    ) -> TriggerOccurrenceRecord {
        let trace = self.trace.into_scope(
            lash_trace::TraceScopeId::admission(lash_trace::TraceScopeOwner::TriggerOccurrence {
                occurrence_id: occurrence_id.clone(),
            }),
            occurred_at_ms,
        );
        TriggerOccurrenceRecord {
            occurrence_id,
            source_type: self.source_type,
            source_key: self.source_key,
            payload: self.payload,
            idempotency_key: self.idempotency_key,
            source: self.source,
            session_id: self.session_id,
            outcome: self.outcome,
            occurred_at_ms,
            trace: Some(trace),
        }
    }
}
