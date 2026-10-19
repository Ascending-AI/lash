//! Where a task's raise stands in document code (FIG-6810): what names the
//! site of a run's uncaught error.
//!
//! A raise is born at an interrupt. Born while the task's innermost frame
//! is document code, it stands at that frame's wait action, when the frame
//! waits in a `perform`, `sleep` or `join` whose outcome raised, else at
//! the statement it runs. Born in library code, it stands nowhere until it
//! unwinds into a document frame, whose statement then names it: a library
//! call answered in place and one that runs its body in a frame name the
//! same statement, so the site does not depend on the layout.
//!
//! The machine holds the site of the raise the running step carries and
//! nothing else: a step starts with none, and a `catch` ends it. A run
//! sealed between two steps and reopened ends as the uninterrupted run
//! (CONT-EXACT), so nothing here is saved or outlives a step. A raise a
//! `finally` held unwinds again in a later step, from its `try` statement,
//! which names it; one a run-root cleanup held leaves the entry with no
//! document statement to name it.

use lash_kernel_doc::{Site, TaskId, Unit};
use lash_kernel_exec::ActIx;

use super::{Control, Frame, Halt, KernelMachine};
use crate::compile::{ActionKind, BlockId, Executable, Form, NestedKind, StmtId};

/// A raise's coordinate in its frame, resolved to a [`Site`] only when it
/// ends the run.
#[derive(Clone, Copy, Debug)]
pub(super) enum Coordinate {
    /// The statement `index` of `block`.
    Statement { block: BlockId, index: u32 },
    /// The wait action of statement `stmt`.
    Action(StmtId),
    /// The wait action nested in statement `stmt`'s expression.
    Nested(StmtId, ActIx),
}

fn document(frame: &Frame, exe: &Executable) -> bool {
    !matches!(exe.code(frame.code).site.unit, Unit::Library(_))
}

/// Where a raise unwinding through `frame` stands, when it is a document
/// frame: its statement.
pub(super) fn unwinding(frame: &Frame, exe: &Executable) -> Option<Coordinate> {
    document(frame, exe).then(|| statement(frame)).flatten()
}

/// The statement `frame` runs, waits or unwinds in.
fn statement(frame: &Frame) -> Option<Coordinate> {
    frame
        .control
        .iter()
        .rev()
        .find_map(|control| match control {
            Control::Block { block, next } if *next > 0 => Some(Coordinate::Statement {
                block: *block,
                index: (*next - 1) as u32,
            }),
            _ => None,
        })
}

/// Where a raise born in `frame` stands: its wait action, when it waits in
/// one, else its statement.
fn standing(frame: &Frame, exe: &Executable) -> Option<Coordinate> {
    let waits = |kind: &NestedKind| {
        matches!(
            kind,
            NestedKind::Perform { .. }
                | NestedKind::Sleep { .. }
                | NestedKind::Join
                | NestedKind::JoinMany(_)
        )
    };
    match (frame.awaiting, frame.nested) {
        (Some(stmt), Some(act)) if waits(&exe.statement(stmt).0.nested[act.ix()].kind) => {
            Some(Coordinate::Nested(stmt, act))
        }
        (Some(stmt), None)
            if matches!(
                &exe.stmt(stmt).form,
                Form::Action(action) if matches!(
                    action.kind,
                    ActionKind::Perform { .. }
                        | ActionKind::Sleep { .. }
                        | ActionKind::Join(_)
                        | ActionKind::JoinMany(..)
                )
            ) =>
        {
            Some(Coordinate::Action(stmt))
        }
        _ => statement(frame),
    }
}

impl Coordinate {
    fn site(self, exe: &Executable) -> Option<Site> {
        Some(match self {
            Self::Statement { block, index } => exe.block(block).site.child(index),
            Self::Action(stmt) => match &exe.stmt(stmt).form {
                Form::Action(action) => action.site.clone(),
                _ => return None,
            },
            Self::Nested(stmt, act) => exe.statement(stmt).0.nested[act.ix()].site.clone(),
        })
    }
}

impl KernelMachine {
    /// `task` raises: a raise born in its innermost frame, when that frame
    /// is document code, stands there.
    pub(super) fn raise_born(&mut self, task: TaskId) -> Result<(), Halt> {
        let exe = std::sync::Arc::clone(&self.exe);
        let Some(frame) = self.task(task)?.frames.last_mut()? else {
            return Ok(());
        };
        if document(frame, &exe) {
            self.raised = standing(frame, &exe).map(|at| (task, at));
        }
        Ok(())
    }

    /// `task`'s raise unwinds from `at`, the statement of the frame it is
    /// in: it names a raise that stands nowhere yet.
    pub(super) fn raise_unwinds(&mut self, task: TaskId, at: Option<Coordinate>) {
        if let Some(at) = at
            && self.raised.is_none_or(|(held, _)| held != task)
        {
            self.raised = Some((task, at));
        }
    }

    /// A `catch`, a `finally` or a run-root cleanup holds the raise: the
    /// raise it lets go of again is one of its own.
    pub(super) fn raise_held(&mut self) {
        self.raised = None;
    }

    /// `task` has ended: the site its uncaught raise stands at, if any.
    pub(super) fn raise_ended(&mut self, task: TaskId) -> Option<Site> {
        let (_, at) = self.raised.filter(|(held, _)| *held == task)?;
        self.raised = None;
        at.site(&self.exe)
    }
}
