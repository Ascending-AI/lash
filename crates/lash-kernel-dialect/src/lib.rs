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

pub mod authoring;
pub mod builder;
pub mod construct;
pub mod conventions;
mod diagnostic;
mod dialect;
pub mod dispatch;
mod exports;
mod imperative;
mod library;
mod metadata;
mod name_plan;
mod namespaces;
mod normalization;
mod notes;
pub mod readability;
pub mod regions;
mod saved;
mod source;
mod spelling;
pub mod value_constructors;
pub mod workflow;
mod written;

#[cfg(test)]
mod names_laws;
#[cfg(test)]
mod namespaces_laws;
#[cfg(test)]
mod tests;

pub use diagnostic::{
    Diagnostic, DiagnosticKind, DiagnosticReason, DialectCode, SourceOperandType,
};
pub use dialect::{
    CaptionSupport, CellEnvironment, Dialect, EffectControl, Environment, FrontEnd, Lowered, Mode,
    Printer, ReadSource, ToolCalls,
};
pub use exports::{
    BRAND_FIELD, BUILT_FIELD, FunctionMember, NamespaceKind, NamespaceMember, NamespaceView,
    OperationSignature, TypeMember, ValueType, brand_tuple, brand_type,
};
pub use lash_kernel_doc::ImplicitTextConversion;
pub use lash_kernel_state::WaitId;
pub use library::{Library, LibraryError, NamedLibrary};
pub use metadata::{check_metadata_uniqueness, metadata_json};
pub use name_plan::NamePlan;
pub use {lash_kernel_value::NamespaceCollision, lash_kernel_value::NamespaceShadowing};

pub use namespaces::{
    Misspelled, NamespaceRefusal, Namespaces, Resolution, deferred_signature, operation_keys,
    operation_text, service_name, sleep_signature, spell_operation,
};
pub use notes::{Note, SiteLayers, SiteLayersRefusal, site_layers};
pub use saved::{
    Bundle, BundleBody, BundleRefusal, CaptureRefusal, DOCUMENT_NODE_KIND,
    FUNCTION_BUNDLE_NODE_KIND, Left, NotSaved, RelinkGap, Saved, Unusable, closure_of,
    relink_saved,
};
/// A saved function as the kernel holds it: its captures and token are
/// frozen heap values, which only the kernel crates read (FIG-6688). A
/// host carries one between the dialect and the machine, never into it.
#[doc(hidden)]
pub use saved::{HeldAt, MemberBody, SavedFunction, Token};
pub use source::{SourceError, define_function_sources, define_functions, define_functions_with};
pub use spelling::{SpellKind, Spelling};
pub use written::{CallParam, CallShape, Written, closure_site, function_captions, written_table};

pub use imperative::{
    GeneratedSourceLimits, PrintError, PrintOptions, PrintedSource, SourceFormat, SourceWriter,
    check_captions, check_document_depth,
};

/// Operand facts for authored source reconstruction.
#[doc(hidden)]
pub mod source_types;

/// What the machine crate builds a session cell's end on (FIG-6688): the
/// dialect's saving rules over the kernel's heap. Nothing outside
/// `crates/lash-kernel-*` names this module (scripts/check-kernel-boundary.py).
#[doc(hidden)]
pub mod kernel_internals {
    pub use crate::saved::save;
}
