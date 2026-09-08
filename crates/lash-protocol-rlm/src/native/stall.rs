pub(super) use crate::protocol::stall::reply_fingerprint;
use lash_core::{DriverAction, DriverContextView};
pub(super) const LLM_EXTRACTION_PHASE: &str = "native_extraction";
pub(super) const NO_PROGRESS_BUDGET_PHASE: &str = "no_progress_budget";
pub(super) fn stalled_attempts(ctx: &DriverContextView<'_>, actions: &[DriverAction]) -> usize {
    crate::protocol::stall::stalled_attempts_in_phase(ctx, actions, LLM_EXTRACTION_PHASE)
}
