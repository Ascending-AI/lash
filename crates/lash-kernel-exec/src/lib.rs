//! The lash kernel's executor interface.
//!
//! In Lash, recovery loads committed state; uncommitted computation and diagnostics may
//! repeat. Committed effect outcomes do not re-execute; unfinished calls follow
//! their execution policy. Hosts deduplicate external effects on `call_id`.
//! See [What survives a crash](https://github.com/Ascending-AI/lash/blob/main/README.md#what-survives-a-crash)
//! for the storage assumption, `Once`/`Repeatable` rules and model resend windows.
//!
//! The kernel keeps one durable meaning: documents, function definitions,
//! content identities, statement sites and parked runs. Below it, the
//! executable form the machine runs is a disposable derivative built
//! against this crate:
//!
//! - the Boundary model: [`BoundaryKind`], every step boundary of the
//!   machine, and [`StepEdge`], where a statement's step leaves its frame,
//!   which the prepared form states for every statement;
//! - [`Op`]: the executor IR, a flat stream in which every statement is one
//!   range of semantic operations from one Boundary to the next;
//! - [`SEMANTICS`]: the charge events each op emits, in order;
//! - [`verify_semantics`] and [`verify_stream`]: the coverage invariants.
//!
//! Each operation's meaning is written once, as the machine's
//! (`lash-kernel-vm`); this crate holds types and checks only, and depends
//! on no machine.

mod boundary;
mod kir;
mod sem;
mod verify;

pub use boundary::{BoundaryKind, StepEdge};
pub use kir::{
    BlockIx, CodeIx, ConstIx, FieldIx, FieldListIx, LibIx, LocalRef, NameIx, Op, OpKind, OpRange,
    Operand, Reg, RegRange,
};
pub use sem::{ChargeEvent, OpSpec, SEMANTICS};
pub use verify::{CoverageError, KirError, StmtRanges, Tables, verify_semantics, verify_stream};
