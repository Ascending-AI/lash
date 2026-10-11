//! The lash kernel's executor interface.
//!
//! The kernel keeps one durable meaning: documents, function definitions,
//! content identities, statement sites and parked runs. Below it, every
//! executable form is a disposable derivative built against this crate:
//!
//! - the Boundary model: [`Boundary`], every step boundary of the machine
//!   with its static frame shape, and [`StepEdge`], where a statement's step
//!   leaves its frame, which the prepared form states for every statement;
//! - [`KirFunction`]: the executor IR, a flat CFG of semantic operations
//!   with an explicit [`Boundary`] at every step boundary of the machine;
//! - [`SEMANTICS`]: the charge events each op emits, in order;
//! - [`verify_semantics`] and [`verify_function`]: the coverage invariants.
//!
//! Each operation's meaning is written once, as the machine method the
//! interpreter executes (`lash-kernel-vm`); this crate holds types and
//! checks only, depends on the document model and no machine.

mod boundary;
pub mod fixtures;
mod kir;
mod sem;
mod verify;

pub use boundary::{Boundary, BoundaryKind, ControlRecipe, CtlStatic, StepEdge, TryPhaseStatic};
pub use kir::{
    ActionIx, ActionKind, BoundaryIx, CalleeRef, CodeIx, ConstIx, Departure, FieldIx, FieldListIx,
    KAction, KBlock, KBlockIx, KLoop, KTry, KirFunction, LibIx, LocalRef, LoopIx, LoopKind, MsgIx,
    NameIx, Op, OpKind, OpOrigin, OpRange, Operand, Origin, Reg, RegRange, Scope, ScopeIx, SlotAt,
    SlotInfo, StmtIx, TryIx, VarRef,
};
pub use sem::{ChargeEvent, OpSpec, SEMANTICS};
pub use verify::{At, CoverageError, KirError, verify_function, verify_semantics};
