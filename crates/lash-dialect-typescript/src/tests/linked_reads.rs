//! Scripted Read modes and answers for linked machine fixtures.
use super::linked::operation;
use lash_vocabulary::OperationUse;
pub(super) fn read(
    use_: &OperationUse,
    bits: u64,
) -> Result<Result<lash_kernel_doc::Datum, lash_kernel_doc::ErrorDatum>, lash_kernel_vm::HostFault>
{
    use lash_kernel_doc::{Datum, Integer, Timestamp};

    let name = operation(use_);
    Ok(Ok(if name.as_str().ends_with(".clock") {
        Datum::Timestamp(Timestamp {
            nanoseconds: Integer::from(0),
        })
    } else if name.as_str().ends_with(".random") {
        Datum::Bytes(lash_kernel_doc::Bytes::new(bits.to_be_bytes().as_slice()))
    } else {
        return Err(lash_kernel_vm::HostFault::new(format!(
            "unexpected read {name}"
        )));
    }))
}
