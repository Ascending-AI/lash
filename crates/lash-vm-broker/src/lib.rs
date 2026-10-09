//! The parent side of lash's worker boundary (ADR 0123).
//!
//! Model code runs as a kernel machine in a worker process that holds guest
//! state only. The parent holds every grant, binding, route, admission and
//! ledger, and brokers every effect the run asks for. This crate is that
//! broker (kernel spec §9 rule 4: the machine parks, the embedder commits):
//!
//! - [`kernel`]: the set of pending effects, the park transaction, delivery
//!   in any order, the loop that drives a run through its parks, and
//!   number-exact effect values.
//! - [`snapshot`]: the durable store a run's saved states commit to.
//! - [`members`]: the admitted executions of a run's effects, run through
//!   the admitted-execution lifecycle.
//! - [`effects`]: what an admitted effect is to the durable engine.
//! - [`identity`]: the one derivation of a code command's `ToolCallId`.

pub mod effects;
pub mod identity;
pub mod kernel;
pub mod members;
pub mod snapshot;

pub use effects::{MemberDraft, ParentFault};
pub use identity::CodeCallIdentities;
pub use lash_core_execution::runtime::actor::round::ExecutionDraft;
pub use lash_core_execution::runtime::actor::waits::{PinnedKey, WaitRef, WaitSpec};
pub use lash_durable::DurableInstant;
pub use lash_durable::domain::{CellId, ExecKey, SnapshotRev};
pub use members::{Decide, Driven};
pub use snapshot::{DurableSnapshotStore, OperationId, QuietPointRefusal};
