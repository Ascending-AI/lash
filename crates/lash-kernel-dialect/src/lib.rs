//! What a kernel dialect is.
//!
//! A dialect is a package of three things (`docs/kernel/design.md` §4): a
//! [`FrontEnd`] that lowers source in its language to a kernel document, a
//! [`Printer`] that writes any document back as equivalent source, and the
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
mod saved;
mod source;

#[cfg(test)]
mod tests;

pub use diagnostic::{Diagnostic, DiagnosticKind, Span};
pub use dialect::{
    EffectControl, Environment, FrontEnd, FunctionValues, Lowered, Package, Printer,
};
pub use library::{Library, LibraryError, NamedLibrary};
pub use saved::{
    CaptureRefusal, Constant, Kept, Left, NotSaved, SavedFunction, Token, Unusable, WRITTEN,
    Written, closure_of, function_reference, install, save, token_of,
};
pub use source::{SourceError, define_functions};

pub use imperative::{
    SourceExpr, SourcePlace, SourceStmt, Spelling, imperative_source, render_source,
};
