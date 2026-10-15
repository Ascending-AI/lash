//! The example hosts' own trace file: one JSON record per line. Lash ships no
//! trace file writer; a host that wants records on disk writes its own
//! [`TraceSink`], as this one does.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};

use lash::tracing::{TraceRecord, TraceSink, TraceSinkError};

pub(crate) struct JsonlFileSink {
    path: PathBuf,
    lock: Mutex<()>,
}

impl JsonlFileSink {
    pub(crate) fn new(path: impl Into<PathBuf>) -> Self {
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
        let _guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        self.path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| {
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&self.path)
            })
            .and_then(|mut file| file.write_all(line.as_bytes()))
            .map_err(|source| TraceSinkError::Write {
                path: self.path.clone(),
                source,
            })
    }
}

/// The language-execution observations of a host's product graph, written
/// in the same file shape. A failed write loses the observation: the file
/// is a diagnostic export, not the product graph.
impl lash::tracing::ProductObservationSink for JsonlFileSink {
    fn observe(&self, record: &TraceRecord) {
        let _ = self.append(record);
    }
}
