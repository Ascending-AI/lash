//! The released helper sets a build holds (FIG-5799): each as data, and
//! its index.
//!
//! A function's identity hashes its definition and, through a body, the
//! identities of the functions it calls, so a fixed helper is a new
//! function and every helper that calls it is one too. A run pins the
//! identities it was written against and resumes only on exactly those, so
//! each release holds the exact set of functions it ships: definitions,
//! never source, which would resolve its names against today's library.
//! A build holds the functions of the releases it retains beside its own,
//! and resolves names against its own alone: a release's set is an
//! identity catalog, never a second answer to a name.
//!
//! A release is data: every function a build of it holds, by identity, with
//! its definition and, for a native implementation, the digest of what the
//! code behind it answers. A sealed release is kept as it shipped
//! (`src/sealed.rs`); the build defines the release the tree builds until it
//! is sealed (`build.rs`, FIG-5839), and checks every sealed release against
//! what it builds.
//!
//! The build script includes this file too.

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
    /// Every function the release holds: `writes`, and the runnable
    /// functions of the sealed release before it, which a run may still pin.
    pub functions: BTreeSet<FunctionId>,
}

/// One released helper set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HelperRelease {
    pub release: String,
    pub ordinal: u32,
    /// The release has shipped: what it holds is fixed, and a build that
    /// cannot hold it exactly fails.
    #[serde(default, skip_serializing_if = "is_false")]
    pub sealed: bool,
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
    /// The digest of what its native implementation answers, as the build
    /// that defined the release found it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// The helper releases a build holds, as the build resolves them from its
/// declarations (`build.rs`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeldReleases {
    /// The releases the build retains, oldest first: the last is the
    /// release the tree builds.
    pub retained: Vec<HelperRelease>,
    /// The releases removed from the runnable union, kept for the startup
    /// retirement survey (FIG-5828).
    pub retiring: Vec<HelperRelease>,
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
    /// Reads a release from its JSON.
    ///
    /// # Errors
    ///
    /// The decoder's message.
    pub fn decode(text: &str) -> Result<Self, String> {
        decode(text)
    }

    /// Seals the release the tree builds, once, at the cut that ships it
    /// (`SEAL`).
    ///
    /// # Errors
    ///
    /// A release that has already shipped cannot be sealed again.
    pub fn seal(&mut self) -> Result<(), String> {
        if self.sealed {
            return Err(format!("helper release {} is already sealed", self.release));
        }
        self.sealed = true;
        Ok(())
    }

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
