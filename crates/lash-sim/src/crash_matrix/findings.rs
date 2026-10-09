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
/// FIG-5184 M1 (a leaked step ordinal) and M2 (a close's lost terminal
/// wait) no longer show: FIG-5226 and FIG-5222 rewrote the code they lived
/// in, and their matrices held unmasked at eight seeds (FIG-5193).
pub const OPEN: &[Finding] = &[];

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
        let signature = ["close is not done", "the closed session kept pending waits"];
        let own = vec![
            "close is not done".to_owned(),
            "close: the closed session kept pending waits".to_owned(),
        ];
        assert!(explains(&signature, &own));
        let mut other = own.clone();
        other.push("turn is not done".to_owned());
        assert!(!explains(&signature, &other));
        assert!(!explains(&signature, &[]));
        assert!(!explains(&[], &own));
    }
}
