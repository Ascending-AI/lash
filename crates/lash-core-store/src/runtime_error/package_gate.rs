//! The package gate's typed refusal (FIG-6993): no live serving node's own
//! surface is one every live serving node can run.

use super::{RuntimeEffectControllerError, RuntimeErrorCause, RuntimeErrorCode};

/// One surface the gate considered and could not answer: a live serving
/// node's own package set, and what the live serving nodes lack to run it.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct SnapshotCandidate {
    /// The candidate set's digest, in hex.
    pub surface: String,
    /// Each requirement of its surface some live serving node does not
    /// meet, as the requirement reads: `native <id>` or `kernel <n>`.
    pub unmet: Vec<String>,
}

impl RuntimeEffectControllerError {
    /// The gate's refusal: none of `candidates` is a surface every live
    /// serving node can run, so a new cell has none to be written under.
    #[must_use]
    pub fn no_compatible_snapshot(candidates: Vec<SnapshotCandidate>) -> Self {
        let named: Vec<String> = candidates
            .iter()
            .map(|candidate| {
                format!(
                    "{} (lacking {})",
                    candidate.surface.get(..12).unwrap_or(&candidate.surface),
                    candidate.unmet.join(", ")
                )
            })
            .collect();
        let mut error = Self::new(
            RuntimeErrorCode::NoCompatibleSnapshot,
            format!(
                "no package surface is one every live serving node can run: {}",
                if named.is_empty() {
                    "no live serving node advertises one".to_owned()
                } else {
                    named.join("; ")
                }
            ),
        );
        error.cause = Some(RuntimeErrorCause::NoCompatibleSnapshot { candidates });
        error
    }

    /// The candidates a `NoCompatibleSnapshot` refusal names. `None` on any
    /// other error.
    #[must_use]
    pub fn no_compatible_snapshot_candidates(&self) -> Option<&[SnapshotCandidate]> {
        match self.cause.as_ref()? {
            RuntimeErrorCause::NoCompatibleSnapshot { candidates } => Some(candidates),
            _ => None,
        }
    }
}
