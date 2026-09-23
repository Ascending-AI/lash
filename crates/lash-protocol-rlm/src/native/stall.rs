use lash_core::{DriverAction, DriverContextView, Part, PartKind};

use crate::protocol::stall::reply_fingerprint;

pub(super) const LLM_EXTRACTION_PHASE: &str = "native_extraction";
pub(super) const NO_PROGRESS_BUDGET_PHASE: &str = "no_progress_budget";
pub(super) fn stalled_attempts(ctx: &DriverContextView<'_>, actions: &[DriverAction]) -> usize {
    crate::protocol::stall::stalled_attempts_in_phase(ctx, actions, LLM_EXTRACTION_PHASE)
}

/// Field separator between the parts of a native reply, so that a reply whose
/// prose ends where the next part's content begins cannot digest as one whose
/// parts split elsewhere. Unit separator: it cannot occur unescaped inside the
/// JSON arguments of a tool call, and prose carrying one digests as prose.
const NATIVE_PART_SEPARATOR: char = '\u{1f}';

/// The digest of one native reply, over the reply and nothing else.
///
/// The cell channel hashes its assistant text, which is prose and the cell
/// together. A native reply says the same two things in separate parts — prose
/// and the arguments of one `execute_code` call — so both are hashed, and
/// nothing a provider varies between two identical answers is: reasoning
/// summaries, the `reasoning_meta`/`response_meta` replay blobs, and the
/// per-request tool call id all live outside `Part::content`.
///
/// Serializing the whole `Vec<Part>` instead put every one of those inside the
/// digest, so a reasoning model's twelve identical replies fingerprinted twelve
/// different ways and the evidence said the opposite of what happened.
pub(super) fn native_reply_fingerprint(parts: &[Part]) -> String {
    let mut reply = String::new();
    for part in parts {
        if matches!(part.kind(), PartKind::Reasoning) {
            continue;
        }
        reply.push_str(&part.content());
        reply.push(NATIVE_PART_SEPARATOR);
    }
    reply_fingerprint(&reply)
}
