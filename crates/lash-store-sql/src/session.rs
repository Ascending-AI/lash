//! The session-core family: session identity, the durable head, the session
//! graph and its fork lineage, turn-commit receipts, the usage ledger, the
//! deleted-session evidence set, checkpoint blob edges and the release stamp.
//!
//! One wrinkle this family has and no other does: the durable head table is
//! spelled `session_head` on SQLite and `sessions` on PostgreSQL. ADR 0098
//! freezes both names — renaming either would invalidate every existing
//! database — so the head is two table modules, [`head`] and [`sessions`],
//! each named only by the backend that has it, and every head statement is
//! dialect-only by construction rather than by choice.

pub mod checkpoint_blob_refs;
pub mod deleted_sessions;
pub mod fork_lineage;
pub mod graph_nodes;
pub mod head;
pub mod meta;
pub mod meta_fork_inheritance_processes;
pub mod meta_pending_observer_intents;
pub mod node_anchors;
pub mod release_stamp;
pub mod sessions;
pub mod turn_commits;
pub mod usage_deltas;
