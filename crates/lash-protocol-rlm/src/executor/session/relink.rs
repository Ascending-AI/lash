//! Cell-start relinking of session saved functions (FIG-6926).

use super::*;

impl CodeModeExecutionState {
    /// Relinks every carried saved function by name to `packages`, the
    /// package surface the cell runs on (K-LIB-009, RELINK-CLOSURE). Each relinked function's object
    /// takes its new Link in place, under the same identity. One the
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
        code: &mut lash_vm_runtime::CellCode,
    ) -> Result<BTreeSet<lash_kernel_value::LinkId>, RelinkError> {
        let mut functions = self.view.functions.clone().unwrap_or_default();
        let mut rewritten = Vec::new();
        let mut dropped = Vec::new();
        for (name, function) in &mut functions {
            let label = function
                .held
                .as_ref()
                .map_or_else(|| name.clone(), lash_kernel_dialect::HeldAt::label);
            let fulfilled = documents
                .fulfil_cell(
                    &function.document,
                    &lash_execution_contracts::process::linking::FulfilRequest {
                        document: function.document.id(),
                        selector:
                            lash_execution_contracts::process::linking::ReachSelector::AllExports,
                        snapshot: boundary.snapshot.clone(),
                        context: boundary.context.clone(),
                        purpose: lash_execution_contracts::process::linking::LinkPurpose::Cell,
                    },
                    code,
                )
                .await
                .map_err(RelinkError::Package)?;
            match lash_kernel_dialect::relink_saved(
                &label,
                &function.document,
                function.callable,
                &fulfilled.content,
            ) {
                Ok(callable) if callable == function.callable => {
                    code.retain_link(function.link);
                }
                Ok(callable) => {
                    function.callable = callable;
                    function.link = callable.link;
                    let reach = documents
                        .inspect(
                            &code.content,
                            function.link,
                            &lash_kernel_package::ReachSelection::AllExports,
                        )
                        .map_err(|error| RelinkError::Library(error.to_string()))?;
                    let held = self
                        .kernel
                        .library()
                        .map_err(|error| RelinkError::Library(error.to_string()))?;
                    // The relinked reach must declare under the held
                    // library, as a fresh cell's does.
                    lash_vm_broker::kernel::loader::declare_reach(
                        &function.document,
                        &code
                            .content
                            .documents
                            .iter()
                            .map(|(id, document)| (*id, Arc::clone(document)))
                            .collect(),
                        &reach.functions,
                        &reach.machine,
                        &held,
                        documents.helpers().as_ref(),
                    )
                    .map_err(|error| RelinkError::Library(error.to_string()))?;
                    rewritten.push((name.clone(), function.description(), function.callable));
                }
                Err(unusable) => dropped.push((
                    name.clone(),
                    function.held.as_ref().map(|held| held.binding.clone()),
                    unusable.to_string(),
                )),
            }
        }
        for (name, _, _) in &dropped {
            functions.remove(name);
        }
        let roots: BTreeSet<_> = functions.values().map(|function| function.link).collect();
        if !roots.is_empty() {
            documents
                .hold_cell_code(claim, code, &roots, &[])
                .await
                .map_err(RelinkError::Package)?;
        }
        let spool = self.turn_spool()?;
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
            let edits = rewritten;
            let drops: Vec<(Name, Option<Name>)> = dropped
                .iter()
                .map(|(name, holder, _)| (name.clone(), holder.clone()))
                .collect();
            let (session, removed) = self
                .pages
                .read(move |source| {
                    let mut editor = SessionEditor::open(Some(&session), source)?;
                    for (name, description, callable) in &edits {
                        editor.relink_function(name, description, *callable)?;
                    }
                    // A function's binding goes with it. One saved under a
                    // generated name takes the binding that held it, while
                    // that binding still refers to it.
                    let mut removed = Vec::new();
                    for (name, holder) in &drops {
                        let holder = match holder {
                            Some(holder) if editor.refers(holder, name)? => Some(holder.clone()),
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
            let functions: Vec<Name> = dropped.iter().map(|(name, _, _)| name.clone()).collect();
            let notes: Vec<(Name, String)> = unbound
                .iter()
                .filter_map(|(function, binding)| {
                    dropped
                        .iter()
                        .find(|(name, _, _)| name == function)
                        .map(|(_, _, why)| (binding.clone(), why.clone()))
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
            for (name, _, _) in &dropped {
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
            if let (Some(notes), Some((_, _, why))) = (
                &mut self.view.notes,
                dropped.iter().find(|(name, _, _)| name == function),
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
        Ok(roots)
    }
}
