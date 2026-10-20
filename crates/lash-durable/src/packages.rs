//! Package artifacts and sets as the store keeps them, what a node holds,
//! and what an actor's code was linked under (FIG-6918, S1-F2).
//!
//! An artifact is one build of a package, keyed by its digest, which its
//! path is among the facts of; the store assigns it an ordinal on its
//! package line when it is first installed, for listings. A set is one
//! dependency-closed artifact per line, keyed by its digest, holding its
//! members and the store's install sequence: the only key behind "newest
//! first". A node advertises the artifacts it holds; "node N holds set S"
//! is derived, S's members being among N's, and no list of sets a node
//! holds is stored. An actor whose state pins code records the set it was
//! linked under, and a node claims it to run it only when it holds that
//! set (CLAIM-HOLDS).

use std::collections::{BTreeMap, BTreeSet};

pub use lash_kernel_doc::{ArtifactDigest, PackageSetDigest};

use crate::formats::FormatSet;
use crate::{DurableError, StoreFailure, StoreFailureKind};

/// One artifact a node holds or a set names: its package path, and its
/// digest, which alone identifies it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HeldArtifact {
    /// The digest of the artifact's canonical facts, its path among them.
    pub digest: ArtifactDigest,
    /// The artifact's package path, canonically spelled.
    pub path: String,
}

/// One live node, as the fleet gates read it (P1: one entry per node).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NodeCapability {
    /// The format sets the node decodes.
    pub decodes: Vec<FormatSet>,
    /// The artifacts the node holds.
    pub held: BTreeSet<HeldArtifact>,
}

impl NodeCapability {
    /// Whether the node holds every member of `members`, a set's.
    #[must_use]
    pub fn holds(&self, members: &BTreeSet<HeldArtifact>) -> bool {
        members.is_subset(&self.held)
    }
}

/// One artifact as a host installs it: its key, and its header, so that a
/// listing reads what each stored artifact is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactInstall {
    pub artifact: HeldArtifact,
    /// The artifact's header, as JSON.
    pub header: String,
}

/// One set as a host installs it: its digest and its members.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetInstall {
    pub digest: PackageSetDigest,
    pub members: BTreeSet<HeldArtifact>,
}

/// One stored artifact, without its bytes: what a listing reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredArtifact {
    pub artifact: HeldArtifact,
    /// Its place on its package line, assigned by the store when it was
    /// first installed, the first being 1.
    pub ordinal: u32,
}

/// One stored set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredSet {
    pub digest: PackageSetDigest,
    pub members: BTreeSet<HeldArtifact>,
    /// The store's install sequence: a later install is newer.
    pub sequence: u64,
}

/// Every artifact and set the store holds: artifacts by line and ordinal,
/// sets by install sequence, oldest first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PackageCatalog {
    pub artifacts: Vec<StoredArtifact>,
    pub sets: Vec<StoredSet>,
}

/// The canonical stored spelling of a node's held artifacts and a set's
/// members: a JSON array of `[digest, path]` pairs, sorted by digest.
#[must_use]
pub fn encode_artifacts(artifacts: &BTreeSet<HeldArtifact>) -> String {
    serde_json::Value::from(
        artifacts
            .iter()
            .map(|artifact| {
                serde_json::Value::from(vec![artifact.digest.to_string(), artifact.path.clone()])
            })
            .collect::<Vec<_>>(),
    )
    .to_string()
}

/// The digests of `artifacts`, as the JSON array of hex text a claim
/// compares a set's members against.
#[must_use]
pub fn encode_digests(artifacts: &BTreeSet<HeldArtifact>) -> String {
    serde_json::Value::from(
        artifacts
            .iter()
            .map(|artifact| artifact.digest.to_string())
            .collect::<Vec<_>>(),
    )
    .to_string()
}

/// A stored digest's text as the digest.
///
/// # Errors
///
/// The text is not a digest: the row is corrupt.
pub fn parse_digest<T: std::str::FromStr>(text: &str) -> Result<T, DurableError> {
    text.parse()
        .map_err(|_| corrupt(format!("a stored digest `{text}`")))
}

