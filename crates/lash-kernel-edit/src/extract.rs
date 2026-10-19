//! Reduce a document to one entry using the checker's reference derivation.

use std::collections::BTreeSet;

use crate::apply::Working;
use lash_kernel_check::{Environment, entry_reachability};
use lash_kernel_doc::{Document, EffectName, Name, Site, Unit};

use crate::refusal::refused;
use crate::{Applied, Edit, EditDiagnostic, EditDiagnosticKind, EditRefusal};

/// An entry extraction and the effects its retained code performs.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Extraction {
    /// The admitted extracted document.
    pub document: Document,
    /// In-process admission and surviving correspondence, present on extraction.
    /// Omitted on the wire; a decoded result carries only the document and effects.
    #[serde(skip)]
    pub applied: Option<Applied>,
    /// Exactly the effects performed by the entry, its reachable declared
    /// functions and closures, and the library bodies they reach. Unrelated
    /// entries and `main` contribute none.
    pub requirements: BTreeSet<EffectName>,
}

/// Extract only `entry` and its reachable declared functions and closures.
/// Main, private bindings and unrelated declarations are removed atomically.
/// The manifest is derived and the result admitted in `environment` before
/// returning it. The input is unchanged on success and on refusal.
///
/// # Errors
/// Unknown entries, dangling references and checker causes remain typed.
pub fn extract_entry(
    document: &Document,
    entry: &Name,
    environment: &Environment<'_>,
) -> Result<Extraction, EditRefusal> {
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
    let base = document.identity();
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
    let mut working = Working::open(document);
    for edit in edits {
        working.edit(&edit).map_err(|fault| match *fault {
            crate::apply::FaultKind::At(location, kind) => EditRefusal {
                diagnostics: vec![EditDiagnostic {
                    edit: None,
                    location,
                    kind,
                }],
            },
            crate::apply::FaultKind::Unlinked(refusal) => refused(None, refusal),
        })?;
    }
    let published = working.publish(base, environment)?;
    Ok(Extraction {
        // The extracted document requires what its manifest lists: its own
        // performs and those of the bodies it reaches (`K-DOC-002`).
        requirements: published
            .document
            .manifest
            .effects
            .keys()
            .cloned()
            .collect(),
        document: published.document,
        applied: Some(Applied {
            correspondence: published.correspondence,
            admitted: published.admitted,
        }),
    })
}
