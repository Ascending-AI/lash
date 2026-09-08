//! Generated operation strategy shared by the store-contract law and differential harness.

use proptest::strategy::ValueTree;
use proptest::test_runner::{Config, RngSeed, TestRunner};

use super::*;

pub(super) fn generated_case() -> impl Strategy<Value = GeneratedCase> {
    (
        any::<u64>(),
        prop::collection::vec(operation(), 1..=(MAX_OPS - GENERATED_PREFIX_OPS)),
    )
        .prop_map(|(seed, random_operations)| {
            let mut operations = generated_prefix();
            operations.extend(random_operations);
            GeneratedCase { seed, operations }
        })
}

/// Deterministically sample the shared store-contract operation alphabet.
///
/// The required prefix prevents small differential budgets from starving the
/// process, lease, wake-delivery, queue, and prune surfaces. Remaining steps
/// come from the exact strategy used by the property-law harness.
pub fn sample_store_contract_operations(runner_seed: u64, max_ops: usize) -> Vec<StoreContractOp> {
    let mut operations = generated_prefix();
    operations.truncate(max_ops);
    if operations.len() == max_ops {
        return operations;
    }

    let mut runner = TestRunner::new(Config {
        cases: 1,
        failure_persistence: None,
        rng_seed: RngSeed::Fixed(runner_seed),
        ..Config::default()
    });
    while operations.len() < max_ops {
        operations.push(
            operation()
                .new_tree(&mut runner)
                .expect("store-contract operation strategy must generate")
                .current(),
        );
    }
    operations
}

fn operation() -> impl Strategy<Value = StoreContractOp> {
    prop_oneof![
        4 => (0..PROCESS_COUNT, 0_u8..3, 1_u8..4, prop::option::of(0..SESSION_COUNT))
            .prop_map(|(process, disposition, max_attempts, wake_target)| StoreContractOp::Register {
                process, disposition, max_attempts, wake_target,
            }),
        4 => (0..PROCESS_COUNT, 0_u8..3, 1_u8..5)
            .prop_map(|(process, owner, attempt)| StoreContractOp::FirstStart { process, owner, attempt }),
        2 => (0..PROCESS_COUNT, any::<bool>())
            .prop_map(|(process, stale)| StoreContractOp::EnterWait { process, stale }),
        2 => (0..PROCESS_COUNT, any::<bool>())
            .prop_map(|(process, stale)| StoreContractOp::ClearWait { process, stale }),
        2 => (0..PROCESS_COUNT, 0_u8..3)
            .prop_map(|(process, value)| StoreContractOp::SetExternalRef { process, value }),
        5 => (0..PROCESS_COUNT, 0_u8..4, any::<u8>(), any::<bool>(), any::<bool>())
            .prop_map(|(process, replay, value, wake, stale)| StoreContractOp::Signal { process, replay, value, wake, stale }),
        2 => (0..PROCESS_COUNT, any::<u8>())
            .prop_map(|(process, reason)| StoreContractOp::CancelRequest { process, reason }),
        3 => (0..PROCESS_COUNT, 0_u8..4)
            .prop_map(|(process, disposition)| StoreContractOp::Terminal { process, disposition }),
        2 => (0..PROCESS_COUNT, 0..SESSION_COUNT)
            .prop_map(|(process, session)| StoreContractOp::AddObserver { process, session }),
        2 => (0..PROCESS_COUNT, 0..SESSION_COUNT)
            .prop_map(|(process, session)| StoreContractOp::RemoveObserver { process, session }),
        2 => (0..PROCESS_COUNT, prop::option::of(0..SESSION_COUNT))
            .prop_map(|(process, session)| StoreContractOp::Retarget { process, session }),
        2 => (0..PROCESS_COUNT, 0_u8..3)
            .prop_map(|(process, owner)| StoreContractOp::ClaimLease { process, owner }),
        2 => (0..PROCESS_COUNT, any::<bool>())
            .prop_map(|(process, stale)| StoreContractOp::ReleaseLease { process, stale }),
        2 => Just(StoreContractOp::ClaimWake),
        2 => any::<bool>().prop_map(|stale| StoreContractOp::MarkWake { stale }),
        2 => any::<bool>().prop_map(|stale| StoreContractOp::DiscardWake { stale }),
        2 => any::<bool>().prop_map(|stale| StoreContractOp::DeferWake { stale }),
        3 => (0..PROCESS_COUNT)
            .prop_map(|process| StoreContractOp::EnqueueWake { process }),
        3 => (any::<u8>(), any::<bool>(), any::<bool>())
            .prop_map(|(selection, highest_in_group, stale)| StoreContractOp::ConsumeWake { selection, highest_in_group, stale }),
        3 => any::<bool>().prop_map(|watermark| StoreContractOp::Prune { watermark }),
        2 => any::<bool>().prop_map(|caught_up| StoreContractOp::CompactTombstones { caught_up }),
    ]
}
