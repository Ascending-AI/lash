//! The lash kernel's edits.
//!
//! A host changes a document through a [`Draft`]. It writes a
//! [`Transaction`]: a list of typed [`Edit`]s whose payloads are kernel
//! forms and whose targets are [`lash_kernel_doc::Site`]s of the document it
//! read. The draft applies the transaction to a copy, has
//! `lash-kernel-check` admit the result, and either publishes it with a
//! [`lash_kernel_doc::Correspondence`] (which node of the old document is which node of the
//! new one) or refuses it with typed [`EditDiagnostic`]s and changes
//! nothing.
//!
//! Edits, transactions and correspondences are data: they serialise, and a
//! transaction means the same against any copy of its base document. The
//! rules are the `K-EDIT` rules of `docs/kernel/semantics.md`.
//!
//! [`authoring`] constructs those transactions from expression-valued effect
//! inputs mixed with ordinary edits. Admission remains on [`Draft::apply`].
//! The read a host edits against is `lash-kernel-view`'s, which needs no
//! editor.
//!
//! The crate depends on `lash-kernel-doc` and `lash-kernel-check` and on no
//! other lash crate. It links no dialect.

mod apply;
pub mod authoring;
mod draft;
mod edit;
mod extract;
mod refusal;
mod tree;

#[cfg(test)]
mod tests;

pub use draft::{Applied, Draft};
pub use edit::{Edit, Position, Transaction};
pub use extract::{Extraction, extract_entry};
pub use refusal::{EditDiagnostic, EditDiagnosticKind, EditRefusal, Location};
pub use tree::NodeClass;
