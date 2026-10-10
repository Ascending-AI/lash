//! Carrying a saved function to the next kernel version.
//!
//! A saved function is self-contained: its code is a document that
//! declares it and nothing else, and its captures are data. So each one
//! migrates alone, by the document rewrite: no run stands in it and no
//! other document names its nodes.

use lash_kernel_dialect::SavedFunction;
use lash_kernel_doc::FunctionCatalog;

use crate::migration::{DocumentRefusal, Migration};

/// `function`, its code rewritten for `migration`'s next version. Its name,
/// its captures and what its dialect stated of it are kept: a capture is
/// data, and one that references another saved function names it, which no
/// rewrite of that function changes.
///
/// # Errors
///
/// The document rewrite's refusal, and [`DocumentRefusal::FormNotCarried`]
/// when the rewritten document no longer declares the function under its
/// name.
pub fn saved_function(
    migration: &Migration,
    function: &SavedFunction,
    functions: &dyn FunctionCatalog,
) -> Result<SavedFunction, DocumentRefusal> {
    let rewritten = (migration.document)(&function.document, functions)?;
    if !rewritten.document.functions.contains_key(&function.name) {
        return Err(DocumentRefusal::FormNotCarried {
            site: lash_kernel_doc::Site::new(
                lash_kernel_doc::Unit::Function(function.name.clone()),
                [],
            ),
            detail: format!(
                "the rewritten document does not declare `{}`, the function it saves",
                function.name
            ),
        });
    }
    Ok(SavedFunction {
        name: function.name.clone(),
        document: rewritten.document,
        captures: function.captures.clone(),
        written: function.written.clone(),
        token: function.token.clone(),
    })
}
