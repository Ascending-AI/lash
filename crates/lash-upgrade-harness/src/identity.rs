//! What a build reports about itself.
//!
//! The report is the proof that the two binaries are two builds: N and N+1
//! differ in their label and in every constant a `synthetic-next` block
//! moves, and the drain generation `G` hashes some of them.

use lash_core_store::compat::{DESCRIPTORS, VersionRange};
use serde::{Deserialize, Serialize};

/// Which of the two Phase A builds this binary is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BuildLabel {
    /// The default build: head as it ships.
    #[serde(rename = "n")]
    N,
    /// Head with the `synthetic-next` feature: a synthetic next release.
    #[serde(rename = "n+1")]
    Next,
}

impl BuildLabel {
    /// The build this binary was compiled as.
    pub const fn current() -> Self {
        if cfg!(feature = "synthetic-next") {
            Self::Next
        } else {
            Self::N
        }
    }

    /// The label's spelling in reports and in the scripted provider's reply.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::N => "n",
            Self::Next => "n+1",
        }
    }
}

impl std::fmt::Display for BuildLabel {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One stored component's declared ranges (ADR 0115 §1.1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComponentRanges {
    pub component: String,
    pub reads: VersionRange,
    pub writes: VersionRange,
}

/// Everything a build declares that a roll depends on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildIdentity {
    pub build: BuildLabel,
    /// The drain generation `G` this build stamps on its Restate deployment.
    pub generation: String,
    /// The manual journal-logic epoch hashed into `G`.
    pub journal_logic_epoch: u32,
    /// The fleet epochs `F` this build writes under.
    pub fleet_writable: VersionRange,
    /// The remote-protocol versions this build speaks.
    pub remote_protocol: VersionRange,
    /// The Restate handler wire versions this build reads and answers.
    pub restate_wire: VersionRange,
    /// Every stored component this build declares.
    pub components: Vec<ComponentRanges>,
}

impl BuildIdentity {
    /// This binary's identity.
    pub fn current() -> Self {
        Self {
            build: BuildLabel::current(),
            generation: lash::formats::build_generation().to_string(),
            journal_logic_epoch: lash_restate::JOURNAL_LOGIC_EPOCH,
            fleet_writable: lash_core_store::store::FleetFormat::writable(),
            remote_protocol: lash_remote_protocol::REMOTE_PROTOCOL,
            restate_wire: lash_restate::RESTATE_WIRE,
            components: DESCRIPTORS
                .iter()
                .map(|descriptor| ComponentRanges {
                    component: descriptor.component.as_str().to_string(),
                    reads: descriptor.reads,
                    writes: descriptor.writes,
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{BuildIdentity, BuildLabel};

    #[test]
    fn identity_round_trips_through_its_json_report() {
        let identity = BuildIdentity::current();
        let json = serde_json::to_string(&identity).expect("encode");
        let decoded: BuildIdentity = serde_json::from_str(&json).expect("decode");
        assert_eq!(decoded, identity);
        assert_eq!(identity.build, BuildLabel::current());
        assert_eq!(identity.generation.len(), 12, "G is six bytes of hex");
        assert!(!identity.components.is_empty());
    }

    #[test]
    fn build_labels_have_frozen_spellings() {
        assert_eq!(serde_json::to_string(&BuildLabel::N).expect("n"), "\"n\"");
        assert_eq!(
            serde_json::to_string(&BuildLabel::Next).expect("n+1"),
            "\"n+1\""
        );
    }
}
