//! The workbench's console trace sink: each record as one JSON line on
//! stderr. Lash ships no console sink; the workbench writes its own.

use std::io::Write;

use lash::tracing::{TraceRecord, TraceSink, TraceSinkError};

pub(crate) struct StderrTraceSink;

impl TraceSink for StderrTraceSink {
    /// The record and its newline leave in one `write_all` under the stderr
    /// lock, so a log line from another task cannot sever the record's line.
    fn append(&self, record: &TraceRecord) -> Result<(), TraceSinkError> {
        let mut line = serde_json::to_string(record)?;
        line.push('\n');
        std::io::stderr()
            .lock()
            .write_all(line.as_bytes())
            .map_err(|source| TraceSinkError::Write {
                path: "<stderr>".into(),
                source,
            })
    }
}
