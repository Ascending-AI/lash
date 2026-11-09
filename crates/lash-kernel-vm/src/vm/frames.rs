//! Frames, handlers and the operand stack (cutover contract §1, §5).
//!
//! A task's frames share one value stack: a frame's locals start at its
//! `base`, its operands above them. A local a closure shares holds its
//! cell as `Value::Handle`, a form no guest value reaches through a
//! local: only `CellGet`/`CellSet`/`Box` and capture reads touch it.

use lash_kernel_doc::Value;
use lash_kernel_doc::arena::ArenaHandle;

use crate::bytecode::program::UnitId;

/// One activation.
#[derive(Clone, Debug)]
pub(crate) struct Frame {
    pub unit: UnitId,
    /// The next op to run.
    pub pc: u32,
    /// Where the frame's locals start on the stack.
    pub base: u32,
    /// The closure the frame runs, whose captures it reads.
    pub closure: Option<ArenaHandle>,
    /// The completion formula the frame's call bills when it returns or
    /// raises (an outermost uncovered library call, `K-CHG-003`).
    pub bill: Option<Bill>,
}

/// A completion formula owed by a frame. The arguments it measures are
/// [`Context::billed`] from `from` up: frames leave in the order they
/// came, so a frame's bill is the top of that stack when it leaves.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Bill {
    pub charge: u32,
    pub from: u32,
}

/// An installed handler: a raise in frame `frame` or a frame above it
/// truncates the stack to `height`, pushes the raised value and continues
/// at `pc`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Handler {
    pub frame: u32,
    pub pc: u32,
    pub height: u32,
}

/// One task's execution state: what the scheduler swaps in and out.
#[derive(Clone, Debug, Default)]
pub(crate) struct Context {
    pub stack: Vec<Value>,
    pub frames: Vec<Frame>,
    pub handlers: Vec<Handler>,
    /// The arguments of the frames' bills, each bill's from its `from`.
    pub billed: Vec<Value>,
}

impl Context {
    /// Every value the context holds, for the collector.
    pub(crate) fn roots(&self, visit: &mut dyn FnMut(&Value)) {
        self.stack.iter().for_each(&mut *visit);
        self.billed.iter().for_each(&mut *visit);
        for frame in &self.frames {
            if let Some(closure) = frame.closure {
                visit(&Value::Closure(closure));
            }
        }
    }
}
