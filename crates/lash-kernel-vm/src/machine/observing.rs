//! How a machine feeds what observes it, and how an embedder asks it to
//! observe. Nothing here runs unless an embedder asked: each observation
//! point is one branch on whether anything observes.

use std::sync::Arc;

use lash_kernel_doc::TaskId;

use super::{Control, Frame, KernelMachine};
use crate::compile::{BlockId, Executable, LibId};
use crate::observe::{
    Counting, Event, GuestContext, GuestFrame, GuestPhase, GuestSymbols, NO_STATEMENT, Observer,
    PathRecorder, WorkReport,
};
use crate::{Observe, RanDelta, StatementPath};

impl KernelMachine {
    /// The occurrence counter each live task holds for the action at
    /// `site`, summed: what an identity-bearing start there has advanced.
    #[cfg(test)]
    pub(crate) fn occurrences_at(
        &mut self,
        site: &lash_kernel_doc::Site,
    ) -> Result<u64, lash_kernel_doc::heap::HeapFault> {
        let key = lash_kernel_state::paged::metadata::metadata_key(site);
        let access = self.heap.access.clone();
        let mut total = 0;
        for index in 0..self.tasks.len() {
            if let Some(task) = self.tasks.get_mut(index)? {
                total += task.record.occurrences.get(&key, &access)?.unwrap_or(0);
            }
        }
        Ok(total)
    }

    /// Selects observation for this machine. A resumed machine starts Off
    /// with zero counts; the caller chooses the new run segment's option.
    pub fn set_observe(&mut self, observe: Observe) {
        match observe {
            Observe::Counts => self.ran.resize(self.exe.site_count, 0),
            Observe::Off => self.ran.clear(),
        }
    }

    /// Takes and resets starts since the previous take. Completed pure
    /// code and a waiting action's start are never counted again on resume.
    pub fn take_ran(&mut self) -> RanDelta {
        self.ran
            .iter_mut()
            .enumerate()
            .filter_map(|(ordinal, count)| {
                let count = std::mem::take(count);
                (count != 0).then_some((ordinal as u32, count))
            })
            .collect()
    }

    /// Flushes before a synchronous host read only when the host wants live
    /// frames. Otherwise the counts remain for the park or end frame.
    pub(super) fn report_ran(&mut self, host: &mut dyn crate::Host) {
        if !self.ran.is_empty() && host.observes_ran() {
            host.ran(&self.take_ran());
        }
    }

    #[inline(always)]
    pub(super) fn observe_empty_block(&mut self, task: TaskId, block: BlockId, exe: &Executable) {
        if !self.ran.is_empty()
            && let Some(ordinal) = exe.block_ordinal(block)
        {
            self.ran[ordinal as usize] += 1;
        }
        if self.observer.is_some() {
            self.trace_block(task, block, exe);
        }
    }

    #[cold]
    #[inline(never)]
    fn trace_block(&mut self, task: TaskId, block: BlockId, exe: &Executable) {
        let site = &exe.block(block).site;
        if let Some(path) = self.observer.as_deref_mut().and_then(|o| o.path.as_mut())
            && !matches!(site.unit, lash_kernel_doc::Unit::Library(_))
        {
            let (tasks, access) = (&mut self.tasks, &self.heap.access);
            path.push(
                task,
                || match tasks.get_mut(task.0 as usize)? {
                    Some(running) => running.identity.resolve(access),
                    None => Err(lash_kernel_doc::heap::HeapFault::Malformed(
                        "a path names no task".to_owned(),
                    )),
                },
                site.clone(),
            );
        }
    }

    /// Records every document statement start and empty-block entry from
    /// here on, in order, by task: the path a replay answers. Recording
    /// changes no result, charge, park or export, and it is never captured.
    pub fn trace_path(&mut self) {
        self.observer_mut().path = Some(PathRecorder::default());
    }

    /// Takes and resets the path recorded since the previous take. Its
    /// coordinates are the live block sites, independent of the compiled
    /// ordinals the counts use.
    ///
    /// # Errors
    ///
    /// The heap fault that kept a task's identity off the path.
    pub fn take_path(&mut self) -> Result<StatementPath, lash_kernel_doc::heap::HeapFault> {
        self.observer
            .as_mut()
            .and_then(|o| o.path.as_mut())
            .map_or_else(|| Ok(StatementPath::new()), PathRecorder::take)
    }

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
            if let Event::Statement { task, .. } = event
                && observer.path.is_some()
                && let Ok(Some(running)) = self.tasks.get_mut(task.0 as usize)
                && let Ok(Some(frame)) = running.frames.last_mut()
                && let Some(Control::Block { block, next }) = frame.control.last()
                && !matches!(
                    self.exe.block(*block).site.unit,
                    lash_kernel_doc::Unit::Library(_)
                )
            {
                // The path uses the live coordinate, independently of the
                // compiled ordinal lookup used by the accumulator.
                let site = self.exe.block(*block).site.child((*next - 1) as u32);
                let (identity, access) = (running.identity, &self.heap.access);
                if let Some(path) = &mut observer.path {
                    path.push(task, || identity.resolve(access), site);
                }
            }
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
