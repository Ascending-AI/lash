//! What running code requires of a node, and what a node offers
//! (FIG-6991).
//!
//! Code needs a native implementation for each function it reaches that
//! has no body and an interpreter for each kernel version it is written
//! in; a run's operation links need their provider and the codec that
//! reads their payload. A node offers each of those it holds for as long
//! as it is live, and names its own package set. Code runs on a node
//! exactly when every requirement is among the node's capabilities: a
//! parked root is claimed only by a node that meets its record, and a
//! package set's surface, whose requirements are natives and kernels alone
//! (a surface carries no links), is promoted to new cells once every live
//! serving node meets them. No credential or grant is ever advertised.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::name::FunctionId;
use crate::package_ids::PackageSetDigest;

/// What running some code requires: each set empty when it requires none
/// of that kind. The sets are sorted, so equal requirements spell equal
/// bytes.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionRequirements {
    /// The functions with no body: each needs the node's native.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub natives: BTreeSet<FunctionId>,
    /// The kernel versions, by number, the code is written in.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub kernels: BTreeSet<u32>,
    /// The provider keys the code's operation links name.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub providers: BTreeSet<String>,
    /// The codecs, as `(provider key, format version)`, that read the
    /// links' payloads.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub codecs: BTreeSet<(String, u32)>,
}

/// What one live node offers running code, as it advertises it for as
/// long as its lease is live: the same kinds as [`ExecutionRequirements`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeCapabilities {
    /// The functions with no body the node binds a native for.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub natives: BTreeSet<FunctionId>,
    /// The kernel versions, by number, the node interprets.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub kernels: BTreeSet<u32>,
    /// The provider keys registered on the node.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub providers: BTreeSet<String>,
    /// The codecs, as `(provider key, format version)`, the node reads.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub codecs: BTreeSet<(String, u32)>,
    /// The node's own package set: its stored surface is the node's
    /// candidate for new cells.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surface: Option<PackageSetDigest>,
}

impl NodeCapabilities {
    /// Add every capability `other` offers; this node's own package set
    /// stays, unless it names none.
    pub fn extend(&mut self, other: &Self) {
        self.natives.extend(other.natives.iter().copied());
        self.kernels.extend(other.kernels.iter().copied());
        self.providers.extend(other.providers.iter().cloned());
        self.codecs.extend(other.codecs.iter().cloned());
        if self.surface.is_none() {
            self.surface = other.surface;
        }
    }
}

impl ExecutionRequirements {
    /// Whether `node` offers every requirement.
    #[must_use]
    pub fn met_by(&self, node: &NodeCapabilities) -> bool {
        self.unmet_by(node).is_empty()
    }

    /// The requirements `node` does not offer: what a refusal names.
    #[must_use]
    pub fn unmet_by(&self, node: &NodeCapabilities) -> Self {
        fn missing<T: Ord + Clone>(required: &BTreeSet<T>, offered: &BTreeSet<T>) -> BTreeSet<T> {
            required.difference(offered).cloned().collect()
        }
        Self {
            natives: missing(&self.natives, &node.natives),
            kernels: missing(&self.kernels, &node.kernels),
            providers: missing(&self.providers, &node.providers),
            codecs: missing(&self.codecs, &node.codecs),
        }
    }

    /// Whether nothing is required.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.natives.is_empty()
            && self.kernels.is_empty()
            && self.providers.is_empty()
            && self.codecs.is_empty()
    }
}

impl std::fmt::Display for ExecutionRequirements {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut parts = Vec::new();
        parts.extend(self.natives.iter().map(|id| format!("native {id}")));
        parts.extend(
            self.kernels
                .iter()
                .map(|version| format!("kernel {version}")),
        );
        parts.extend(self.providers.iter().map(|key| format!("provider `{key}`")));
        parts.extend(
            self.codecs
                .iter()
                .map(|(key, version)| format!("codec `{key}`@{version}")),
        );
        if parts.is_empty() {
            f.write_str("nothing")
        } else {
            f.write_str(&parts.join(", "))
        }
    }
}

/// The record's canonical tokens, as an execution root holds it: four
/// sequences, each ascending, so equal requirements spell equal bytes and
/// a record whose sets are not sorted and distinct does not decode.
impl crate::graph_tokens::TokenCodec for ExecutionRequirements {
    fn encode(&self, out: &mut crate::graph_tokens::TokenWriter) {
        out.seq(4);
        self.natives.iter().copied().collect::<Vec<_>>().encode(out);
        self.kernels.iter().copied().collect::<Vec<_>>().encode(out);
        self.providers
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .encode(out);
        self.codecs
            .iter()
            .map(|(provider, version)| Codec(provider.clone(), *version))
            .collect::<Vec<_>>()
            .encode(out);
    }

    fn decode(
        input: &mut crate::graph_tokens::TokenReader<'_>,
    ) -> Result<Self, crate::graph_tokens::TokenError> {
        use crate::graph_tokens::check_ascending;
        input.expect_struct(4)?;
        let natives = Vec::<FunctionId>::decode(input)?;
        check_ascending(&natives)?;
        let kernels = Vec::<u32>::decode(input)?;
        check_ascending(&kernels)?;
        let providers = Vec::<String>::decode(input)?;
        check_ascending(&providers)?;
        let codecs: Vec<(String, u32)> = Vec::<Codec>::decode(input)?
            .into_iter()
            .map(|Codec(provider, version)| (provider, version))
            .collect();
        check_ascending(&codecs)?;
        Ok(Self {
            natives: natives.into_iter().collect(),
            kernels: kernels.into_iter().collect(),
            providers: providers.into_iter().collect(),
            codecs: codecs.into_iter().collect(),
        })
    }
}

/// One codec requirement's tokens: its provider key and format version.
struct Codec(String, u32);

impl crate::graph_tokens::TokenCodec for Codec {
    fn encode(&self, out: &mut crate::graph_tokens::TokenWriter) {
        out.seq(2);
        out.text(&self.0);
        self.1.encode(out);
    }

    fn decode(
        input: &mut crate::graph_tokens::TokenReader<'_>,
    ) -> Result<Self, crate::graph_tokens::TokenError> {
        input.expect_struct(2)?;
        let provider = input.text()?.to_owned();
        Ok(Self(provider, u32::decode(input)?))
    }
}
