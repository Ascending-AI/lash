//! Load retained linked reach without fulfilling against current advertisements.
use lash_kernel_doc::FunctionRegistry;

use lash_vocabulary::names::LinkedStart;
use std::sync::Arc;

pub(super) struct PreparedExecution {
    pub content: lash_kernel_package::LinkedContent,
    pub documents: std::collections::BTreeMap<
        lash_vocabulary::DocumentId,
        lash_kernel_doc::IdentifiedDocument,
    >,
    pub loaded: lash_vm_broker::kernel::loader::Assembled,
}

pub(super) async fn prepare(
    documents: &lash_vm_runtime::KernelDocuments,
    start: &LinkedStart,
    held: &Arc<FunctionRegistry>,
    authored: &std::collections::BTreeSet<lash_vocabulary::LinkId>,
) -> Result<PreparedExecution, String> {
    let mut content = documents
        .load_linked(start.link)
        .await
        .map_err(|error| error.to_string())?;
    let mut reach = content
        .inspect(
            start.link,
            &lash_kernel_package::ReachSelection::Start(start.selector.clone()),
        )
        .map_err(|error| error.to_string())?;
    // Saved cell functions are roots too: their values arrive through the
    // session heap rather than an Import in the new cell's document.
    for link in authored.iter().filter(|link| **link != start.link) {
        let saved = documents
            .load_linked(*link)
            .await
            .map_err(|error| error.to_string())?;
        let saved_reach = saved
            .inspect(*link, &lash_kernel_package::ReachSelection::AllExports)
            .map_err(|error| error.to_string())?;
        content.documents.extend(saved.documents);
        content.links.extend(saved.links);
        content.packages.extend(saved.packages);
        for (link, functions) in saved_reach.functions {
            reach.functions.entry(link).or_default().extend(functions);
        }
        reach.machine.kernels.extend(saved_reach.machine.kernels);
        reach.machine.natives.extend(saved_reach.machine.natives);
    }
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
    let mut identified = std::collections::BTreeMap::new();
    for id in content.documents.keys() {
        let document = documents
            .get_shared(id)
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("Document {id} is absent"))?;
        identified.insert(*id, document);
    }
    Ok(PreparedExecution {
        content,
        loaded,
        documents: identified,
    })
}
