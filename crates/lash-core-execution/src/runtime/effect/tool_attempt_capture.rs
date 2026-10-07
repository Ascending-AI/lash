//! Facts journaled with one atomic tool attempt.

use crate::PluginMessage;
use serde::{Deserialize, Serialize};

/// The semantic facts one atomic `ToolAttempt` produced, journaled with it.
///
/// Carried by
/// [`RuntimeEffectOutcome::ToolAttempt`](super::envelope::RuntimeEffectOutcome::ToolAttempt)
/// so the attempt's committed result is self-describing: a replay that serves
/// this outcome restores these facts into the dispatch buffers instead of
/// re-running their producers. Empty captures are skipped on the wire, so an
/// attempt that committed no message and is not known to have spent anything
/// writes the same bytes it wrote before this field existed.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolAttemptCapture {
    /// The messages tool result checks contributed during this attempt, in
    /// the order they were enqueued.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<PluginMessage>,
}

impl ToolAttemptCapture {
    /// Whether this attempt captured nothing at all.
    ///
    /// Read by the outcome's `skip_serializing_if`, so an attempt that produced
    /// no message adds no bytes to the journal and leaves the ungrouped
    /// outcome corpus byte-identical. What the attempt spent is not a
    /// capture: hosts meter provider attempts at the Provider seam (ADR 0127).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }
}
