//! Applying edits to a copy of a document.
//!
//! [`Working`] is that copy and its shadows (see [`crate::tree`]). An edit
//! resolves the base sites it names to where those nodes are now, changes
//! the document and changes the shadows the same way. Nothing here admits:
//! [`Working::publish`] hands the result to the checker.

use std::collections::BTreeMap;

use lash_kernel_check::{
    ActionNode, Admitted, AtomNode, BindingId, BindingKind, CalleeNode, Environment, ExprNode,
    Graph, NodeKind, PlaceNode, Reference, StmtNode, admit, derive, requirements,
};
use lash_kernel_doc::{
    Action, Annotations, Atom, Block, Callee, Document, DocumentId, Expr, FunctionCatalog,
    FunctionId, Literal, Name, Node, NodeAnnotation, Place, Site, Stmt, Unit,
};

use crate::correspondence::{Correspondence, Survivor};
use crate::edit::{Edit, Position};
use crate::refusal::{EditDiagnosticKind, EditRefusal, Location, refused};
use crate::tree::{NodeClass, NodeMut, Shadow};

/// Why one edit cannot be applied.
pub(crate) enum FaultKind {
    At(Option<Location>, EditDiagnosticKind),
    /// The document the earlier edits left does not link, and this edit
    /// needs it to.
    Unlinked(lash_kernel_check::Refusal),
}

pub(crate) type Fault = Box<FaultKind>;

fn at(base: &Site, kind: EditDiagnosticKind) -> Fault {
    Box::new(FaultKind::At(Some(Location::Base(base.clone())), kind))
}

fn plain(kind: EditDiagnosticKind) -> Fault {
    Box::new(FaultKind::At(None, kind))
}

/// What a transaction leaves when it is admitted.
pub(crate) struct Published {
    pub(crate) document: Document,
    pub(crate) annotations: Annotations,
    pub(crate) admitted: Admitted,
    pub(crate) correspondence: Correspondence,
}

pub(crate) struct Working {
    document: Document,
    shadows: BTreeMap<Unit, Shadow>,
}

fn body<'a>(document: &'a mut Document, unit: &Unit) -> Option<&'a mut Block> {
    match unit {
        Unit::Main => Some(&mut document.main),
        Unit::Function(name) => document
            .functions
            .get_mut(name)
            .map(|function| &mut function.body),
        Unit::Library(_) => None,
    }
}

fn node<'a>(document: &'a mut Document, site: &Site) -> Option<NodeMut<'a>> {
    NodeMut::Block(body(document, &site.unit)?).descend(&site.path)
}

fn shadow<'a>(shadows: &'a mut BTreeMap<Unit, Shadow>, site: &Site) -> Option<&'a mut Shadow> {
    shadows.get_mut(&site.unit)?.descend(&site.path)
}

/// The arguments of an action, in the order it writes them.
fn arguments(action: &mut Action) -> Vec<&mut Atom> {
    match action {
        Action::Call { args, .. } | Action::Perform { args, .. } | Action::Spawn { args, .. } => {
            args.iter_mut().collect()
        }
        Action::Sleep { duration: atom }
        | Action::Join { task: atom }
        | Action::JoinMany { tasks: atom, .. }
        | Action::Cancel { task: atom } => vec![atom],
        Action::Yield => Vec::new(),
    }
}

