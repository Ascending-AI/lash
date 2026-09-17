//! What a stalling turn knows about itself.
//!
//! A turn stalls when an attempt commits no error-free execution: the reply
//! carried no cell, or the cell it carried failed. One bound ends such a turn,
//! and it is the host's: the no-progress budget, counted here from what the turn
//! committed. This module also mints the reply fingerprint each attempt records,
//! which carries no runtime behavior — it is evidence a host can read.

use lash_core::llm::types::ProviderReasoningReplay;
use lash_core::session_model::SessionHistoryRecord;
use lash_core::{DriverAction, DriverContextView};
use lash_rlm_types::{RlmProtocolEvent, RlmTermination};
use lash_sansio::TurnId;
use serde::ser::{Serialize, SerializeMap, Serializer};
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
    stalled_attempts_in_phase(ctx, actions, LLM_EXTRACTION_PHASE)
}

pub(crate) fn stalled_attempts_in_phase(
    ctx: &DriverContextView<'_>,
    actions: &[DriverAction],
    extraction_phase: &str,
) -> usize {
    let turn_id = ctx.turn_id();
    let trajectory_prefix = trajectory_entry_turn_prefix(turn_id);
    let mut attempts = 0;
    for record in ctx.events().iter().rev() {
        let SessionHistoryRecord::Protocol(event) = record else {
            continue;
        };
        match crate::projection::decode_rlm_protocol_event(event) {
            Some(RlmProtocolEvent::RlmDiagnostic(diagnostic))
                if diagnostic.phase == extraction_phase =>
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
                if !entry.id.starts_with(&trajectory_prefix) || entry.outcome.error().is_none() =>
            {
                break;
            }
            _ => {}
        }
    }
    count_pending_attempts(actions, &trajectory_prefix, &mut attempts, extraction_phase);
    attempts
}

/// Apply the not-yet-committed action batch to `attempts`, forwards, since the
/// batch is this turn's and is ordered.
fn count_pending_attempts(
    actions: &[DriverAction],
    trajectory_prefix: &str,
    attempts: &mut usize,
    extraction_phase: &str,
) {
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
                    if entry.outcome.error().is_none()
                        && entry.id.starts_with(trajectory_prefix) =>
                {
                    *attempts = 0;
                }
                Some(RlmProtocolEvent::RlmDiagnostic(diagnostic))
                    if diagnostic.phase == extraction_phase =>
                {
                    *attempts += 1;
                }
                _ => {}
            }
        }
    }
}

fn trajectory_entry_turn_prefix(turn_id: &TurnId) -> String {
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
pub(crate) fn reply_fingerprint(assistant_text: &str) -> String {
    let digest = lash_sansio::core_support::blake3_domain_hash(
        "lash-rlm-stall-reply/v2",
        assistant_text.as_bytes(),
    );
    digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The per-attempt extraction diagnostic, in the one shape both channels emit.
///
/// Every provider attempt on either channel appends exactly one of these, and
/// the same reader counts them (`stalled_attempts_in_phase`) and compares their
/// fingerprints. A field that exists on one channel only is therefore a field a
/// host cannot rely on, so the payload is a type rather than a `json!` literal
/// per call site: adding a field is one edit that both channels satisfy or
/// neither compiles.
///
/// `turn_id` is what scopes the no-progress count. The driver's view of history
/// is the whole active session path, not one turn, so a diagnostic that cannot
/// name its own turn cannot be told apart from a previous turn's tail — and
/// several terminal shapes (a prose-only chat turn, a finish request, a
/// turn-limit stop) leave a trailing diagnostic with no execution after it.
/// Diagnostics written before this field existed name no turn and are read as
/// belonging to an earlier one, which under-counts rather than mis-stops.
///
/// `reply_fingerprint` is evidence for hosts only; lash attaches no runtime
/// behavior to a repeat. It is derived state, not new information, and it lives
/// in the diagnostic for the same reason the count does: the driver's only
/// durable view of the turn is what the turn committed.
///
/// Which channel produced it is carried by the phase name alone
/// (`LLM_EXTRACTION_PHASE` here, `native::stall::LLM_EXTRACTION_PHASE` there);
/// the payload names no dialect, which ADR 0096 retired.
#[derive(serde::Serialize)]
pub(crate) struct ExtractionDiagnostic<'a> {
    turn_id: &'a TurnId,
    decision: &'a str,
    reply_fingerprint: &'a str,
    termination: &'static str,
    counts: ExtractionCounts<'a>,
}

impl<'a> ExtractionDiagnostic<'a> {
    pub(crate) fn new(
        turn_id: &'a TurnId,
        reply_fingerprint: &'a str,
        decision: &'a str,
        termination: &RlmTermination,
        counts: ExtractionCounts<'a>,
    ) -> Self {
        Self {
            turn_id,
            decision,
            reply_fingerprint,
            termination: termination_diagnostic_name(termination),
            counts,
        }
    }

    #[expect(
        clippy::expect_used,
        reason = "every field of this struct and of ExtractionCounts is a string, a usize or a TurnId, so to_value cannot fail"
    )]
    pub(crate) fn payload(&self) -> Value {
        serde_json::to_value(self).expect("extraction diagnostic serializes to JSON")
    }
}

