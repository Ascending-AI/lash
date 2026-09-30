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
