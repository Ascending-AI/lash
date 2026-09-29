//! Output kept out of history (FIG-1643).
//!
//! A producer about to put an oversized output into session history — a tool
//! presentation, an RLM print or final value — retains its complete bytes as a
//! session attachment first and puts a [`RetainedOutput`] in its place: a
//! bounded witness and the attachment's typed reference. The decision is made
//! once, inside a journaled step, under an [`OutputRetentionPolicy`] the step
//! records beside its result, so a replay serves the recorded witness and
//! reference verbatim whatever the live policy has become.

use serde::{Deserialize, Serialize};

use crate::AttachmentRef;

/// The byte policy an output is measured against before it enters history.
///
/// An output whose encoding is longer than `inline_limit_bytes` is retained
/// as an attachment, and history keeps at most `witness_bytes` of it in its
/// place. The policy is process configuration; every step that applies it
/// journals the policy it applied, so the recorded decision, not the live
/// configuration, is what a replay sees.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OutputRetentionPolicy {
    /// The longest output, in bytes, history keeps inline.
    pub inline_limit_bytes: u64,
    /// The longest witness, in bytes, history keeps in place of a retained
    /// output.
    pub witness_bytes: u64,
}

impl OutputRetentionPolicy {
    /// 64 KiB inline, a 4 KiB witness. The standard renderer's default cut
    /// (16,000 characters) stays inline under it, so the policy retains what
    /// no renderer bounded: failures a renderer passed through, plugin
    /// additions after the cut, and RLM values.
    pub const DEFAULT: Self = Self {
        inline_limit_bytes: 64 * 1024,
        witness_bytes: 4 * 1024,
    };

    /// Whether an output of `byte_len` bytes is too long to enter history
    /// inline.
    pub fn retains(&self, byte_len: usize) -> bool {
        u64::try_from(byte_len).unwrap_or(u64::MAX) > self.inline_limit_bytes
    }

    /// The witness history keeps for `text`: its longest prefix that fits
    /// the witness bound together with `notice`, followed by `notice`. The
    /// whole witness is within the bound, so a notice longer than the bound
    /// is itself cut.
    pub fn witness(&self, text: &str, notice: &str) -> String {
        let bound = usize::try_from(self.witness_bytes).unwrap_or(usize::MAX);
        let notice = prefix_within(notice, bound);
        let mut witness = prefix_within(text, bound - notice.len()).to_string();
        witness.push_str(notice);
        witness
    }
}

/// The longest prefix of `text` at most `bound` bytes long, cut on a
/// character boundary.
fn prefix_within(text: &str, bound: usize) -> &str {
    if text.len() <= bound {
        return text;
    }
    let mut end = bound;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

impl Default for OutputRetentionPolicy {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// An output history does not hold: the attachment that holds its complete
/// bytes, and the bounded witness history keeps in its place.
///
/// The witness is what a reader of history — the model, a transcript — sees
/// of the output. It is never expanded automatically: the complete bytes are
/// read only by resolving `reference` through the session's attachment store.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RetainedOutput {
    /// The session attachment holding the complete output.
    pub reference: AttachmentRef,
    /// The bounded text history keeps in the output's place.
    pub witness: String,
}

/// A value on its way into history: kept inline, or retained with only its
/// witness and reference in history.
///
/// It has no serde spelling of its own: each record that carries one spells
/// an inline value as the bare value its recorded shape always held, and a
/// retained one under a key of its own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutputValue {
    /// The value itself, within the policy's inline limit.
    Inline(serde_json::Value),
    /// The value's encoding was over the inline limit and is retained.
    Retained(RetainedOutput),
}

impl OutputValue {
    /// The inline value, when the value was not retained.
    pub fn inline(&self) -> Option<&serde_json::Value> {
        match self {
            Self::Inline(value) => Some(value),
            Self::Retained(_) => None,
        }
    }

    /// The retention, when the value was retained.
    pub fn retained(&self) -> Option<&RetainedOutput> {
        match self {
            Self::Inline(_) => None,
            Self::Retained(retained) => Some(retained),
        }
    }
}

impl From<serde_json::Value> for OutputValue {
    fn from(value: serde_json::Value) -> Self {
        Self::Inline(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_policy_retains_only_past_its_inline_limit() {
        let policy = OutputRetentionPolicy {
            inline_limit_bytes: 4,
            witness_bytes: 2,
        };
        assert!(!policy.retains(4));
        assert!(policy.retains(5));
    }

    #[test]
    fn the_witness_is_cut_on_a_character_boundary_within_its_bound() {
        let policy = OutputRetentionPolicy {
            inline_limit_bytes: 1,
            witness_bytes: 6,
        };
        assert_eq!(policy.witness("abc", "[n]"), "abc[n]");
        // `é` is two bytes: the three bytes left beside the notice end
        // inside the second one.
        assert_eq!(policy.witness("aéé", "[n]"), "aé[n]");
        assert!(policy.witness("éééé", "").len() <= 6);
        // A notice past the bound is cut to it, and no text fits beside it.
        assert_eq!(policy.witness("abc", "[notice]"), "[notic");
    }
}
