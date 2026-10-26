//! Exact helper and native requirements of executable reach.
use crate::FunctionId;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
/// Exact helper manifests, ordinary natives and kernel versions required by selected executable reach.
pub struct NodeRequirements {
    /// Exact build-owned helper manifests, each discharging its native debt.
    pub libraries: BTreeSet<crate::HelperLibraryId>,
    pub natives: BTreeSet<FunctionId>,
    pub kernels: BTreeSet<u32>,
}

impl NodeRequirements {
    /// Union of selected linked reach. Extra unused Exports add no debt.
    pub fn extend(&mut self, other: &Self) {
        self.libraries.extend(other.libraries.iter().copied());
        self.natives.extend(other.natives.iter().copied());
        self.kernels.extend(other.kernels.iter().copied());
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.libraries.is_empty() && self.natives.is_empty() && self.kernels.is_empty()
    }
}

impl crate::tokens::TokenCodec for NodeRequirements {
    fn encode(&self, out: &mut crate::tokens::TokenWriter) {
        out.seq(3);
        self.libraries.encode(out);
        self.natives.encode(out);
        self.kernels.encode(out);
    }

    fn decode(
        input: &mut crate::tokens::TokenReader<'_>,
    ) -> Result<Self, crate::tokens::TokenError> {
        input.expect_struct(3)?;
        Ok(Self {
            libraries: BTreeSet::decode(input)?,
            natives: BTreeSet::decode(input)?,
            kernels: BTreeSet::decode(input)?,
        })
    }
}
