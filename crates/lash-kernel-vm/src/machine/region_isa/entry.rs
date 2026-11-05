//! The actual acquisition state is separate from immutable verified code.
#[cfg(test)]
use super::Code;
use super::{CodeId, code::Register};
use lash_kernel_doc::Value;

pub(super) struct Entry {
    pub(super) program: CodeId,
    pub(super) invocation: u64,
    pub(super) pc: u32,
    pub(super) prefix: u32,
    pub(super) occurrence: Option<u64>,
    pub(super) values: Box<[(Register, Value)]>,
    pub(super) source_counts: Box<[u64]>,
}
impl Entry {
    #[cfg(test)]
    pub(super) fn fixture(code: &Code) -> Self {
        Self {
            program: code.id,
            invocation: code.invocation,
            pc: code.entry,
            prefix: code.entry_prefix,
            occurrence: None,
            values: code.initial.clone(),
            source_counts: code.source_initial.clone(),
        }
    }
}
