//! Authenticated, ephemeral source edits. Only publication writes artifacts.
use crate::process::ProcessDefinitionId;
use crate::workflow::document::CaptionTarget;

#[derive(Clone, Debug)]
pub struct WorkflowEdit {
    pub(crate) base: ProcessDefinitionId,
    pub(crate) code: lash_vm_runtime::process::InlineRequest,
    pub(crate) replacement: Replacement,
    pub(crate) source: String,
    pub(crate) format: crate::dialect::SourceFormat,
    pub(crate) dropped: Vec<CaptionTarget>,
}
#[derive(Clone, Debug)]
pub(crate) enum Replacement {
    Callable(Box<crate::dialect::CallableSourceRead>),
    Linked {
        document: crate::workflow::document::DocumentBytes,
        captions: crate::workflow::document::Captions,
    },
}
impl WorkflowEdit {
    pub fn base_definition(&self) -> &ProcessDefinitionId {
        &self.base
    }
    pub fn dropped_captions(&self) -> &[CaptionTarget] {
        &self.dropped
    }
}
