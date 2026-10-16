//! What a kernel dialect is.
//!
//! A dialect is a package of three things (`docs/kernel/design.md` §4): a
//! [`FrontEnd`] that lowers source in its language to a kernel document, a
//! [`Printer`] that writes any document back as exact generated source, and the
//! library functions its front end emits calls to. This crate fixes those
//! shapes and nothing about any one language.
//!
//! A front end never names a library function by identity. It asks a
//! [`Library`] for the function a qualified name stands for, so the kernel
//! library and a dialect's helpers can change identity without the front
//! end changing. A dialect's helpers are written as kernel text whose `use`
//! lines carry names only; [`define_functions`] fills the identities in.

mod diagnostic;
mod dialect;
mod imperative;
mod library;
mod metadata;
mod name_plan;
mod notes;
pub mod readability;
mod saved;
mod source;
mod written;

#[cfg(test)]
mod tests;

pub use diagnostic::{Diagnostic, DiagnosticKind};
pub use dialect::{
    EffectControl, Environment, FrontEnd, FunctionValues, Lowered, Package, Printer,
};
/// The persistent heap saved functions are written to and read from.
pub use lash_kernel_state::{WaitId, paged};
pub use library::{Library, LibraryError, NamedLibrary};
pub use metadata::{check_metadata_uniqueness, metadata_json};
pub use name_plan::NamePlan;
pub use notes::{Note, site_layers};
pub use saved::{
    Bundle, BundleBody, BundleRefusal, CaptureRefusal, DOCUMENT_NODE_KIND,
    FUNCTION_BUNDLE_NODE_KIND, InstalledFunction, Kept, Left, MemberBody, NotSaved, SavedFunction,
    Token, Unusable, closure_of, declare_captures, function_reference, install, reached_by_entries,
    save, token_of,
};
pub use source::{SourceError, define_functions};
pub use written::{CallParam, CallShape, Written, closure_site, written_table};

pub use imperative::{
    GeneratedSourceLimits, PrintError, PrintOptions, PrintedSource, SourceFormat, SourceWriter,
    check_document_depth,
};

/// Operand facts used only by exact generated-source codecs.
#[doc(hidden)]
pub mod source_types;
