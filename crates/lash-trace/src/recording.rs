//! An in-memory [`TraceSink`] for tests.

use std::sync::Mutex;

use lash_sansio::sync::MutexExt;

use crate::{TraceRecord, TraceSink, TraceSinkError};

/// A [`TraceSink`] that keeps every appended record in memory, in append
/// order, so a test reads the typed records the runtime emitted.
#[derive(Debug, Default)]
pub struct RecordingTraceSink {
    records: Mutex<Vec<TraceRecord>>,
}

impl RecordingTraceSink {
    pub fn new() -> Self {
        Self::default()
    }

    /// The records appended so far, in append order.
    pub fn records(&self) -> Vec<TraceRecord> {
        self.records.lock_recover().clone()
    }

    /// The records appended so far, each as its serialized JSON shape — the
    /// view for laws whose oracle is a record's wire fields.
    pub fn json_records(&self) -> serde_json::Result<Vec<serde_json::Value>> {
        self.records
            .lock_recover()
            .iter()
            .map(serde_json::to_value)
            .collect()
    }
}

impl TraceSink for RecordingTraceSink {
    fn append(&self, record: &TraceRecord) -> Result<(), TraceSinkError> {
        self.records.lock_recover().push(record.clone());
        Ok(())
    }
}
