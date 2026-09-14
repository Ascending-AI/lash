//! Test-support surface for embedders and the durable artifact stores.
//!
//! Gated behind the `testing` feature (and always available under `cfg(test)`)
//! so it never ships in a production build.

pub mod conformance;

/// A VM host, a host catalog and compile/execute wrappers, so a test outside
/// this crate can run a program the way the crate's own unit tests do.
pub mod harness;

/// Pure AST constructors used by this crate's own unit tests.
///
/// Unavailable to embedders on purpose: it is `cfg(test)` only, not part of the
/// `testing` feature's published surface.
#[cfg(test)]
pub(crate) mod ast_builders;
