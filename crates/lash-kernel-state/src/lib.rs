//! The lash kernel's parked run.
//!
//! A run that is not executing is data in the document's terms
//! (`docs/kernel/design.md` §6, `docs/kernel/parked-state.md`): where each
//! task stands is a site of the document, a variable is a name and the
//! node that declares it, a wait is its effect's identity. Nothing in it
//! names a position in an executable, so a machine resumes it against one
//! it has just compiled, laid out however it likes.
//!
//! [`ParkedRun`] is the whole of it, and a host reads it without a
//! machine. It is stored as one document, or in parts: a [`Header`] and
//! one [`Fragment`] per [`Root`], the roots being each session binding,
//! each task handle, each active call's bindings and the values each
//! call's control state alone holds. Every live heap object is written
//! once, in the fragment of the first root that reaches it.
//! [`ParkedRun::save`] writes the parts and names the ones that changed
//! since a [`Baseline`]; [`ParkedRun::load`] reads them back and refuses
//! bytes this writer would not have produced.
//!
//! The crate stores nothing and commits nothing: it produces and consumes
//! bytes. It depends on `lash-kernel-doc` and on no other lash crate.

mod fragments;
mod schema;

#[cfg(test)]
mod tests;

pub use fragments::{Baseline, LoadError, Objects, SaveError, Saved, SavedFragment, save};
pub use schema::{
    Binding, Bound, Call, EffectOutcome, Ended, Entered, Finally, Fragment, Header, Held, Incoming,
    ListJoin, Loop, Occurrence, Owned, ParkedCall, ParkedRun, ParkedTask, Pause, Perform,
    PerformState, ReadyQueue, Request, Root, RootState, Run, SleepOutcome, Task, TaskState, WaitId,
};
