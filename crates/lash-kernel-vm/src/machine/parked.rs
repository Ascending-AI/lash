//! Parking and resuming: a run's state written in the document's terms,
//! and rebuilt against an executable compiled afresh (`K-MACH-007`,
//! `K-MACH-008`, `docs/kernel/parked-state.md`).
//!
//! Writing turns every position in the executable into a site: a frame's
//! block and index into its pending statement, a slot into the variable's
//! name and the node that declares it, a closure's code into the closure
//! expression. Reading turns them back, in whatever layout the new
//! executable has. What the document determines is not written at all:
//! the blocks and `try` statements around a statement, an effect's result
//! type, which code a call runs.

mod capture;
mod restore;

pub(super) use capture::{HeapView, export, save};
pub(super) use restore::import;

use crate::compile::{Action, Executable, Rhs, Stmt, StmtId};

/// The action of a statement, when it has one.
fn action_of(exe: &Executable, stmt: StmtId) -> Option<&Action> {
    match exe.stmts.get(stmt.0 as usize)? {
        Stmt::Let {
            value: Rhs::Action(action),
            ..
        }
        | Stmt::Assign {
            value: Rhs::Action(action),
            ..
        }
        | Stmt::Do(action) => Some(action),
        _ => None,
    }
}
