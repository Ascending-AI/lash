//! The released helper sets a build holds (FIG-5799), as the build script
//! indexes them for the worker and its parent.
//!
//! A function's identity hashes its definition and, through a body, the
//! identities of the functions it calls, so a fixed helper is a new
//! function and every helper that calls it is one too. A run pins the
//! identities it was written against and resumes only on exactly those, so
//! each release freezes the exact set of functions it ships: definitions,
//! never source, which would resolve its names against today's library.
//! A build holds the functions of the releases it retains beside its own
//! (`build.rs`), and resolves names against its own alone: a release's set
//! is an identity catalog, never a second answer to a name.
//!
//! The build script includes this file too.

use std::collections::BTreeSet;

use lash_kernel_doc::FunctionId;
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
    /// Every function the release holds: `writes`, and the functions of
    /// earlier freezes of it a run may still pin.
    pub functions: BTreeSet<FunctionId>,
}
