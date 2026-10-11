//! A helper release as checkpoint graph nodes (FIG-5887 §3, §9): its exact
//! manifest (node kind 13) and each definition it holds (kind 14).
//!
//! A parked run pins the manifest it was written under by the manifest's
//! digest, never by its name: a release rebuilt under the same name with one
//! function changed is another manifest, and a run resumes on exactly the
//! manifest it pinned or not at all. Native code is not part of a
//! definition's bytes, so holding a definition is not holding the function:
//! resuming also requires the worker to have the native implementation the
//! manifest records.
//!
//! This crate stages through a caller's function and depends on no store:
//! the caller hashes and keeps the nodes.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_doc::FunctionId;
use lash_kernel_doc::graph_tokens::{
    ChildRef, TokenCodec, TokenError, TokenReader, TokenWriter, check_ascending, encode_body,
};

use crate::releases::{HeldReleases, HelperRelease};

/// The node kind of a release manifest.
pub const RELEASE_MANIFEST_KIND: u32 = 13;
/// The node kind of one helper definition.
pub const HELPER_DEFINITION_KIND: u32 = 14;

/// Kind 13's body: a release's name, ordinal and sealed flag, the functions
/// a build of it resolves names against, and every function it holds, by
/// identity, with its definition node and its native fingerprint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseManifest {
    pub release: String,
    pub ordinal: u32,
    pub sealed: bool,
    pub writes: BTreeSet<FunctionId>,
    /// Ascending by function.
    pub functions: Vec<ManifestFunction>,
}

/// One function a manifest holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManifestFunction {
    pub function: FunctionId,
    /// Its HelperDefinition node.
    pub definition: ChildRef,
    /// The digest of what its native implementation answers, when it has
    /// one.
    pub fingerprint: Option<String>,
}

impl TokenCodec for ManifestFunction {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(3);
        self.function.encode(out);
        self.definition.encode(out);
        self.fingerprint.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(3)?;
        Ok(Self {
            function: FunctionId::decode(input)?,
            definition: ChildRef::decode(input)?,
            fingerprint: Option::decode(input)?,
        })
    }
}

impl TokenCodec for ReleaseManifest {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(5);
        out.text(&self.release);
        out.unsigned(u64::from(self.ordinal));
        out.bool(self.sealed);
        self.writes.encode(out);
        self.functions.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(5)?;
        let manifest = Self {
            release: String::decode(input)?,
            ordinal: input.unsigned_u32()?,
            sealed: input.bool()?,
            writes: BTreeSet::decode(input)?,
            functions: Vec::decode(input)?,
        };
        let ids: Vec<FunctionId> = manifest.function_ids().collect();
        check_ascending(&ids)?;
        if !manifest.writes.iter().all(|write| manifest.holds(write)) {
            return Err(TokenError::Invalid(
                "a release resolves a name to a function it does not hold".to_owned(),
            ));
        }
        Ok(manifest)
    }
}

impl ReleaseManifest {
    /// Whether the release holds `function`.
    #[must_use]
    pub fn holds(&self, function: &FunctionId) -> bool {
        self.get(function).is_some()
    }

    #[must_use]
    pub fn get(&self, function: &FunctionId) -> Option<&ManifestFunction> {
        self.functions
            .binary_search_by(|held| held.function.cmp(function))
            .ok()
            .map(|index| &self.functions[index])
    }

    pub fn function_ids(&self) -> impl Iterator<Item = FunctionId> + '_ {
        self.functions.iter().map(|held| held.function)
    }
}

/// Stages `release`: each definition as a HelperDefinition, then its
/// manifest. Returns the manifest and its reference.
///
/// # Errors
///
/// The first error `stage` returns.
pub fn stage_release<E>(
    release: &HelperRelease,
    stage: &mut impl FnMut(u32, &[u8]) -> Result<ChildRef, E>,
) -> Result<(ReleaseManifest, ChildRef), E> {
    let mut functions = BTreeMap::new();
    for held in &release.functions {
        let definition = stage(HELPER_DEFINITION_KIND, &encode_body(&held.definition))?;
        functions.insert(
            held.function,
            ManifestFunction {
                function: held.function,
                definition,
                fingerprint: held.fingerprint.clone(),
            },
        );
    }
    let manifest = ReleaseManifest {
        release: release.release.clone(),
        ordinal: release.ordinal,
        sealed: release.sealed,
        writes: release.writes.clone(),
        functions: functions.into_values().collect(),
    };
    let reference = stage(RELEASE_MANIFEST_KIND, &encode_body(&manifest))?;
    Ok((manifest, reference))
}

