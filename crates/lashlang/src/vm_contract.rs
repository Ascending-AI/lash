//! The VM's writer versions and decoder-backed component read ranges.

use lash_sansio::VersionRange;
use lash_vm_protocol::{VmContract, VmContractReads};

use crate::{
    BYTECODE_FORMAT_VERSION, HEAP_SIZE_SCHEDULE_VERSION, INSTRUCTION_ACCOUNTING_VERSION,
    LASHLANG_SNAPSHOT_VERSION, LASHLANG_VM_ABI_VERSION, VM_CONTINUATION_FORMAT_VERSION,
};

/// The range both opaque-state admission and the continuation decoder use.
/// The synthetic successor lifts N's unchanged continuation shape to its own.
pub const VM_CONTINUATION_READ_RANGE: VersionRange = VersionRange::between(
    VM_CONTINUATION_FORMAT_VERSION
        - if cfg!(feature = "synthetic-next") {
            1
        } else {
            0
        },
    VM_CONTINUATION_FORMAT_VERSION,
);

/// The component versions this VM writes into an opaque-state envelope.
#[expect(
    clippy::expect_used,
    reason = "the VM ABI constant declares its numeric version with the fixed lashlang-vm-abi-v prefix"
)]
pub fn vm_contract_versions() -> VmContract {
    VmContract {
        bytecode: BYTECODE_FORMAT_VERSION,
        continuation: VM_CONTINUATION_FORMAT_VERSION,
        snapshot: LASHLANG_SNAPSHOT_VERSION,
        accounting: INSTRUCTION_ACCOUNTING_VERSION,
        heap: HEAP_SIZE_SCHEDULE_VERSION,
        abi: LASHLANG_VM_ABI_VERSION
            .strip_prefix("lashlang-vm-abi-v")
            .and_then(|version| version.parse().ok())
            .expect("the declared VM ABI has a numeric version"),
    }
}

/// The versions the VM decodes, independent of the fleet's writer epoch.
/// Snapshot history uses the FIG-3802 guarded window and its lift table.
pub fn vm_contract_reads() -> VmContractReads {
    VmContractReads {
        continuation: VM_CONTINUATION_READ_RANGE,
        snapshot: lash_core_execution::FleetFormat::current()
            .read_window(lash_core_execution::surface_format!(
                LASHLANG_SNAPSHOT_VERSION
            ))
            .supported(),
        ..vm_contract_versions().exact_reads()
    }
}
