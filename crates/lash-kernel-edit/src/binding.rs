//! Binding edits use checker identity, including uses inside nested closures.
use std::collections::{BTreeMap, BTreeSet};

use crate::EditDiagnosticKind as Cause;
use crate::apply::{Fault, FaultKind, Working, at, node};
use lash_kernel_check::{
    ActNode, ActionNode, AtomNode, BindingKind, ExprNode, Graph, GraphNode, NodeKind, Reference,
    binding_references,
};
use lash_kernel_doc::{Correspondence, Expr, FunctionCatalog, NodeMut, Stmt};
use lash_kernel_value::{Name, sites::Site};

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
            if binding_references(&checked.kind)
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
            if let lash_kernel_value::sites::Unit::Library(id) = &mut site.unit {
                *id = ids.get(id).copied().unwrap_or(*id);
            }
            site
        };
        for old in before.nodes() {
            let Some(new) = after.node_at(&translate(&old.site)) else {
                return Err(at(declaration, Cause::EscapingBinding));
            };
            let old_refs = binding_references(&old.kind);
            let new_refs = binding_references(&new.kind);
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

/// The effects the tree writes had on lexical scope and use provenance.
#[derive(Default)]
pub(crate) struct BindingEdits {
    pub(crate) scopes_changed: bool,
    /// Base nodes whose contents were replaced.
    pub(super) replaced: BTreeSet<Site>,
    /// Written slots by base action. An unwritten operand keeps its slot
    /// through conversions, even when its expression site disappears.
    pub(super) arguments: BTreeMap<Site, BTreeSet<u32>>,
}

impl BindingEdits {
    pub(crate) fn preserve(
        &self,
        before: &Graph,
        after: &Graph,
        correspondence: &Correspondence,
    ) -> Result<(), crate::EditRefusal> {
        let repeated_before = repeated_sites(before);
        let repeated_after = repeated_sites(after);
        for old in before.nodes() {
            let Some(site) = correspondence.successor(&old.site) else {
                continue;
            };
            if self.replaced.contains(&old.site) {
                continue;
            }
            let written = self.arguments.get(&old.site);
            let old_refs = if written.is_some() {
                perform_operands(&old.kind, before)
            } else {
                binding_references(&old.kind)
                    .into_iter()
                    .map(Some)
                    .collect()
            };
            if (0u32..).zip(&old_refs).all(|(index, old_ref)| {
                old_ref.is_none() || written.is_some_and(|written| written.contains(&index))
            }) {
                continue;
            }
            let matches = |new: &GraphNode| {
                let new_refs = if written.is_some() {
                    perform_operands(&new.kind, after)
                } else {
                    binding_references(&new.kind)
                        .into_iter()
                        .map(Some)
                        .collect()
                };
                (0u32..).zip(&old_refs).all(|(index, old_ref)| {
                    old_ref.is_none_or(|old_ref| {
                        written.is_some_and(|written| written.contains(&index))
                            || new_refs
                                .get(index as usize)
                                .and_then(|reference| *reference)
                                .is_some_and(|new_ref| {
                                    same_reference(old_ref, new_ref, before, after, correspondence)
                                })
                    })
                })
            };
            let mut candidates = after
                .node_at(site)
                .into_iter()
                .chain(repeated_after.get(site).into_iter().flatten().copied());
            if (repeated_before.contains_key(&old.site) || repeated_after.contains_key(site))
                && correspondence
                    .successor(&Site::new(old.site.unit.clone(), []))
                    .is_some_and(|root| root.unit == site.unit)
            {
                // A checked closure body can appear at each of its creation
                // sites. Pair the instances through those sites in addition
                // to the authored site of the use.
                let Some(owners) = closure_owners(before, old)
                    .map(|owner| {
                        if self.replaced.contains(owner) {
                            return None;
                        }
                        correspondence.successor(owner)
                    })
                    .collect::<Option<Vec<_>>>()
                else {
                    continue;
                };
                if !candidates.any(|new| {
                    closure_owners(after, new).eq(owners.iter().copied()) && matches(new)
                }) {
                    return Err(escaping(&old.site));
                }
            } else if !candidates.next().is_some_and(matches) || !candidates.all(matches) {
                return Err(escaping(&old.site));
            }
        }
        Ok(())
    }
}

fn repeated_sites(graph: &Graph) -> BTreeMap<&Site, Vec<&GraphNode>> {
    let mut repeated: BTreeMap<&Site, Vec<&GraphNode>> = BTreeMap::new();
    for node in graph.nodes() {
        if graph
            .node_at(&node.site)
            .is_some_and(|last| last.id != node.id)
        {
            repeated.entry(&node.site).or_default().push(node);
        }
    }
    repeated
}

fn closure_owners<'a>(graph: &'a Graph, node: &'a GraphNode) -> impl Iterator<Item = &'a Site> {
    let mut node = node;
    std::iter::from_fn(move || {
        while !node.site.path.is_empty() {
            node = graph.node(node.parent?);
        }
        node = graph.node(node.parent?);
        Some(&node.site)
    })
}

fn perform_operands<'a>(kind: &'a NodeKind, graph: &'a Graph) -> Vec<Option<&'a Reference>> {
    match kind {
        NodeKind::Action(ActionNode::Perform { args, .. }) => args
            .iter()
            .map(|arg| match arg {
                AtomNode::Variable(reference) => Some(reference),
                AtomNode::Literal(_) => None,
            })
            .collect(),
        NodeKind::Expr(ExprNode::Act {
            kind: ActNode::Perform { .. },
            args,
        }) => args
            .iter()
            .map(|arg| match &graph.node(*arg).kind {
                NodeKind::Expr(ExprNode::Variable(reference)) => Some(reference),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn same_reference(
    old: &Reference,
    new: &Reference,
    before: &Graph,
    after: &Graph,
    correspondence: &Correspondence,
) -> bool {
    match (old, new) {
        (Reference::Binding(old), Reference::Binding(new)) => {
            let old = before.binding(*old);
            let new = after.binding(*new);
            // Parameters and captures share a declaration site with others
            // of their kind and cannot be renamed by a canvas edit.
            old.kind == new.kind
                && (!matches!(old.kind, BindingKind::Param | BindingKind::Capture)
                    || old.name == new.name)
                && correspondence.successor(&before.node(old.declared_at).site)
                    == Some(&after.node(new.declared_at).site)
        }
        (old, new) => old == new,
    }
}

fn escaping(site: &Site) -> crate::EditRefusal {
    crate::EditRefusal {
        diagnostics: vec![crate::EditDiagnostic {
            edit: None,
            location: Some(crate::Location::Base(site.clone())),
            kind: Cause::EscapingBinding,
        }],
    }
}
