//! Rule membership and independently owned corpus shards.

use std::collections::BTreeSet;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{HarnessError, Shard};

/// An uncovered rule and the lane that owns its remaining corpus work.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingRule {
    pub rule: String,
    pub owner: String,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "missing cases or owners: {missing:?}; covered and pending: {overlap:?}; unknown rules: {unknown:?}; malformed shards or owners: {malformed:?}"
)]
pub struct CoverageError {
    pub missing: BTreeSet<String>,
    pub overlap: BTreeSet<String>,
    pub unknown: BTreeSet<String>,
    pub malformed: BTreeSet<String>,
}

/// Reads definitions, not cross-references or section headings.
pub fn rule_ids(semantics: &str) -> BTreeSet<String> {
    semantics
        .lines()
        .filter_map(|line| {
            let id = line.strip_prefix("- **")?.split_once(".**")?.0;
            let mut parts = id.split('-');
            (parts.next() == Some("K")
                && parts.next().is_some_and(|family| {
                    !family.is_empty() && family.bytes().all(|byte| byte.is_ascii_uppercase())
                })
                && parts.next().is_some_and(|number| {
                    number.len() == 3 && number.bytes().all(|byte| byte.is_ascii_digit())
                })
                && parts.next().is_none())
            .then(|| id.to_owned())
        })
        .collect()
}

/// Every rule has a case or a named pending owner, never both. One shard
/// owns exactly one rule. Unknown rules, empty shards, duplicate case
/// names, duplicate pending rules and empty owners are refused.
/// Pass an empty pending list to require completed coverage at cutover.
pub fn check_coverage(
    semantics: &str,
    shards: &[Shard],
    pending: &[PendingRule],
) -> Result<(), CoverageError> {
    let rules = rule_ids(semantics);
    let mut covered = BTreeSet::new();
    let mut malformed = BTreeSet::new();
    let mut names = BTreeSet::new();
    if rules.is_empty() {
        malformed.insert("no rule definitions".into());
    }
    for shard in shards {
        if shard.cases.is_empty() || !covered.insert(shard.rule.clone()) {
            malformed.insert(shard.rule.clone());
        }
        for case in &shard.cases {
            if case.name.trim().is_empty()
                || case.document.trim().is_empty()
                || !names.insert((shard.rule.clone(), case.name.clone()))
            {
                malformed.insert(format!("{}: {}", shard.rule, case.name));
            }
        }
    }
    let mut deferred = BTreeSet::new();
    for row in pending {
        if row.owner.trim().is_empty() || !deferred.insert(row.rule.clone()) {
            malformed.insert(format!("{}: pending owner", row.rule));
        }
    }
    let accounted: BTreeSet<_> = covered.union(&deferred).cloned().collect();
    let error = CoverageError {
        missing: rules.difference(&accounted).cloned().collect(),
        overlap: covered.intersection(&deferred).cloned().collect(),
        unknown: accounted.difference(&rules).cloned().collect(),
        malformed,
    };
    if error.missing.is_empty()
        && error.overlap.is_empty()
        && error.unknown.is_empty()
        && error.malformed.is_empty()
    {
        Ok(())
    } else {
        Err(error)
    }
}

/// Decodes host-supplied shard files in sorted path order. The host owns
/// directory discovery and I/O; readers can supply embedded fixtures instead.
/// The file name must equal the rule id.
pub fn load_corpus<P: AsRef<Path>, B: AsRef<[u8]>>(
    files: impl IntoIterator<Item = (P, B)>,
) -> Result<Vec<Shard>, HarnessError> {
    let mut files: Vec<_> = files.into_iter().collect();
    files.sort_by(|a, b| a.0.as_ref().cmp(b.0.as_ref()));
    files
        .into_iter()
        .map(|(path, bytes)| {
            let path = path.as_ref();
            let shard: Shard = serde_json::from_slice(bytes.as_ref())
                .map_err(|e| HarnessError(format!("{}: {e}", path.display())))?;
            if path.file_stem().and_then(|s| s.to_str()) != Some(&shard.rule) {
                return Err(HarnessError(format!(
                    "{} does not name {}",
                    path.display(),
                    shard.rule
                )));
            }
            Ok(shard)
        })
        .collect()
}
