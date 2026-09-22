//! Reading and repairing JSONL trace documents: one JSON record per line, as
//! [`crate::JsonlTraceSink`] writes them.

use std::io::{self, Read, Seek, SeekFrom};

/// Drop an unterminated final line — the torn record a writer killed mid-append
/// (or short-written on ENOSPC) leaves behind — by truncating just after the
/// file's last newline. A file already ending in `\n` is untouched; a file with
/// no newline at all is truncated to empty. The scan's seeks are safe under
/// append mode, where writes always land at the end.
pub(crate) fn truncate_torn_tail(file: &mut std::fs::File) -> io::Result<()> {
    let len = file.metadata()?.len();
    let mut end = len;
    let mut block = [0u8; 8192];
    while end > 0 {
        let start = end.saturating_sub(block.len() as u64);
        file.seek(SeekFrom::Start(start))?;
        let count = (end - start) as usize;
        file.read_exact(&mut block[..count])?;
        match block[..count].iter().rposition(|byte| *byte == b'\n') {
            // The file's last byte is a newline: nothing is torn.
            Some(index) if start + index as u64 + 1 == len => return Ok(()),
            // Everything after the last newline is the torn tail: drop it.
            Some(index) => return file.set_len(start + index as u64 + 1),
            None => end = start,
        }
    }
    file.set_len(0)
}

/// A JSONL trace line that is neither a valid record nor a torn final tail.
#[derive(Debug, thiserror::Error)]
#[error("trace line {line}: {source}")]
#[non_exhaustive]
pub struct JsonlTraceReadError {
    /// The 1-based line number of the malformed record.
    pub line: usize,
    /// Why the line is not a valid record.
    #[source]
    pub source: serde_json::Error,
}

/// Parse the records of a JSONL trace document — one JSON record per line, as
/// [`crate::JsonlTraceSink`] writes it.
///
/// A final line without a terminating newline is a torn record left by a
/// writer killed mid-append: if it parses it counts like any other record, and
/// if it does not it is skipped. A malformed line anywhere else fails the
/// read — mid-file corruption is an anomaly, not a torn write.
pub fn parse_jsonl_records<T>(text: &str) -> Result<Vec<T>, JsonlTraceReadError>
where
    T: serde::de::DeserializeOwned,
{
    let terminated = text.is_empty() || text.ends_with('\n');
    let mut records = Vec::new();
    let mut lines = text.lines().enumerate().peekable();
    while let Some((index, line)) = lines.next() {
        let torn_tail = lines.peek().is_none() && !terminated;
        match serde_json::from_str(line) {
            Ok(record) => records.push(record),
            Err(_) if torn_tail => {}
            Err(source) => {
                return Err(JsonlTraceReadError {
                    line: index + 1,
                    source,
                });
            }
        }
    }
    Ok(records)
}