/// How much of the attempt was prose, reasoning and program.
///
/// The code counters live behind [`ProgramCounts`] rather than beside the prose
/// ones so that an attempt that committed no program cannot be described as
/// having written code: the zero a host reads for such an attempt is this
/// type's rendering of "no program", not a counter someone forgot to fill in.
pub(crate) struct ExtractionCounts<'a> {
    language_id: &'a str,
    full_text_chars: usize,
    prose_chars: usize,
    reasoning_chars: usize,
    program: ProgramCounts,
}

enum ProgramCounts {
    /// The attempt carried no executable program: prose, a malformed cell, or a
    /// reply cut off before one closed.
    None,
    Cells {
        code_chars: usize,
        cell_count: usize,
    },
}

impl<'a> ExtractionCounts<'a> {
    pub(crate) fn prose(
        language_id: &'a str,
        full_text_chars: usize,
        prose_chars: usize,
        reasoning_chars: usize,
    ) -> Self {
        Self {
            language_id,
            full_text_chars,
            prose_chars,
            reasoning_chars,
            program: ProgramCounts::None,
        }
    }

    pub(crate) fn program(
        language_id: &'a str,
        full_text_chars: usize,
        prose_chars: usize,
        reasoning_chars: usize,
        code_chars: usize,
        cell_count: usize,
    ) -> Self {
        Self {
            language_id,
            full_text_chars,
            prose_chars,
            reasoning_chars,
            program: ProgramCounts::Cells {
                code_chars,
                cell_count,
            },
        }
    }
}

impl Serialize for ExtractionCounts<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let (code_chars, cell_count) = match self.program {
            ProgramCounts::None => (0, 0),
            ProgramCounts::Cells {
                code_chars,
                cell_count,
            } => (code_chars, cell_count),
        };
        let mut map = serializer.serialize_map(Some(5))?;
        map.serialize_entry("full_text_chars", &self.full_text_chars)?;
        map.serialize_entry("prose_chars", &self.prose_chars)?;
        map.serialize_entry("code_chars", &code_chars)?;
        map.serialize_entry("reasoning_chars", &self.reasoning_chars)?;
        map.serialize_entry(&format!("{}_cell_count", self.language_id), &cell_count)?;
        map.end()
    }
}

fn termination_diagnostic_name(termination: &RlmTermination) -> &'static str {
    match termination {
        RlmTermination::FinishRequired { .. } => "finish_required",
        RlmTermination::Natural => "natural",
    }
}

/// What the reasoning parts of one attempt contribute to `reasoning_chars`.
///
/// A replay-only part (an encrypted blob with no human-readable summary) counts
/// as one character so that "the model reasoned" is never rendered as zero.
pub(crate) fn reasoning_diagnostic_chars<'a>(
    reasoning: impl IntoIterator<Item = (&'a str, Option<&'a ProviderReasoningReplay>)>,
) -> usize {
    reasoning
        .into_iter()
        .map(|(text, replay)| {
            text.chars()
                .count()
                .max(usize::from(replay.is_some_and(|replay| !replay.is_empty())))
        })
        .sum()
}
