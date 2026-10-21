//! Carrying a saved function to the next kernel version.
//!
//! A saved function is self-contained: its code is a document that
//! declares it and nothing else, and its captures are data. So each one
//! migrates alone, by the document rewrite: no run stands in it and no
//! other document names its nodes. A function value a host holds is
//! carried the same way.

use lash_kernel_dialect::SavedFunction;
use lash_kernel_doc::Document;
use lash_kernel_doc::FunctionCatalog;
use lash_kernel_doc::FunctionValue;
use lash_vocabulary::legacy_names::Name;
use lash_vocabulary::sites::Site;
use lash_vocabulary::sites::Unit;

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
    // A library function the rewrite renamed is held against the next
    // version's signature of it.
    let against = document
        .document
        .manifest
        .functions
        .keys()
        .filter_map(|id| {
            let signature = function
                .against
                .get(id)
                .cloned()
                .or_else(|| functions.header(id).map(|header| header.signature.clone()))?;
            Some((*id, signature))
        })
        .collect();
    Ok(SavedFunction {
        name: function.name.clone(),
        document: document.document,
        captures: function.captures.clone(),
        closure: function.closure,
        helper_owners: function.helper_owners.clone(),
        references: function.references.clone(),
        written: function.written.clone(),
        token: function.token.clone(),
        held: function.held.clone(),
        against,
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
    let rewritten = rooted(migration, &value.document, &value.entry, functions)?;
    let document = rewritten.document;
    let captions = value
        .captions
        .rekey(&rewritten.correspondence, &document)
        .ok_or_else(|| DocumentRefusal::FormNotCarried {
            site: Site::new(Unit::Function(value.entry.clone()), []),
            detail: "the rewrite's correspondence names another document".into(),
        })?
        .captions;
    let same_start = document
        .entries
        .get(&value.entry)
        .zip(value.document.entries.get(&value.entry))
        .is_some_and(|(to, from)| {
            to.signature == from.signature
                && std::mem::discriminant(&to.convention)
                    == std::mem::discriminant(&from.convention)
        });
    if !same_start {
        return Err(DocumentRefusal::FormNotCarried {
            site: Site::new(Unit::Function(value.entry.clone()), []),
            detail: format!(
                "the rewritten document changes the signature or calling convention of `{}`, the entry the value starts",
                value.entry
            ),
        });
    }
    Ok(FunctionValue {
        document,
        entry: value.entry.clone(),
        captions,
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
) -> Result<crate::migration::Rewritten, DocumentRefusal> {
    let rewritten = (migration.document)(document, functions)?;
    if !rewritten.document.functions.contains_key(root) {
        return Err(DocumentRefusal::FormNotCarried {
            site: Site::new(Unit::Function(root.clone()), []),
            detail: format!(
                "the rewritten document does not declare `{root}`, the function it is the code of"
            ),
        });
    }
    Ok(rewritten)
}
