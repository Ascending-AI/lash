//! Exact helper and native requirements of executable reach.
use crate::FunctionId;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Authenticated first appearance and package coverage of one archived helper.
pub struct HelperGeneration {
    pub archive: crate::HelperLibraryId,
    pub generation: u32,
    pub package: crate::HelperLibraryId,
}

impl crate::tokens::TokenCodec for HelperGeneration {
    fn encode(&self, out: &mut crate::tokens::TokenWriter) {
        out.seq(3);
        self.archive.encode(out);
        self.generation.encode(out);
        self.package.encode(out);
    }
    fn decode(
        input: &mut crate::tokens::TokenReader<'_>,
    ) -> Result<Self, crate::tokens::TokenError> {
        input.expect_struct(3)?;
        Ok(Self {
            archive: crate::HelperLibraryId::decode(input)?,
            generation: u32::decode(input)?,
            package: crate::HelperLibraryId::decode(input)?,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(transparent)]
/// One generation floor per archive lineage, with exact code outside those archives.
pub struct HelperRequirements(std::sync::Arc<HelperRequirementData>);

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "HelperRequirements")]
struct HelperRequirementData {
    archives: BTreeMap<crate::HelperLibraryId, u32>,
    outside: BTreeMap<crate::HelperLibraryId, BTreeSet<FunctionId>>,
}

impl Default for HelperRequirements {
    fn default() -> Self {
        static EMPTY: std::sync::OnceLock<std::sync::Arc<HelperRequirementData>> =
            std::sync::OnceLock::new();
        Self(std::sync::Arc::clone(EMPTY.get_or_init(Default::default)))
    }
}

impl HelperRequirements {
    pub fn new(
        archives: BTreeMap<crate::HelperLibraryId, u32>,
        outside: BTreeMap<crate::HelperLibraryId, BTreeSet<FunctionId>>,
    ) -> Self {
        if archives.is_empty() && outside.is_empty() {
            Self::default()
        } else {
            Self(std::sync::Arc::new(HelperRequirementData {
                archives,
                outside,
            }))
        }
    }

    pub fn archives(&self) -> &BTreeMap<crate::HelperLibraryId, u32> {
        &self.0.archives
    }
    pub fn archives_mut(&mut self) -> &mut BTreeMap<crate::HelperLibraryId, u32> {
        &mut std::sync::Arc::make_mut(&mut self.0).archives
    }
    pub fn outside(&self) -> &BTreeMap<crate::HelperLibraryId, BTreeSet<FunctionId>> {
        &self.0.outside
    }
    pub fn outside_mut(&mut self) -> &mut BTreeMap<crate::HelperLibraryId, BTreeSet<FunctionId>> {
        &mut std::sync::Arc::make_mut(&mut self.0).outside
    }

    pub fn extend(&mut self, other: &Self) {
        if other.is_empty() || std::sync::Arc::ptr_eq(&self.0, &other.0) {
            return;
        }
        if self.is_empty() {
            self.0 = std::sync::Arc::clone(&other.0);
            return;
        }
        let data = std::sync::Arc::make_mut(&mut self.0);
        for (archive, generation) in &other.0.archives {
            data.archives
                .entry(*archive)
                .and_modify(|held| *held = (*held).max(*generation))
                .or_insert(*generation);
        }
        for (release, functions) in &other.0.outside {
            data.outside.entry(*release).or_default().extend(functions);
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.archives.is_empty() && self.0.outside.is_empty()
    }

    pub fn unmet_by(&self, node: &Self, libraries: &BTreeSet<crate::HelperLibraryId>) -> Self {
        Self::new(
            self.0
                .archives
                .iter()
                .filter_map(|(archive, generation)| {
                    (node
                        .0
                        .archives
                        .get(archive)
                        .is_none_or(|held| held < generation))
                    .then_some((*archive, *generation))
                })
                .collect(),
            self.0
                .outside
                .iter()
                .filter(|(release, _)| !libraries.contains(release))
                .map(|(release, functions)| (*release, functions.clone()))
                .collect(),
        )
    }
}

impl crate::tokens::TokenCodec for HelperRequirements {
    fn encode(&self, out: &mut crate::tokens::TokenWriter) {
        out.seq(2);
        self.0.archives.encode(out);
        self.0.outside.encode(out);
    }

    fn decode(
        input: &mut crate::tokens::TokenReader<'_>,
    ) -> Result<Self, crate::tokens::TokenError> {
        input.expect_struct(2)?;
        Ok(Self::new(
            BTreeMap::decode(input)?,
            BTreeMap::decode(input)?,
        ))
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
/// Exact helper manifests, ordinary natives and kernel versions required by selected executable reach.
pub struct NodeRequirements {
    /// Exact build-owned helper manifests, each discharging its native debt.
    pub libraries: BTreeSet<crate::HelperLibraryId>,
    /// Cached availability debt of reached helpers, including interpreted bodies.
    #[serde(default, skip_serializing_if = "HelperRequirements::is_empty")]
    pub helpers: HelperRequirements,
    pub natives: BTreeSet<FunctionId>,
    pub kernels: BTreeSet<u32>,
}

impl NodeRequirements {
    /// Union of selected linked reach. Extra unused Exports add no debt.
    pub fn extend(&mut self, other: &Self) {
        self.libraries.extend(other.libraries.iter().copied());
        self.helpers.extend(&other.helpers);
        self.natives.extend(other.natives.iter().copied());
        self.kernels.extend(other.kernels.iter().copied());
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.libraries.is_empty()
            && self.helpers.is_empty()
            && self.natives.is_empty()
            && self.kernels.is_empty()
    }
}

impl crate::tokens::TokenCodec for NodeRequirements {
    fn encode(&self, out: &mut crate::tokens::TokenWriter) {
        out.seq(4);
        self.libraries.encode(out);
        self.helpers.encode(out);
        self.natives.encode(out);
        self.kernels.encode(out);
    }

    fn decode(
        input: &mut crate::tokens::TokenReader<'_>,
    ) -> Result<Self, crate::tokens::TokenError> {
        input.expect_struct(4)?;
        Ok(Self {
            libraries: BTreeSet::decode(input)?,
            helpers: HelperRequirements::decode(input)?,
            natives: BTreeSet::decode(input)?,
            kernels: BTreeSet::decode(input)?,
        })
    }
}
