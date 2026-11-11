//! The core's instruction set (cutover contract §1): a stack-plus-locals
//! bytecode over the 16-byte [`lash_kernel_doc::Value`], run by one
//! `loop { match op }` dispatcher (`crate::vm`).
//!
//! Every op is eight bytes. An operand that does not fit names a side
//! table of its unit or of the [`Program`](super::program::Program).
//!
//! Charging is not an op's business: each fixed `K-CHG-002` event is billed
//! by exactly one op at that op's start, and the unit's event table says
//! how much (contract §3). An op that bills a *dynamic* fee (a completion
//! formula, a copy-out or decode) bills it itself.

/// One instruction. Operand order on the stack is left to right: the last
/// operand evaluated is on top.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// A block's head: the block of that index in the unit's table starts
    /// here. The dispatcher prepays it, or enters it event by event.
    Block(u32),
    /// Trace `n` of the unit (`trace`), in place of a block head's
    /// `Block`: on a prepaid, unobserved block, the head and the ops after
    /// it run as the trace's micro-ops; otherwise the head runs as its
    /// `Block`. Only the trace pass writes it, after fusion.
    Trace(u32),
    /// Does nothing; it carries events that precede a block boundary.
    Nop,

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
    /// Moves local `n`'s value into a new cell left in local `n`: a
    /// captured declaration's fresh variable, or a captured `for`
    /// binding's cell for this pass (`K-CLO-002`). Closures made earlier
    /// keep their cells.
    Box(u16),
    /// Pushes the value of the closure's capture `n`.
    CaptureGet(u16),
    /// Pops into the closure's capture `n`.
    CaptureSet(u16),
    /// Pushes host binding `n` of the program (`CellStart::host`).
    Global(u16),
    /// Pops into host binding `n`.
    SetGlobal(u16),
    /// Discards the top value.
    Pop,

    // --- Control.
    Jump(u32),
    /// Pops a condition; jumps when it is false. A non-bool raises
    /// `type_error` (`K-EVAL-005`).
    JumpIfFalse(u32),
    /// A `while` condition's test: as [`Op::JumpIfFalse`], with the
    /// `while` form's `type_error`.
    WhileFalse(u32),

    // --- Construction (`K-VAL`), `n` operands from the stack.
    MakeTuple(u32),
    MakeList(u32),
    /// `n` key/value pairs, key first.
    MakeMap(u32),
    MakeSet(u32),
    /// Field names from record shape `n` of the unit.
    MakeRecord(u32),
    /// Closure `n` of the unit: its body and capture sources.
    MakeClosure(u32),

    // --- Members. A field name is a text constant of the pool.
    /// target, index → value.
    Index,
    /// target → value of field (constant) `n`.
    Field(u32),
    /// value, target, index: writes an index (value first, `K-EVAL-003`).
    SetIndex,
    /// target, index, value: writes an index (place first).
    SetIndexPlaceFirst,
    /// value, target → writes field `n`.
    SetField(u32),
    /// target, value → writes field `n` (place first).
    SetFieldPlaceFirst(u32),
    /// target, index → removes the member.
    RemoveIndex,
    /// target → removes field `n`.
    RemoveField(u32),

    // --- Calls.
    /// Call site `n` of the unit: its callee and argument count. The
    /// arguments are on the stack; a value callee is below them.
    Call(u32),
    /// Returns the top value from the frame. Leaving the run, the value
    /// is copied out (`K-EFF-002`).
    Return,
    /// The body ends without a `return`: the frame returns null, and
    /// nothing is copied out of the run (`K-FORM-015`).
    Fall,
    /// Raises the top value (`K-ERR`).
    Raise,

    // --- Handlers.
    /// Installs a handler continuing at `pc` until the matching
    /// [`Op::TryExit`].
    TryEnter(u32),
    /// Removes the innermost handler.
    TryExit,

    // --- Loops. Loop `n` of the unit names its hidden locals.
    /// Pops the iterable into loop `n`'s state.
    ForOpen(u32),
    /// Loop `n`'s header: binds the next item, or jumps to its exit.
    ForNext(u32),
    /// A protocol loop opens what its `open` hook gave (`K-ITER-005`).
    ProtoOpen(u32),
    /// A protocol loop's step: a walked value gives its next item, a
    /// driver jumps to its `next` call.
    ProtoStep(u32),
    /// A checked map or set pushes `check`'s arguments, or jumps past.
    ProtoGuard(u32),
    /// `check`'s count is kept as what remains.
    ProtoChecked(u32),
    /// What `next` gave: `()` exhausts, `(x)` gives `x`.
    ProtoPull(u32),
    /// Ends the loop when the step gave nothing.
    ProtoMore(u32),

    // --- Actions (`K-EVAL-008`): each ends its block.
    /// Action `n` of the unit: perform, spawn, join, cancel, yield.
    Action(u32),
    /// A loop is entered: its pass counter, local `n`, is zero. Bills
    /// nothing.
    LoopStart(u16),
    /// A loop's pass begins: its counter, local `n`, counts it. Bills
    /// nothing.
    Pass(u16),
    Print,
    Finish,
    Fail,

    /// A completed-only helper that open could not compile (contract §7).
    /// The validator proves no resume point reaches it.
    Unavailable(u32),

    // --- Fusion (`fuse`).
    /// Fused run `n` of the unit: on the prepaid path, the run's ops at
    /// once; on the exact path, the run's first op alone. Only the fusion
    /// pass writes it, after validation.
    Fused(u32),

    // --- Inline regions (`inline`).
    /// Inline site `n` of the unit: the call its site names, its body run
    /// in the unit's region for it. Only the inline pass writes it.
    Inline(u32),
    /// The region of inline site `n` returns the top value: the body's
    /// activation completes, and the unit continues after the call.
    Leave(u32),
    /// The region leaves its guard path at the body's op `pc`: the body's
    /// frame is materialized there, standing over the region's locals and
    /// operands.
    Deopt(u32),
}

const _: () = assert!(std::mem::size_of::<Op>() == 8);