/// Stages the release the build holds under `ordinal` and returns its
/// manifest's reference; `None` when the build holds no such release.
///
/// # Errors
///
/// The first error `stage` returns.
///
/// # Panics
///
/// When the build's own releases do not decode: a defect of the build.
pub fn stage_held_release<E>(
    ordinal: u32,
    mut stage: impl FnMut(u32, &[u8]) -> Result<ChildRef, E>,
) -> Result<Option<ChildRef>, E> {
    #[expect(clippy::expect_used, reason = "the build's own data decodes")]
    let held = crate::held_releases().expect("the build's helper releases decode");
    held.retained
        .iter()
        .find(|release| release.ordinal == ordinal)
        .map(|release| stage_release(release, &mut stage).map(|(_, reference)| reference))
        .transpose()
}

/// Why a parked run's pinned release does not resume here.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ReleaseRefusal {
    /// The run pinned a manifest this build does not hold: a different
    /// release, or one of the same name with other contents.
    #[error("the run pins release manifest {pinned:?}, which this build does not hold")]
    ReleaseMismatch { pinned: ChildRef },
    /// The pinned manifest, or this worker, lacks a function the run needs.
    #[error("function {function} is not held by the pinned release or this worker")]
    MissingHelper { function: FunctionId },
    /// The manifest records a native implementation this worker lacks.
    #[error("function {function} needs a native implementation this worker lacks")]
    MissingNative { function: FunctionId },
}

/// The manifests of the releases a build retains, by reference: what a run
/// may pin and resume on.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HeldManifests {
    by_reference: BTreeMap<ChildRef, ReleaseManifest>,
    /// The release the tree builds: the one a new run pins.
    current: Option<ChildRef>,
}

impl HeldManifests {
    /// Stages every retained release of `held`.
    ///
    /// # Errors
    ///
    /// The first error `stage` returns.
    pub fn stage<E>(
        held: &HeldReleases,
        mut stage: impl FnMut(u32, &[u8]) -> Result<ChildRef, E>,
    ) -> Result<Self, E> {
        let mut manifests = Self::default();
        for release in &held.retained {
            let (manifest, reference) = stage_release(release, &mut stage)?;
            manifests.by_reference.insert(reference, manifest);
            manifests.current = Some(reference);
        }
        Ok(manifests)
    }

    /// The manifest a new run pins.
    #[must_use]
    pub fn current(&self) -> Option<ChildRef> {
        self.current
    }

    /// Whether a release the build retains holds `function`: a helper
    /// whose identity a pinned manifest answers for. A function no release
    /// holds is an embedding's own package's, which its registry answers
    /// for.
    #[must_use]
    pub fn releases(&self, function: &FunctionId) -> bool {
        self.by_reference
            .values()
            .any(|manifest| manifest.get(function).is_some())
    }

    #[must_use]
    pub fn get(&self, reference: &ChildRef) -> Option<&ReleaseManifest> {
        self.by_reference.get(reference)
    }

    /// Admits a run that pins `pinned` and calls `required`, on a worker
    /// whose registry answers `installed` with whether it holds a function
    /// and has its native implementation.
    ///
    /// # Errors
    ///
    /// [`ReleaseRefusal`]: the manifest is not held exactly, or a required
    /// function or its native implementation is missing.
    pub fn admit(
        &self,
        pinned: &ChildRef,
        required: &BTreeSet<FunctionId>,
        installed: impl Fn(&FunctionId) -> Option<bool>,
    ) -> Result<&ReleaseManifest, ReleaseRefusal> {
        let manifest = self
            .by_reference
            .get(pinned)
            .ok_or(ReleaseRefusal::ReleaseMismatch { pinned: *pinned })?;
        for function in required {
            let held = manifest
                .get(function)
                .ok_or(ReleaseRefusal::MissingHelper {
                    function: *function,
                })?;
            match installed(function) {
                None => {
                    return Err(ReleaseRefusal::MissingHelper {
                        function: *function,
                    });
                }
                Some(false) if held.fingerprint.is_some() => {
                    return Err(ReleaseRefusal::MissingNative {
                        function: *function,
                    });
                }
                Some(_) => {}
            }
        }
        Ok(manifest)
    }
}
