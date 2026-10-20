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
    let documents =
        lash_vm_runtime::KernelDocuments::new(ctx.actor_context().backend().module_artifacts());
    // A cell written under a stored surface lists functions this node does
    // not hold: their declarations are the definitions its install stored,
    // read and checked like any pinned code (FIG-6985).
    let stored = documents
        .stored_definitions(document, functions)
        .await
        .map_err(|error| error.to_string())?
        .map_err(|unretained| format!("the cell's code is not stored: {unretained:?}"))?;
    let declarations = if stored.is_empty() {
        lash_kernel_doc::CheckedDeclarations::required(document, functions)
    } else {
        let declared = lash_vm_broker::kernel::loader::declare(
            document.manifest.functions.keys().copied(),
            document.manifest.kernel,
            functions,
            &stored,
        )
        .map_err(|refusal| refusal.to_string())?;
        lash_kernel_doc::CheckedDeclarations::required(document, &declared)
    }
    .map_err(|error| error.to_string())?;
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

/// The execution closure of the cell's `document` over the definitions it
/// was written against (FIG-6985): each `functions`, the node's registry,
/// holds as it holds it, and each other as the store keeps it, admitted
/// under the store's internal-operation authority (R5). A fresh cell was
/// lowered against `functions`, so it reads nothing from the store.
///
/// # Errors
///
/// The store's failure, a definition it does not keep or refuses, or a
/// closure that does not encode, as text.
pub(super) async fn pinned_closure(
    ctx: &RuntimeExecutionContext<'_>,
    document: &lash_kernel_doc::Document,
    functions: &lash_kernel_doc::FunctionRegistry,
) -> Result<lash_vm_broker::kernel::closure::Closure, String> {
    let documents =
        lash_vm_runtime::KernelDocuments::new(ctx.actor_context().backend().module_artifacts());
    let stored = documents
        .stored_definitions(document, functions)
        .await
        .map_err(|error| error.to_string())?
        .map_err(|unretained| format!("the cell's code is not retained: {unretained:?}"))?;
    // Stored code performs a package's internal operation only as that
    // package's member (R5).
    lash_vm_runtime::admit_internal(
        &lash_vm_runtime::StoredAuthority::new(ctx.actor_context().backend().stores()),
        &stored,
    )
    .await
    .map_err(|unread| unread.to_string())?
    .map_err(|refusal| refusal.to_string())?;
    lash_vm_broker::kernel::closure::derive(
        document.manifest.functions.keys().copied(),
        functions,
        &stored,
    )
    .map_err(|refusal| refusal.to_string())
}
