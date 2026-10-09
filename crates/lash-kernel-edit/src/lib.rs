//! The lash kernel's edits.
//!
//! A host changes a document through a [`Draft`]. It writes a
//! [`Transaction`]: a list of typed [`Edit`]s whose payloads are kernel
//! forms and whose targets are [`lash_kernel_doc::Site`]s of the document it
//! read. The draft applies the transaction to a copy, has
//! `lash-kernel-check` admit the result, and either publishes it with a
//! [`Correspondence`] (which node of the old document is which node of the
//! new one) or refuses it with typed [`EditDiagnostic`]s and changes
//! nothing. Annotations move with the node they are attached to.
//!
//! Edits, transactions and correspondences are data: they serialise, and a
//! transaction means the same against any copy of its base document. The
//! rules are the `K-EDIT` rules of `docs/kernel/semantics.md`.
//!
//! The crate depends on `lash-kernel-doc` and `lash-kernel-check` and on no
//! other lash crate. It links no dialect.

mod apply;
mod correspondence;
mod draft;
mod edit;
mod refusal;
mod tree;

#[cfg(test)]
mod tests;

pub use correspondence::{Correspondence, Survivor};
pub use draft::{Applied, Draft};
pub use edit::{Edit, Position, Transaction};
pub use refusal::{EditDiagnostic, EditDiagnosticKind, EditRefusal, Location};
pub use tree::NodeClass;
