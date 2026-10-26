//! Reduce a document to one entry using the checker's reference derivation.

use lash_kernel_value::names::Name;
use std::collections::BTreeSet;

use lash_kernel_check::{Environment, entry_reachability};
use lash_kernel_doc::Document;

use crate::refusal::refused;
use crate::{Applied, EditDiagnostic, EditDiagnosticKind, EditRefusal};

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
    pub requirements: BTreeSet<lash_kernel_package::ImportSlot>,
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
    let reached = entry_reachability(document, entry, environment.functions).map_err(|_error| {
        EditRefusal {
            diagnostics: vec![EditDiagnostic {
                edit: None,
                location: None,
                kind: EditDiagnosticKind::NoSuchEntry {
                    name: entry.clone(),
                },
            }],
        }
    })?;
    let mut extracted = document.clone();
    extracted.main = None;
    extracted.private_bindings.clear();
    extracted
        .library
        .0
        .retain(|id| reached.functions.contains_key(id));
    extracted.exports.retain(|name, _| name == entry);
    extracted
        .functions
        .retain(|id, _| reached.functions.contains_key(id));
    extracted
        .imports
        .retain(|slot, _| reached.imports.contains(slot));
    let admitted =
        lash_kernel_check::admit(&extracted, environment).map_err(|error| refused(None, error))?;
    let survivors = extracted
        .ordinal_nodes()
        .into_iter()
        .map(|(site, _)| lash_kernel_doc::Survivor {
            from: site.clone(),
            to: site,
            edited: false,
        })
        .collect();
    let correspondence =
        lash_kernel_doc::Correspondence::of(document.identity(), extracted.identity(), survivors)
            .ok_or_else(|| refused(None, lash_kernel_check::Refusal { errors: vec![] }))?;
    Ok(Extraction {
        requirements: reached.imports,
        document: extracted,
        applied: Some(Applied {
            admitted,
            correspondence,
        }),
    })
}
