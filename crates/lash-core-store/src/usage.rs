//! Per-response usage for context-window decisions.

use crate::session_model::LlmUsage;

/// The last call's usage, `Some` only when the call reported any nonzero
/// counter. A fully zeroed report carries no prompt-side information and is
/// stored as `None`, the same as no completed call.
pub fn nonzero_usage(usage: LlmUsage) -> Option<LlmUsage> {
    (!usage.is_zero()).then_some(usage)
}
