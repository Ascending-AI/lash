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
use lash_kernel_doc::{Datum, IdentifiedDocument, Name, SourceMap};
use lash_vm_runtime::{KernelProcessDefinition, definition_draft, definition_of_entry};

use super::session::CodeModeExecutionState;

/// The document a cell's function references name entries of.
pub(super) struct CellEntries {
    pub document: IdentifiedDocument,
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
            let definition = definition_of_entry(&self.document, self.document.id(), function)?;
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
                let definition =
                    definition_of_entry(&self.document, self.document.id(), entry).ok()?;
                let datum =
                    lash_vm_broker::kernel::datum_from_json(&definition.to_string()).ok()?;
                Some((entry.clone(), datum))
            })
            .collect()
    }
}

/// The cell's document `document` with every saved function a
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
    document: IdentifiedDocument,
    mut source_map: Option<SourceMap>,
) -> Result<(IdentifiedDocument, Option<SourceMap>), String> {
    if document.entries.is_empty() {
        return Ok((document, source_map));
    }
    let saved: BTreeSet<Name> = state
        .lowering_functions()
        .into_keys()
        .filter(|name| document.functions.contains_key(name))
        .collect();
    if saved.is_empty() {
        return Ok((document, source_map));
    }
    let mut lowered = document.document().clone();
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
    let document = IdentifiedDocument::new(lowered);
    if let Some(map) = &mut source_map {
        map.document = document.id();
    }
    Ok((document, source_map))
}

/// Stores the cell's document under the cell's execution, so the document
/// its language execution names is readable while that execution is
/// unsettled (FIG-5576).
///
/// The document is held with the declarations of the helpers it was
/// lowered against, so it reads without this build's library (FIG-6379).
/// The frame the cell's turn was admitted on retains them across its cells,
/// so a cell writes only the declarations its frame does not hold yet, and
/// the frame's end, by a switch or the session's deletion, reclaims them
/// (FIG-6481). A cell of a process has no frame, and a frame switch's
/// follow-on runs on the frame the switch ended: there, the cell's
/// execution holds them beside the document.
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
    let declarations = lash_kernel_doc::CheckedDeclarations::required(&entries.document, functions)
        .map_err(|error| error.to_string())?;
    let documents =
        lash_vm_runtime::KernelDocuments::new(ctx.actor_context().backend().module_artifacts());
    let frame = match ctx.frame_claim() {
        Ok(frame) => Some(frame),
        Err(lash_core::PluginError::NotASessionRuntime { .. }) => None,
        Err(error) => return Err(error.to_string()),
    };
    let retained = match &frame {
        Some(frame) => {
            documents
                .publish_retained(&claim, frame, &entries.document, &declarations)
                .await
        }
        None => {
            documents
                .publish(&claim, &entries.document, &declarations)
                .await
        }
    };
    let published = match (retained, &frame) {
        // The refusal held nothing: the frame's fence is checked first.
        (
            Err(lash_vm_runtime::DocumentStoreError::Store(
                lash_core::ArtifactStoreError::ReferrerEnded { referrer },
            )),
            Some(frame),
        ) if referrer == frame.referrer() => {
            documents
                .publish(&claim, &entries.document, &declarations)
                .await
        }
        (published, _) => published,
    };
    published.map(|_| ()).map_err(|error| error.to_string())
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
        module_ref: entries.document.id().to_string(),
        bytes: entries
            .document
            .to_json()
            .map_err(|error| error.to_string())?,
    };
    for entry in entries.document.entries.keys() {
        let draft = definition_draft(&KernelProcessDefinition::of(
            &entries.document,
            entries.document.id(),
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
