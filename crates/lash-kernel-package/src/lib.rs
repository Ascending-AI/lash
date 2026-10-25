//! Fulfil signature-only document Imports into immutable, instance-local Links.
//! Publication validates provenance; Link/Package content hashes confer no authority.
mod build;
mod held;
mod labels;
pub use labels::read_labels;
mod refusal;
mod shape;
pub use build::{Namespace, ReachSelection};
pub use held::{Inspections, LinkedContent, LinkedReach, OperationNeed, RetainedContent};
pub use lash_vocabulary::names::{
    Brand, CallableId, Convention, ExportName, Import, ImportKind, ImportSlot, LinkedStart, Name,
    OperationUse, PackagePath, QualifiedName, Segment, StartSelector,
};
pub use lash_vocabulary::package::*;
pub use refusal::LinkRefusal;
pub use shape::{STALE_VALUE, check_branded};

#[cfg(test)]
mod laws;

mod materialize;
pub use materialize::{
    MaterializeRefusal, MaterializedWorkflow, UnrepresentableValue, materialize_workflow,
};
