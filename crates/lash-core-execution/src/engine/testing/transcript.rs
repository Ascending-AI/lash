//! What a drive run issued, in issue order, and the comparator that finds the
//! first place two runs disagree.
//!
//! A drive is deterministic when every run of it over the same recorded
//! history issues the same commands, in the same order, with the same bytes,
//! and commits the same bytes. [`DriveTranscript`] is that record and
//! [`DriveTranscript::compare`] is the check.

use std::fmt;

use serde::{Deserialize, Serialize};

/// One thing a drive run did that another run must repeat exactly.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "entry", rename_all = "snake_case")]
pub enum TranscriptEntry {
    /// A recorded operation the drive issued.
    Command {
        /// The operation's replay identity.
        key: String,
        /// The operation's kind, as the engine names it.
        kind: String,
        /// The command's canonical bytes.
        bytes: String,
    },
    /// Bytes the drive committed.
    Commit {
        /// The committed request's canonical bytes.
        bytes: String,
    },
}

impl TranscriptEntry {
    fn label(&self) -> String {
        match self {
            Self::Command { key, kind, .. } => format!("command `{kind}` at `{key}`"),
            Self::Commit { .. } => "commit".to_string(),
        }
    }

    fn bytes(&self) -> &str {
        match self {
            Self::Command { bytes, .. } | Self::Commit { bytes } => bytes,
        }
    }
}

/// The command stream plus commit bytes of one drive run.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriveTranscript {
    /// Every entry, in the order the drive issued it.
    pub entries: Vec<TranscriptEntry>,
}

impl DriveTranscript {
    /// The commands alone, in issue order.
    pub fn commands(&self) -> impl Iterator<Item = &TranscriptEntry> {
        self.entries
            .iter()
            .filter(|entry| matches!(entry, TranscriptEntry::Command { .. }))
    }

    /// The commits alone, in issue order.
    pub fn commits(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().filter_map(|entry| match entry {
            TranscriptEntry::Commit { bytes } => Some(bytes.as_str()),
            TranscriptEntry::Command { .. } => None,
        })
    }

    /// Compare `actual` against this transcript, the reference, and report the
    /// first entry at which they differ.
    pub fn compare(&self, actual: &DriveTranscript) -> Result<(), Box<TranscriptDivergence>> {
        let len = self.entries.len().max(actual.entries.len());
        for index in 0..len {
            let expected = self.entries.get(index);
            let found = actual.entries.get(index);
            if expected != found {
                return Err(Box::new(TranscriptDivergence {
                    index,
                    expected: expected.cloned(),
                    actual: found.cloned(),
                }));
            }
        }
        Ok(())
    }
}

/// The first entry at which a run's transcript left the reference's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranscriptDivergence {
    /// The entry's position in issue order.
    pub index: usize,
    /// The reference run's entry there, or `None` when the reference ended.
    pub expected: Option<TranscriptEntry>,
    /// This run's entry there, or `None` when this run ended.
    pub actual: Option<TranscriptEntry>,
}

impl fmt::Display for TranscriptDivergence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let index = self.index;
        match (&self.expected, &self.actual) {
            (Some(expected), None) => write!(
                formatter,
                "entry {index}: the reference issued {} but this run ended",
                expected.label()
            ),
            (None, Some(actual)) => write!(
                formatter,
                "entry {index}: this run issued {} past the reference's end",
                actual.label()
            ),
            (Some(expected), Some(actual)) if expected.label() != actual.label() => write!(
                formatter,
                "entry {index}: the reference issued {} but this run issued {}",
                expected.label(),
                actual.label()
            ),
            (Some(expected), Some(actual)) => write!(
                formatter,
                "entry {index}: {} differs from the reference {}",
                actual.label(),
                byte_difference(expected.bytes(), actual.bytes())
            ),
            (None, None) => write!(formatter, "entry {index}: no difference"),
        }
    }
}

impl std::error::Error for TranscriptDivergence {}

/// Where two byte strings first differ, with a little context on each side.
pub(super) fn byte_difference(expected: &str, actual: &str) -> String {
    const CONTEXT: usize = 48;
    let offset = expected
        .char_indices()
        .zip(actual.chars())
        .find(|((_, left), right)| left != right)
        .map(|((offset, _), _)| offset)
        .unwrap_or_else(|| expected.len().min(actual.len()));
    let window = |bytes: &str| {
        let start = floor_char_boundary(bytes, offset.saturating_sub(CONTEXT));
        let end = floor_char_boundary(bytes, offset.saturating_add(CONTEXT).min(bytes.len()));
        bytes.get(start..end).unwrap_or_default().to_string()
    };
    format!(
        "at byte {offset}: expected `…{}…`, found `…{}…`",
        window(expected),
        window(actual)
    )
}

fn floor_char_boundary(bytes: &str, mut index: usize) -> usize {
    index = index.min(bytes.len());
    while index > 0 && !bytes.is_char_boundary(index) {
        index -= 1;
    }
    index
}
