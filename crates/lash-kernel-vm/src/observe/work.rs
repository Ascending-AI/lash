//! Exact counters of a run's work, by the function that did it.
//!
//! Charge and heap insertions are not counted where they happen: each is
//! read from the machine's own meters whenever the function being run
//! changes, and the difference goes to the function that ran in between.
//! The self charges therefore add up to the run's charge exactly, and the
//! counters add nothing to the paths that charge.

use std::collections::BTreeMap;

/// Whose work a stretch of a run is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Owner {
    /// No frame and no native call: the machine's own work between tasks.
    Nobody,
    /// A code: `main`, a declared function, a closure or a library body.
    Code(u32),
    /// A library function that runs without a frame: a native
    /// implementation or one of the machine's own.
    Lib(u32),
}

/// What a code did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CodeCounts {
    /// Frames pushed for it.
    pub calls: u64,
    /// Statements it ran.
    pub statements: u64,
    /// Expressions and action operands it evaluated.
    pub expressions: u64,
    /// Statements and expressions it ran while its forms were not charged:
    /// the body of a library function with a native implementation.
    pub uncharged_operations: u64,
    /// Charge while it was the innermost frame and no native call ran.
    pub self_charge: u64,
    /// Charge from a call's start to its end, callees and the natives it
    /// called included; a recursive call is counted once, by its outermost
    /// frame. Inclusive charges overlap: they never add up to a total.
    pub inclusive_charge: u64,
    /// Objects the heap took while it ran.
    pub heap_insertions: u64,
}

/// What a library function that runs without a frame did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NativeCounts {
    pub calls: u64,
    /// Calls its allocation-free fast path answered.
    pub fast_calls: u64,
    /// Calls charged nothing: the call did work the charge does not show.
    pub zero_charge_calls: u64,
    /// Calls made again after a collection, for room.
    pub retries: u64,
    /// The units its general calls counted on their work counter: a
    /// guarded function's guard units.
    pub guard_work: u64,
    /// Its formulas evaluated: charge formulas and guard limits.
    pub formula_evaluations: u64,
    pub self_charge: u64,
    pub heap_insertions: u64,
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    code: u32,
    /// The task's charge when the frame was pushed.
    at: u64,
    /// No frame below it in its task runs the same code.
    outermost: bool,
}

#[derive(Clone, Debug, Default)]
struct TaskStack {
    entries: Vec<Entry>,
    /// The charge the task has been attributed.
    charged: u64,
}

#[derive(Clone, Copy, Debug)]
struct OpenNative {
    lib: u32,
    task: usize,
    charged: u64,
}

/// The counters of one machine.
#[derive(Clone, Debug)]
pub(crate) struct Counting {
    pub(crate) codes: Vec<CodeCounts>,
    pub(crate) natives: Vec<NativeCounts>,
    /// Native calls by the function that made them and the native.
    pub(crate) native_calls: BTreeMap<(Owner, u32), u64>,
    pub(crate) unattributed_charge: u64,
    pub(crate) unattributed_insertions: u64,
    /// The meters when counting began.
    pub(crate) began: (u64, u64),
    tasks: Vec<TaskStack>,
    task: usize,
    open: Vec<OpenNative>,
    current: Owner,
    charged: u64,
    allocated: u64,
}

impl Counting {
    pub(crate) fn new(codes: usize, libs: usize, charged: u64, allocated: u64) -> Self {
        Self {
            codes: vec![CodeCounts::default(); codes],
            natives: vec![NativeCounts::default(); libs],
            native_calls: BTreeMap::new(),
            unattributed_charge: 0,
            unattributed_insertions: 0,
            began: (charged, allocated),
            tasks: Vec::new(),
            task: 0,
            open: Vec::new(),
            current: Owner::Nobody,
            charged,
            allocated,
        }
    }

    /// Gives what the meters moved since the last change of owner to the
    /// owner that ran.
    pub(crate) fn flush(&mut self, charged: u64, allocated: u64) {
        let charge = charged.saturating_sub(self.charged);
        let insertions = allocated.saturating_sub(self.allocated);
        self.charged = charged;
        self.allocated = allocated;
        match self.current {
            Owner::Nobody => {
                self.unattributed_charge += charge;
                self.unattributed_insertions += insertions;
            }
            Owner::Code(code) => {
                if let Some(counts) = self.codes.get_mut(code as usize) {
                    counts.self_charge += charge;
                    counts.heap_insertions += insertions;
                }
            }
            Owner::Lib(lib) => {
                if let Some(counts) = self.natives.get_mut(lib as usize) {
                    counts.self_charge += charge;
                    counts.heap_insertions += insertions;
                }
            }
        }
        self.stack(self.task).charged += charge;
    }

    fn stack(&mut self, task: usize) -> &mut TaskStack {
        if self.tasks.len() <= task {
            self.tasks.resize_with(task + 1, TaskStack::default);
        }
        &mut self.tasks[task]
    }

