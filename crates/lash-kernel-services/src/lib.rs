//! The shipped dialect service assembly owns its operation inventory and
//! semantic contracts. Helper loaders and runtime publication project these
//! same declarations; the kernel authoring machinery knows no dialect names.
use lash_kernel_dialect::authoring::HelperOperation;
use std::sync::LazyLock;
use {
    lash_kernel_value::ContractId, lash_kernel_value::ImportSlot,
    lash_kernel_value::OperationRequirement, lash_kernel_value::Signature, lash_kernel_value::Type,
};

pub static TYPESCRIPT: LazyLock<Vec<HelperOperation>> =
    LazyLock::new(|| declarations("typescript"));
pub static PYTHON: LazyLock<Vec<HelperOperation>> = LazyLock::new(|| declarations("python"));

fn declarations(package: &str) -> Vec<HelperOperation> {
    [
        (
            ImportSlot(0),
            "clock",
            Signature {
                params: vec![],
                result: Type::Timestamp,
            },
        ),
        (
            ImportSlot(1),
            "random",
            Signature {
                params: vec![],
                result: Type::Bytes,
            },
        ),
        (
            ImportSlot(2),
            "sleep",
            lash_kernel_dialect::sleep_signature(),
        ),
    ]
    .into_iter()
    .map(|(slot, member, signature)| HelperOperation {
        slot,
        name: lash_kernel_dialect::service_name(package, member),
        requirement: OperationRequirement::new(
            signature,
            ContractId::new(format!("lash.dialect.{package}.{member}")),
        ),
    })
    .collect()
}
