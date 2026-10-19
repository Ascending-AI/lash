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
pub mod workflow;
mod written;

#[cfg(test)]
mod tests;

pub use diagnostic::{Diagnostic, DiagnosticKind};
pub use dialect::{
    CaptionSupport, CellEnvironment, EffectControl, Environment, FrontEnd, Lowered, Mode, Package,
    Printer, ReadSource,
};
pub use lash_kernel_state::WaitId;
pub use library::{Library, LibraryError, NamedLibrary};
pub use metadata::{check_metadata_uniqueness, metadata_json};
pub use name_plan::NamePlan;
pub use notes::{Note, SiteLayers, SiteLayersRefusal, site_layers};
pub use saved::{
    Bundle, BundleBody, BundleRefusal, CaptureRefusal, DOCUMENT_NODE_KIND,
    FUNCTION_BUNDLE_NODE_KIND, Kept, Left, NotSaved, Unusable, closure_of, install,
};
/// A saved function as the kernel holds it: its captures and token are
/// frozen heap values, which only the kernel crates read (FIG-6688). A
/// host carries one between the dialect and the machine, never into it.
#[doc(hidden)]
pub use saved::{InstalledFunction, MemberBody, SavedFunction, Token};
pub use source::{SourceError, define_functions};
pub use written::{CallParam, CallShape, Written, closure_site, written_table};

pub use imperative::{
    GeneratedSourceLimits, PrintError, PrintOptions, PrintedSource, SourceFormat, SourceWriter,
    check_captions, check_document_depth,
};

/// Operand facts used only by exact generated-source codecs.
#[doc(hidden)]
pub mod source_types;

/// What the machine crate builds a session cell's end on (FIG-6688): the
/// dialect's saving rules over the kernel's heap. Nothing outside
/// `crates/lash-kernel-*` names this module (scripts/check-kernel-boundary.py).
#[doc(hidden)]
pub mod kernel_internals {
    use lash_kernel_doc::heap::PageValue;
    use lash_kernel_state::kernel_internals::PagedHeap;
    use lash_kernel_state::paged::HeapWriteError;

    pub use crate::saved::save;

    /// The value every binding that held `function` is bound to.
    ///
    /// # Errors
    ///
    /// The heap's refusals.
    pub fn saved_value(
        function: &crate::SavedFunction,
        heap: &mut PagedHeap,
    ) -> Result<PageValue, HeapWriteError> {
        function.value(heap)
    }
}
