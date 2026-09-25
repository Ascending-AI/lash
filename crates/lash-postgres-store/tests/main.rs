#![expect(
    clippy::expect_used,
    reason = "test target: clippy's allow-unwrap-in-tests only exempts #[test] functions, and the setup helpers around them in this target are test code too"
)]

#[allow(dead_code)]
#[path = "support/mod.rs"]
mod support;

#[path = "checkpoint_commit_delete_race.rs"]
mod checkpoint_commit_delete_race;
#[path = "commit_size_benchmark.rs"]
mod commit_size_benchmark;
#[path = "orphaned_tombstone_reclaim.rs"]
mod orphaned_tombstone_reclaim;
#[path = "parent_end_payload.rs"]
mod parent_end_payload;
#[path = "parent_end_registration_race.rs"]
mod parent_end_registration_race;
#[path = "postgres_clock_contract.rs"]
mod postgres_clock_contract;
#[path = "postgres_lease_multiconnection.rs"]
mod postgres_lease_multiconnection;
#[path = "pre_submission_digest_cutover.rs"]
mod pre_submission_digest_cutover;
#[path = "process_prune_reclaim.rs"]
mod process_prune_reclaim;
#[path = "refcount_benchmark.rs"]
mod refcount_benchmark;
#[path = "release_stamp.rs"]
mod release_stamp;
#[path = "session_execution_lease_renewal.rs"]
mod session_execution_lease_renewal;
#[path = "turn_cancel_receipt_consistency.rs"]
mod turn_cancel_receipt_consistency;
