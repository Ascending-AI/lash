//! Package artifacts and sets as the store keeps them, and what each live
//! node offers running code (FIG-6918, S1-F2, FIG-6993).
//!
//! An artifact is one build of a package, keyed by its digest, which its
//! path is among the facts of; the store assigns it an ordinal on its
//! package line when it is first installed, for listings. A set is one
//! dependency-closed artifact per line, keyed by its digest, holding its
//! members, the store's install sequence (the only key behind "newest
//! first"), its surface snapshot and the execution requirements its code
//! derives (FIG-6991). Both are immutable: the install that first writes a
//! set writes them, and nothing deletes them. A node advertises its
//! capabilities for as long as it is live; an actor's claim requirements
//! name the natives, kernels, providers and codecs its outstanding work
//! needs, and a node claims it to run it only when its capabilities serve
//! each (FIG-6993). [`strand`] is what the strand report reads: the work
//! no live node can run.

use std::collections::{BTreeMap, BTreeSet};

pub use lash_kernel_doc::{
    ArtifactDigest, EffectName, ExecutionRequirements, FunctionId, NodeCapabilities,
    PackageSetDigest,
};

use crate::formats::FormatSet;
use crate::{DurableError, StoreFailure, StoreFailureKind};

/// One artifact a set names or an install writes: its package path, and
/// its digest, which alone identifies it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PackageArtifact {
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
    /// What the node offers running code, its own package set among it.
    pub capabilities: NodeCapabilities,
}

/// One artifact as a host installs it: its key, its header, so that a
/// listing reads what each stored artifact is, and the identity of every
/// definition it holds, helpers included, and the operations it declares
/// internal, which the authority index records under its path (FIG-6991,
/// Ruling 5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactInstall {
    pub artifact: PackageArtifact,
    /// The artifact's header, as JSON.
    pub header: String,
    pub definitions: BTreeSet<FunctionId>,
    /// The operations it declares internal, by effect name: the index of
    /// who may perform them records them under its path.
    pub internal: BTreeSet<EffectName>,
    /// Its release and display labels, as JSON: they are outside the artifact's
    /// content identity, so the store appends each install's beside it.
    pub labels: String,
}

impl ArtifactInstall {
    /// The fixed-width index key for this install's label payload. Labels can
    /// exceed a database's index-row bound; their bytes remain stored whole.
    #[must_use]
    pub fn labels_digest(&self) -> [u8; 32] {
        *blake3::hash(self.labels.as_bytes()).as_bytes()
    }
}

/// One set as a host installs it: its digest, its members, the bytes of
/// its surface snapshot and what running its code requires.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetInstall {
    pub digest: PackageSetDigest,
    pub members: BTreeSet<PackageArtifact>,
    pub surface: Vec<u8>,
    pub requirements: ExecutionRequirements,
}

/// One stored artifact, without its bytes: what a listing reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredArtifact {
    pub artifact: PackageArtifact,
    /// Its place on its package line, assigned by the store when it was
    /// first installed, the first being 1.
    pub ordinal: u32,
}

/// One stored set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredSet {
    pub digest: PackageSetDigest,
    pub members: BTreeSet<PackageArtifact>,
    /// The store's install sequence: a later install is newer.
    pub sequence: u64,
    /// What running the set's code requires.
    pub requirements: ExecutionRequirements,
}

/// Every artifact and set the store holds: artifacts by line and ordinal,
/// sets by install sequence, oldest first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PackageCatalog {
    pub artifacts: Vec<StoredArtifact>,
    pub sets: Vec<StoredSet>,
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

/// The stored spelling of a set's requirements: canonical JSON, each set
/// sorted.
#[must_use]
pub fn encode_requirements(requirements: &ExecutionRequirements) -> String {
    serde_json::to_string(requirements).unwrap_or_else(|_| unreachable!("string keys, no floats"))
}

/// The stored spelling of a node's capabilities: canonical JSON, each set
/// sorted.
#[must_use]
pub fn encode_capabilities(capabilities: &NodeCapabilities) -> String {
    serde_json::to_string(capabilities).unwrap_or_else(|_| unreachable!("string keys, no floats"))
}

fn decode_requirements(text: &str, set: &str) -> Result<ExecutionRequirements, DurableError> {
    serde_json::from_str(text)
        .map_err(|_| corrupt(format!("package set {set}'s requirements `{text}`")))
}

