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
pub use lash_kernel_value::package::*;
pub use refusal::LinkRefusal;
pub use shape::{STALE_VALUE, check_branded};
pub use {
    lash_kernel_value::names::Brand, lash_kernel_value::names::CallableId,
    lash_kernel_value::names::Convention, lash_kernel_value::names::ExportName,
    lash_kernel_value::names::Import, lash_kernel_value::names::ImportKind,
    lash_kernel_value::names::ImportSlot, lash_kernel_value::names::LinkedStart,
    lash_kernel_value::names::Name, lash_kernel_value::names::OperationUse,
    lash_kernel_value::names::PackagePath, lash_kernel_value::names::QualifiedName,
    lash_kernel_value::names::Segment, lash_kernel_value::names::StartSelector,
};

#[cfg(test)]
mod laws;

mod materialize;
pub use materialize::{
    MaterializeRefusal, MaterializedWorkflow, UnrepresentableValue, materialize_workflow,
};
