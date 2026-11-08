//! The core's instruction set (cutover contract §1): a stack-plus-locals
//! bytecode over the 16-byte [`lash_kernel_doc::Value`], run by one
//! `loop { match op }` dispatcher (`crate::vm`).
//!
//! Every op is eight bytes. An operand that does not fit names a side
//! table of its unit or of the [`Program`](super::program::Program).
//!
//! Charging is not an op's business: each fixed `K-CHG-002` event is billed
//! by exactly one op at that op's start, and the unit's block table says
//! which (contract §3). An op that bills a *dynamic* fee (a declared
//! `metered N`, a completion formula) says so in its own variant.

/// One instruction. Operand order on the stack is left to right: the last
/// operand evaluated is on top.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// A block's head: the block of that index in the unit's table starts
    /// here. The dispatcher prepays it, or enters it event by event.
    Block(u32),

    // --- Values and variables.
    /// Pushes constant `n` of the program's pool.
    Const(u32),
    /// Pushes local `n` of the frame.
    Local(u16),
    /// Pops into local `n`.
    SetLocal(u16),
    /// Pushes the value of the cell in local `n` (`K-CLO-001`).
    CellGet(u16),
    /// Pops into the cell in local `n`.
    CellSet(u16),
    /// Replaces local `n` with a new cell holding its value: a captured
    /// declaration, or a captured `for` binding's fresh cell for this
    /// iteration (`K-CLO-002`).
    FreshCell(u16),
    /// Pushes the value of the closure's capture `n`.
    CaptureGet(u16),
    /// Pops into the closure's capture `n`.
    CaptureSet(u16),
    /// Discards the top value.
    Pop,
    /// Duplicates the top value.
    Dup,

    // --- Control.
    Jump(u32),
    /// Pops a condition; jumps when it is false. A non-bool raises
    /// `type_error` (`K-EVAL-005`).
    JumpIfFalse(u32),

    // --- Construction (`K-VAL`), `n` operands from the stack.
    MakeTuple(u32),
    MakeList(u32),
    /// `n` key/value pairs, key first.
    MakeMap(u32),
    MakeSet(u32),
    /// Field names from record shape `n` of the unit.
    MakeRecord(u32),
    /// Closure body `n` of the program, capturing the unit's capture list
    /// for that closure.
    MakeClosure(u32),

    // --- Members.
    /// target, index → value.
    Index,
    /// target → value of field `n` of the pool.
    Field(u32),
    /// Writes member `n` of the unit, its operands in the order the
    /// document gave (`K-EVAL-003`).
    SetMember(u32),
    /// Removes a member (`K-FORM-001` `remove`).
    Remove(u32),

    // --- Calls.
    /// A native call: native `n` of the program, its arguments on the
    /// stack (`argc` in the native's table entry).
    CallNative(u32),
    /// A call of unit `n`, its arguments on the stack.
    CallUnit(u32),
    /// The callee below its `argc` arguments: a closure or a function
    /// value (`K-EVAL-004`).
    Apply(u8),
    /// Returns the top value from the frame.
    Return,
    /// Raises the top value (`K-ERR`).
    Raise,

    // --- Handlers.
    /// Installs handler `n` of the unit until the matching [`Op::TryExit`].
    TryEnter(u32),
    TryExit,
    /// Runs held cleanup disposition after a `finally` body ends.
    FinallyEnd(u32),

    // --- Loops.
    /// Opens iteration `n` of the unit over the popped iterable.
    ForOpen(u32),
    /// Loop `n`'s header: binds the next item, or jumps to its end.
    ForNext(u32),
    /// A declared `metered N` continuation test of loop `n`: a dynamic fee.
    Metered(u32),

    // --- Actions (`K-EVAL-008`): each ends its block.
    /// Action `n` of the unit: perform, spawn, join, cancel, yield, await.
    Action(u32),
    Print,
    Finish,
    Fail,

    /// A completed-only helper that open could not compile (contract §7).
    /// The validator proves no resume point reaches it.
    Unavailable(u32),
}

const _: () = assert!(std::mem::size_of::<Op>() == 8);
