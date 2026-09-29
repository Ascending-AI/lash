//! Phase A's synthetic N+1 harness (ADR 0115 §6).
//!
//! Head is built twice from one tree: **N** is the default build, **N+1** is
//! the same tree with the `synthetic-next` feature, which moves the constants
//! whose `#[cfg(feature = "synthetic-next")]` blocks sit beside them. Both
//! builds are the [`node`] binary, `lash-upgrade-node`, and the tests run the
//! two binaries as separate processes against real PostgreSQL, a real
//! `restate-server` and SQLite store directories.
//!
//! - [`identity`] is what a build reports about itself: its label, its drain
//!   generation `G` and every range it declares.
//! - [`node`] is the node binary's commands: migrate a store, serve a
//!   Restate deployment over it, and run one turn as a host.
//! - [`harness`] is the test side: it finds the two builds and the services,
//!   spawns and stops nodes, and reads their JSON reports.
//!
//! `just e2e-rolling` builds both binaries and runs the roll and rollback
//! choreography in `tests/rolling/`. The Phase A legs in `tests/phase_a/`
//! each wait for the lane that builds what they prove.

pub mod harness;
pub mod identity;
pub mod node;
