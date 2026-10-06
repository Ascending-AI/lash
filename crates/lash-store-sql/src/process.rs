//! The process family: the process registry's tables.
//!
//! One family because they are one transactional unit: an event append writes
//! the event, the process projection and the parent-end ledger in the same
//! transaction (and, for a wake, the target session's queued work), and a
//! prune moves a process row, its events, its tombstone and its artifact
//! cleanup together.
//!
//! # Vocabulary
//!
//! `processes.status` carries domain vocabulary generated from
//! `lash_core::ProcessStatus` (FIG-2815). No statement in this family spells
//! it: each names the predicate it wants as a `{{term(column)}}` token and the
//! backend's [`crate::Vocabulary`] expands it at startup from the one source
//! those labels have.

pub mod abandoned_consumer_holds;
pub mod change_clock;
pub mod event_horizons;
pub mod events;
pub mod observers;
pub mod parent_end_plans;
pub mod processes;
pub mod tombstones;

// No family-wide shared statement: every read that spans two of these tables —
// the change feed, the retention classification, the prune, the listings —
// forks between the backends, so each is declared in the backend that issues
// it and named in the manifest. The `process_registry` statement prefix is
// reserved for exactly those.
