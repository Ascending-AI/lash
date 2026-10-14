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
pub mod readability;
mod saved;
mod source;

#[cfg(test)]
mod tests;

pub use diagnostic::{Diagnostic, DiagnosticKind, Span};
pub use dialect::{
    EffectControl, Environment, FrontEnd, FunctionValues, Lowered, Package, Printer,
};
pub use library::{Library, LibraryError, NamedLibrary};
pub use metadata::{check_metadata_uniqueness, metadata_json};
pub use name_plan::NamePlan;
pub use saved::{
    Bundle, BundleBody, BundleRefusal, CaptureRefusal, Constant, DOCUMENT_NODE_KIND,
    FUNCTION_BUNDLE_NODE_KIND, Kept, Left, MemberBody, NotSaved, SavedFunction, Token, Unusable,
    WRITTEN, Written, closure_of, function_reference, install, save, token_of,
};
pub use source::{SourceError, define_functions};

pub use imperative::{
    GeneratedSourceLimits, PrintError, PrintOptions, PrintedSource, SourceFormat, SourceMap,
    SourceSite, SourceWriter, check_document_depth,
};

/// Operand facts used only by exact generated-source codecs.
#[doc(hidden)]
pub mod source_types;
