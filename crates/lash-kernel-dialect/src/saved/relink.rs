//! Fresh cell fulfilment replaces only the saved function's defining Link.
//! Function bodies, FunctionIds and frozen values are immutable.
use super::Unusable;
use lash_kernel_value::CallableId;
use lash_kernel_value::names::Name;
use lash_kernel_value::package::Link;

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RelinkGap {
    #[error("the new Link names another Document")]
    Document,
    #[error("the defining function is absent from the Document")]
    Removed,
}
/// The caller freshly fulfils the saved Document under accepted round advertisements.
/// This function consumes that validated Link and never rebinds a body by global function name.
/// Answers the saved function `label`'s callable under `link`: its
/// defining `document` and function as they were.
///
/// # Errors
///
/// [`Unusable::Relink`] when `link` names another document, or the
/// document no longer defines the function.
pub fn relink_saved(
    label: &Name,
    document: &lash_kernel_doc::IdentifiedDocument,
    callable: CallableId,
    link: &Link,
) -> Result<CallableId, Unusable> {
    let refuse = |gap| Unusable::Relink {
        function: label.clone(),
        callee: callable.function.to_string(),
        gap,
    };
    if link.document != document.id() {
        return Err(refuse(RelinkGap::Document));
    }
    if !document.functions.contains_key(&callable.function) {
        return Err(refuse(RelinkGap::Removed));
    }
    Ok(CallableId {
        link: link.identity(),
        function: callable.function,
    })
}
