//! The processes a cell declares.
//!
//! A cell's document lists the functions a host may start as its entries.
//! Before the cell runs, each entry is published as a process definition
//! over the cell's document, under the cell's execution, and held by the
//! frame so a later cell can start it (ADR 0113 §3.1). In the cell a
//! process is a function reference; where it leaves the cell (a tool's
//! input, the cell's answer) it is that entry's definition, which is data.
//! A function a later cell starts is the session's saved function, which
//! that cell declares under an entry of its own.

use lash_core::RuntimeExecutionContext;
use lash_kernel_doc::{Datum, Document, DocumentId};
use lash_vm_runtime::{KernelProcessDefinition, definition_draft, definition_of_entry};

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
) -> Result<(), String> {
    let claim = ctx.execution_claim().map_err(|error| error.to_string())?;
    lash_vm_runtime::KernelDocuments::new(ctx.actor_context().backend().module_artifacts())
        .publish(&claim, &entries.document)
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
        let draft = definition_draft(&KernelProcessDefinition {
            document: entries.identity,
            entry: entry.clone(),
        })
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
