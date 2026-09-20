//! The attachment family: the write-ahead manifest and the GC condemnation
//! fence.
//!
//! Two tables — [`manifest`] and [`condemnation`] — and one rule that spans
//! them: a digest's bytes may be deleted only while no manifest row roots it,
//! and a writer may record a root only while no delete is armed. The two
//! tables are therefore always read and written inside one transaction, which
//! is why they are one family.
//!
//! # Not yet here
//!
//! Four of the family's operations — the deleted-session root reclaim, the
//! aged-intent forget, the live-root probe and the per-session forget — read
//! `deleted_sessions`, `graph_nodes`, `runtime_turn_commits` and the process
//! registry, and two of them build a predicate from the `AttachmentOwnerKind`
//! vocabulary. Neither a table the renderer does not own nor a vocabulary
//! token can be spelled in a neutral statement today; FIG-3399 adds both
//! axes, and those statements move here then. Until it lands they stay at
//! their call sites in the backend crates, unchanged, and this family is not
//! in the ownership gate's `converted` list.

pub mod condemnation;
pub mod manifest;
