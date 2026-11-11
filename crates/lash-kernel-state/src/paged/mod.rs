//! The persistent semantic heap's page-level seams (FIG-6195): what the
//! heap, the machine, the accounting, the cell lifecycle and the embedder
//! agree on. See `docs/design/persistent-heap-seams.md`.
//!
//! Declarations only. Each trait names the layer that implements it; no
//! production code calls any of them before the cutover (FIG-6193).
//!
//! - [`source`] (S3): the synchronous embedder port pages are read and
//!   sealed through, and the semantic page kinds.
//! - [`root`] (S4): [`CheckpointRoot`] and the persistent execution records.
//! - [`family`] (S8): frozen values, lazy identity families and cell carry.

pub(crate) mod checkpoint;
pub use checkpoint::{Checkpoint, CheckpointHeader, FormatValue, StatedFormat, inspect_checkpoint};
pub mod family;
pub mod root;
pub mod run;
pub mod session;
pub mod source;

pub use family::{FrozenRef, FrozenSummary, SessionRoot};
pub use lash_kernel_doc::heap::{FieldName, NormalKey};
pub use root::{
    CellTargetKind, Charge, CheckpointRoot, EndedRecord, KeyedRoot, MemoryMeter, RootStatus,
    SavedLimits, SavedMeters, SequenceRoot,
};
pub use source::{PageAnswer, PageBody, PageKind, PagePortLimits, PageSource, SealBudget};

mod codec;
pub(crate) mod tree;

pub mod identity;
pub(crate) mod metadata;
pub use metadata::{NameRef, SiteRef, TaskIdentityRef};
pub(crate) mod payload;

#[cfg(test)]
mod laws {
    mod boundaries;
    mod identity;
    mod metadata;
    mod pages;
    pub mod root;
    mod run;
}
