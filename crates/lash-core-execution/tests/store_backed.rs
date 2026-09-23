//! The crate's tests that need a concrete store or effect host, over a SQLite
//! memory backend (ADR 0102), under `kernel::` with their original module
//! paths and test names.
//!
//! They cannot stay in-crate: lash-sqlite-store depends on this crate, so a
//! `cfg(test)` module that used it would link a second copy of the kernel
//! whose traits are distinct from the ones under test. The crate root below
//! re-exports the kernel's root, its facade support and the `testing`-gated
//! internals seam, so a relocated test reaches `crate::X` exactly as it did
//! in-crate.

#![expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "relocated unit-test fixtures assert that their setup is valid; in-crate they sat in `cfg(test)` modules the lints exempt"
)]

pub use lash_core_execution::facade_support::*;
pub use lash_core_execution::testing::kernel_internals::*;
pub use lash_core_execution::*;

#[path = "store_backed/support.rs"]
mod support;

#[path = "store_backed/kernel/mod.rs"]
mod kernel;
