//! Cell-start relinking of session saved functions (FIG-6926).

use super::*;

impl CodeModeExecutionState {
    /// Relinks every carried saved function by name to `packages`, the
    /// package surface the cell runs on (K-LIB-009, RELINK-CLOSURE). Each relinked function is re-saved with its new
    /// execution closure, which the capture that names it stages. One the
    /// set does not serve is dropped with its binding and listed as not
    /// carried, naming it and the callee. The session then records
    /// `packages` as the set its functions are linked to: a cell of the
    /// same set relinks and re-saves nothing.
    ///
    /// # Errors
    ///
    /// A library that does not assemble, and the pages' faults.
    pub(in crate::executor) async fn relink(
        &mut self,
        documents: &lash_vm_runtime::KernelDocuments,
        boundary: &lash_vm_runtime::HostBoundary,
        claim: &lash_core::ReferrerClaim,
    ) -> Result<BTreeSet<lash_vocabulary::LinkId>, RelinkError> {
        let mut functions = self.view.functions.clone().unwrap_or_default();
        let mut rewritten = Vec::new();
        let mut dropped = Vec::new();
        for (name, function) in &mut functions {
            let saved = function.clone().into_saved();
            documents
                .admit_identified_document(claim, &function.document)
                .await
                .map_err(RelinkError::Package)?;
            let fulfilled = documents
                .fulfil(&lash_execution_contracts::process::linking::FulfilRequest {
                    document: function.document.id(),
                    selector: lash_execution_contracts::process::linking::ReachSelector::AllExports,
                    snapshot: boundary.snapshot.clone(),
                    context: boundary.context.clone(),
                    purpose: lash_execution_contracts::process::linking::LinkPurpose::Cell,
                })
                .await
                .map_err(RelinkError::Package)?;
            documents
                .hold(
                    claim,
                    &lash_execution_contracts::process::linking::ContentRef::Link(
                        fulfilled.inspection.link,
                    ),
                )
                .await
                .map_err(RelinkError::Package)?;
            match lash_kernel_dialect::relink_saved(&saved, &fulfilled.content) {
                Ok(relinked) if relinked.callable == function.callable => {}
                Ok(relinked) => {
                    function.callable = relinked.callable;
                    function.link = relinked.callable.link;
                    let content = documents
                        .load_linked(function.link)
                        .await
                        .map_err(|error| RelinkError::Library(error.to_string()))?;
                    let reach = documents
                        .inspect(
                            &content,
                            function.link,
                            &lash_kernel_package::ReachSelection::AllExports,
                        )
                        .map_err(|error| RelinkError::Library(error.to_string()))?;
                    let held = self
                        .kernel
                        .library()
                        .map_err(|error| RelinkError::Library(error.to_string()))?;
                    let loaded = lash_vm_broker::kernel::loader::declare_reach(
                        &function.document,
                        &content
                            .documents
                            .iter()
                            .map(|(id, document)| (*id, Arc::clone(document)))
                            .collect(),
                        &reach.functions,
                        &reach.machine,
                        &held,
                        documents.helpers().as_ref(),
                        &lash_kernel_doc::Selection::Fresh,
                    )
                    .map_err(|error| RelinkError::Library(error.to_string()))?;
                    rewritten.push((
                        name.clone(),
                        function.description(),
                        loaded.closure,
                        function.callable,
                    ));
                }
                Err(unusable) => dropped.push((
                    name.clone(),
                    function.held.as_ref().map(|held| held.binding.clone()),
                    unusable.to_string(),
                    saved.environment,
                )),
            }
        }
        for (name, _, _, _) in &dropped {
            functions.remove(name);
        }
        // A relinked record names its new closure, which the cell's run
        // then names unread: it is staged in the turn's spool beside the
        // pages that name it, and published with them.
        let mut spool = self.turn_spool()?;
        for (_, _, closure, _) in &rewritten {
            for node in closure.nodes().values() {
                let node = lash_core_store::checkpoint_graph::node::VerifiedNode::check(
                    node.clone(),
                    &lash_core_store::checkpoint_graph::DecodeLimits::within_total(u64::MAX),
                )
                .map_err(|refusal| RelinkError::Library(refusal.to_string()))?;
                spool
                    .seal(node, &|_| true)
                    .map_err(|fault| RelinkError::Pages(PageAccessError::Staging(fault)))?;
            }
        }
        let mut next = self.current.clone();
        next.pins = functions
            .values()
            .flat_map(|function| function.document.functions.keys().copied())
            .collect();
        // Each dropped function's name, and the binding dropped with it.
        let mut unbound: Vec<(Name, Name)> = Vec::new();
        if let Some(session) = self.current.session
            && (!rewritten.is_empty() || !dropped.is_empty())
        {
            let edits: Vec<_> = rewritten
                .iter()
                .map(|(name, description, closure, callable)| {
                    (
                        name.clone(),
                        description.clone(),
                        PageRef {
                            id: PageId(closure.reference().0),
                            grant: PageGrant::NONE,
                        },
                        *callable,
                    )
                })
                .collect();
            let drops: Vec<(Name, Option<Name>, lash_kernel_doc::heap::EntityId)> = dropped
                .iter()
                .map(|(name, holder, _, environment)| (name.clone(), holder.clone(), *environment))
                .collect();
            let (session, removed) = self
                .pages
                .read(move |source| {
                    let mut editor = SessionEditor::open(Some(&session), source)?;
                    for (name, description, closure, callable) in &edits {
                        editor.relink_function(name, description, *closure, *callable)?;
                    }
                    // A function's binding goes with it. One saved under a
                    // generated name takes the binding that held it, while
                    // that binding still refers to it.
                    let mut removed = Vec::new();
                    for (name, holder, environment) in &drops {
                        let holder = match holder {
                            Some(holder) if editor.refers(holder, environment)? => {
                                Some(holder.clone())
                            }
                            Some(_) => None,
                            None => Some(name.clone()),
                        };
                        editor.drop_function(name)?;
                        if let Some(holder) = holder {
                            editor.remove(&holder)?;
                            removed.push((name.clone(), holder));
                        }
                    }
                    Ok((editor.seal()?, removed))
                })
                .await?;
            next.session = Some(session);
            unbound = removed;
            let root = self.current.notes;
            let functions: Vec<Name> = dropped.iter().map(|(name, _, _, _)| name.clone()).collect();
            let notes: Vec<(Name, String)> = unbound
                .iter()
                .filter_map(|(function, binding)| {
                    dropped
                        .iter()
                        .find(|(name, _, _, _)| name == function)
                        .map(|(_, _, why, _)| (binding.clone(), why.clone()))
                })
                .collect();
            next.notes = self
                .pages
                .read(move |source| {
                    edit_notes(&root, source, |edit| {
                        for name in &functions {
                            edit.saved(name, None)?;
                        }
                        for (binding, why) in &notes {
                            edit.not_carried(binding, why)?;
                        }
                        Ok(())
                    })
                })
                .await?;
        }
        if let Some(notes) = &mut self.view.notes {
            for (name, _, _, _) in &dropped {
                if let Some(note) = notes.get_mut(name) {
                    note.saved = None;
                    if note.not_carried.is_none() {
                        notes.remove(name);
                    }
                }
            }
        }
        for (function, binding) in &unbound {
            if let Some(names) = &mut self.view.names {
                names.remove(binding);
            }
            self.view.values.remove(binding);
            if let (Some(notes), Some((_, _, why, _))) = (
                &mut self.view.notes,
                dropped.iter().find(|(name, _, _, _)| name == function),
            ) {
                let note = notes.entry(binding.clone()).or_default();
                note.not_carried = Some(why.clone());
                note.saved = None;
            }
        }
        self.view.functions = Some(functions);
        self.current = if next.session.is_some() {
            self.seal_descriptor(next, spool)?
        } else {
            next
        };
        Ok(self
            .view
            .functions
            .as_ref()
            .into_iter()
            .flat_map(|functions| functions.values().map(|function| function.link))
            .collect())
    }
}
