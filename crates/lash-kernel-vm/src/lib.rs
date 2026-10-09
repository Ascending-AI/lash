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
//! This crate fixes the interface. Its rules are the `K-MACH` rules of
//! `docs/kernel/semantics.md`.

mod interface;

pub use interface::{
    Bindings, Bound, BoundExceeded, Bounds, DeliverError, Delivered, EffectRequest, End,
    ExportError, Finished, Host, ImportError, Machine, MachineError, Meters, Outcome, Park,
    Program, Request, RunError, SleepRequest, Start, StartError, Step, Target, WaitId,
};
