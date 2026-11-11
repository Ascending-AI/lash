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
    /// The Link the frame runs as, by its index in the program's Links:
    /// its unit's own, or for library code, its caller's.
    pub link: u32,
    /// The next op to run.
    pub pc: u32,
    /// Where the frame's locals start on the stack.
    pub base: u32,
    /// The closure the frame runs, whose captures it reads.
    pub closure: Option<ArenaHandle>,
    /// The completion formula the frame's call bills when it returns or
    /// raises (an outermost uncovered library call, `K-CHG-003`).
    pub bill: Option<Bill>,
    /// The active calls at and below this frame (`K-BND-002`): its
    /// parent's, plus one when its unit is a call, so not for `main`'s
    /// body, a spawn's stub or a function value's trampoline.
    pub depth: u32,
}

/// A completion formula owed by a frame. The arguments it measures are
/// [`Context::billed`] from `from` up: frames leave in the order they
/// came, so a frame's bill is the top of that stack when it leaves.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Bill {
    pub charge: u32,
    pub from: u32,
}

/// A covered body's activation while its inline region runs in the top
/// frame (`bytecode::inline`): the body's frame in all but the frame. Its
/// locals start at `base`, its call owes `bill`, and its unit's frame is
/// admitted until it leaves.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Activation {
    /// The top frame's inline site whose region runs.
    pub site: u32,
    pub callee: UnitId,
    pub base: u32,
    pub bill: Option<Bill>,
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
    /// The activation of the inline region the top frame runs, if any. A
    /// region reaches no action, step or frame call, so a context never
    /// parks or switches with one.
    pub inline: Option<Activation>,
}

impl Context {
    /// The active calls the context's frames hold (`K-BND-002`).
    #[inline(always)]
    pub fn depth(&self) -> u32 {
        self.frames.last().map_or(0, |frame| frame.depth)
    }

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
