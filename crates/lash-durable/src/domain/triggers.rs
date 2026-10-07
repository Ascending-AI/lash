//! Trigger starts (L6, FIG-5175): an occurrence recorded with every
//! delivery it fires, each bound to the process it starts, in one mailbox
//! transaction (ADR 0132 §12).
//!
//! The start's owner prepares every delivery's process before the
//! transaction and encodes the occurrence, the subscriptions it planned
//! against and the prepared registrations as `start_json`. The store decodes
//! it, refuses when the occurrence or its subscriptions moved since the plan,
//! and otherwise writes the occurrence, the process rows, their actors ready
//! and the bound deliveries together: a crash before the commit leaves
//! nothing, and after it each delivery has exactly its one process.

use lash_sansio::ProcessId;

/// One trigger occurrence's start, from outside any owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TriggerStart {
    /// The occurrence.
    pub occurrence_id: String,
    /// The occurrence, its planned subscriptions and its deliveries'
    /// prepared registrations, encoded by their owner.
    pub start_json: String,
}

/// The answer to a [`TriggerStart`]: the process each delivery started, in
/// the order the start listed its deliveries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TriggerStartAnswer {
    /// The started processes.
    pub processes: Vec<ProcessId>,
}
