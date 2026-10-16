//! Closed canvas authoring and atomic entry extraction.
//! Hosts apply base-addressed [`authoring::Operation`] batches through [`Draft`].
//! Source import owns changes outside that vocabulary. Tree writes are private.
mod apply;
pub mod authoring;
mod canvas;
mod draft;
mod edit;
mod extract;
mod refusal;
mod tree;

#[cfg(test)]
mod tests;

pub use draft::{Applied, Draft};
pub(crate) use edit::Edit;
pub use edit::{Position, Transaction};
pub use extract::{Extraction, extract_entry};
pub use refusal::{EditDiagnostic, EditDiagnosticKind, EditRefusal, Location};
pub use tree::NodeClass;
