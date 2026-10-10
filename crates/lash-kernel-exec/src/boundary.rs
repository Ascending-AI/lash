//! The Boundary model: every step boundary of the machine, with the frame
//! shape the interpreter holds there.
//!
//! A step is one statement's run, one resume of an action statement, or a
//! zero-charge step at a block's end. Slice 0 returns after every step, so
//! the set of Boundaries is observable, and every tier stops at exactly the
//! same ones (`K-MACH-002`, `K-MACH-005`). Every Boundary is a potential
//! saveable exit: the frame there is canonical, with this static shape.

use lash_kernel_doc::Site;

use crate::kir::{LoopIx, ScopeIx, StmtIx, TryIx};

/// One step boundary of a [`crate::KirFunction`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Boundary {
    /// The pending statement's site, as a parked run records it.
    pub site: Site,
    pub kind: BoundaryKind,
    /// The frame's control stack here, derived statically.
    pub shape: ControlRecipe,
    /// The action statement the frame waits on: set exactly for
    /// [`BoundaryKind::CallReturn`] and [`BoundaryKind::WaitResume`].
    pub awaiting: Option<StmtIx>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BoundaryKind {
    /// A statement is next.
    StatementStart,
    /// A kernel block has run its last statement.
    BlockEnd,
    /// A loop's continuation is tested next.
    LoopTest,
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

/// The frame's control stack at a Boundary, outermost first, as the
/// interpreter's `Control` entries without their dynamic parts: a loop's
/// cursor and `started` count and a `finally`'s pending departure are the
/// frame's own.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ControlRecipe {
    pub frames: Box<[CtlStatic]>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CtlStatic {
    /// A kernel block, and the position of its next statement.
    Block {
        scope: ScopeIx,
        next: u32,
    },
    For {
        lp: LoopIx,
    },
    While {
        lp: LoopIx,
    },
    Try {
        t: TryIx,
        phase: TryPhaseStatic,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TryPhaseStatic {
    Body,
    Catch,
    /// The `finally` runs; the departure it resumes is dynamic.
    Finally,
}
