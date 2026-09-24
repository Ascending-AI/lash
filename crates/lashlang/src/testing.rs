//! Test-support surface for embedders and the durable artifact stores.
//!
//! Gated behind the `testing` feature (and always available under `cfg(test)`)
//! so it never ships in a production build.

pub mod conformance;

/// A VM host, a host catalog and compile/execute wrappers, so a test outside
/// this crate can run a program the way the crate's own unit tests do.
pub mod harness;

/// Pure AST constructors for building fixture programs.
///
/// ADR 0096 leaves the crate without a source front-end, so a test that used
/// to spell its fixture in Lashlang states the AST instead. The constructors
/// are shared rather than re-derived per crate, and stay behind the `testing`
/// feature so they never ship in a production build.
pub mod ast_builders;

/// The name of every heap object kind: the value kinds a durable session can
/// hold, one per variant of the heap's object enum, whose exhaustive match
/// names each. The snapshot round-trip law (FIG-3608) holds every kind to a
/// row, or to the stated reason no program builds one.
pub fn heap_object_kinds() -> &'static [&'static str] {
    &crate::runtime::HEAP_OBJECT_KINDS
}
