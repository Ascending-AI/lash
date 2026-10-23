//! Read/edit seam: drafts and transactions use typed documents; convert a
//! completed edit to `lash_kernel_doc::DocumentBytes` for host retention.
//!
//! Closed canvas authoring and atomic entry extraction.
//! Hosts apply base-addressed [`authoring::Operation`] batches through [`Draft`].
//! Typed whole-document replacement covers general authoring. Tree writes are private.
mod apply;
pub mod authoring;
mod canvas;
mod draft;
mod edit;
mod extract;
mod refusal;
mod relink;
mod tree;

#[cfg(test)]
mod tests;

pub use draft::{Applied, Draft};
pub(crate) use edit::Edit;
pub use edit::{Position, Transaction};
pub use extract::{Extraction, extract_entry};
pub use refusal::{EditDiagnostic, EditDiagnosticKind, EditRefusal, Location};

pub use tree::NodeClass;

mod binding;
