//! How a machine feeds what observes it, and how an embedder asks it to
//! observe. Nothing here runs unless an embedder asked: each observation
//! point is one branch on whether anything observes.

use std::sync::Arc;

use lash_kernel_doc::TaskId;

use super::{Control, Frame, KernelMachine};
use crate::compile::{Executable, LibId};
use crate::observe::{
    Counting, Event, GuestContext, GuestFrame, GuestPhase, GuestSymbols, NO_STATEMENT, Observer,
    WorkReport,
};

impl KernelMachine {
    /// Counts the run's work by the function that does it, from here on:
    /// calls, statements and expressions, self and inclusive charge,
    /// native calls and heap insertions. [`Self::work`] reads the counts.
    /// Counting changes no result, charge, park or export.
    pub fn count_work(&mut self) {
        let counting = Counting::new(
            self.exe.code_count(),
            self.exe.lib_count(),
            self.charged,
            self.heap.next_ordinal(),
        );
        self.observer_mut().work = Some(counting);
    }

    /// The work counted since [`Self::count_work`], or nothing when the
    /// machine does not count.
    pub fn work(&self) -> Option<WorkReport> {
        let counting = self.observer.as_ref()?.work.as_ref()?;
        Some(WorkReport::of(
            counting,
            &GuestSymbols::of(&self.exe),
            self.charged,
            self.heap.next_ordinal(),
        ))
    }

    /// Publishes the running task's frames and the machine's phase to
    /// `context` from here on, for a sampler the embedder runs. The machine
    /// is the context's only writer until it is dropped or publishes to
    /// another; the context reads as inactive once it is dropped.
    pub fn publish_context(&mut self, context: Arc<GuestContext>) {
        self.observer_mut().publish(context);
    }

    /// What the tokens of this run's counts and context name.
    pub fn guest_symbols(&self) -> GuestSymbols {
        GuestSymbols::of(&self.exe)
    }

    fn observer_mut(&mut self) -> &mut Observer {
        self.observer
            .get_or_insert_with(|| Box::new(Observer::new()))
    }

    /// One branch where nothing observes; the rest is out of the way.
    #[inline(always)]
    pub(super) fn observe(&mut self, event: Event) {
        if self.observer.is_some() {
            self.observe_unlikely(event);
        }
    }

    #[cold]
    #[inline(never)]
    fn observe_unlikely(&mut self, event: Event) {
        if let Some(observer) = self.observer.as_deref_mut() {
            observer.on(event, self.charged, self.heap.next_ordinal());
        }
    }

    #[inline(always)]
    pub(super) fn observe_expression(&mut self) {
        self.observe_expressions(1);
    }

    /// `count` expressions or action operands are evaluated.
    #[inline(always)]
    pub(super) fn observe_expressions(&mut self, count: u32) {
        if self.observer.is_some() {
            self.observe_expressions_unlikely(count);
        }
    }

    #[cold]
    #[inline(never)]
    fn observe_expressions_unlikely(&mut self, count: u32) {
        if let Some(observer) = self.observer.as_deref_mut() {
            for _ in 0..count {
                observer.expression(self.charging);
            }
        }
    }

    /// Evaluates a formula of `lib`, observed as a phase of its own when
    /// `evaluates` says the closure evaluates one.
    #[inline(always)]
    pub(super) fn in_formula<T>(
        &mut self,
        lib: LibId,
        evaluates: bool,
        evaluate: impl FnOnce(&Self) -> T,
    ) -> T {
        if !evaluates || self.observer.is_none() {
            return evaluate(self);
        }
        self.observe(Event::FormulaEnter(lib));
        let value = evaluate(self);
        self.observe(Event::FormulaExit);
        value
    }

    /// The phase a `run` begins or ends in.
    pub(super) fn observe_phase(&mut self, phase: GuestPhase) {
        if let Some(context) = self
            .observer
            .as_deref_mut()
            .and_then(|observer| observer.context.as_mut())
        {
            context.settle(phase);
        }
    }

    /// A step of `task` begins: the counters follow its frames, and the
    /// context shows them when it showed another task's.
    #[cold]
    #[inline(never)]
    pub(super) fn observe_step(&mut self, task: TaskId, exe: &Executable) {
        let Some(observer) = self.observer.as_deref_mut() else {
            return;
        };
        let frames = self.tasks.cached_frames(task);
        if let Some(work) = &mut observer.work {
            work.flush(self.charged, self.heap.next_ordinal());
            work.step(
                task.0 as usize,
                frames.as_ref().map(|(len, known)| {
                    (
                        *len,
                        known.iter().map(|(index, frame)| (*index, frame.code.0)),
                    )
                }),
            );
        }
        if let Some(context) = &mut observer.context
            && context.task() != Some(task.0)
        {
            let known = frames
                .as_ref()
                .map_or(&[][..], |(_, known)| known.as_slice());
            context.republish(task.0, known.iter().map(|(_, frame)| token(exe, frame)));
        }
    }
}

/// A frame as a context shows it: its code, and the statement it waits in
/// or ran last.
fn token(exe: &Executable, frame: &Frame) -> GuestFrame {
    let running = || {
        frame
            .control
            .iter()
            .rev()
            .find_map(|control| match control {
                Control::Block { block, next } if *next > 0 => {
                    exe.block(*block).stmts.get(*next - 1).map(|stmt| stmt.0)
                }
                _ => None,
            })
    };
    GuestFrame {
        code: frame.code.0,
        statement: frame
            .awaiting
            .map(|stmt| stmt.0)
            .or_else(running)
            .unwrap_or(NO_STATEMENT),
    }
}
