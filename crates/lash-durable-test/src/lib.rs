//! The fault-injecting durable test harness.
//!
//! It runs the production lash-durable runtime, never a second scheduler:
//! several [`SimNodes`] run the production [`Runner`](lash_durable::runner::Runner)
//! over one database, each through a [`FaultStore`] that labels and can cut
//! every write it makes, all on one [`SimClock`]. A [`Matrix`] runs a
//! scenario once to learn its labelled writes, then re-runs it cut at each
//! write under each fault, recovers on another node and checks the
//! scenario's domain laws.
//!
//! A [`Tripwire`] is the replay tripwire every law reads after resume
//! (ADR 0132 §2): the runtime's owners report to it through
//! [`DurableProbe`](lash_durable::DurableProbe).
//!
//! Only the clock and the fault script are substituted: the store is a real
//! store set (SQLite, or PostgreSQL under its `testing` clock), and the
//! runner, claims, fences and reaps are the production code.

mod clock;
mod fault;
pub mod fencing;
mod life;
mod matrix;
mod nodes;
mod script;
#[cfg(test)]
mod testing;
mod tripwire;

pub use clock::{SimClock, settle};
pub use fault::FaultStore;
pub use life::Life;
pub use matrix::{Cell, CutPoint, Matrix, MatrixReport, Scenario, Verdict};
pub use nodes::{SimNodes, SimNodesConfig};
pub use script::{Cut, Fault, Point, Script, Stored, Write, WriteKind};
pub use tripwire::{Tripwire, TripwireCounts};
