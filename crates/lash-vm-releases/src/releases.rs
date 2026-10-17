//! The current build's helper definitions and identity index.
//! Included by the build script too.

use std::collections::BTreeSet;

use lash_kernel_doc::{FunctionDefinition, FunctionId};
use serde::{Deserialize, Serialize};

/// What a build keeps of one released helper set: which release it is, the
/// functions a build of it resolves names against, and every function it
/// holds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HelperReleaseIndex {
    /// The release's name, such as `1.0`.
    pub release: String,
    /// The release's place in the line of helper releases: the version of
    /// the `kernel-helpers` format surface a state written by a build of it
    /// holds, the first being 1.
    pub ordinal: u32,
    /// The functions a build of the release resolves a name against: one
    /// per name.
    pub writes: BTreeSet<FunctionId>,
    /// Every function the current release holds.
    pub functions: BTreeSet<FunctionId>,
}

/// One released helper set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HelperRelease {
    pub release: String,
    pub ordinal: u32,
    /// The functions a build of the release resolves names against.
    pub writes: BTreeSet<FunctionId>,
    /// Every function the release holds, each after the functions its body
    /// calls.
    pub functions: Vec<ReleasedFunction>,
}

/// A function of a released set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleasedFunction {
    pub function: FunctionId,
    pub definition: FunctionDefinition,
}

/// The current build's helper release (`build.rs`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeldReleases {
    /// Exactly one release, defined by the current build.
    pub retained: Vec<HelperRelease>,
}

impl HeldReleases {
    /// Reads the releases from their JSON.
    ///
    /// # Errors
    ///
    /// The decoder's message.
    pub fn decode(text: &str) -> Result<Self, String> {
        decode(text)
    }
}

impl HelperRelease {
    /// What a build keeps of the release.
    pub fn index(&self) -> HelperReleaseIndex {
        HelperReleaseIndex {
            release: self.release.clone(),
            ordinal: self.ordinal,
            writes: self.writes.clone(),
            functions: self
                .functions
                .iter()
                .map(|function| function.function)
                .collect(),
        }
    }
}

/// Reads `text` as JSON. A body nests as deep as its definition does.
fn decode<T: for<'de> Deserialize<'de>>(text: &str) -> Result<T, String> {
    let mut decoder = serde_json::Deserializer::from_str(text);
    decoder.disable_recursion_limit();
    let value = T::deserialize(&mut decoder).map_err(|error| error.to_string())?;
    decoder.end().map_err(|error| error.to_string())?;
    Ok(value)
}
