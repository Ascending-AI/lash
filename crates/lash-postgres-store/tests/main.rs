#[allow(dead_code)]
#[path = "support/mod.rs"]
mod support;

#[path = "attempt_atomicity.rs"]
mod attempt_atomicity;
#[path = "checkpoint_commit_delete_race.rs"]
mod checkpoint_commit_delete_race;
#[path = "commit_size_benchmark.rs"]
mod commit_size_benchmark;
#[path = "orphaned_tombstone_reclaim.rs"]
mod orphaned_tombstone_reclaim;
#[path = "postgres_clock_contract.rs"]
mod postgres_clock_contract;
#[path = "postgres_lease_multiconnection.rs"]
mod postgres_lease_multiconnection;
#[path = "pre_frame_key_cutover.rs"]
mod pre_frame_key_cutover;
#[path = "process_parent_atomicity.rs"]
mod process_parent_atomicity;
#[path = "process_prune_reclaim.rs"]
mod process_prune_reclaim;
#[path = "queued_work_ordering.rs"]
mod queued_work_ordering;
#[path = "refcount_benchmark.rs"]
mod refcount_benchmark;
#[path = "session_execution_lease_renewal.rs"]
mod session_execution_lease_renewal;
#[path = "store_effect_group_drain_conformance.rs"]
mod store_effect_group_drain_conformance;