fn decode_capabilities(text: &str, node: &str) -> Result<NodeCapabilities, DurableError> {
    serde_json::from_str(text).map_err(|_| corrupt(format!("node {node}'s capabilities `{text}`")))
}

/// The catalog the listing rows spell: each artifact as `(digest, path,
/// ordinal)`, and each set member as `(set, sequence, requirements,
/// member, path)` by sequence. Both backends' reads.
///
/// # Errors
///
/// A row that does not decode: the store is corrupt.
pub fn catalog_of(
    artifacts: Vec<(String, String, i64)>,
    members: Vec<(String, i64, String, String, String)>,
) -> Result<PackageCatalog, DurableError> {
    let artifacts = artifacts
        .into_iter()
        .map(|(digest, path, ordinal)| {
            Ok(StoredArtifact {
                artifact: PackageArtifact {
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
    for (digest, sequence, requirements, artifact, path) in members {
        let index = match at.get(&digest) {
            Some(index) => *index,
            None => {
                sets.push(StoredSet {
                    digest: parse_digest(&digest)?,
                    members: BTreeSet::new(),
                    sequence: u64::try_from(sequence).map_err(|_| {
                        corrupt(format!("package set {digest} has sequence {sequence}"))
                    })?,
                    requirements: decode_requirements(&requirements, &digest)?,
                });
                at.insert(digest, sets.len() - 1);
                sets.len() - 1
            }
        };
        sets[index].members.insert(PackageArtifact {
            digest: parse_digest(&artifact)?,
            path,
        });
    }
    Ok(PackageCatalog { artifacts, sets })
}

/// One row of the live-node listing: `(node, formats_json,
/// capabilities_json)`.
pub type NodeRow = (String, String, String);

/// The live nodes the listing rows spell, one row per node. Both backends'
/// reads.
///
/// # Errors
///
/// A row that does not decode: the store is corrupt.
pub fn nodes_of(rows: Vec<NodeRow>) -> Result<Vec<NodeCapability>, DurableError> {
    rows.into_iter()
        .map(|(node, formats, capabilities)| {
            let sets: Vec<String> = serde_json::from_str(&formats)
                .map_err(|_| corrupt(format!("node {node}'s format sets `{formats}`")))?;
            Ok(NodeCapability {
                decodes: sets.into_iter().map(FormatSet::new).collect(),
                capabilities: decode_capabilities(&capabilities, &node)?,
            })
        })
        .collect()
}

/// The actors and stored requirements the strand read's rows spell, each
/// `(actor_key, requires)`. Both backends' reads.
///
/// # Errors
///
/// A row that does not decode: the store is corrupt.
pub fn requirement_rows(
    rows: Vec<(String, String)>,
) -> Result<Vec<(crate::ActorKey, Vec<String>)>, DurableError> {
    rows.into_iter()
        .map(|(actor, requires)| {
            let key = crate::ActorKey::parse(&actor)
                .map_err(|error| corrupt(format!("actor key `{actor}`: {error}")))?;
            let requires = crate::port::parse_requires(Some(&requires))
                .map_err(|error| corrupt(format!("actor {actor}'s requirements: {error}")))?;
            Ok((key, requires))
        })
        .collect()
}

/// What strands work whose claim requirements are `requires` while `live`
/// are the live nodes (FIG-6993): `None` when some live node offers every
/// hard requirement it names, else what the live node closest to running
/// it lacks (fewest unmet), or the whole record when no node is live. Only
/// the hard families are read: a registration token is a node's, not the
/// code's.
#[must_use]
pub fn strand(requires: &[String], live: &[NodeCapability]) -> Option<ExecutionRequirements> {
    let record = lash_core_store::obligations::ObligationToken::requirements_of(
        requires.iter().map(String::as_str),
    );
    if record.is_empty() {
        return None;
    }
    let size = |unmet: &ExecutionRequirements| {
        unmet.natives.len() + unmet.kernels.len() + unmet.providers.len() + unmet.codecs.len()
    };
    let closest = live
        .iter()
        .map(|node| record.unmet_by(&node.capabilities))
        .min_by_key(size);
    match closest {
        Some(unmet) if unmet.is_empty() => None,
        Some(unmet) => Some(unmet),
        None => Some(record),
    }
}

/// The refusal of a set naming an artifact neither stored nor installed
/// with it.
#[must_use]
pub fn unknown_member(set: &SetInstall, member: &PackageArtifact) -> DurableError {
    DurableError::UnknownArtifact {
        set: set.digest,
        artifact: member.digest,
        path: member.path.clone(),
    }
}
