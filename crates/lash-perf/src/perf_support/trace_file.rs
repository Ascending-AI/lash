//! The harness's own trace file: one JSON record per line. Lash ships no
//! trace file writer; a host that wants records on disk writes its own
//! [`TraceSink`], as this one does.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use lash::tracing::{TraceRecord, TraceSink, TraceSinkError};
use lash_sansio::sync::MutexExt;

pub struct JsonlFileSink {
    path: PathBuf,
    lock: Mutex<()>,
}

impl JsonlFileSink {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            lock: Mutex::new(()),
        }
    }
}

impl TraceSink for JsonlFileSink {
    /// Append the record and its newline in one write, so a reader never sees
    /// two records glued onto one line.
    fn append(&self, record: &TraceRecord) -> Result<(), TraceSinkError> {
        let mut line = serde_json::to_string(record)?;
        line.push('\n');
        let _guard = self.lock.lock_recover();
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .and_then(|mut file| file.write_all(line.as_bytes()))
            .map_err(|source| TraceSinkError::Write {
                path: self.path.clone(),
                source,
            })
    }
}
