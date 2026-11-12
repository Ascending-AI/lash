//! How a raise leaves frames: unwinding to the innermost handler, a
//! run-root cleanup, or an uncaught end (`K-ERR-003`, `K-TASK-019`).

use lash_kernel_doc::Value;

use super::{Flow, Restart, Run, Stop, Vm};
use crate::heap::Heap;

impl<H: Heap> Vm<H> {
    /// Whether program global `n` is one no binding gave a value.
    #[inline]
    pub(super) fn unbound(&self, n: u16) -> bool {
        self.unbound.get(usize::from(n)).copied().unwrap_or(false)
    }

    /// The raise a read or write of unbound global `n` makes: a free name
    /// of `main` the environment does not provide (`K-FORM-003`,
    /// `K-ADM-004`).
    #[cold]
    #[inline(never)]
    pub(super) fn unbound_variable(&mut self, n: u16) -> Flow {
        let name = self.program.globals[usize::from(n)].clone();
        self.error(
            "unbound_variable",
            &format!("`{}` is not declared here", name.as_str()),
        )
    }

    /// Unwinds to the innermost handler of the running task: each frame
    /// left bills its completion formula with a result of size 0
    /// (`K-CHG-003`). Without a handler the task's raise is uncaught. A
    /// completion that reads a cold chunk resumes the unwind at its frame,
    /// the frames already left gone.
    pub(super) fn raise(&mut self, value: Value) -> Run<()> {
        self.raised_at = self.raise_site();
        self.unwind(value)
    }

    /// Unwinds `value` from where the frames stand, the raise's site
    /// already taken: a cold read's restart goes on here.
    pub(super) fn unwind(&mut self, value: Value) -> Run<()> {
        self.cancel();
        if let Some(activation) = self.cx.inline {
            self.unwind_inline(activation, value)?;
        }
        loop {
            let Some(top) = self.cx.frames.len().checked_sub(1) else {
                return Err(Flow::from(Stop::Uncaught(value)));
            };
            if let Some(handler) = self.cx.handlers.last().copied()
                && handler.frame as usize == top
            {
                self.cx.handlers.pop();
                self.cx.stack.truncate(handler.height as usize);
                self.cx.stack.push(value);
                if let Some(frame) = self.cx.frames.last_mut() {
                    frame.pc = handler.pc;
                }
                return Ok(());
            }
            if self.clean_up_raise(top, value) {
                return Ok(());
            }
            let bill = self.cx.frames[top].bill;
            let completion = match self.completion(bill, None) {
                Ok(completion) => completion,
                Err(flow) => {
                    if let Flow::Cold { .. } = flow {
                        self.restart = Some(Restart::Unwind(value));
                    }
                    return Err(flow);
                }
            };
            let Some(frame) = self.cx.frames.pop() else {
                return Err(Flow::from(Stop::Uncaught(value)));
            };
            self.heap.release(self.frame_cost(frame.unit));
            self.drop_handlers();
            self.settle(completion, frame.unit)?;
            self.cx.stack.truncate(frame.base as usize);
            if self.cx.frames.is_empty() {
                return Err(Flow::from(Stop::Uncaught(value)));
            }
        }
    }

    /// A raise leaving an entry's body from frame `top` goes on at the
    /// unit's run-root cleanup, the frame holding only the raised value
    /// (`K-TASK-019`): the run's root, or a call through an import, which
    /// enters the export as a start does. A raise of the cleanup itself
    /// leaves the entry.
    fn clean_up_raise(&mut self, top: usize, value: Value) -> bool {
        let Some(frame) = self.cx.frames.get_mut(top) else {
            return false;
        };
        let unit = self.program.unit(frame.unit);
        let Some(cleanup) = unit.cleanup.filter(|cleanup| frame.pc <= cleanup.body) else {
            return false;
        };
        frame.pc = cleanup.raised;
        let held = frame.base as usize + usize::from(unit.locals);
        self.cx.stack.truncate(held);
        self.cx.stack.push(value);
        true
    }

    /// Drops the handlers of frames that are gone: a `return` inside a
    /// `try` leaves its handlers with its frame.
    pub(super) fn drop_handlers(&mut self) {
        let frames = self.cx.frames.len();
        while self
            .cx
            .handlers
            .last()
            .is_some_and(|handler| handler.frame as usize >= frames)
        {
            self.cx.handlers.pop();
        }
    }
}
