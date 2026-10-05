//! The process workflow journal vocabulary and its fixed output types.

use super::*;

pub(super) struct PublishTerminalStep;
impl crate::JournalStep for PublishTerminalStep {
    type Output = Result<bool, String>;
    const SURFACE: lash_core::store::SurfaceFormat =
        lash_core::surface_format!(crate::JOURNAL_LOGIC_EPOCH);
    const KIND: &'static str = "lash.process.terminal.published";
    fn instance(&self) -> String {
        String::new()
    }
}

pub(super) struct RecoverLostStep;
impl crate::JournalStep for RecoverLostStep {
    type Output = Result<SubstrateLostRecovery, String>;
    const SURFACE: lash_core::store::SurfaceFormat =
        lash_core::surface_format!(crate::JOURNAL_LOGIC_EPOCH);
    const KIND: &'static str = "lash.process.recover-lost";
    fn instance(&self) -> String {
        String::new()
    }
}

pub(super) struct CompleteProcessStep;
impl crate::JournalStep for CompleteProcessStep {
    type Output = Result<ProcessAwaitOutput, String>;
    const SURFACE: lash_core::store::SurfaceFormat =
        lash_core::surface_format!(crate::JOURNAL_LOGIC_EPOCH);
    const KIND: &'static str = "lash.process.complete";
    fn instance(&self) -> String {
        String::new()
    }
}

pub(super) struct ParentEndStep;
impl crate::JournalStep for ParentEndStep {
    type Output = Result<u32, String>;
    const SURFACE: lash_core::store::SurfaceFormat =
        lash_core::surface_format!(crate::JOURNAL_LOGIC_EPOCH);
    const KIND: &'static str = "lash.process.parent-end";
    fn instance(&self) -> String {
        String::new()
    }
}

pub(super) struct RecordCancelStep;
impl crate::JournalStep for RecordCancelStep {
    type Output = Result<(), String>;
    const SURFACE: lash_core::store::SurfaceFormat =
        lash_core::surface_format!(crate::JOURNAL_LOGIC_EPOCH);
    const KIND: &'static str = "lash.process.cancel.record";
    fn instance(&self) -> String {
        String::new()
    }
}

pub(super) struct ResumeSegmentStep;
impl crate::JournalStep for ResumeSegmentStep {
    type Output = Result<lash_core::SegmentHandover, SegmentFailure>;
    const SURFACE: lash_core::store::SurfaceFormat =
        lash_core::surface_format!(crate::JOURNAL_LOGIC_EPOCH);
    const KIND: &'static str = "lash.segment.resume";
    fn instance(&self) -> String {
        String::new()
    }
}

pub(super) struct BoundaryStep;
impl crate::JournalStep for BoundaryStep {
    type Output = Result<bool, String>;
    const SURFACE: lash_core::store::SurfaceFormat =
        lash_core::surface_format!(crate::JOURNAL_LOGIC_EPOCH);
    const KIND: &'static str = "lash.segment.boundary";
    fn instance(&self) -> String {
        String::new()
    }
}

pub(super) struct CancelChildTurnStep;
impl crate::JournalStep for CancelChildTurnStep {
    type Output = Result<(), String>;
    const SURFACE: lash_core::store::SurfaceFormat =
        lash_core::surface_format!(crate::JOURNAL_LOGIC_EPOCH);
    const KIND: &'static str = "lash.process.cancel.child-turn";
    fn instance(&self) -> String {
        String::new()
    }
}

pub(super) struct CancelRouteStep;
impl crate::JournalStep for CancelRouteStep {
    type Output = Result<Option<CancelTarget>, String>;
    const SURFACE: lash_core::store::SurfaceFormat =
        lash_core::surface_format!(crate::JOURNAL_LOGIC_EPOCH);
    const KIND: &'static str = "lash.process.cancel.route";
    fn instance(&self) -> String {
        String::new()
    }
}
