//! Open findings: runtime bugs the crash matrix or the chaos soak found,
//! reported to the lane that owns the code, each with the violations it
//! produces.
//!
//! A cell or an epoch that fails is explained by an open finding only when
//! every violation it reports is one of that finding's: any other violation
//! still fails it, and a finding never excuses a cell of another case. A
//! cell a finding names may hold, since some of its bugs are races; the
//! change that fixes one deletes its entry.

use lash_durable_test::{Cell, Verdict};

use super::Case;

/// One open finding.
#[derive(Clone, Copy, Debug)]
pub struct Finding {
    /// Its id, quoted to its owner.
    pub id: &'static str,
    /// The lane and ticket that own the fix.
    pub owner: &'static str,
    pub summary: &'static str,
    /// The case whose matrix shows it.
    pub case: Case,
    /// The one cut (`label#nth mode`, on whichever node the seed made
    /// primary) it shows at, or `None` for a race that may show at any cell
    /// of the case.
    pub cell: Option<&'static str>,
    /// What every violation of a matrix cell it explains contains one of.
    pub cell_violations: &'static [&'static str],
    /// What every violation of a soak epoch it explains contains one of;
    /// empty when the soak cannot show it.
    pub epoch_violations: &'static [&'static str],
}

/// The open findings.
pub const OPEN: &[Finding] = &[
    Finding {
        id: "FIG-5184 M1",
        owner: "L6 (FIG-5175)",
        summary: "a failed step.outcome commit leaks its ordinal from the admitted execution's in-memory run cursor, so the next outcome is refused as a gap, the reload's as taken, and a Repeatable step re-runs twice",
        case: Case::Process,
        cell: Some("step.outcome#1 fail-before"),
        cell_violations: &["NR-3: Repeatable body"],
        epoch_violations: &[],
    },
    Finding {
        id: "FIG-5184 M2",
        owner: "L6b (FIG-5176)",
        summary: "the session close's triggers step reads live Until descendants outside its transaction and mints a ProcessTerminal wait on one whose terminal may already have committed, so the wait is never resolved and the close never reaches its tombstone",
        case: Case::Close,
        cell: None,
        cell_violations: &[
            "not done after",
            "deadline: done at",
            "the close did not end at its tombstone",
            "the closed session kept pending waits",
            "the closed session's storage is not deleted",
        ],
        epoch_violations: &[
            "not done 600 s after the plan",
            "close is not done",
            "close: the close did not end at its tombstone",
            "close: the closed session kept pending waits",
            "close: the closed session's storage is not deleted",
        ],
    },
];

fn explains(signature: &[&str], violations: &[String]) -> bool {
    !signature.is_empty()
        && !violations.is_empty()
        && violations
            .iter()
            .all(|violation| signature.iter().any(|shown| violation.contains(shown)))
}

/// The open finding that explains `cell` of `case`'s matrix, if one does.
#[must_use]
pub fn explaining_cell(case: Case, cell: &Cell) -> Option<&'static Finding> {
    let Verdict::Violated(violations) = &cell.verdict else {
        return None;
    };
    let at = format!("{}#{} {}", cell.point.label, cell.point.nth, cell.fault);
    OPEN.iter().find(|finding| {
        finding.case == case
            && finding.cell.is_none_or(|cell| cell == at)
            && explains(finding.cell_violations, violations)
    })
}

/// The open finding that explains a soak epoch's `violations`, if one does.
#[must_use]
pub fn explaining_epoch(violations: &[String]) -> Option<&'static Finding> {
    OPEN.iter()
        .find(|finding| explains(finding.epoch_violations, violations))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A finding explains only its own violations: one other violation
    /// beside them fails the cell or epoch.
    #[test]
    fn a_finding_explains_only_its_own_violations() {
        let own = vec![
            "close is not done".to_owned(),
            "close: the closed session kept pending waits".to_owned(),
        ];
        assert!(explaining_epoch(&own).is_some());
        let mut other = own.clone();
        other.push("turn is not done".to_owned());
        assert!(explaining_epoch(&other).is_none());
        assert!(explaining_epoch(&[]).is_none());
    }
}
