//! An in-process fake worker behind the protocol types, for the broker's
//! laws.
//!
//! The fake speaks real frames: it encodes and decodes every message with the
//! shared [`FrameCodec`](lash_vm_protocol::FrameCodec), fences what its parent
//! sends, and numbers what it sends. Its "VM" runs a [`ScriptedProgram`]
//! whose state it serializes into opaque state the parent never reads. A
//! [`Fault`] planned for a checkout kills the worker at a chosen point, or
//! makes it send what a broken or stale worker would.

mod checkpoints;
mod pool;
mod worker;

pub use checkpoints::MemoryCheckpoints;
pub use pool::{FakeWorkerPool, PoolStats};
pub use worker::{FAKE_STATE_FORMAT, FAKE_VM_CONTRACT, Fault, ScriptedProgram, Step};

/// The execution a test parent admits `operation` as: its command's call
/// under `tool`, with `policy` and a generous limit from `now_ms`.
pub fn operation_draft(
    context: &crate::AdmittedContext,
    operation: &crate::AdmittedOperation,
    tool: &str,
    policy: lash_sansio::ExecutionPolicy,
    now_ms: u64,
) -> lash_core_execution::runtime::actor::round::ExecutionDraft {
    crate::effects::operation_draft(
        operation.command_id(context),
        lash_sansio::ToolId::new(tool),
        operation
            .request
            .as_ref()
            .unwrap_or(&lash_vm_protocol::EncodedPayload(Vec::new())),
        context.identities.opener(),
        policy,
        lash_sansio::ExecutionLimit::starting_at(
            now_ms,
            std::time::Duration::from_secs(3600),
            std::time::Duration::from_secs(3600),
        ),
    )
    .unwrap_or_else(|_| unreachable!("a blake3 digest is 64 lowercase hex digits"))
}
