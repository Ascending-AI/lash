//! Retention of a cell's document and helper declarations.

use lash_core::RuntimeExecutionContext;
use lash_kernel_doc::IdentifiedDocument;

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
    document: &IdentifiedDocument,
    functions: &lash_kernel_doc::FunctionRegistry,
) -> Result<(), String> {
    let claim = ctx.execution_claim().map_err(|error| error.to_string())?;
    let declarations = lash_kernel_doc::CheckedDeclarations::required(document, functions)
        .map_err(|error| error.to_string())?;
    let documents =
        lash_vm_runtime::KernelDocuments::new(ctx.actor_context().backend().module_artifacts());
    let frame = match ctx.frame_claim() {
        Ok(frame) => Some(frame),
        Err(lash_execution_contracts::plugin_error::PluginError::NotASessionRuntime { .. }) => None,
        Err(error) => return Err(error.to_string()),
    };
    let retained = match &frame {
        Some(frame) => {
            documents
                .publish_retained(&claim, frame, document, &declarations)
                .await
        }
        None => documents.publish(&claim, document, &declarations).await,
    };
    let published = match (retained, &frame) {
        // The refusal held nothing: the frame's fence is checked first.
        (
            Err(lash_vm_runtime::DocumentStoreError::Store(
                lash_execution_contracts::module_artifacts::ArtifactStoreError::ReferrerEnded {
                    referrer,
                },
            )),
            Some(frame),
        ) if referrer == frame.referrer() => {
            documents.publish(&claim, document, &declarations).await
        }
        (published, _) => published,
    };
    published.map(|_| ()).map_err(|error| error.to_string())
}
