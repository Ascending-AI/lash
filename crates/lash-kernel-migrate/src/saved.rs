//! Carrying a saved function to the next kernel version.
//!
//! A saved function is self-contained: its code is a document that
//! declares it and nothing else, and its captures are data. So each one
//! migrates alone, by the document rewrite: no run stands in it and no
//! other document names its nodes. A function value a host holds is
//! carried the same way.

use lash_kernel_dialect::SavedFunction;
use lash_kernel_doc::{Document, FunctionCatalog, FunctionValue, Name, Site, Unit};

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
    let document = rooted(migration, &function.document, &function.name, functions)?;
    Ok(SavedFunction {
        name: function.name.clone(),
        document,
        captures: function.captures.clone(),
        release: function.release,
        references: function.references.clone(),
        helper_owners: function.helper_owners.clone(),
        written: function.written.clone(),
        token: function.token.clone(),
    })
}

/// `value`, a function value a host holds, its code rewritten for
/// `migration`'s next version as its saved counterpart's is. Its captures
/// are constants in its code, so they are carried with it; its entry keeps
/// its name and signature, and its identity follows its new document.
///
/// # Errors
///
/// The document rewrite's refusal, and [`DocumentRefusal::FormNotCarried`]
/// when the rewritten document no longer declares its entry, or declares
/// it with another signature.
pub fn function_value(
    migration: &Migration,
    value: &FunctionValue,
    functions: &dyn FunctionCatalog,
) -> Result<FunctionValue, DocumentRefusal> {
    let document = rooted(migration, &value.document, &value.entry, functions)?;
    if document.entries.get(&value.entry) != value.document.entries.get(&value.entry) {
        return Err(DocumentRefusal::FormNotCarried {
            site: Site::new(Unit::Function(value.entry.clone()), []),
            detail: format!(
                "the rewritten document changes the signature of `{}`, the entry the value starts",
                value.entry
            ),
        });
    }
    Ok(FunctionValue {
        document,
        entry: value.entry.clone(),
    })
}

/// `document` rewritten for `migration`'s next version, still declaring
/// `root`, the function it is the code of: a saved function's or a function
/// value's, with the start adapters it holds rewritten with it.
fn rooted(
    migration: &Migration,
    document: &Document,
    root: &Name,
    functions: &dyn FunctionCatalog,
) -> Result<Document, DocumentRefusal> {
    let rewritten = (migration.document)(document, functions)?;
    if !rewritten.document.functions.contains_key(root) {
        return Err(DocumentRefusal::FormNotCarried {
            site: Site::new(Unit::Function(root.clone()), []),
            detail: format!(
                "the rewritten document does not declare `{root}`, the function it is the code of"
            ),
        });
    }
    Ok(rewritten.document)
}
