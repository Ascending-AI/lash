//! Why a document is refused, as typed data.
use lash_kernel_value::sites::Site;
pub use lash_kernel_value::{
    CheckFailure as Refused, CheckReason as RefusalReason, CheckRefusal as Refusal,
};

pub use {lash_kernel_value::types::SignatureMismatch, lash_kernel_value::types::SignatureSide};
pub(crate) fn refused(site: Option<&Site>, reason: RefusalReason) -> Refused {
    Refused {
        site: site.cloned(),
        reason,
    }
}
