//! Fresh cell fulfilment replaces only the saved function's defining Link.
//! Function bodies, FunctionIds and frozen values are immutable.
use super::{SavedFunction, Unusable};
use lash_vocabulary::package::Link;

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RelinkGap {
    #[error("the new Link names another Document")]
    Document,
    #[error("the defining function is absent from the Document")]
    Removed,
}
/// The caller freshly fulfils the saved Document under accepted round advertisements.
/// This function consumes that validated Link and never rebinds a body by global function name.
pub fn relink_saved(function: &SavedFunction, link: &Link) -> Result<SavedFunction, Unusable> {
    let refuse = |gap| Unusable::Relink {
        function: function.label(),
        callee: function.callable.function.to_string(),
        gap,
    };
    if link.document != function.document.id() {
        return Err(refuse(RelinkGap::Document));
    }
    if !function
        .document
        .functions
        .contains_key(&function.callable.function)
    {
        return Err(refuse(RelinkGap::Removed));
    }
    let mut relinked = function.clone();
    relinked.link = link.identity();
    relinked.callable.link = relinked.link;
    Ok(relinked)
}
