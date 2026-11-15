//! Which failed waits a catch recovered from, followed by error identity.

use lash_kernel_doc::Value;

use crate::WaitId;

#[derive(Default)]
pub(crate) struct Catches {
    pub(crate) failures: Vec<Failure>,
    pub(crate) caught: Vec<WaitId>,
}

pub(crate) struct Failure {
    pub(crate) task: usize,
    pub(crate) wait: WaitId,
    pub(crate) error: Value,
    pub(crate) catch: Option<Scope>,
}

pub(crate) struct Scope {
    pub(crate) frame: u32,
    pub(crate) at: u32,
    pub(crate) end: u32,
}

impl Catches {
    pub(crate) fn wait_raised(&mut self, task: usize, wait: WaitId, error: Value) {
        self.failures.push(Failure {
            task,
            wait,
            error,
            catch: None,
        });
    }

    pub(crate) fn roots(&self, visit: &mut dyn FnMut(&Value)) {
        for failure in &self.failures {
            visit(&failure.error);
        }
    }

    pub(crate) fn took(&mut self, task: usize, frame: u32, at: u32, end: u32, error: Value) {
        if let Some(index) = self.failures.iter().position(|failure| {
            failure.task == task
                && failure.catch.is_none()
                && failure.error.handle() == error.handle()
        }) {
            let mut failure = self.failures.remove(index);
            failure.catch = Some(Scope { frame, at, end });
            self.failures.push(failure);
        }
    }

    pub(crate) fn left(&mut self, task: usize, frame: u32, at: u32) {
        if let Some(index) = self.failures.iter().position(|failure| {
            failure.task == task
                && failure
                    .catch
                    .as_ref()
                    .is_some_and(|scope| scope.frame == frame && scope.at == at)
        }) {
            self.caught.push(self.failures.remove(index).wait);
        }
    }

    /// A raise leaves the catches outside its handler's range. An unchanged
    /// error remains associated with its failed wait for an outer catch.
    pub(crate) fn raised_to(&mut self, task: usize, frame: u32, to: Option<u32>, error: Value) {
        self.failures.retain_mut(|failure| {
            let leaving = failure.task == task
                && failure.catch.as_ref().is_some_and(|scope| {
                    scope.frame == frame && to.is_none_or(|pc| pc < scope.at || pc >= scope.end)
                });
            if !leaving {
                return true;
            }
            failure.catch = None;
            failure.error.handle() == error.handle()
        });
    }

    /// At a park or end, an open catch has gone on to wait or end. Its
    /// recovery is reported once, as on the reference machine.
    pub(crate) fn take(&mut self) -> Vec<WaitId> {
        self.caught.extend(
            self.failures
                .drain(..)
                .filter_map(|failure| failure.catch.map(|_| failure.wait)),
        );
        std::mem::take(&mut self.caught)
    }
}

impl<H: crate::heap::Heap> super::Vm<H> {
    pub(super) fn catch_enter(&mut self, end: u32) {
        let frame = (self.cx.frames.len() - 1) as u32;
        let at = self.cx.frames[frame as usize].pc - 1;
        if let Some(error) = self.cx.stack.last() {
            self.catches
                .took(self.tasks.running, frame, at, end, *error);
        }
    }

    pub(super) fn catch_exit(&mut self, at: u32) {
        let frame = (self.cx.frames.len() - 1) as u32;
        self.catches.left(self.tasks.running, frame, at);
    }
}
