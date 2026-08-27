//! What a stalling turn knows about itself.
//!
//! A turn stalls when an attempt commits no error-free execution: the reply
//! carried no cell, or the cell it carried failed. One bound ends such a turn,
//! and it is the host's: the no-progress budget, counted here from what the turn
//! committed. This module also mints the reply fingerprint each attempt records,
//! which carries no runtime behavior — it is evidence a host can read.

use lash_core::session_model::SessionHistoryRecord;
use lash_core::{DriverAction, DriverContextView};
use lash_rlm_types::RlmProtocolEvent;
use serde_json::Value;

/// Diagnostic phase emitted exactly once per provider attempt.
pub(super) const LLM_EXTRACTION_PHASE: &str = "llm_extraction";
/// Diagnostic phase that records a turn stopped by its no-progress budget.
pub(super) const NO_PROGRESS_BUDGET_PHASE: &str = "no_progress_budget";
/// Consecutive provider attempts *in this turn*, ending at its latest record,
/// that committed no error-free execution.
///
/// Derived from committed records rather than carried in driver state: driver
/// state is rebuilt per protocol iteration, and a counter that survives park,
/// replay, and continuation has to be readable from what the turn committed.
/// Every attempt appends exactly one `llm_extraction` diagnostic, so counting
/// those since the last error-free trajectory entry counts attempts.
///
/// The scan runs backwards and stops at the first record that is not this
/// turn's, because the driver's history is the whole active session path. A
/// forward scan over that path leaks earlier turns into the count: only an
/// error-free execution resets, and a prose-only chat turn, a finish request, a
/// turn-limit stop, and a no-progress stop all end their turn leaving a
/// trailing diagnostic with no execution behind it. Counting those would spend
/// a fresh turn's whole budget on its first attempt.
///
/// Pending actions are counted too. Whether this attempt's own diagnostic is
/// already committed depends on which handler is deciding — the extraction
/// paths decide in the same action batch that appends it, the execution path
/// decides an effect later — and a count that reads only committed history
/// would be one short on exactly one of them.
///
pub(super) fn stalled_attempts(ctx: &DriverContextView<'_>, actions: &[DriverAction]) -> usize {
    let turn_id = ctx.turn_id();
    let trajectory_prefix = trajectory_entry_turn_prefix(turn_id);
    let mut attempts = 0;
    for record in ctx.events().iter().rev() {
        let SessionHistoryRecord::Protocol(event) = record else {
            continue;
        };
        match crate::projection::decode_rlm_protocol_event(event) {
            Some(RlmProtocolEvent::RlmDiagnostic(diagnostic))
                if diagnostic.phase == LLM_EXTRACTION_PHASE =>
            {
                if diagnostic.payload.get("turn_id").and_then(Value::as_str) != Some(turn_id) {
                    break;
                }
                attempts += 1;
            }
            // A closed turn's own stop record. Nothing before it is this turn's.
            Some(RlmProtocolEvent::RlmDiagnostic(diagnostic))
                if diagnostic.phase == NO_PROGRESS_BUDGET_PHASE =>
            {
                break;
            }
            // An earlier turn's execution, or this turn's last progress point.
            Some(RlmProtocolEvent::RlmTrajectoryEntry(entry))
                if !entry.id.starts_with(&trajectory_prefix) || entry.error.is_none() =>
            {
                break;
            }
            _ => {}
        }
    }
    count_pending_attempts(actions, &trajectory_prefix, &mut attempts);
    attempts
}

/// Apply the not-yet-committed action batch to `attempts`, forwards, since the
/// batch is this turn's and is ordered.
fn count_pending_attempts(actions: &[DriverAction], trajectory_prefix: &str, attempts: &mut usize) {
    for action in actions {
        let DriverAction::AppendEvents(records) = action else {
            continue;
        };
        for record in records {
            let SessionHistoryRecord::Protocol(event) = record else {
                continue;
            };
            match crate::projection::decode_rlm_protocol_event(event) {
                Some(RlmProtocolEvent::RlmTrajectoryEntry(entry))
                    if entry.error.is_none() && entry.id.starts_with(trajectory_prefix) =>
                {
                    *attempts = 0;
                }
                Some(RlmProtocolEvent::RlmDiagnostic(diagnostic))
                    if diagnostic.phase == LLM_EXTRACTION_PHASE =>
                {
                    *attempts += 1;
                }
                _ => {}
            }
        }
    }
}

fn trajectory_entry_turn_prefix(turn_id: &str) -> String {
    format!("lashlang_step_{turn_id}_")
}

/// A stable short digest of one model reply's assistant text.
///
/// Recorded on every extraction diagnostic as *evidence*, with no runtime
/// behavior attached: two attempts carrying one fingerprint are a host's signal
/// that its retry guidance changed nothing, and what to do about that is the
/// host's call, made through the no-progress budget it already configures. Lash
/// draws no conclusion from a repeat — a provider sending the same bytes twice is
/// legitimate output.
///
/// Assistant text only, so the digest names the reply a host would compare and
/// not the reasoning summary that varies between two identical answers. Hashed
/// rather than stored, because the diagnostic is durable session history and a
/// reply is not small. Derived from the reply alone, so it is replay-stable.
pub(super) fn reply_fingerprint(assistant_text: &str) -> String {
    let digest = lash_sansio::core_support::blake3_domain_hash(
        "lash-rlm-stall-reply/v2",
        assistant_text.as_bytes(),
    );
    digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
