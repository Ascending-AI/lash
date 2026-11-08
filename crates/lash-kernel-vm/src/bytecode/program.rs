//! A compiled document (cutover contract §1): one [`Program`] per machine
//! instance, never cached across instances. It holds code for each unit
//! (main, every declared function, every kernel-code body, every closure
//! body), the constant pool, each block's charge and pace tables (§3) and
//! each unit's resume and role tables (§5).
//!
//! A `Program` is built by the compiler and checked once by the validator;
//! nothing at run time re-checks its structure.

use std::sync::Arc;

use lash_kernel_doc::Integer;
use lash_kernel_doc::Value;
use lash_kernel_doc::arena::native::{NativeFunction, NativeSpecialization};
use lash_kernel_value::names::FunctionId;

use super::op::Op;

/// Index of a unit in [`Program::units`].
pub type UnitId = u32;

/// One compiled document.
pub struct Program {
    pub units: Vec<Unit>,
    pub constants: Vec<Constant>,
    pub natives: Vec<NativeEntry>,
    /// The unit a `Main` start runs.
    pub main: UnitId,
}

/// What a constant is before an instance materializes it: an immediate is
/// already a value; an object constant is placed in the instance's heap,
/// admitted and counted when materialized (contract §2).
#[derive(Clone, Debug)]
pub enum Constant {
    Immediate(Value),
    LongText(Arc<str>),
    BigInt(Integer),
    Bytes(Arc<[u8]>),
    /// A field name read or written by `Field`/`SetMember`.
    Field(Arc<str>),
}

/// What a unit is, for frames and diagnostics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UnitKind {
    Main,
    /// A declared function of the document.
    Declared(FunctionId),
    /// A library function's kernel-code body.
    Body(FunctionId),
    /// A closure's body, by its own identity.
    Closure(FunctionId),
}

/// How a unit's own calls are charged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Metering {
    /// Guest code, closures and statement-metered bodies: statements and
    /// expressions bill their `K-CHG-002` events.
    Statements,
    /// A formula-metered body: covered (`K-CHG-007`), its outermost
    /// uncovered call bills formula `n` of the program at completion.
    Formula(u32),
}

/// One unit's code and tables.
pub struct Unit {
    pub kind: UnitKind,
    pub metering: Metering,
    pub params: u16,
    /// Locals, parameters first.
    pub locals: u16,
    /// The deepest operand stack the unit's code reaches, proved by the
    /// validator.
    pub max_stack: u16,
    pub ops: Vec<Op>,
    pub blocks: Vec<Block>,
    /// For each op, the fixed charge billed by the ops of its block up to
    /// and including it. With the block's cost it gives
    /// `rest(o) = cost - through[o]` (contract §3).
    pub through: Vec<u64>,
    /// For each op, the fixed charge its own start bills (`K-CHG-002`).
    pub event: Vec<u64>,
    pub handlers: Vec<Handler>,
}

/// A block (contract §3): its events are billed by its ops in source
/// order, and it ends at every step end, action, call, back edge and
/// handler boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Block {
    pub start: u32,
    pub end: u32,
    /// The sum of the block's fixed charge events, or `None` when it is
    /// not representable in `u64`: such a block always runs event by
    /// event.
    pub charge: Option<u64>,
    /// The block's pace units under `Charge::Off`: one per statement, loop
    /// continuation test and action atom of guest code and closures.
    pub pace: u64,
}

/// A handler a `TryEnter` installs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Handler {
    /// Where a caught raise continues, if the try has a catch.
    pub catch: Option<u32>,
    /// Where a `finally` body starts, if the try has one.
    pub finally: Option<u32>,
    /// The operand stack depth the handler restores.
    pub depth: u16,
}

/// A native a call site names: the implementation, its argument count and
/// the specialization it declared, resolved once at compile (R8).
pub struct NativeEntry {
    pub function: FunctionId,
    pub argc: u8,
    pub native: Arc<dyn NativeFunction>,
    pub specialization: NativeSpecialization,
    /// The definition's kernel body, for a Decline (K-LIB-006).
    pub body: Option<UnitId>,
}
