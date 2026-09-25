//! The process family: the process registry and the two wake-floor tables.
//!
//! Fifteen tables, the largest family in either store. They are one family
//! because they are one transactional unit: an event append writes the event,
//! the process projection, the parent-end ledger, the wake delivery and the
//! allocation floor in the same transaction, and a prune moves a process row,
//! its events, its tombstone and its artifact cleanup together.
//!
//! Two of them — [`wake_allocation_floors`] and [`wake_redelivery_fences`] —
//! live in the session databases rather than the registry's own, because they
//! record what a *session* has been promised and has consumed. They are the
//! process family's tables all the same: nothing but a process wake writes
//! them, and the sequence they carry is a process event sequence.
//!
//! # Vocabulary
//!
//! `processes.status` and `process_wake_deliveries.state` carry domain
//! vocabulary generated from `lash_core::ProcessStatus` and
//! `lash_core::WakeDeliveryState` (FIG-2815, FIG-2844). No statement in this
//! family spells either: each names the predicate it wants as a
//! `{{term(column)}}` token and the backend's [`crate::Vocabulary`] expands it
//! at startup from the one source those labels have. The ownership gate
//! refuses a spelled literal over those two columns, and `lash-sim`'s
//! `process_lifecycle_vocabulary` refuses one anywhere in either store.

pub mod artifact_cleanup;
pub mod change_clock;
pub mod definitions;
pub mod events;
pub mod leases;
pub mod observers;
pub mod parent_end_plans;
pub mod park_clock;
pub mod park_events;
pub mod processes;
pub mod segment_handovers;
pub mod tombstones;
pub mod wake_allocation_floors;
pub mod wake_deliveries;
pub mod wake_redelivery_fences;

// No family-wide shared statement: every read that spans two of these tables —
// the change feed, the retention classification, the prune, the listings —
// forks between the backends, so each is declared in the backend that issues
// it and named in the manifest. The `process_registry` statement prefix is
// reserved for exactly those.
