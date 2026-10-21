//! One pre-indexed parent image, frozen when holdings are installed.
use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::{Arc, OnceLock};

use lash_kernel_dialect::{MemberPath, PackagePath};
use lash_kernel_doc::{ArtifactDigest, FunctionRegistry};

use lash_vocabulary::legacy_names::FunctionName;
use lash_kernel_vm::PreparedLibrary;
use serde::{Deserialize, Serialize};

use crate::{HeldPackages, Package, PackageHeader, PackageRefusal, PackageSet};

#[derive(Serialize, Deserialize)]
struct Artifact {
    header: PackageHeader,
    positions: Vec<Range<u32>>,
    labels: Vec<(MemberPath, lash_helper_labels::MemberDisplay)>,
    definitions: Vec<(FunctionName, lash_helper_labels::MemberDisplay)>,
}
#[derive(Serialize, Deserialize)]
struct Holdings {
    artifacts: Vec<Artifact>,
    set: BTreeMap<PackagePath, ArtifactDigest>,
}

fn refuse(message: impl std::fmt::Display) -> PackageRefusal {
    PackageRefusal::Interpretation {
        message: message.to_string(),
    }
}

/// Freezes `held`, whose redeclared functions hold the natives of their
/// `roots`.
pub(crate) fn build(
    held: &HeldPackages,
    roots: &BTreeMap<lash_vocabulary::legacy_names::FunctionId, lash_vocabulary::legacy_names::FunctionId>,
) -> Result<Arc<[u8]>, PackageRefusal> {
    let registry = held.functions();
    let image = registry.to_image_with_census(&lash_kernel_vm::body_census);
    let library = PreparedLibrary::frame_index(registry);
    let positions = registry.frame_positions(roots).map_err(refuse)?;
    let artifacts = held
        .artifacts()
        .values()
        .map(|package| {
            let mut positions: Vec<u32> = package
                .code()
                .ids()
                .iter()
                .filter_map(|function| registry.position(function).map(|at| at as u32))
                .collect();
            positions.sort_unstable();
            positions.dedup();
            let mut ranges: Vec<Range<u32>> = Vec::new();
            for at in positions {
                if let Some(last) = ranges.last_mut()
                    && last.end == at
                {
                    last.end += 1;
                } else {
                    ranges.push(at..at + 1);
                }
            }
            Artifact {
                header: package.header(),
                positions: ranges,
                labels: package
                    .display()
                    .members
                    .iter()
                    .map(|(key, label)| (key.clone(), label.clone()))
                    .collect(),
                definitions: package
                    .display()
                    .definitions
                    .iter()
                    .map(|(key, label)| (key.clone(), label.clone()))
                    .collect(),
            }
        })
        .collect();
    let metadata = rmp_serde::to_vec_named(&Holdings {
        artifacts,
        set: held.set().members().clone(),
    })
    .map_err(refuse)?;
    let mut bytes = Vec::with_capacity(image.len() + library.len() + metadata.len() + 16);
    bytes.extend_from_slice(b"LPF1");
    for part in [&image, &library, &metadata, &positions] {
        let length = u32::try_from(part.len()).map_err(refuse)?;
        bytes.extend_from_slice(&length.to_le_bytes());
        bytes.extend_from_slice(part);
    }
    Ok(bytes.into())
}

fn sections(bytes: &'static [u8]) -> Result<[&'static [u8]; 4], PackageRefusal> {
    if bytes.get(..4) != Some(b"LPF1") {
        return Err(refuse("not a package frame"));
    }
    let mut at: usize = 4;
    let mut parts = [&[][..]; 4];
    for part in &mut parts {
        let word: [u8; 4] = bytes
            .get(at..at + 4)
            .ok_or_else(|| refuse("frame section length"))?
            .try_into()
            .map_err(refuse)?;
        at += 4;
        let end = at
            .checked_add(u32::from_le_bytes(word) as usize)
            .ok_or_else(|| refuse("frame section overflow"))?;
        *part = bytes
            .get(at..end)
            .ok_or_else(|| refuse("frame section is truncated"))?;
        at = end;
    }
    if at != bytes.len() {
        return Err(refuse("frame has trailing bytes"));
    }
    Ok(parts)
}

impl HeldPackages {
    /// The same immutable bytes for every pool and replacement worker. All
    /// merging, census work and encoding happened once during installation.
    pub fn frame(&self) -> Arc<[u8]> {
        self.frame.clone()
    }

    /// The registry image and library reservation, held for this node's life.
    pub fn frame_indexes(&self) -> Result<(&'static [u8], &'static [u8]), PackageRefusal> {
        let bytes = self.live_frame.get_or_init(|| {
            let owner: &'static Arc<[u8]> = Box::leak(Box::new(self.frame.clone()));
            owner.as_ref()
        });
        let [image, library, _, _] = sections(bytes)?;
        Ok((image, library))
    }

    /// Native and plan lookup positions, projected by the parent at install.
    pub fn frame_positions(&self) -> Result<&'static [u8], PackageRefusal> {
        self.frame_indexes()?;
        let bytes = self
            .live_frame
            .get()
            .ok_or_else(|| refuse("frame is not held"))?;
        Ok(sections(bytes)?[3])
    }

    /// Reconstructs the parent's already-admitted holdings over its one image.
    /// This is a trusted parent-to-worker frame, never a guest admission path.
    pub fn from_frame(frame: Arc<[u8]>) -> Result<Self, PackageRefusal> {
        let owner: &'static Arc<[u8]> = Box::leak(Box::new(frame.clone()));
        let bytes = owner.as_ref();
        let [image, _, metadata, _] = sections(bytes)?;
        let holdings: Holdings = rmp_serde::from_slice(metadata).map_err(refuse)?;
        let functions = Arc::new(FunctionRegistry::over_frame(image).map_err(refuse)?);
        let artifacts = holdings
            .artifacts
            .into_iter()
            .map(|artifact| {
                let ids = artifact
                    .positions
                    .into_iter()
                    .flatten()
                    .map(|at| {
                        functions
                            .function_at(at as usize)
                            .ok_or_else(|| refuse("artifact position outside the frame"))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let mut package = Package::from_registry(artifact.header, ids, functions.clone())?;
                package.display.members = artifact.labels.into_iter().collect();
                package.display.definitions = artifact.definitions.into_iter().collect();
                Ok((package.key(), Arc::new(package)))
            })
            .collect::<Result<BTreeMap<_, _>, PackageRefusal>>()?;
        let set = PackageSet::from_members(holdings.set);
        let surface = crate::held::surface(&artifacts, &set, &functions);
        Ok(Self {
            artifacts,
            set,
            surface,
            functions,
            frame,
            live_frame: OnceLock::from(bytes),
        })
    }
}
