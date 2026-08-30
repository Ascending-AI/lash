//! How a cell's failure is reported back to the model.
//!
//! Two things can go wrong with a cell, and they call for opposite responses.
//! The runtime can *refuse* it — a construct outside the dialect, an execution
//! bound, a budget — in which case the program is not wrong so much as not
//! allowed, and resending it unchanged is guaranteed to fail again. Or the
//! program can *run and fail* — a throw, a rejected tool call, a bad value —
//! in which case the shape was fine and the logic was not.
//! A host cancellation is neither: it terminates the turn and must never become
//! model feedback.
//!
//! Until now both arrived as `Error:` followed by prose, and a model had to
//! infer which it was from the wording of the diagnostic. Guessing wrong is
//! expensive in both directions: reading a refusal as a runtime bug produces a
//! turn spent adding error handling around a construct that will never be
//! accepted, and reading a runtime bug as a refusal produces a turn spent
//! rewriting a program that was structurally fine.
//!
//! The kind is decided where it is known — at the site in the executor that
//! produced the failure — and carried on the error text as a leading tag. It is
//! recovered at the one place that renders an observation. Encoding it in the
//! string rather than adding a field keeps the persisted trajectory shape
//! unchanged; the encoding is closed (this module writes every tag and reads
//! every tag) and `every_kind_round_trips_through_its_tag` keeps it honest.

/// Why a cell produced no result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RlmFeedbackKind {
    /// The runtime refused the cell: a dialect rejection, an execution bound, a
    /// budget. The same program will be refused again.
    Policy,
    /// The program is wrong. It may have failed to compile — a misspelled name,
    /// an unterminated string — or run and thrown. Either way the approach was
    /// allowed and the code was not correct, so the fix is in the code.
    Error,
    /// The host stopped the executing cell. The turn ends at its last
    /// checkpoint; no part of the cancelled tail is repair feedback.
    Stop,
}

const POLICY_TAG: &str = "[POLICY]";
const ERROR_TAG: &str = "[ERROR]";
const STOP_TAG: &str = "[STOP]";

impl RlmFeedbackKind {
    /// The tag that opens the evidence line.
    pub(crate) const fn tag(self) -> &'static str {
        match self {
            Self::Policy => POLICY_TAG,
            Self::Error => ERROR_TAG,
            Self::Stop => STOP_TAG,
        }
    }

    /// Tags `evidence` so the kind survives the trip through the persisted
    /// trajectory entry.
    pub(crate) fn label(self, evidence: impl AsRef<str>) -> String {
        format!("{} {}", self.tag(), evidence.as_ref())
    }

    /// Recovers the kind and the untagged evidence.
    ///
    /// Untagged text is reported as [`RlmFeedbackKind::Error`]: an untagged
    /// failure is one no production site classified, and telling a model its
    /// program broke when the runtime actually refused it wastes a turn, while
    /// the reverse tells it to stop trying something that might have worked.
    /// The safer default is the one that keeps the model debugging.
    pub(crate) fn split(text: &str) -> (Self, &str) {
        if let Some(evidence) = text.strip_prefix(POLICY_TAG) {
            (Self::Policy, evidence.trim_start_matches(' '))
        } else if let Some(evidence) = text.strip_prefix(ERROR_TAG) {
            (Self::Error, evidence.trim_start_matches(' '))
        } else if let Some(evidence) = text.strip_prefix(STOP_TAG) {
            (Self::Stop, evidence.trim_start_matches(' '))
        } else {
            (Self::Error, text)
        }
    }

    /// What to do about it, as its own sentence rather than a clause appended
    /// to the evidence. The two halves answer different questions — what
    /// happened, and what to write next — and a model that has already read the
    /// diagnostic needs only the second.
    pub(crate) fn imperative(self, cell_noun: &str) -> Option<String> {
        match self {
            Self::Policy => Some(format!(
                "Next: the runtime refused this {cell_noun}; sending it again unchanged will be refused again. Rewrite it in the form named above."
            )),
            Self::Error => Some(format!(
                "Next: the defect is in the program, not in what the runtime allows. Fix the cause named above, then send the corrected {cell_noun}."
            )),
            Self::Stop => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tag is the entire carrier, so a kind that cannot be recovered from
    /// its own label silently degrades every refusal into a runtime error.
    #[test]
    fn every_kind_round_trips_through_its_tag() {
        for kind in [
            RlmFeedbackKind::Policy,
            RlmFeedbackKind::Error,
            RlmFeedbackKind::Stop,
        ] {
            let labelled = kind.label("classes are not in the TypeScript dialect");
            assert_eq!(
                RlmFeedbackKind::split(&labelled),
                (kind, "classes are not in the TypeScript dialect"),
                "{kind:?} did not survive its own tag"
            );
        }
    }

    #[test]
    fn an_untagged_failure_reads_as_a_runtime_error() {
        assert_eq!(
            RlmFeedbackKind::split("something went wrong"),
            (RlmFeedbackKind::Error, "something went wrong")
        );
    }

    /// The two imperatives have to actually differ, or the split is decoration.
    #[test]
    fn the_two_imperatives_give_opposite_instructions() {
        let policy = RlmFeedbackKind::Policy
            .imperative("cell")
            .expect("policy needs repair guidance");
        let error = RlmFeedbackKind::Error
            .imperative("cell")
            .expect("error needs repair guidance");
        assert_ne!(policy, error);
        assert!(policy.contains("refused"), "{policy}");
        assert!(error.contains("the defect is in the program"), "{error}");
        // The Error branch also covers compile-time defects, which never ran, so
        // it must not claim the program did.
        assert!(!error.contains("ran and failed"), "{error}");
        assert_eq!(RlmFeedbackKind::Stop.imperative("cell"), None);
    }
}
