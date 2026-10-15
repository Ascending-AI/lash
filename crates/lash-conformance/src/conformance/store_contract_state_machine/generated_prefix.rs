use super::*;

pub(super) fn generated_prefix() -> Vec<StoreContractOp> {
    let operations = vec![
        StoreContractOp::Register { process: 0 },
        StoreContractOp::SetExternalRef {
            process: 0,
            value: 0,
        },
        StoreContractOp::AddObserver {
            process: 0,
            session: 0,
        },
        StoreContractOp::CancelRequest {
            process: 0,
            requester: 0,
        },
        StoreContractOp::Register { process: 1 },
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
