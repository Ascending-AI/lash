use super::*;

pub(super) fn generated_prefix() -> Vec<StoreContractOp> {
    let operations = vec![
        StoreContractOp::Register {
            process: 0,
            disposition: 0,
            max_attempts: 3,
            wake_target: Some(0),
        },
        StoreContractOp::FirstStart {
            process: 0,
            owner: 0,
            attempt: 1,
        },
        StoreContractOp::FirstStart {
            process: 0,
            owner: 1,
            attempt: 2,
        },
        StoreContractOp::EnterWait {
            process: 0,
            stale: true,
        },
        StoreContractOp::EnqueueWake { process: 0 },
        StoreContractOp::EnqueueWake { process: 0 },
        StoreContractOp::ConsumeWake {
            selection: 0,
            highest_in_group: true,
            stale: false,
        },
        StoreContractOp::Register {
            process: 1,
            disposition: 0,
            max_attempts: 3,
            wake_target: None,
        },
        StoreContractOp::Terminal {
            process: 1,
            disposition: 0,
        },
        StoreContractOp::Prune { watermark: false },
        StoreContractOp::CompactTombstones { caught_up: true },
    ];
    debug_assert_eq!(operations.len(), GENERATED_PREFIX_OPS);
    operations
}
