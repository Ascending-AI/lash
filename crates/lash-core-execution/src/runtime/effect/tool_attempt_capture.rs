//! Facts journaled with one atomic tool attempt.

use super::executor::RuntimeEffectControllerError;
use crate::PluginMessage;
use serde::{Deserialize, Serialize};

/// The durable format version of one atomic attempt's captured facts.
///
/// Retained at version 6 under the pre-1.0 stored-format freeze.
///
/// version_guard(
///     roots(ToolAttemptCapture),
///     roots(path = "crates/lash-sansio/src/llm/types.rs", LlmCallId),
///     roots(path = "crates/lash-sansio/src/session_model/message.rs", FlatPart, FlatPartRef),
///     roots(path = "crates/lash-sansio/src/session_model/mod.rs", TokenUsage),
///     file(
///         path = "crates/lash-sansio/src/identity.rs",
///         cover("string_identity!", SessionId, ProcessId, TurnId, InputId),
///     ),
/// )
/// version_surface = "drain"
/// format_manifest = "ToolAttemptCapture"
pub const TOOL_ATTEMPT_CAPTURE_VERSION: u16 = 6;

/// The semantic facts one atomic `ToolAttempt` produced, journaled with it.
///
/// Carried by
/// [`RuntimeEffectOutcome::ToolAttempt`](super::envelope::RuntimeEffectOutcome::ToolAttempt)
/// so the attempt's committed result is self-describing: a replay that serves
/// this outcome restores these facts into the dispatch buffers instead of
/// re-running their producers. Empty captures are skipped on the wire, so an
/// attempt that committed no message and is not known to have spent anything
/// writes the same bytes it wrote before this field existed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolAttemptCapture {
    /// The durable format version, refused rather than defaulted.
    pub version: u16,
    /// The messages tool result checks contributed during this attempt, in
    /// the order they were enqueued.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<PluginMessage>,
}

impl Default for ToolAttemptCapture {
    fn default() -> Self {
        Self {
            version: TOOL_ATTEMPT_CAPTURE_VERSION,
            messages: Vec::new(),
        }
    }
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

    /// Refuses a capture this build cannot read completely.
    ///
    /// Called by outcome validation, so a capture from a format this build does
    /// not reconstruct is refused where it is decoded rather than replayed as a
    /// partial restore of what the attempt produced.
    pub fn validate(&self) -> Result<(), RuntimeEffectControllerError> {
        if self.version != TOOL_ATTEMPT_CAPTURE_VERSION {
            return Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectToolAttemptCaptureVersion,
                format!(
                    "tool-attempt capture records format version {}, and this build reads version \
                     {TOOL_ATTEMPT_CAPTURE_VERSION}; a capture that cannot be read completely is \
                     refused rather than restored as a prefix of what the attempt produced",
                    self.version
                ),
            ));
        }
        Ok(())
    }
}
