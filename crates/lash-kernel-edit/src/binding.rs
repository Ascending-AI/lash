//! Binding edits use checker identity, including uses inside nested closures.
use crate::EditDiagnosticKind as Cause;
use crate::apply::{Fault, FaultKind, Working, at, node};
use lash_kernel_doc::{Expr, FunctionCatalog, NodeMut, Stmt};
use lash_vocabulary::{Name, sites::Site};

impl Working {
    pub(super) fn rename_binding(
        &mut self,
        declaration: &Site,
        name: &Name,
        catalog: &dyn FunctionCatalog,
    ) -> Result<(), Fault> {
        self.reidentify().map_err(|message| {
            at(
                declaration,
                Cause::Invalid(lash_kernel_doc::InvalidReason::Document { message }),
            )
        })?;
        let site = self.locate(declaration)?;
        let before = lash_kernel_check::derive(&self.document, catalog)
            .map_err(|e| Box::new(FaultKind::Unlinked(e)))?;
        let binding = before
            .bindings()
            .iter()
            .find(|b| {
                before.node(b.declared_at).site == site
                    && !matches!(
                        b.kind,
                        lash_kernel_check::BindingKind::Param
                            | lash_kernel_check::BindingKind::Capture
                    )
            })
            .ok_or_else(|| at(declaration, Cause::NotValueTarget))?;
        if binding.name == *name {
            return Ok(());
        }
        if before
            .bindings()
            .iter()
            .any(|b| b.scope == binding.scope && b.name == *name)
        {
            return Err(at(declaration, Cause::NameCollision { name: name.clone() }));
        }
        for checked in before.nodes() {
            if crate::canvas::references(&checked.kind)
                .iter()
                .any(|reference| **reference == lash_kernel_check::Reference::Binding(binding.id))
            {
                self.shadow(&checked.site).edited = true;
            }
        }
        self.shadow(&site).edited = true;
        lash_kernel_check::rename_binding_uses(&mut self.document, &before, binding.id, name);
        match node(&mut self.document, &site) {
            Some(NodeMut::Stmt(Stmt::Let { name: written, .. }))
            | Some(NodeMut::Expr(Expr::Let { name: written, .. })) => *written = name.clone(),
            Some(NodeMut::Stmt(Stmt::For { binding, .. })) => *binding = name.clone(),
            Some(NodeMut::Stmt(Stmt::Try(scope))) => {
                let Some(catch) = &mut scope.catch else {
                    return Err(at(declaration, Cause::NotValueTarget));
                };
                catch.binding = name.clone();
            }
            _ => return Err(at(declaration, Cause::NotValueTarget)),
        }
        let ids = self.reidentify().map_err(|message| {
            at(
                declaration,
                Cause::Invalid(lash_kernel_doc::InvalidReason::Document { message }),
            )
        })?;
        let after = lash_kernel_check::derive(&self.document, catalog)
            .map_err(|e| Box::new(FaultKind::Unlinked(e)))?;
        // Rehashing changes function coordinates, never which declaration a use names.
        let translate = |site: &Site| {
            let mut site = site.clone();
            if let lash_vocabulary::sites::Unit::Library(id) = &mut site.unit {
                *id = ids.get(id).copied().unwrap_or(*id);
            }
            site
        };
        for old in before.nodes() {
            let Some(new) = after.node_at(&translate(&old.site)) else {
                return Err(at(declaration, Cause::EscapingBinding));
            };
            let old_refs = crate::canvas::references(&old.kind);
            let new_refs = crate::canvas::references(&new.kind);
            let same = old_refs.len() == new_refs.len()
                && old_refs
                    .iter()
                    .zip(new_refs)
                    .all(|(old, new)| match (old, new) {
                        (
                            lash_kernel_check::Reference::Binding(old),
                            lash_kernel_check::Reference::Binding(new),
                        ) => {
                            let old = before.binding(*old);
                            let new = after.binding(*new);
                            translate(&before.node(old.declared_at).site)
                                == after.node(new.declared_at).site
                                && old.kind == new.kind
                        }
                        (old, new) => *old == new,
                    });
            if !same {
                return Err(at(declaration, Cause::NameCollision { name: name.clone() }));
            }
        }
        Ok(())
    }
}
