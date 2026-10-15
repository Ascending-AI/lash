//! Reduce a document to one entry using the checker's reference derivation.

use std::collections::BTreeSet;

use lash_kernel_check::entry_reachability;
use lash_kernel_doc::{Document, EffectName, Name, Site, Unit};

use crate::refusal::refused;
use crate::{Edit, EditDiagnostic, EditDiagnosticKind, EditRefusal, Transaction};

/// An entry extraction and the effects its retained code performs.
#[derive(Clone, Debug, PartialEq)]
pub struct Extraction {
    /// The edits to apply against the source document.
    pub transaction: Transaction,
    /// Exactly the effects performed by the entry and its reachable declared
    /// functions and closures. Unrelated entries and `main` contribute none.
    pub requirements: BTreeSet<EffectName>,
}

/// Plan a document containing only `entry` and its reachable declared functions.
///
/// Reachability includes calls, spawns, closures and function-reference
/// literals in every expression and atomic argument. The transaction removes
/// all `main` statements, private bindings, other entries and unreachable
/// functions. The returned requirements come from the same reachability walk.
/// It is based on `document` and does not mutate it.
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
pub fn extract_entry(document: &Document, entry: &Name) -> Result<Extraction, EditRefusal> {
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
    let reached = entry_reachability(document, entry).map_err(|error| refused(None, error))?;
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
        .filter(|name| !reached.functions.contains(*name))
    {
        edits.push(Edit::RemoveFunction { name: name.clone() });
    }
    Ok(Extraction {
        transaction: Transaction { base, edits },
        requirements: reached.effects,
    })
}
