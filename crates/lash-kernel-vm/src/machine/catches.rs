//! Which failed waits a `catch` recovered from (FIG-6817): what reports a
//! failed effect occurrence as recovered rather than failed.
//!
//! A wait whose outcome failed raises its error at its wait action. The
//! machine notes the error the outcome raised, by its identity. A `catch`
//! that takes that same error, however it reached the catch (through a
//! `finally`, or raised again unchanged by an inner `catch`), takes the
//! wait's failure. The take holds while the catch block runs and becomes
//! the run's fact when the block is left by anything but a raise: it ends,
//! or returns, breaks or continues out. A raise out of the block undoes the
//! take, so a catch that raises again has not recovered; when it raises the
//! same error, that error is still the wait's, and an outer catch can take
//! it.
//!
//! Nothing here is saved or outlives a run segment: a process runs each
//! segment whole from its park, and a take still open when the segment
//! parks or ends holds, since its catch block went on to wait or end.

use lash_kernel_doc::TaskId;
use lash_kernel_doc::arena::ArenaHandle;

use super::{Halt, KernelMachine};
use crate::interface::WaitId;

/// The failed waits of one run segment and the catches that took them.
#[derive(Debug, Default)]
pub(super) struct Catches {
    /// The error each failed wait raised that no catch holds now, by task.
    raised: Vec<(TaskId, WaitId, ArenaHandle)>,
    /// Takes whose catch block still runs.
    open: Vec<Open>,
    /// The waits whose failure a catch recovered from, in order.
    caught: Vec<WaitId>,
}

/// A catch block that took a failed wait's error, by the `try` it belongs
/// to: its task, the depth of its frame and its place in the frame's
/// controls.
#[derive(Debug)]
struct Open {
    task: TaskId,
    frame: usize,
    control: usize,
    wait: WaitId,
    error: ArenaHandle,
}

impl KernelMachine {
    /// `task`'s wait `wait` failed: its outcome raises `error`.
    pub(super) fn wait_raised(
        &mut self,
        task: TaskId,
        wait: WaitId,
        error: &lash_kernel_doc::Value,
    ) {
        if let Some(error) = error.handle() {
            self.catches.raised.push((task, wait, error));
        }
    }

    /// The catch of the `try` at `control` in `task`'s innermost frame
    /// takes `error`.
    pub(super) fn catch_took(
        &mut self,
        task: TaskId,
        control: usize,
        error: &lash_kernel_doc::Value,
    ) -> Result<(), Halt> {
        let Some(error) = error.handle() else {
            return Ok(());
        };
        let catches = &mut self.catches;
        let Some(at) = catches
            .raised
            .iter()
            .position(|(held, _, raised)| *held == task && *raised == error)
        else {
            return Ok(());
        };
        let (_, wait, _) = catches.raised.remove(at);
        let frame = self.task(task)?.frames.len();
        self.catches.open.push(Open {
            task,
            frame,
            control,
            wait,
            error,
        });
        Ok(())
    }

    /// The control at `control` in `task`'s innermost frame is left, by a
    /// raise of `raised` or by anything else: when it is a `try` whose catch
    /// block runs, its take holds or is undone.
    pub(super) fn catch_left(
        &mut self,
        task: TaskId,
        control: usize,
        raised: Option<&lash_kernel_doc::Value>,
    ) -> Result<(), Halt> {
        if self.catches.open.is_empty() {
            return Ok(());
        }
        let frame = self.task(task)?.frames.len();
        let catches = &mut self.catches;
        let Some(at) = catches
            .open
            .iter()
            .position(|open| open.task == task && open.frame == frame && open.control == control)
        else {
            return Ok(());
        };
        let open = catches.open.remove(at);
        match raised.map(|error| error.handle()) {
            None => catches.caught.push(open.wait),
            Some(Some(error)) if error == open.error => {
                catches.raised.push((task, open.wait, error));
            }
            Some(_) => {}
        }
        Ok(())
    }

    /// Takes the waits whose failure a catch recovered from since the
    /// previous take, in order. Called at a park or an end: a take still
    /// open holds, and an error no catch took is no longer followed.
    pub(crate) fn take_caught(&mut self) -> Vec<WaitId> {
        let catches = &mut self.catches;
        catches.raised.clear();
        let open = std::mem::take(&mut catches.open);
        catches
            .caught
            .extend(open.into_iter().map(|open| open.wait));
        std::mem::take(&mut catches.caught)
    }
}
