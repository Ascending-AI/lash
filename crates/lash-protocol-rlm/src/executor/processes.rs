//! Load retained linked reach without fulfilling against current advertisements.
use lash_core::RuntimeExecutionContext;
use lash_kernel_doc::FunctionRegistry;

use lash_vocabulary::names::LinkedStart;
use std::sync::Arc;

pub(super) struct PreparedExecution {
    pub content: lash_kernel_package::LinkedContent,
    pub loaded: lash_vm_broker::kernel::loader::Assembled,
}

pub(super) async fn prepare(
    ctx: &RuntimeExecutionContext<'_>,
    start: &LinkedStart,
    held: &Arc<FunctionRegistry>,
) -> Result<PreparedExecution, String> {
    let documents = lash_vm_runtime::KernelDocuments::new(
        ctx.actor_context().backend().module_artifacts(),
        ctx.actor_context().backend().stores().package_store(),
    );
    let content = documents
        .load_linked(start.link)
        .await
        .map_err(|error| error.to_string())?;
    let reach = content
        .inspect(
            start.link,
            &lash_kernel_package::ReachSelection::Start(start.selector.clone()),
        )
        .map_err(|error| error.to_string())?;
    let link = content
        .links
        .get(&start.link)
        .ok_or("the root Link is absent")?;
    let root = content
        .documents
        .get(&link.document)
        .ok_or("the root Document is absent")?;
    let loaded = lash_vm_broker::kernel::loader::declare_reach(
        root,
        &content
            .documents
            .iter()
            .map(|(id, document)| (*id, Arc::new(document.clone())))
            .collect(),
        &reach.functions,
        &reach.machine,
        held,
    )
    .map_err(|error| error.to_string())?;
    Ok(PreparedExecution { content, loaded })
}
