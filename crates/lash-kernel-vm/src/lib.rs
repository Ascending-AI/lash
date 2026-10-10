//! The lash kernel's machine.
//!
//! The machine is a plain library (`docs/kernel/design.md` §9). It runs an
//! admitted document as tasks until none is ready, hands the embedder the
//! effects and sleeps they asked for, and takes their outcomes back one at a
//! time. It does no I/O, starts no thread and keeps no global state: clock,
//! random, projection reads, `print` output and a pending cancel go through
//! the [`Host`] the embedder passes to [`Machine::run`]. The machine parks;
//! the embedder commits.
//!
//! A run that is not executing is a `ParkedRun` of `lash-kernel-state`:
//! [`Machine::export`] writes one at any safe point and [`Machine::import`]
//! resumes it against an executable compiled afresh.
//!
//! An embedder prepares its registry once as a [`PreparedLibrary`], which
//! compiles every library body, and hands it to every [`Program`]: a run's
//! start compiles only its document.
//!
//! [`KernelMachine`] is the machine; [`Machine`] is the interface an embedder
//! and the conformance harness drive it through. Its rules are the `K-MACH` rules of
//! `docs/kernel/semantics.md`.

mod compile;
mod costs;
mod data;
mod functions;
mod heap;
mod interface;
mod jit;
mod machine;

#[cfg(test)]
mod laws;

pub use compile::{Layout, PreparedLibrary};
pub use functions::{MachineFunctions, register_machine_functions};
pub use jit::CodeStats;
pub use machine::KernelMachine;

pub use interface::{
    Bindings, Bound, BoundExceeded, Bounds, DeliverError, Delivered, EffectRequest, End,
    ExportError, Finished, Host, ImportError, Machine, MachineError, Meters, Outcome, Park,
    Program, Request, RunError, SleepRequest, Start, StartError, Step, Target, WaitId,
};
