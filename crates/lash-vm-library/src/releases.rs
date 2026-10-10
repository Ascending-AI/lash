//! The released helper sets a build holds (FIG-5799): each as the
//! repository keeps it, and its index.
//!
//! A function's identity hashes its definition and, through a body, the
//! identities of the functions it calls, so a fixed helper is a new
//! function and every helper that calls it is one too. A run pins the
//! identities it was written against and resumes only on exactly those, so
//! each release freezes the exact set of functions it ships: definitions,
//! never source, which would resolve its names against today's library.
//! A build holds the functions of the releases it retains beside its own,
//! and resolves names against its own alone: a release's set is an
//! identity catalog, never a second answer to a name.
//!
//! A release is frozen as data: every function a build of it holds, by
//! identity, with its definition and, for a native implementation, the
//! digest of what the code behind it answers. The worker's build checks
//! each release against what it builds (`lash-vm-worker`'s `build.rs`).

use std::collections::BTreeSet;
use std::fmt::Write as _;

use lash_kernel_doc::{FunctionDefinition, FunctionId};
use serde::{Deserialize, Serialize};

/// The generator that freezes the release the tree builds again.
pub const FREEZE: &str = "kiln test //crates/lash-vm-worker:lash-vm-worker__unit_test --local-test-execution --no-test-cache --test_arg=--ignored --test_arg=--exact --test_arg=embedding::release_tests::regenerate_helper_release --test_env=LASH_REGENERATE=1 --test_env=BUILD_WORKSPACE_DIRECTORY=$PWD";

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
    /// Every function the release holds: `writes`, and the functions of
    /// earlier freezes of it a run may still pin.
    pub functions: BTreeSet<FunctionId>,
}

/// One released helper set, as `src/generated/` keeps it.
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
    /// The digest of what its native implementation answered when the
    /// release was frozen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl HelperRelease {
    /// Reads a release as `src/generated/` keeps it. A body nests as deep as
    /// its definition does.
    ///
    /// # Errors
    ///
    /// The decoder's message.
    pub fn decode(text: &str) -> Result<Self, String> {
        let mut decoder = serde_json::Deserializer::from_str(text);
        decoder.disable_recursion_limit();
        let release = Self::deserialize(&mut decoder).map_err(|error| error.to_string())?;
        decoder.end().map_err(|error| error.to_string())?;
        Ok(release)
    }

    /// The Rust source `src/generated/` keeps the release in: its JSON, one
    /// function to a line, as a string constant.
    ///
    /// # Errors
    ///
    /// The encoder's message, which no definition raises.
    pub fn source(&self) -> Result<String, String> {
        let mut text = format!(
            "{{\"release\":{},\"ordinal\":{},",
            json(&self.release)?,
            self.ordinal
        );
        if self.sealed {
            text.push_str("\"sealed\":true,");
        }
        let _ = write!(text, "\"writes\":{},\"functions\":[", json(&self.writes)?);
        for (index, function) in self.functions.iter().enumerate() {
            text.push_str(if index == 0 { "\n" } else { ",\n" });
            text.push_str(&json(function)?);
        }
        text.push_str("\n]}\n");
        let mut hashes = String::from("#");
        while text.contains(&format!("\"{hashes}")) {
            hashes.push('#');
        }
        Ok(format!(
            "// @generated by `{FREEZE}`; do not edit.\n\
             //! Helper release {release}: every function a build of it holds (FIG-5799).\n\n\
             pub(crate) const RELEASE: &str = r{hashes}\"{text}\"{hashes};\n",
            release = self.release,
        ))
    }

    /// Seals the unshipped baseline once, fixing its exact artifact at the cut.
    ///
    /// # Errors
    /// A release that has already shipped cannot be sealed again.
    pub fn seal(&mut self) -> Result<(), String> {
        if self.sealed {
            return Err(format!("helper release {} is already sealed", self.release));
        }
        self.sealed = true;
        Ok(())
    }

    /// The generated artifact's file name, derived from this release's identity.
    ///
    /// # Errors
    /// A release name that is not dot-separated ASCII alphanumeric components.
    pub fn file_name(&self) -> Result<String, String> {
        if self
            .release
            .split('.')
            .any(|part| part.is_empty() || !part.bytes().all(|b| b.is_ascii_alphanumeric()))
        {
            return Err("invalid helper release file name".into());
        }
        Ok(format!("helpers_{}.rs", self.release.replace('.', "_")))
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

fn json(value: &impl Serialize) -> Result<String, String> {
    serde_json::to_string(value).map_err(|error| error.to_string())
}