/// Whether a derived node itself declares or names the variable `binding`.
fn names(kind: &NodeKind, binding: BindingId) -> bool {
    let is = |reference: &Reference| *reference == Reference::Binding(binding);
    let atom = |atom: &AtomNode| matches!(atom, AtomNode::Variable(reference) if is(reference));
    let callee =
        |callee: &CalleeNode| matches!(callee, CalleeNode::Value(reference) if is(reference));
    match kind {
        NodeKind::Block(_) => false,
        NodeKind::Stmt(stmt) => match stmt {
            StmtNode::Let {
                binding: declared, ..
            }
            | StmtNode::For {
                binding: declared, ..
            } => *declared == binding,
            StmtNode::Assign {
                place: PlaceNode::Variable(reference),
                ..
            } => is(reference),
            StmtNode::Try {
                catch: Some(catch), ..
            } => catch.binding == binding,
            _ => false,
        },
        NodeKind::Action(action) => match action {
            ActionNode::Call {
                callee: called,
                args,
            }
            | ActionNode::Spawn {
                callee: called,
                args,
            } => callee(called) || args.iter().any(atom),
            ActionNode::Perform { args, .. } => args.iter().any(atom),
            ActionNode::Sleep { duration: one }
            | ActionNode::Join { task: one }
            | ActionNode::JoinMany { tasks: one, .. }
            | ActionNode::Cancel { task: one } => atom(one),
            ActionNode::Yield => false,
        },
        NodeKind::Expr(expr) => match expr {
            ExprNode::Variable(reference) => is(reference),
            ExprNode::Closure { params, .. } => params.contains(&binding),
            _ => false,
        },
    }
}

/// Renames the variable `from` in the names a node itself holds.
fn rename_own(node: &mut NodeMut<'_>, from: &Name, to: &Name) {
    let rename = |name: &mut Name| {
        if name == from {
            *name = to.clone();
        }
    };
    match node {
        NodeMut::Block(_) => {}
        NodeMut::Stmt(stmt) => match stmt {
            Stmt::Let { name, .. }
            | Stmt::For { binding: name, .. }
            | Stmt::Assign {
                place: Place::Variable(name),
                ..
            } => rename(name),
            Stmt::Try(scope) => {
                if let Some(catch) = &mut scope.catch {
                    rename(&mut catch.binding);
                }
            }
            _ => {}
        },
        NodeMut::Action(action) => {
            if let Action::Call { callee, .. } | Action::Spawn { callee, .. } = action
                && let Callee::Value(name) = callee
            {
                rename(name);
            }
            for atom in arguments(action) {
                if let Atom::Variable(name) = atom {
                    rename(name);
                }
            }
        }
        NodeMut::Expr(expr) => match expr {
            Expr::Variable(name) => rename(name),
            Expr::Closure(closure) => closure.params.iter_mut().for_each(rename),
            _ => {}
        },
    }
}

