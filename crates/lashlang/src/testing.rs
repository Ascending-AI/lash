//! Test-support surface for embedders and the durable artifact stores.
//!
//! Gated behind the `testing` feature (and always available under `cfg(test)`)
//! so it never ships in a production build.

pub mod conformance;

/// Pure AST constructors used by this crate's own unit tests.
///
/// Unavailable to embedders on purpose: it is `cfg(test)` only, not part of the
/// `testing` feature's published surface.
#[cfg(test)]
pub(crate) mod ast_builders;