fn corrupt(message: String) -> DurableError {
    DurableError::Store(StoreFailure {
        kind: StoreFailureKind::Corrupt,
        message,
    })
}

/// The sets `catalog` holds that `node` holds wholly, newest first.
#[must_use]
pub fn held_sets<'a>(catalog: &'a PackageCatalog, node: &NodeCapability) -> Vec<&'a StoredSet> {
    let mut sets: Vec<&StoredSet> = catalog
        .sets
        .iter()
        .filter(|set| node.holds(&set.members))
        .collect();
    sets.sort_by_key(|set| std::cmp::Reverse(set.sequence));
    sets
}

/// The catalog the listing rows spell: each artifact as `(digest, path,
/// ordinal)`, and each set member as `(set, sequence, member, path)` by
/// sequence. Both backends' reads.
///
/// # Errors
///
/// A row that does not decode: the store is corrupt.
pub fn catalog_of(
    artifacts: Vec<(String, String, i64)>,
    members: Vec<(String, i64, String, String)>,
) -> Result<PackageCatalog, DurableError> {
    let artifacts = artifacts
        .into_iter()
        .map(|(digest, path, ordinal)| {
            Ok(StoredArtifact {
                artifact: HeldArtifact {
                    digest: parse_digest(&digest)?,
                    path,
                },
                ordinal: u32::try_from(ordinal)
                    .map_err(|_| corrupt(format!("artifact {digest} has ordinal {ordinal}")))?,
            })
        })
        .collect::<Result<Vec<_>, DurableError>>()?;
    let mut sets: Vec<StoredSet> = Vec::new();
    let mut at: BTreeMap<String, usize> = BTreeMap::new();
    for (digest, sequence, artifact, path) in members {
        let index = match at.get(&digest) {
            Some(index) => *index,
            None => {
                sets.push(StoredSet {
                    digest: parse_digest(&digest)?,
                    members: BTreeSet::new(),
                    sequence: u64::try_from(sequence).map_err(|_| {
                        corrupt(format!("package set {digest} has sequence {sequence}"))
                    })?,
                });
                at.insert(digest, sets.len() - 1);
                sets.len() - 1
            }
        };
        sets[index].members.insert(HeldArtifact {
            digest: parse_digest(&artifact)?,
            path,
        });
    }
    Ok(PackageCatalog { artifacts, sets })
}

/// The live nodes the listing rows spell, each row `(node, formats_json,
/// artifact, path)` by node, the artifact columns empty for a node that
/// holds none. Both backends' reads.
///
/// # Errors
///
/// A row that does not decode: the store is corrupt.
pub fn nodes_of(
    rows: Vec<(String, String, Option<String>, Option<String>)>,
) -> Result<Vec<NodeCapability>, DurableError> {
    let mut nodes: Vec<(String, NodeCapability)> = Vec::new();
    for (node, formats, artifact, path) in rows {
        if nodes.last().is_none_or(|(last, _)| *last != node) {
            let sets: Vec<String> = serde_json::from_str(&formats)
                .map_err(|_| corrupt(format!("node {node}'s format sets `{formats}`")))?;
            nodes.push((
                node,
                NodeCapability {
                    decodes: sets.into_iter().map(FormatSet::new).collect(),
                    held: BTreeSet::new(),
                },
            ));
        }
        if let (Some(artifact), Some(path)) = (artifact, path)
            && let Some((_, capability)) = nodes.last_mut()
        {
            capability.held.insert(HeldArtifact {
                digest: parse_digest(&artifact)?,
                path,
            });
        }
    }
    Ok(nodes.into_iter().map(|(_, node)| node).collect())
}

/// The refusal of a set naming an artifact neither stored nor installed
/// with it.
#[must_use]
pub fn unknown_member(set: &SetInstall, member: &HeldArtifact) -> DurableError {
    DurableError::UnknownArtifact {
        set: set.digest,
        artifact: member.digest,
        path: member.path.clone(),
    }
}