/// Applies `change` to a node and to everything under it, marking the
/// nodes it says it changed. Returns how many it changed.
fn rewrite(
    mut node: NodeMut<'_>,
    shadow: &mut Shadow,
    change: &mut dyn FnMut(&mut NodeMut<'_>) -> bool,
) -> usize {
    let mut changed = 0;
    if change(&mut node) {
        shadow.edited = true;
        changed += 1;
    }
    for (child, shadow) in node.children().into_iter().zip(&mut shadow.children) {
        changed += rewrite(child, shadow, change);
    }
    changed
}

impl Working {
    /// A copy of `document`, every node of which is a node of the base, with
    /// `annotations` on the nodes they are attached to.
    pub(crate) fn open(document: &Document, annotations: &Annotations) -> Self {
        let mut shadows = BTreeMap::new();
        let main = Site::new(Unit::Main, []);
        shadows.insert(Unit::Main, Shadow::base(Node::Block(&document.main), main));
        for (name, function) in &document.functions {
            let unit = Unit::Function(name.clone());
            let site = Site::new(unit.clone(), []);
            shadows.insert(unit, Shadow::base(Node::Block(&function.body), site));
        }
        for annotation in &annotations.nodes {
            if let Some(shadow) = shadow(&mut shadows, &annotation.site) {
                shadow.note.label = annotation.label.clone();
                shadow.note.data = annotation.data.clone();
            }
        }
        Self {
            document: document.clone(),
            shadows,
        }
    }

    /// Every site of the document.
    pub(crate) fn sites(&self) -> Vec<Site> {
        let mut sites = Vec::new();
        for (unit, root) in &self.shadows {
            root.walk(&mut Vec::new(), &mut |path, _| {
                sites.push(Site::new(unit.clone(), path));
            });
        }
        sites
    }

    /// Where the node that was at `base` is now.
    fn locate(&self, base: &Site) -> Result<Site, Fault> {
        self.shadows
            .iter()
            .find_map(|(unit, root)| Some(Site::new(unit.clone(), root.find(base)?)))
            .ok_or_else(|| at(base, EditDiagnosticKind::NoSuchNode))
    }

    /// Where the node that was at `base` is now, when it is a node of
    /// `class`.
    fn locate_as(&mut self, base: &Site, class: NodeClass) -> Result<Site, Fault> {
        let site = self.locate(base)?;
        let found = node(&mut self.document, &site)
            .ok_or_else(|| at(base, EditDiagnosticKind::NoSuchNode))?
            .class();
        if found == class {
            Ok(site)
        } else {
            Err(at(
                base,
                EditDiagnosticKind::WrongNode {
                    expected: class,
                    found,
                },
            ))
        }
    }

    /// The block that holds the statement that was at `base`, and the
    /// statement's index in it.
    fn statement(&mut self, base: &Site) -> Result<(Site, usize), Fault> {
        let mut block = self.locate_as(base, NodeClass::Statement)?;
        let index = block
            .path
            .pop()
            .ok_or_else(|| at(base, EditDiagnosticKind::NoSuchNode))?;
        Ok((block, index as usize))
    }

    /// The block a position names, and the index it puts a statement at.
    fn position(&mut self, position: &Position) -> Result<(Site, usize), Fault> {
        let block = self.locate_as(&position.block, NodeClass::Block)?;
        let index = match &position.before {
            Some(before) => {
                let (holder, index) = self.statement(before)?;
                if holder != block {
                    return Err(at(before, EditDiagnosticKind::NotInBlock));
                }
                index
            }
            None => self.block(&block).len(),
        };
        Ok((block, index))
    }

    /// The statements of the block at `site`, which a `locate_as` has shown
    /// to be one.
    fn block(&mut self, site: &Site) -> &mut Block {
        match node(&mut self.document, site) {
            Some(NodeMut::Block(block)) => block,
            _ => unreachable!("the site was resolved to a block"),
        }
    }

    fn shadow(&mut self, site: &Site) -> &mut Shadow {
        match shadow(&mut self.shadows, site) {
            Some(shadow) => shadow,
            None => unreachable!("every node of the document has a shadow"),
        }
    }

    fn insert(&mut self, block: &Site, index: usize, statement: Stmt, shadow: Shadow) {
        self.shadow(block).children.insert(index, shadow);
        self.block(block).insert(index, statement);
    }

    fn take(&mut self, block: &Site, index: usize) -> (Stmt, Shadow) {
        let shadow = self.shadow(block).children.remove(index);
        (self.block(block).remove(index), shadow)
    }

    /// Replaces the node at `site`'s content through `write`, which returns
    /// a fault to leave it as it was.
    fn write(
        &mut self,
        site: &Site,
        write: impl FnOnce(&mut NodeMut<'_>) -> Result<(), Fault>,
    ) -> Result<(), Fault> {
        let Some(mut node) = node(&mut self.document, site) else {
            unreachable!("the site was resolved to a node");
        };
        write(&mut node)?;
        let Some(shadow) = shadow(&mut self.shadows, site) else {
            unreachable!("every node of the document has a shadow");
        };
        shadow.replace(node.as_node());
        Ok(())
    }

    /// Applies `change` to every node of the document.
    fn rewrite(&mut self, change: &mut dyn FnMut(&mut NodeMut<'_>) -> bool) -> usize {
        let mut changed = 0;
        for (unit, root) in &mut self.shadows {
            if let Some(body) = body(&mut self.document, unit) {
                changed += rewrite(NodeMut::Block(body), root, change);
            }
        }
        changed
    }

    pub(crate) fn edit(&mut self, edit: &Edit, catalog: &dyn FunctionCatalog) -> Result<(), Fault> {
        match edit {
            Edit::InsertStatement { at, statement } => {
                let (block, index) = self.position(at)?;
                let shadow = Shadow::fresh(Node::Stmt(statement));
                self.insert(&block, index, statement.clone(), shadow);
            }
            Edit::RemoveStatement { statement } => {
                let (block, index) = self.statement(statement)?;
                self.take(&block, index);
            }
            Edit::MoveStatement { statement, to } => self.move_statement(statement, to)?,
            Edit::CloneStatement { statement, to } => {
                let (block, index) = self.statement(statement)?;
                let copy = self.block(&block)[index].clone();
                let shadow = self.shadow(&block).children[index].copy();
                let (block, index) = self.position(to)?;
                self.insert(&block, index, copy, shadow);
            }
            Edit::ReplaceStatement { statement, with } => {
                let site = self.locate_as(statement, NodeClass::Statement)?;
                self.write(&site, |node| {
                    if let NodeMut::Stmt(stmt) = node {
                        **stmt = with.clone();
                    }
                    Ok(())
                })?;
            }
            Edit::ReplaceExpression { expression, with } => {
                let site = self.locate_as(expression, NodeClass::Expression)?;
                self.write(&site, |node| {
                    if let NodeMut::Expr(expr) = node {
                        **expr = with.clone();
                    }
                    Ok(())
                })?;
            }
            Edit::ReplaceAction { action, with } => {
                let site = self.locate_as(action, NodeClass::Action)?;
                self.write(&site, |node| {
                    if let NodeMut::Action(action) = node {
                        **action = with.clone();
                    }
                    Ok(())
                })?;
            }
            Edit::SetArgument {
                action,
                index,
                argument,
            } => {
                let site = self.locate_as(action, NodeClass::Action)?;
                self.write(&site, |node| {
                    let NodeMut::Action(node) = node else {
                        return Ok(());
                    };
                    let mut arguments = arguments(node);
                    let count = u32::try_from(arguments.len()).unwrap_or(u32::MAX);
                    match arguments.get_mut(*index as usize) {
                        Some(slot) => {
                            **slot = argument.clone();
                            Ok(())
                        }
                        None => Err(at(
                            action,
                            EditDiagnosticKind::NoSuchArgument {
                                index: *index,
                                arguments: count,
                            },
                        )),
                    }
                })?;
            }
            Edit::SetCondition {
                statement,
                condition,
            } => {
                let site = self.locate_as(statement, NodeClass::Statement)?;
                let Some(NodeMut::Stmt(
                    Stmt::If {
                        condition: slot, ..
                    }
                    | Stmt::While {
                        condition: slot, ..
                    },
                )) = node(&mut self.document, &site)
                else {
                    return Err(at(statement, EditDiagnosticKind::NoCondition));
                };
                *slot = condition.clone();
                // The condition is the statement's first child (`K-ID-004`).
                self.shadow(&site.child(0)).replace(Node::Expr(condition));
            }
            Edit::SetCatch { statement, catch } => {
                let site = self.locate_as(statement, NodeClass::Statement)?;
                let Some(NodeMut::Stmt(Stmt::Try(scope))) = node(&mut self.document, &site) else {
                    return Err(at(statement, EditDiagnosticKind::NotATry));
                };
                let had = scope.catch.is_some();
                scope.catch = catch.clone();
                // A `try`'s children are its body, its `catch` body and its
                // `finally`, those it has.
                self.clause(&site, 1, had, catch.as_ref().map(|catch| &catch.body));
            }
            Edit::SetFinally { statement, finally } => {
                let site = self.locate_as(statement, NodeClass::Statement)?;
                let Some(NodeMut::Stmt(Stmt::Try(scope))) = node(&mut self.document, &site) else {
                    return Err(at(statement, EditDiagnosticKind::NotATry));
                };
                let had = scope.finally.is_some();
                let index = 1 + usize::from(scope.catch.is_some());
                scope.finally = finally.clone();
                self.clause(&site, index, had, finally.as_ref());
            }
            Edit::RenameVariable {
                declared_at,
                name,
                to,
            } => self.rename_variable(declared_at, name, to, catalog)?,
            Edit::SetLabel { node, label } => {
                let site = self.locate(node)?;
                self.shadow(&site).note.label = label.clone();
            }
            Edit::SetData { node, key, value } => {
                let site = self.locate(node)?;
                let data = &mut self.shadow(&site).note.data;
                match value {
                    Some(value) => {
                        data.insert(key.clone(), value.clone());
                    }
                    None => {
                        data.remove(key);
                    }
                }
            }
            Edit::InsertFunction { name, function } => {
                if self.document.functions.contains_key(name) {
                    return Err(plain(EditDiagnosticKind::FunctionExists {
                        name: name.clone(),
                    }));
                }
                self.shadows.insert(
                    Unit::Function(name.clone()),
                    Shadow::fresh(Node::Block(&function.body)),
                );
                self.document
                    .functions
                    .insert(name.clone(), function.clone());
            }
            Edit::RemoveFunction { name } => {
                self.declared(name)?;
                self.document.functions.remove(name);
                self.document.entries.remove(name);
                self.shadows.remove(&Unit::Function(name.clone()));
            }
            Edit::ReplaceFunction { name, function } => {
                self.declared(name)?;
                self.shadow(&Site::new(Unit::Function(name.clone()), []))
                    .replace(Node::Block(&function.body));
                self.document
                    .functions
                    .insert(name.clone(), function.clone());
            }
            Edit::RenameFunction { from, to } => self.rename_function(from, to)?,
            Edit::SetPrivateBindings { names } => {
                self.document.private_bindings.clone_from(names);
            }
            Edit::InsertEntry {
                function,
                signature,
            } => {
                self.declared(function)?;
                if self.document.entries.contains_key(function) {
                    return Err(plain(EditDiagnosticKind::EntryExists {
                        name: function.clone(),
                    }));
                }
                self.document
                    .entries
                    .insert(function.clone(), signature.clone());
            }
            Edit::RemoveEntry { function } => {
                self.entry(function)?;
                self.document.entries.remove(function);
            }
            Edit::RenameEntry { from, to } => {
                self.entry(from)?;
                self.rename_function(from, to)?;
            }
            Edit::SetEntrySignature {
                function,
                signature,
            } => {
                self.entry(function)?;
                self.document
                    .entries
                    .insert(function.clone(), signature.clone());
            }
            Edit::SetEffectSignature { effect, signature } => {
                self.document
                    .manifest
                    .effects
                    .insert(effect.clone(), signature.clone());
            }
            Edit::SetNumberPolicy { numbers } => self.document.manifest.numbers = *numbers,
            Edit::ReplaceFunctionIdentity { from, to } => self.adopt(*from, *to)?,
        }
        Ok(())
    }

    fn declared(&self, name: &Name) -> Result<(), Fault> {
        if self.document.functions.contains_key(name) {
            Ok(())
        } else {
            Err(plain(EditDiagnosticKind::NoSuchFunction {
                name: name.clone(),
            }))
        }
    }

    fn entry(&self, name: &Name) -> Result<(), Fault> {
        if self.document.entries.contains_key(name) {
            Ok(())
        } else {
            Err(plain(EditDiagnosticKind::NoSuchEntry {
                name: name.clone(),
            }))
        }
    }

    /// Makes the child at `index` of the `try` at `site` the shadow of
    /// `block`: `had` says whether the clause was there before.
    fn clause(&mut self, site: &Site, index: usize, had: bool, block: Option<&Block>) {
        let shadow = self.shadow(site);
        shadow.edited = true;
        if had {
            shadow.children.remove(index);
        }
        if let Some(block) = block {
            shadow
                .children
                .insert(index, Shadow::fresh(Node::Block(block)));
        }
    }

    fn move_statement(&mut self, statement: &Site, to: &Position) -> Result<(), Fault> {
        let (from, at_index) = self.statement(statement)?;
        let (mut block, mut index) = self.position(to)?;
        let moved = from.child(u32::try_from(at_index).unwrap_or(u32::MAX));
        if block.unit == moved.unit && block.path.starts_with(&moved.path) {
            return Err(at(statement, EditDiagnosticKind::IntoItself));
        }
        // Taking the statement out shifts what follows it in its block.
        if block == from {
            if index > at_index {
                index -= 1;
            }
        } else if block.unit == from.unit
            && block.path.starts_with(&from.path)
            && let Some(step) = block.path.get_mut(from.path.len())
            && *step as usize > at_index
        {
            *step -= 1;
        }
        let (taken, shadow) = self.take(&from, at_index);
        self.insert(&block, index, taken, shadow);
        Ok(())
    }

    fn derive(&self, catalog: &dyn FunctionCatalog) -> Result<Graph, Fault> {
        derive(&self.document, catalog).map_err(|refusal| Box::new(FaultKind::Unlinked(refusal)))
    }

    fn rename_variable(
        &mut self,
        declared_at: &Site,
        name: &Name,
        to: &Name,
        catalog: &dyn FunctionCatalog,
    ) -> Result<(), Fault> {
        let site = self.locate(declared_at)?;
        let before = self.derive(catalog)?;
        let declaring = before
            .node_at(&site)
            .ok_or_else(|| at(declared_at, EditDiagnosticKind::NoSuchNode))?;
        let binding = before
            .bindings()
            .iter()
            .find(|binding| binding.declared_at == declaring.id && &binding.name == name)
            .ok_or_else(|| {
                at(
                    declared_at,
                    EditDiagnosticKind::NoSuchBinding { name: name.clone() },
                )
            })?;

        for graph_node in before.nodes() {
            if !names(&graph_node.kind, binding.id) {
                continue;
            }
            if let Some(mut node) = node(&mut self.document, &graph_node.site) {
                rename_own(&mut node, name, to);
            }
            self.shadow(&graph_node.site).edited = true;
        }
        // A declared function's parameters are declared by its body.
        if binding.kind == BindingKind::Param
            && declaring.parent.is_none()
            && let Unit::Function(function) = &site.unit
            && let Some(function) = self.document.functions.get_mut(function)
        {
            for param in &mut function.params {
                if param == name {
                    *param = to.clone();
                }
            }
            self.shadow(&site).edited = true;
        }
        // A private binding is private under its new name too.
        if site.unit == Unit::Main
            && site.path.len() == 1
            && binding.kind == BindingKind::Let
            && self.document.private_bindings.contains(name)
        {
            self.document.private_bindings.insert(to.clone());
            let still_declared =
                self.document.main.iter().any(
                    |stmt| matches!(stmt, Stmt::Let { name: declared, .. } if declared == name),
                );
            if !still_declared {
                self.document.private_bindings.remove(name);
            }
        }

        // The rename is sound when every name still refers to the
        // declaration it referred to: the derived nodes, which hold
        // references and not names, are unchanged.
        let collides = || {
            at(
                declared_at,
                EditDiagnosticKind::RenameCollides {
                    from: name.clone(),
                    to: to.clone(),
                },
            )
        };
        let after = derive(&self.document, catalog).map_err(|_| collides())?;
        let same = before
            .nodes()
            .iter()
            .map(|node| &node.kind)
            .eq(after.nodes().iter().map(|node| &node.kind));
        if same { Ok(()) } else { Err(collides()) }
    }

    fn rename_function(&mut self, from: &Name, to: &Name) -> Result<(), Fault> {
        self.declared(from)?;
        if self.document.functions.contains_key(to) {
            return Err(plain(EditDiagnosticKind::FunctionExists {
                name: to.clone(),
            }));
        }
        if let Some(function) = self.document.functions.remove(from) {
            self.document.functions.insert(to.clone(), function);
        }
        if let Some(signature) = self.document.entries.remove(from) {
            self.document.entries.insert(to.clone(), signature);
        }
        if let Some(shadow) = self.shadows.remove(&Unit::Function(from.clone())) {
            self.shadows.insert(Unit::Function(to.clone()), shadow);
        }
        let reference = |literal: &mut Literal| match literal {
            Literal::Function(name) if name == from => {
                *name = to.clone();
                true
            }
            _ => false,
        };
        self.rewrite(&mut |node| match node {
            NodeMut::Expr(Expr::Literal(literal)) => reference(literal),
            NodeMut::Action(action) => {
                let mut changed = false;
                if let Action::Call { callee, .. } | Action::Spawn { callee, .. } = &mut **action
                    && let Callee::Declared(name) = callee
                    && name == from
                {
                    *name = to.clone();
                    changed = true;
                }
                for atom in arguments(action) {
                    if let Atom::Literal(literal) = atom {
                        changed |= reference(literal);
                    }
                }
                changed
            }
            _ => false,
        });
        Ok(())
    }

    fn adopt(&mut self, from: FunctionId, to: FunctionId) -> Result<(), Fault> {
        let changed = self.rewrite(&mut |node| {
            let function = match node {
                NodeMut::Expr(Expr::Call { function, .. }) => function,
                NodeMut::Action(
                    Action::Call {
                        callee: Callee::Library(function),
                        ..
                    }
                    | Action::Spawn {
                        callee: Callee::Library(function),
                        ..
                    },
                ) => function,
                _ => return false,
            };
            if *function == from {
                *function = to;
                true
            } else {
                false
            }
        });
        if changed == 0 {
            return Err(plain(EditDiagnosticKind::FunctionNotCalled {
                function: from,
            }));
        }
        // Until the manifest is derived again the adopted function goes by
        // the name of the one it corrects, so that an environment that
        // lacks it is told which function is missing.
        let functions = &mut self.document.manifest.functions;
        if let Some(name) = functions.get(&from).cloned() {
            functions.entry(to).or_insert(name);
        }
        Ok(())
    }

    /// Derives the manifest the code now requires (`K-EDIT-005`), admits
    /// the document and publishes it with its annotations and the
    /// correspondence from `base`.
    pub(crate) fn publish(
        mut self,
        base: &Annotations,
        environment: &Environment<'_>,
    ) -> Result<Published, EditRefusal> {
        let required = requirements(&self.document, environment.functions);
        let manifest = &mut self.document.manifest;
        manifest.functions = required.functions;
        manifest
            .effects
            .retain(|effect, _| required.effects.contains(effect));
        for effect in required.effects {
            if let Some(signature) = environment.effects.get(&effect) {
                manifest
                    .effects
                    .entry(effect)
                    .or_insert_with(|| signature.clone());
            }
        }

        let admitted =
            admit(&self.document, environment).map_err(|refusal| refused(None, refusal))?;
        let (annotations, correspondence) = self.records(base, admitted.identity);
        Ok(Published {
            document: self.document,
            annotations,
            admitted,
            correspondence,
        })
    }

    /// The annotation layer and the correspondence the shadows hold, for a
    /// document of identity `result` edited from the one `base` annotates.
    fn records(&self, base: &Annotations, result: DocumentId) -> (Annotations, Correspondence) {
        let mut nodes = Vec::new();
        let mut survivors = Vec::new();
        for (unit, root) in &self.shadows {
            root.walk(&mut Vec::new(), &mut |path, shadow| {
                let site = Site::new(unit.clone(), path);
                if !shadow.note.is_empty() {
                    nodes.push(NodeAnnotation {
                        site: site.clone(),
                        label: shadow.note.label.clone(),
                        data: shadow.note.data.clone(),
                    });
                }
                if let Some(origin) = &shadow.origin {
                    survivors.push(Survivor {
                        from: origin.clone(),
                        to: site,
                        edited: shadow.edited,
                    });
                }
            });
        }
        nodes.sort_by(|a, b| a.site.cmp(&b.site));
        let annotations = Annotations {
            document: result,
            dialect: base.dialect.clone(),
            // Authored source is the text the base was lowered from; it is
            // not the text of a document that behaves differently.
            source: base.source.clone().filter(|_| base.document == result),
            nodes,
        };
        let correspondence = Correspondence::new(base.document, result, survivors);
        (annotations, correspondence)
    }
}
