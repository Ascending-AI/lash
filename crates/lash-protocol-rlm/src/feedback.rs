//! How a cell's failure is reported back to the model.
//!
//! Three things can go wrong with a cell, and they call for different responses.
//! The runtime can *refuse* it — a construct outside the dialect, an execution
//! bound, a budget — in which case the program is not wrong so much as not
//! allowed, and resending it unchanged is guaranteed to fail again. Or the
//! program can *run and fail* — a throw, a rejected tool call, a bad value —
//! in which case the shape was fine and the logic was not.
//! Host infrastructure can also fail while preparing or executing valid code;
//! that failure must not send the model off to rewrite its program.
//!
//! Until now both arrived as `Error:` followed by prose, and a model had to
//! infer which it was from the wording of the diagnostic. Guessing wrong is
//! expensive in both directions: reading a refusal as a runtime bug produces a
//! turn spent adding error handling around a construct that will never be
//! accepted, and reading a runtime bug as a refusal produces a turn spent
//! rewriting a program that was structurally fine.
//!
//! The executor decides the kind where the failure source is known and carries
//! it structurally in [`lash_core::CellFailure`]. This module renders that typed
//! value for the model; it does not encode or recover type information in prose.

use lash_core::{CellFailure, CellFailureKind};

/// Renders typed failure evidence followed by kind-specific recovery guidance.
pub(crate) fn render(failure: &CellFailure, cell_noun: &str) -> String {
    format!(
        "{}\n\n{}",
        failure.message,
        imperative(failure.kind, cell_noun)
    )
}

fn imperative(kind: CellFailureKind, cell_noun: &str) -> String {
    match kind {
        CellFailureKind::Policy => format!(
            "Next: the runtime refused this {cell_noun}; sending it again unchanged will be refused again. Rewrite it in the form named above."
        ),
        CellFailureKind::Program => format!(
            "Next: the defect is in the program, not in what the runtime allows. Fix the cause named above, then send the corrected {cell_noun}."
        ),
        CellFailureKind::Host => format!(
            "Next: the host failed while handling this {cell_noun}. Retry it; if the failure persists, report the host problem."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two imperatives have to actually differ, or the split is decoration.
    #[test]
    fn the_two_imperatives_give_opposite_instructions() {
        let policy = imperative(CellFailureKind::Policy, "cell");
        let error = imperative(CellFailureKind::Program, "cell");
        assert_ne!(policy, error);
        assert!(policy.contains("refused"), "{policy}");
        assert!(error.contains("the defect is in the program"), "{error}");
        // The Error branch also covers compile-time defects, which never ran, so
        // it must not claim the program did.
        assert!(!error.contains("ran and failed"), "{error}");
    }

    #[test]
    fn a_host_failure_does_not_blame_the_program() {
        let host = imperative(CellFailureKind::Host, "cell");

        assert!(!host.contains("program"), "{host}");
        assert!(host.contains("host"), "{host}");
    }
}
