//! Core items whose only consumers anywhere in the workspace are test code.
//!
//! `facade_support` is the workspace's internal cross-crate seam, and
//! membership in it means some crate's *shipped* code needs the item
//! (FIG-1223). A test importing an item is not that claim, so these live here
//! instead: doc-hidden like the seam they came from, and behind the `testing`
//! feature that a consuming crate opts into.
//!
//! This is internal test plumbing, not a downstream-facing fixture — the
//! public `lash::testing` surface is a different promise and stays one.

pub use crate::attachments::{
    AttachmentProducer, AttachmentSourcePolicy, AttachmentSourcePolicyError,
    OpenAttachmentSourcePolicy,
};
pub use crate::plugin::{
    PluginOperationFailure, RuntimeServices, SessionObservedProcessOutcome,
    SessionObservedProcessReceipt,
};
pub use crate::runtime::UnavailableProcessService;
pub use lash_sansio::{SchemaDialect, ToolCatalogBuildInput, validate_tool_input};
