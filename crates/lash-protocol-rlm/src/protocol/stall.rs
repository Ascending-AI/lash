//! Reply fingerprints and extraction diagnostics for host inspection.
//! The core turn checkpoint owns the no-progress meter.

/// version_surface = "coexist"
/// version_guard(items(LASH_RLM_STALL_REPLY_DOMAIN_VERSION, reply_fingerprint))
const LASH_RLM_STALL_REPLY_DOMAIN_VERSION: &str = "lash-rlm-stall-reply/v2";

use crate::rlm_support::RlmCompletion;
use lash_core::llm::types::ProviderReasoningReplay;
use lash_vocabulary::identity::TurnId;
use serde::ser::{Serialize, SerializeMap, Serializer};
use serde_json::Value;

/// Diagnostic phase that records a turn stopped by its no-progress budget.
pub(crate) const NO_PROGRESS_BUDGET_PHASE: lash_rlm_types::RlmDiagnosticPhase =
    lash_rlm_types::RlmDiagnosticPhase::NoProgressBudget;
fn trajectory_entry_turn_prefix(turn_id: &TurnId) -> String {
    format!("lash_vm_step_{turn_id}_")
}

/// The id of the cell a turn ran at one protocol iteration: its record's
/// identity, and the correlation its assistant context names.
pub(crate) fn cell_id(turn_id: &TurnId, protocol_iteration: usize) -> String {
    format!(
        "{}{protocol_iteration}",
        trajectory_entry_turn_prefix(turn_id)
    )
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
    let digest = lash_protocol_types::core_support::blake3_domain_hash(
        LASH_RLM_STALL_REPLY_DOMAIN_VERSION,
        assistant_text.as_bytes(),
    );
    digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The per-attempt extraction diagnostic, for native program admission.
///
/// Every provider attempt appends exactly one typed diagnostic; the reader counts
/// stalled attempts and compares their fingerprints. All producers supply the
/// same fields.
///
/// `turn_id` scopes this evidence to one turn. `reply_fingerprint` lets a
/// host compare replies; neither field controls runtime admission.
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
        termination: &RlmCompletion,
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

fn termination_diagnostic_name(termination: &RlmCompletion) -> &'static str {
    match termination.mode {
        lash_core::TerminationMode::TerminalRequired => "terminal_required",
        lash_core::TerminationMode::Natural => "natural",
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
