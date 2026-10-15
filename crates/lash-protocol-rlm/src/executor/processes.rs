//! The processes a cell declares.
//!
//! A cell's document lists the functions a host may start as its entries.
//! Before the cell runs, each entry is published as a process definition
//! over the cell's document, under the cell's execution, and held by the
//! frame so a later cell can start it (ADR 0113 §3.1). In the cell a
//! process is a function reference; where it leaves the cell (a tool's
//! input, a session binding, the cell's answer) it is that entry's
//! definition, which is data. A function a later cell starts is the
//! session's saved function, which that cell declares under an entry of
//! its own, with its captures as constants: a process has no session.

use std::collections::{BTreeMap, BTreeSet};

use lash_core::RuntimeExecutionContext;
use lash_kernel_doc::{Datum, Document, DocumentId, Name, SourceMap};
use lash_vm_runtime::{KernelProcessDefinition, definition_draft, definition_of_entry};

use super::session::CodeModeExecutionState;

/// The document a cell's function references name entries of.
pub(super) struct CellEntries {
    pub document: Document,
    pub identity: DocumentId,
}

impl CellEntries {
    /// `value` as it leaves the cell: every function reference in it is the
    /// definition of the entry it names.
    ///
    /// # Errors
    ///
    /// A reference to a function that is not an entry.
    pub(super) fn leaving(&self, value: &Datum) -> Result<Datum, String> {
        if self.document.entries.is_empty() {
            return Ok(value.clone());
        }
        lash_vm_runtime::with_definitions(value, &|function| {
            let definition = definition_of_entry(&self.document, self.identity, function)?;
            lash_vm_broker::kernel::datum_from_json(&definition.to_string())
                .map_err(|error| error.to_string())
        })
    }

    /// Each entry's definition, as a session binding that held the entry
    /// holds it once the cell ends.
    pub(super) fn definitions(&self) -> BTreeMap<Name, Datum> {
        self.document
            .entries
            .keys()
            .filter_map(|entry| {
                let definition = definition_of_entry(&self.document, self.identity, entry).ok()?;
                let datum =
                    lash_vm_broker::kernel::datum_from_json(&definition.to_string()).ok()?;
                Some((entry.clone(), datum))
            })
            .collect()
    }
}

/// The cell's document `document` (its JSON) with every saved function a
/// process of one of its entries runs declared with its captures as
/// constants, read from the session's pages, and `source_map` locating
/// that document: a process starts with no session, so a saved function
/// crosses into it by value. Both as given when no entry runs one that
/// captures.
///
/// # Errors
///
/// The session's pages' faults, or a capture with no constant, as text.
pub(super) async fn declare_saved_captures(
    state: &mut CodeModeExecutionState,
    document: String,
    mut source_map: Option<SourceMap>,
) -> Result<(String, Option<SourceMap>), String> {
    // A document that is not one is refused where the cell reads it.
    let Ok(mut lowered) = Document::from_json(&document) else {
        return Ok((document, source_map));
    };
    if lowered.entries.is_empty() {
        return Ok((document, source_map));
    }
    let saved: BTreeSet<Name> = state
        .lowering_functions()
        .into_keys()
        .filter(|name| lowered.functions.contains_key(name))
        .collect();
    let mut declared = BTreeSet::new();
    let mut changed = false;
    // A capture that names another saved function reaches it too.
    loop {
        let reached: BTreeSet<Name> = lash_kernel_dialect::reached_by_entries(&lowered, &saved)
            .difference(&declared)
            .cloned()
            .collect();
        if reached.is_empty() {
            break;
        }
        declared.extend(reached.iter().cloned());
        let captures = state
            .saved_captures(reached)
            .await
            .map_err(|error| error.to_string())?;
        changed |= captures.values().any(|captured| !captured.is_empty());
        lash_kernel_dialect::declare_captures(&mut lowered, &captures)
            .map_err(|error| error.to_string())?;
    }
    if !changed {
        return Ok((document, source_map));
    }
    if let Some(map) = &mut source_map {
        map.document = lowered.identity().map_err(|error| error.to_string())?;
    }
    let document = lowered.to_json().map_err(|error| error.to_string())?;
    Ok((document, source_map))
}

/// Stores the cell's document under the cell's execution, so the document
/// its language execution names is readable while that execution is
/// unsettled (FIG-5576).
///
/// # Errors
///
/// The store's refusal or failure, as text.
pub(super) async fn retain_document(
    ctx: &RuntimeExecutionContext<'_>,
    entries: &CellEntries,
    functions: &lash_kernel_doc::FunctionRegistry,
) -> Result<(), String> {
    let claim = ctx.execution_claim().map_err(|error| error.to_string())?;
    // The cell's document is held with the declarations of the helpers it
    // was lowered against, so it reads without this build's library
    // (FIG-6379).
    let declarations = lash_kernel_doc::required_declarations(&entries.document, functions)
        .map_err(|error| error.to_string())?;
    lash_vm_runtime::KernelDocuments::new(ctx.actor_context().backend().module_artifacts())
        .publish(&claim, &entries.document, &declarations)
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// Publishes every entry of the cell's document as a process definition.
///
/// # Errors
///
/// The publication's refusal or failure, as text.
pub(super) async fn publish_entries(
    ctx: &RuntimeExecutionContext<'_>,
    entries: &CellEntries,
) -> Result<(), String> {
    if entries.document.entries.is_empty() {
        return Ok(());
    }
    let module = lash_core::DeclaredModuleArtifact {
        module_ref: entries.identity.to_string(),
        bytes: entries
            .document
            .to_json()
            .map_err(|error| error.to_string())?,
    };
    for entry in entries.document.entries.keys() {
        let draft = definition_draft(&KernelProcessDefinition::of(
            &entries.document,
            entries.identity,
            entry.clone(),
        ))
        .map_err(|error| error.to_string())?;
        let definition = ctx
            .publish_compiled_definition(
                format!("literal-definition:{}", draft.id()),
                draft,
                Some(module.clone()),
            )
            .await
            .map_err(|error| error.to_string())?;
        // A process has no frame: its cell's execution alone holds them.
        let Ok(claim) = ctx.frame_claim() else {
            continue;
        };
        let engines = ctx.definition_engines();
        let ports = engines
            .artifact_ports()
            .ok_or_else(|| "definition artifact ports are unavailable".to_owned())?;
        match ports
            .acquire_definition(engines, &claim, &definition.id)
            .await
            .map_err(|error| error.to_string())?
        {
            lash_core::DefinitionAcquisition::Held(_) | lash_core::DefinitionAcquisition::Ended => {
            }
        }
    }
    Ok(())
}
