//! Dialect declarations shared with hosts that do not link a source parser.
pub mod python;
pub mod typescript;

/// The source conventions understood by Lash's document authoring interface.
pub const ALL: &[lash_kernel_dialect::conventions::Conventions] =
    &[typescript::CONVENTIONS, python::CONVENTIONS];
