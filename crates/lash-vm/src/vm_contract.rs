//! The VM's writer versions and decoder-backed component read ranges.

use lash_sansio::VersionRange;
use lash_vm_protocol::{VmContract, VmContractReads};

use crate::{
    BYTECODE_FORMAT_VERSION, INSTRUCTION_ACCOUNTING_VERSION, LASH_VM_ABI_VERSION,
    LASH_VM_SNAPSHOT_VERSION, VM_CONTINUATION_FORMAT_VERSION,
};

/// Opaque-state admission matches the decoder's exact continuation format.
pub const VM_CONTINUATION_READ_RANGE: VersionRange =
    VersionRange::exactly(VM_CONTINUATION_FORMAT_VERSION);

/// The component versions this VM writes into an opaque-state envelope.
#[expect(
    clippy::expect_used,
    reason = "the VM ABI constant declares its numeric version with the fixed lash-vm-abi-v prefix"
)]
pub fn vm_contract_versions() -> VmContract {
    VmContract {
        bytecode: BYTECODE_FORMAT_VERSION,
        continuation: VM_CONTINUATION_FORMAT_VERSION,
        snapshot: LASH_VM_SNAPSHOT_VERSION,
        accounting: INSTRUCTION_ACCOUNTING_VERSION,
        abi: LASH_VM_ABI_VERSION
            .strip_prefix("lash-vm-abi-v")
            .and_then(|version| version.parse().ok())
            .expect("the declared VM ABI has a numeric version"),
    }
}

/// The versions the VM decodes, independent of the fleet's writer epoch.
/// The snapshot range follows the guarded window its decoder admits.
pub fn vm_contract_reads() -> VmContractReads {
    VmContractReads {
        continuation: VM_CONTINUATION_READ_RANGE,
        snapshot: lash_core_execution::FleetFormat::current()
            .read_window(lash_core_execution::surface_format!(
                LASH_VM_SNAPSHOT_VERSION
            ))
            .supported(),
        ..vm_contract_versions().exact_reads()
    }
}
