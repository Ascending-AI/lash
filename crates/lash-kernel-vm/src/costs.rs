//! The cost table (`K-CHG-002`), by kernel version.
//!
//! Only what a version prices differently from version 1 is a member: every
//! other entry of the table is written where it is charged.

use lash_kernel_doc::KernelVersion;

/// What a kernel version charges for the operations versions price apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Costs {
    /// One test of a loop's continuation.
    pub(crate) loop_test: u64,
}

impl Costs {
    pub(crate) const fn of(version: KernelVersion) -> Self {
        match version {
            KernelVersion::One => Self { loop_test: 1 },
            #[cfg(feature = "synthetic-next")]
            KernelVersion::SyntheticNext => Self { loop_test: 2 },
        }
    }

    /// The table of the version `document` states. A document a machine
    /// runs is validated, so its version is one this build interprets.
    pub(crate) fn of_document(document: &lash_kernel_doc::Document) -> Self {
        Self::of(KernelVersion::of(document.manifest.kernel).unwrap_or(KernelVersion::NEWEST))
    }
}
