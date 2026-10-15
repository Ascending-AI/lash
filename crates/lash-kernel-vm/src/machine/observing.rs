//! How a machine feeds what observes it, and how an embedder asks it to
//! observe. Nothing here runs unless an embedder asked: each observation
//! point is one branch on whether anything observes.

use std::sync::Arc;

use lash_kernel_doc::TaskId;

use super::{Control, Frame, KernelMachine};
use crate::compile::{Executable, LibId};
use crate::observe::{
    Counting, Event, GuestContext, GuestFrame, GuestPhase, GuestSymbols, NO_STATEMENT, Observer,
    StatementAt, WorkReport,
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
            self.heap.allocated(),
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
            self.heap.allocated(),
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

    /// Spike (FIG-6364): counts every statement start from here on.
    pub fn count_statements(&mut self) {
        self.observer_mut().counts = Some(Vec::new());
    }

    /// Spike (FIG-6364): records every statement start, in order, with
    /// its task, from here on.
    pub fn trace_statements(&mut self) {
        self.observer_mut().trace = Some(Vec::new());
    }

    /// Spike (FIG-6364): the document statements counted since
    /// [`Self::count_statements`], by coordinate. Library statements are
    /// dropped.
    pub fn statement_counts(&self) -> std::collections::BTreeMap<StatementAt, u64> {
        let (statements, _) = self.exe.document_statements();
        let mut out = std::collections::BTreeMap::new();
        if let Some(counts) = self.observer.as_ref().and_then(|o| o.counts.as_ref()) {
            for (id, count) in counts.iter().enumerate() {
                if *count == 0 {
                    continue;
                }
                if let Some((block, index)) = statements.get(&(id as u32)) {
                    *out.entry(StatementAt {
                        block: block.clone(),
                        index: *index,
                    })
                    .or_default() += count;
                }
            }
        }
        out
    }

    /// Spike (FIG-6364): the document statement starts recorded since
    /// [`Self::trace_statements`], in order, each with its task, and how
    /// many library statement starts were dropped.
    pub fn statement_trace(&self) -> (Vec<(u32, StatementAt)>, u64) {
        let (statements, _) = self.exe.document_statements();
        let mut out = Vec::new();
        let mut library = 0;
        if let Some(trace) = self.observer.as_ref().and_then(|o| o.trace.as_ref()) {
            for (task, id) in trace {
                match statements.get(id) {
                    Some((block, index)) => out.push((
                        *task,
                        StatementAt {
                            block: block.clone(),
                            index: *index,
                        },
                    )),
                    None => library += 1,
                }
            }
        }
        (out, library)
    }

    /// Spike (FIG-6364): every document block's site and length.
    pub fn document_blocks(&self) -> std::collections::BTreeMap<lash_kernel_doc::Site, u32> {
        self.exe.document_statements().1
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
            observer.on(event, self.charged, self.heap.allocated());
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
        let frames = self
            .tasks
            .get(task.0 as usize)
            .map_or(&[][..], |running| running.frames.as_slice());
        if let Some(work) = &mut observer.work {
            work.flush(self.charged, self.heap.allocated());
            work.step(task.0 as usize, frames.iter().map(|frame| frame.code.0));
        }
        if let Some(context) = &mut observer.context
            && context.task() != Some(task.0)
        {
            context.republish(task.0, frames.iter().map(|frame| token(exe, frame)));
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
