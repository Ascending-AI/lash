//! The component versions parked VM state carries and the ranges a reader admits.

use lash_sansio::VersionRange;
use serde::{Deserialize, Serialize};

use crate::OpaqueStateRefusal;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VmContractComponent {
    Bytecode,
    Continuation,
    Snapshot,
    Accounting,
    Heap,
    Abi,
}

impl std::fmt::Display for VmContractComponent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Bytecode => "bytecode",
            Self::Continuation => "continuation",
            Self::Snapshot => "snapshot",
            Self::Accounting => "accounting",
            Self::Heap => "heap",
            Self::Abi => "abi",
        })
    }
}

/// Versions of the independent contracts under which a worker wrote state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmContract {
    pub bytecode: u32,
    pub continuation: u32,
    pub snapshot: u32,
    pub accounting: u32,
    pub heap: u32,
    pub abi: u32,
}

impl VmContract {
    /// A reader that supports only these component versions.
    pub const fn exact_reads(self) -> VmContractReads {
        VmContractReads {
            bytecode: VersionRange::exactly(self.bytecode),
            continuation: VersionRange::exactly(self.continuation),
            snapshot: VersionRange::exactly(self.snapshot),
            accounting: VersionRange::exactly(self.accounting),
            heap: VersionRange::exactly(self.heap),
            abi: VersionRange::exactly(self.abi),
        }
    }
}

/// Each range must be backed by the owning VM component's decoder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VmContractReads {
    pub bytecode: VersionRange,
    pub continuation: VersionRange,
    pub snapshot: VersionRange,
    pub accounting: VersionRange,
    pub heap: VersionRange,
    pub abi: VersionRange,
}

impl VmContractReads {
    /// Checks every component independently, without decoding any VM bytes.
    pub fn admit(self, contract: VmContract) -> Result<(), OpaqueStateRefusal> {
        use VmContractComponent as Component;
        for (component, found, reads) in [
            (Component::Bytecode, contract.bytecode, self.bytecode),
            (
                Component::Continuation,
                contract.continuation,
                self.continuation,
            ),
            (Component::Snapshot, contract.snapshot, self.snapshot),
            (Component::Accounting, contract.accounting, self.accounting),
            (Component::Heap, contract.heap, self.heap),
            (Component::Abi, contract.abi, self.abi),
        ] {
            if !reads.contains(found) {
                return Err(OpaqueStateRefusal::ComponentOutsideReadRange {
                    component,
                    found,
                    reads,
                });
            }
        }
        Ok(())
    }
}
