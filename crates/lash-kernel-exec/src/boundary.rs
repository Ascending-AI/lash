//! The Boundary model: every step boundary of the machine.
//!
//! A step is one statement's run, one resume of an action statement, or a
//! zero-charge step at a block's end, which also takes a loop's next
//! iteration test. Slice 0 returns after every step, so the set of
//! Boundaries is observable (`K-MACH-002`, `K-MACH-005`). Every Boundary is
//! a potential saveable exit, where the frame is canonical: its control
//! stack and slots, and no register.
//!
//! In the flat stream a statement's range starts at a
//! [`BoundaryKind::StatementStart`], and the op after its [`crate::Op::Act`]
//! is its [`BoundaryKind::CallReturn`] or [`BoundaryKind::WaitResume`].
//! [`BoundaryKind::BlockEnd`] lies between ranges: the machine's control
//! stack takes it.

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BoundaryKind {
    /// A statement is next.
    StatementStart,
    /// A kernel block has run its last statement: what encloses it goes
    /// on, a loop by testing its continuation.
    BlockEnd,
    /// A call statement's callee has returned.
    CallReturn,
    /// A waiting action statement (perform, sleep, join) has its outcome.
    WaitResume,
}

impl BoundaryKind {
    /// Whether the frame waits on an action statement here.
    pub const fn awaits(self) -> bool {
        matches!(self, Self::CallReturn | Self::WaitResume)
    }
}

/// Where a statement's step leaves its frame when the statement completes
/// in its own block: no block, loop, `try` or frame entered, and no wait.
///
/// The prepared form states one per statement, so no tier re-derives it
/// from the frame at run time. Leaving a kernel block ends its variables and
/// runs and charges nothing, so the statement's step takes it rather than a
/// step of its own, while that block sits inside another block of the same
/// frame: the block of the last statement of an `if` arm is left with it,
/// and so is each enclosing arm whose `if` was the last statement of its
/// block. A loop's, a `try`'s or a function's body ends in a
/// [`BoundaryKind::BlockEnd`] step of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct StepEdge {
    /// How many blocks the step leaves, innermost first.
    pub leaves: u32,
    /// The Boundary the frame is at after the step:
    /// [`BoundaryKind::StatementStart`] when a statement is next in the block
    /// it is left in, [`BoundaryKind::BlockEnd`] when that block has run its
    /// last.
    pub next: BoundaryKind,
}
