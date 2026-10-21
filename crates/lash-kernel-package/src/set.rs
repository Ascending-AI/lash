//! A package set: one artifact per line, dependency-closed.

use std::collections::BTreeMap;

use lash_kernel_doc::{ArtifactDigest, PackageSetDigest};
use lash_vocabulary::names::PackagePath;

use crate::{ArtifactKey, Package, PackageRefusal, SetHolds};

/// The content identity of a dependency-closed package set.
/// version_surface = "coexist"
/// version_guard(items(PACKAGE_SET_DOMAIN, PackageSet))
const PACKAGE_SET_DOMAIN: &str = "lash package set 2026-10 v1";

/// One artifact per package line. Its constructor admits only a
/// dependency-closed set: every artifact's requirement of a line names the
/// set's artifact of that line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageSet {
    members: BTreeMap<PackagePath, ArtifactDigest>,
}

impl PackageSet {
    /// The set of exactly `artifacts`.
    ///
    /// # Errors
    ///
    /// [`PackageRefusal::TwoOfLine`] for two artifacts of one line, and
    /// [`PackageRefusal::NotClosed`] when an artifact requires an artifact
    /// the set does not hold (PKG-SET-CLOSED).
    pub fn new<'a>(
        artifacts: impl IntoIterator<Item = &'a Package>,
    ) -> Result<Self, PackageRefusal> {
        let artifacts: Vec<&Package> = artifacts.into_iter().collect();
        let mut members = BTreeMap::new();
        for artifact in &artifacts {
            if let Some(first) = members.insert(artifact.path().clone(), artifact.digest())
                && first != artifact.digest()
            {
                return Err(PackageRefusal::TwoOfLine {
                    path: artifact.path().clone(),
                    first,
                    second: artifact.digest(),
                });
            }
        }
        let set = Self { members };
        set.check_closed(|key| {
            artifacts
                .iter()
                .copied()
                .find(|artifact| artifact.key() == *key)
        })?;
        Ok(set)
    }

    /// A set named by its members, as a store row or a frame holds it. A
    /// node checks it against the artifacts it holds
    /// ([`crate::HeldPackages::new`]).
    pub fn from_members(members: BTreeMap<PackagePath, ArtifactDigest>) -> Self {
        Self { members }
    }

    pub fn members(&self) -> &BTreeMap<PackagePath, ArtifactDigest> {
        &self.members
    }

    /// Each member's key.
    pub fn keys(&self) -> impl Iterator<Item = ArtifactKey> + '_ {
        self.members.iter().map(|(path, digest)| ArtifactKey {
            path: path.clone(),
            digest: *digest,
        })
    }

    pub fn holds(&self, key: &ArtifactKey) -> bool {
        self.members.get(&key.path) == Some(&key.digest)
    }

    /// The blake3 digest of the members: the set's one name, computed.
    pub fn digest(&self) -> PackageSetDigest {
        let mut hasher = blake3::Hasher::new_derive_key(PACKAGE_SET_DOMAIN);
        hasher.update(&(self.members.len() as u64).to_le_bytes());
        for (path, digest) in &self.members {
            let path = path.to_string();
            hasher.update(&(path.len() as u64).to_le_bytes());
            hasher.update(path.as_bytes());
            hasher.update(digest.as_bytes());
        }
        PackageSetDigest::from_bytes(*hasher.finalize().as_bytes())
    }

    /// Refuses a member `artifact` cannot find, or one requiring another
    /// artifact of a line than the set's.
    pub(crate) fn check_closed<'a>(
        &self,
        artifact: impl Fn(&ArtifactKey) -> Option<&'a Package>,
    ) -> Result<(), PackageRefusal> {
        for key in self.keys() {
            let package =
                artifact(&key).ok_or(PackageRefusal::ArtifactNotHeld { key: key.clone() })?;
            for (line, required) in package.requires() {
                if self.members.get(line) != Some(required) {
                    return Err(PackageRefusal::NotClosed {
                        artifact: Box::new(key),
                        requires: Box::new(ArtifactKey {
                            path: line.clone(),
                            digest: *required,
                        }),
                        held: match self.members.get(line) {
                            Some(other) => SetHolds::Other(*other),
                            None => SetHolds::Nothing,
                        },
                    });
                }
            }
        }
        Ok(())
    }
}