    /// The owner when the innermost thing running is `task`'s top frame,
    /// or an open native call.
    fn settle_owner(&mut self) {
        self.current = match self.open.last() {
            Some(open) => Owner::Lib(open.lib),
            None => self
                .tasks
                .get(self.task)
                .and_then(|stack| stack.entries.last())
                .map_or(Owner::Nobody, |entry| Owner::Code(entry.code)),
        };
    }

    /// A step of `task` begins: `frames` is how many frames it has and
    /// the codes of those the machine holds decoded, by index (`None`
    /// when it holds none of the task). Frames that ended without a
    /// return, as a task's do when it ends, are closed here, and a frame
    /// whose code changed is closed and opened again. A frame below the
    /// decoded ones keeps the code its call was counted under.
    pub(crate) fn step(
        &mut self,
        task: usize,
        frames: Option<(usize, impl Iterator<Item = (usize, u32)>)>,
    ) {
        self.task = task;
        if let Some((len, known)) = frames {
            while self.stack(task).entries.len() > len {
                self.close(task);
            }
            for (index, code) in known.filter(|(index, _)| *index < len) {
                if self
                    .stack(task)
                    .entries
                    .get(index)
                    .is_some_and(|entry| entry.code != code)
                {
                    while self.stack(task).entries.len() > index {
                        self.close(task);
                    }
                }
                // Frames entered before counting began: their codes are
                // not known here, and they count nothing.
                while self.stack(task).entries.len() < index {
                    self.open_frame(task, u32::MAX, false);
                }
                if self.stack(task).entries.len() == index {
                    self.open_frame(task, code, false);
                }
            }
        }
        self.settle_owner();
    }

    fn open_frame(&mut self, task: usize, code: u32, call: bool) {
        let stack = self.stack(task);
        let outermost = stack.entries.iter().all(|entry| entry.code != code);
        let at = stack.charged;
        stack.entries.push(Entry {
            code,
            at,
            outermost,
        });
        if call && let Some(counts) = self.codes.get_mut(code as usize) {
            counts.calls += 1;
        }
    }

    fn close(&mut self, task: usize) {
        let stack = self.stack(task);
        let charged = stack.charged;
        if let Some(entry) = stack.entries.pop()
            && entry.outermost
            && let Some(counts) = self.codes.get_mut(entry.code as usize)
        {
            counts.inclusive_charge += charged - entry.at;
        }
    }

    pub(crate) fn push(&mut self, task: usize, code: u32) {
        self.task = task;
        self.open_frame(task, code, true);
        self.settle_owner();
    }

    pub(crate) fn pop(&mut self, task: usize) {
        self.task = task;
        self.close(task);
        self.settle_owner();
    }

    pub(crate) fn task_ended(&mut self, task: usize) {
        while !self.stack(task).entries.is_empty() {
            self.close(task);
        }
        self.settle_owner();
    }

    pub(crate) fn statement(&mut self, code: u32, charging: bool) {
        if let Some(counts) = self.codes.get_mut(code as usize) {
            counts.statements += 1;
            counts.uncharged_operations += u64::from(!charging);
        }
    }

    pub(crate) fn expression(&mut self, charging: bool) {
        if let Owner::Code(code) = self.current
            && let Some(counts) = self.codes.get_mut(code as usize)
        {
            counts.expressions += 1;
            counts.uncharged_operations += u64::from(!charging);
        }
    }

    pub(crate) fn native_enter(&mut self, task: usize, lib: u32) {
        *self.native_calls.entry((self.current, lib)).or_default() += 1;
        if let Some(counts) = self.natives.get_mut(lib as usize) {
            counts.calls += 1;
        }
        self.open.push(OpenNative {
            lib,
            task: self.task,
            charged: self.charged,
        });
        self.task = task;
        self.current = Owner::Lib(lib);
    }

    pub(crate) fn native_exit(&mut self) {
        let Some(open) = self.open.pop() else {
            return;
        };
        if self.charged == open.charged
            && let Some(counts) = self.natives.get_mut(open.lib as usize)
        {
            counts.zero_charge_calls += 1;
        }
        self.task = open.task;
        self.settle_owner();
    }

    fn open_native(&mut self) -> Option<&mut NativeCounts> {
        let lib = self.open.last()?.lib;
        self.natives.get_mut(lib as usize)
    }

    pub(crate) fn native_fast(&mut self) {
        if let Some(counts) = self.open_native() {
            counts.fast_calls += 1;
        }
    }

    pub(crate) fn guard_work(&mut self, units: u64) {
        if let Some(counts) = self.open_native() {
            counts.guard_work += units;
        }
    }

    pub(crate) fn formula(&mut self, lib: u32) {
        if let Some(counts) = self.natives.get_mut(lib as usize) {
            counts.formula_evaluations += 1;
        }
    }

    /// Closes every open frame and native call, as the end of counting:
    /// what a report reads.
    pub(crate) fn close_all(&mut self) {
        while self.open.pop().is_some() {}
        for task in 0..self.tasks.len() {
            while !self.tasks[task].entries.is_empty() {
                self.close(task);
            }
        }
        self.current = Owner::Nobody;
    }
}
