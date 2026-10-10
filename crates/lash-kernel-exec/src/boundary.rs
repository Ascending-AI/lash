//! The Boundary model: every step boundary of the machine, with the frame
//! shape the interpreter holds there.
//!
//! A step is one statement's run, one resume of an action statement, or a
//! zero-charge step at a block's end. Slice 0 returns after every step, so
//! the set of Boundaries is observable, and every tier stops at exactly the
//! same ones (`K-MACH-002`, `K-MACH-005`). Every Boundary is a potential
//! saveable exit: a compiled exit there materializes the canonical frame
//! from its [`crate::FrameMap`] and this static shape.

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

/// The frame's control stack at a Boundary, outermost first, as the
/// interpreter's `Control` entries without their dynamic parts. A loop's
/// cursor and `started` count and a `finally`'s pending departure come from
/// the exit's [`crate::FrameMap`].
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
