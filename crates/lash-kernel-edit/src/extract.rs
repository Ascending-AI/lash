//! Reduce a document to one entry using the checker's reference derivation.

use lash_kernel_check::reachable_functions;
use lash_kernel_doc::{Document, Name, Site, Unit};

use crate::refusal::refused;
use crate::{Edit, EditDiagnostic, EditDiagnosticKind, EditRefusal, Transaction};

/// Plan a document containing only `entry` and its reachable declared functions.
///
/// Reachability includes calls, spawns, closures and function-reference
/// literals in every expression and atomic argument. The transaction removes
/// all `main` statements, private bindings, other entries and unreachable
/// functions. It is based on `document` and does not mutate it.
///
/// Apply with [`crate::Draft::apply`] against the publication environment. Application
/// derives the remaining manifest and performs normal admission atomically.
/// Planning needs no library catalog and does not certify types, scope or the
/// environment; a planned transaction can still be refused on application.
///
/// # Errors
///
/// The entry is unknown, a reachable function is undeclared, or the document
/// cannot be canonically encoded. Refusals retain the edit/checker's typed causes.
pub fn extract_entry(document: &Document, entry: &Name) -> Result<Transaction, EditRefusal> {
    if !document.entries.contains_key(entry) {
        return Err(EditRefusal {
            diagnostics: vec![EditDiagnostic {
                edit: None,
                location: None,
                kind: EditDiagnosticKind::NoSuchEntry {
                    name: entry.clone(),
                },
            }],
        });
    }
    let reached = reachable_functions(document, entry).map_err(|error| refused(None, error))?;
    let base = crate::draft::identity(document)?;
    let mut edits = Vec::new();
    let statements: Vec<_> = (0u32..)
        .zip(&document.main)
        .map(|(index, _)| Site::new(Unit::Main, [index]))
        .collect();
    for statement in statements.into_iter().rev() {
        edits.push(Edit::RemoveStatement { statement });
    }
    edits.push(Edit::SetPrivateBindings {
        names: Default::default(),
    });
    for function in document.entries.keys().filter(|name| *name != entry) {
        edits.push(Edit::RemoveEntry {
            function: function.clone(),
        });
    }
    for name in document
        .functions
        .keys()
        .filter(|name| !reached.contains(*name))
    {
        edits.push(Edit::RemoveFunction { name: name.clone() });
    }
    Ok(Transaction { base, edits })
}
