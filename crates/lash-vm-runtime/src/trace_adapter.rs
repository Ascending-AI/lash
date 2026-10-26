//! Runtime projections into neutral trace observations.
pub fn workflow_overlay_document(
    document: &lash_kernel_check::WorkflowDocument,
) -> lash_trace::WorkflowOverlayDocument {
    let mut sites: Vec<_> = document
        .graph()
        .execution_sites()
        .iter()
        .map(|site| site.site.clone())
        .collect();
    // Library effect identities stay in their library unit. Index their
    // exact action sites from the same structural child inventory.
    for (id, definition) in document.declarations() {
        if let Some(body) = definition.body() {
            let mut pending = vec![(lash_kernel_doc::Node::Block(&body.block), Vec::new())];
            while let Some((node, path)) = pending.pop() {
                if matches!(
                    node,
                    lash_kernel_doc::Node::Action(_)
                        | lash_kernel_doc::Node::Expr(lash_kernel_doc::Expr::Act(_))
                ) {
                    sites.push(lash_kernel_value::sites::Site::new(
                        lash_kernel_value::sites::Unit::Library(*id),
                        path.clone(),
                    ));
                }
                pending.extend(
                    node.children()
                        .into_iter()
                        .enumerate()
                        .map(|(index, child)| {
                            let mut path = path.clone();
                            path.push(index as u32);
                            (child, path)
                        }),
                );
            }
        }
    }
    lash_trace::WorkflowOverlayDocument::new(document.reference().clone(), sites)
}
