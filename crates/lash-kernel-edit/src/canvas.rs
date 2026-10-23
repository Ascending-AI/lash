//! Admit canvas targets before any private tree write.
use lash_vocabulary::names::FunctionId;
use std::collections::BTreeMap;

use crate::apply::{Fault, Working, at, node};
use crate::authoring::{Branch, Operation, ValueTarget, Wrapper};
use crate::tree::{NodeClass, Shadow};
use crate::{Edit, EditDiagnosticKind as Cause};
use lash_kernel_check::{
    ActNode, ActionNode, AtomNode, CalleeNode, ExprNode, NodeKind, PlaceNode, Reference, StmtNode,
};
use lash_kernel_doc::{
    ActExpr, ActKind, Action, Callee, Catch, Expr, FunctionCatalog, Literal, Node, NodeMut, Place,
    Rhs, Stmt, TryStmt,
};

use lash_vocabulary::sites::Site;

fn has_action(node: Node<'_>) -> bool {
    matches!(node, Node::Action(_) | Node::Expr(Expr::Act(_)))
        || node.children().into_iter().any(has_action)
}

fn contains_closure(node: Node<'_>) -> bool {
    matches!(node, Node::Expr(Expr::Closure(_)))
        || node.children().into_iter().any(contains_closure)
}

fn data(expr: &Expr, catalog: &dyn FunctionCatalog) -> bool {
    match expr {
        Expr::Call { function, args } => {
            catalog.header(function).is_some_and(|h| {
                h.has_native()
                    && matches!(h.name.as_str(), "error.new" | "timestamp.from_nanoseconds")
            }) && args.iter().all(|arg| data(arg, catalog))
        }
        Expr::Literal(Literal::Function(_)) => false,
        Expr::Literal(_) | Expr::Variable(_) => true,
        Expr::Tuple(items) | Expr::List(items) | Expr::Set(items) => {
            items.iter().all(|item| data(item, catalog))
        }
        Expr::Map(items) => items
            .iter()
            .all(|item| data(&item.key, catalog) && data(&item.value, catalog)),
        Expr::Record(items) => items.iter().all(|item| data(&item.value, catalog)),
        _ => false,
    }
}

fn header(expr: &Expr, base: &Site) -> Result<(), Fault> {
    if has_action(Node::Expr(expr)) {
        Err(at(base, Cause::ActionInHeader))
    } else {
        Ok(())
    }
}

/// Argument helpers may call other library helpers, but cannot wait, apply a
/// callback, or reach another effect. Library identity pins the complete body.
fn pure_argument(
    node: Node<'_>,
    catalog: &dyn FunctionCatalog,
    known: &mut BTreeMap<FunctionId, bool>,
) -> bool {
    let forbidden = match node {
        Node::Action(Action::Call {
            callee: Callee::Local(_),
            ..
        }) => false,
        Node::Expr(Expr::Act(act)) => !matches!(act.kind, ActKind::Call(Callee::Local(_))),
        Node::Action(_) | Node::Expr(Expr::Closure(_)) => true,
        Node::Block(_) | Node::Stmt(_) | Node::Expr(_) => false,
    };
    if forbidden
        || !node
            .functions()
            .into_iter()
            .all(|function| pure_library(*function, catalog, known))
    {
        return false;
    }
    node.children()
        .into_iter()
        .all(|child| pure_argument(child, catalog, known))
}

fn pure_library(
    function: FunctionId,
    catalog: &dyn FunctionCatalog,
    known: &mut BTreeMap<FunctionId, bool>,
) -> bool {
    if let Some(pure) = known.get(&function) {
        return *pure;
    }
    let pure = catalog.definition(&function).is_some_and(|definition| {
        definition
            .body()
            .is_none_or(|body| pure_argument(Node::Block(&body.block), catalog, known))
    });
    known.insert(function, pure);
    pure
}

/// The sole perform must occupy the entire carrier, with pure argument helpers.
/// Assignment receivers cannot contain actions or other effects either.
fn step(stmt: &Stmt, catalog: &dyn FunctionCatalog) -> Option<u32> {
    let mut known = BTreeMap::new();
    let (rhs, index) = match stmt {
        Stmt::Let { value, .. } => (Some(value), 0),
        Stmt::Assign { place, value, .. } => {
            let index = match place {
                Place::Variable(_) => 0,
                Place::Member(lash_kernel_doc::Member::Field { .. }) => 1,
                Place::Member(lash_kernel_doc::Member::Index { .. }) => 2,
            };
            (Some(value), index)
        }
        Stmt::Do {
            value: Expr::Act(act),
        } if matches!(act.kind, ActKind::Perform { .. })
            && act
                .args
                .iter()
                .all(|arg| pure_argument(Node::Expr(arg), catalog, &mut known)) =>
        {
            return Some(0);
        }
        _ => return None,
    };
    if Node::Stmt(stmt)
        .children()
        .into_iter()
        .take(index as usize)
        .any(has_action)
    {
        return None;
    }
    match rhs? {
        Rhs::Action(Action::Perform { .. }) => Some(index),
        Rhs::Expr(Expr::Act(act))
            if matches!(act.kind, ActKind::Perform { .. })
                && act
                    .args
                    .iter()
                    .all(|arg| pure_argument(Node::Expr(arg), catalog, &mut known)) =>
        {
            Some(index)
        }
        _ => None,
    }
}

impl Working {
    fn non_separable(
        &mut self,
        action: &Site,
        site: &Site,
        catalog: &dyn FunctionCatalog,
    ) -> Fault {
        let mut ancestor = site.clone();
        let mut enclosing_statement = None;
        let mut separable_step = None;
        while ancestor.path.pop().is_some() {
            if let Some(NodeMut::Stmt(stmt)) = node(&mut self.document, &ancestor) {
                let index = step(stmt, catalog);
                if enclosing_statement.is_none() {
                    enclosing_statement = self.origin(&ancestor);
                }
                if let Some(index) = index {
                    separable_step = self.origin(&ancestor.child(index));
                    if separable_step.is_some() {
                        break;
                    }
                }
            }
        }
        at(
            action,
            Cause::NonSeparableStep {
                enclosing_statement,
                separable_step,
            },
        )
    }

    fn carrier(&mut self, action: &Site, catalog: &dyn FunctionCatalog) -> Result<Site, Fault> {
        let site = self.locate(action)?;
        let mut parent = site.clone();
        let index = parent
            .path
            .pop()
            .ok_or_else(|| self.non_separable(action, &site, catalog))?;
        let Some(NodeMut::Stmt(stmt)) = node(&mut self.document, &parent) else {
            return Err(self.non_separable(action, &site, catalog));
        };
        if step(stmt, catalog) != Some(index) {
            return Err(self.non_separable(action, &site, catalog));
        }
        // Work with base addresses even after an earlier batch operation moved it.
        let mut base_parent = action.clone();
        base_parent.path.pop();
        if self.locate(&base_parent)? != parent {
            return Err(self.non_separable(action, &site, catalog));
        }
        Ok(base_parent)
    }

    pub(crate) fn canvas(
        &mut self,
        operation: &Operation,
        catalog: &dyn lash_kernel_doc::FunctionCatalog,
    ) -> Result<(), Fault> {
        match operation {
            Operation::RenameBinding {
                declaration, name, ..
            } => self.rename_binding(declaration, name, catalog)?,
            Operation::InsertBinding {
                at: position,
                name,
                value,
                ..
            } => {
                let value = value.expression();
                if !data(&value, catalog) {
                    return Err(at(&position.block, Cause::ValueOutsideCanvasSet));
                }
                self.edit(&Edit::InsertStatement {
                    at: position.clone(),
                    statement: Box::new(Stmt::Let {
                        name: name.clone(),
                        value: Rhs::Expr(value),
                    }),
                })?;
            }
            Operation::RemoveBinding { declaration, .. } => {
                let site = self.locate(declaration)?;
                match node(&mut self.document, &site) {
                    Some(NodeMut::Stmt(Stmt::Let {
                        value: Rhs::Expr(value),
                        ..
                    })) if !has_action(Node::Expr(value))
                        && !contains_closure(Node::Expr(value)) => {}
                    _ => return Err(at(declaration, Cause::NotValueTarget)),
                }
                self.edit(&Edit::RemoveStatement {
                    statement: declaration.clone(),
                })?;
            }
            Operation::RenameExport { from, to, .. } => {
                if from == to && self.document.exports.contains_key(from) {
                    return Ok(());
                }
                if self.document.exports.contains_key(to) {
                    return Err(Box::new(crate::apply::FaultKind::At(
                        None,
                        Cause::NameCollision { name: to.clone() },
                    )));
                }
                let export = self.document.exports.remove(from).ok_or_else(|| {
                    Box::new(crate::apply::FaultKind::At(
                        None,
                        Cause::NoSuchEntry { name: from.clone() },
                    ))
                })?;
                self.document.exports.insert(to.clone(), export);
            }
            Operation::ReplaceDocument { document, .. } => self.replace_document(document),
            Operation::SetValue { target, value, .. } => {
                let value = value.expression();
                let address = match target {
                    ValueTarget::Literal { expression } => expression,
                    ValueTarget::Initializer { statement } => statement,
                    ValueTarget::ToolArgument { action, .. } => action,
                };
                if !data(&value, catalog) {
                    return Err(at(address, Cause::ValueOutsideCanvasSet));
                }
                match target {
                    ValueTarget::Literal { expression } => {
                        let site = self.locate(expression)?;
                        if !matches!(
                            node(&mut self.document, &site),
                            Some(NodeMut::Expr(Expr::Literal(_)))
                        ) {
                            return Err(at(expression, Cause::NotValueTarget));
                        }
                        self.edit(&Edit::ReplaceExpression {
                            expression: expression.clone(),
                            with: value,
                        })?;
                    }
                    ValueTarget::Initializer { statement } => {
                        let site = self.locate(statement)?;
                        let Some(NodeMut::Stmt(Stmt::Let {
                            value: Rhs::Expr(old),
                            ..
                        })) = node(&mut self.document, &site)
                        else {
                            return Err(at(statement, Cause::NotValueTarget));
                        };
                        if has_action(Node::Expr(old)) || contains_closure(Node::Expr(old)) {
                            return Err(at(statement, Cause::NotValueTarget));
                        }
                        self.edit(&Edit::ReplaceExpression {
                            expression: statement.child(0),
                            with: value,
                        })?;
                    }
                    ValueTarget::ToolArgument {
                        action,
                        index,
                        path,
                    } => {
                        let site = self.locate(action)?;
                        match node(&mut self.document, &site) {
                            Some(NodeMut::Action(Action::Perform { .. })) if path.is_empty() => {}
                            Some(NodeMut::Expr(Expr::Act(act)))
                                if matches!(act.kind, ActKind::Perform { .. }) =>
                            {
                                let old = act.args.get(*index as usize).ok_or_else(|| {
                                    at(
                                        action,
                                        Cause::NoSuchArgument {
                                            index: *index,
                                            arguments: act.args.len() as u32,
                                        },
                                    )
                                })?;
                                let mut old = Node::Expr(old);
                                for part in path {
                                    old = old
                                        .children()
                                        .get(*part as usize)
                                        .copied()
                                        .ok_or_else(|| at(action, Cause::NotValueTarget))?;
                                }
                                if has_action(old) || contains_closure(old) {
                                    return Err(at(action, Cause::NotValueTarget));
                                }
                            }
                            _ => return Err(at(action, Cause::NotValueTarget)),
                        }
                        if path.is_empty() {
                            self.edit(&Edit::SetArgument {
                                action: action.clone(),
                                index: *index,
                                argument: value,
                            })?;
                        } else {
                            let mut expression = action.child(*index);
                            expression.path.extend(path);
                            self.edit(&Edit::ReplaceExpression {
                                expression,
                                with: value,
                            })?;
                        }
                    }
                }
            }
            Operation::InsertStep {
                at: position,
                slot,
                args,
                result,
                binding,
                ..
            } => {
                let args: Vec<_> = args.iter().map(|arg| arg.expression()).collect();
                if !args.iter().all(|item| data(item, catalog)) {
                    return Err(at(&position.block, Cause::ValueOutsideCanvasSet));
                }
                let act = ActExpr {
                    kind: ActKind::Perform {
                        slot: *slot,
                        result: result.clone(),
                        then_yield: false,
                    },
                    args,
                };
                let statement = match binding {
                    Some(name) => Stmt::Let {
                        name: name.clone(),
                        value: match act.as_action() {
                            Some(action) => Rhs::Action(action),
                            None => Rhs::Expr(Expr::Act(Box::new(act))),
                        },
                    },
                    None => Stmt::Do {
                        value: Expr::Act(Box::new(act)),
                    },
                };
                self.edit(&Edit::InsertStatement {
                    at: position.clone(),
                    statement: Box::new(statement),
                })?;
            }
            Operation::RemoveStep { action, .. } => {
                let statement = self.carrier(action, catalog)?;
                self.edit(&Edit::RemoveStatement { statement })?;
            }
            Operation::MoveStep { action, to, .. } => {
                let site = self.locate(action)?;
                let destination = self.locate(&to.block)?;
                let mut parent = site.clone();
                parent.path.pop();
                if destination.unit == parent.unit && destination.path.starts_with(&parent.path) {
                    return Err(at(action, Cause::IntoItself));
                }
                let statement = self.carrier(action, catalog)?;
                self.edit(&Edit::MoveStatement {
                    statement,
                    to: to.clone(),
                })?;
            }
            Operation::ChangeCondition {
                statement,
                condition,
                ..
            } => {
                header(condition, statement)?;
                let site = self.locate(statement)?;
                match node(&mut self.document, &site) {
                    Some(NodeMut::Stmt(
                        Stmt::If { condition, .. } | Stmt::While { condition, .. },
                    )) => header(condition, statement)?,
                    _ => return Err(at(statement, Cause::NoCondition)),
                }
                self.edit(&Edit::SetCondition {
                    statement: statement.clone(),
                    condition: condition.clone(),
                })?;
            }
            Operation::WrapSteps {
                first,
                last,
                wrapper,
                ..
            } => self.wrap(first, last, wrapper, catalog)?,
            Operation::UnwrapSteps {
                wrapper, branch, ..
            } => self.unwrap(wrapper, *branch, catalog)?,
        }
        Ok(())
    }

    fn wrap(
        &mut self,
        first: &Site,
        last: &Site,
        wrapper: &Wrapper,
        catalog: &dyn FunctionCatalog,
    ) -> Result<(), Fault> {
        let first_statement = self.carrier(first, catalog)?;
        let last_statement = self.carrier(last, catalog)?;
        let (block, start) = self.statement(&first_statement)?;
        let (end_block, end) = self.statement(&last_statement)?;
        if block != end_block
            || start > end
            || self.block(&block)[start..=end]
                .iter()
                .any(|stmt| step(stmt, catalog).is_none())
        {
            return Err(at(first, Cause::NotContiguousSteps));
        }
        match wrapper {
            Wrapper::If { condition } => header(condition, first)?,
            Wrapper::For { iterable, .. } => header(iterable, first)?,
            Wrapper::Try { .. } => {}
        }
        let body_index = if matches!(wrapper, Wrapper::Try { .. }) {
            0
        } else {
            1
        };
        // One change, so the block is never empty part way through.
        self.splice(&block, |statements, shadows| {
            let body: Vec<Stmt> = statements.drain(start..=end).collect();
            let statement = wrapped(wrapper, body);
            let mut shadow = Shadow::fresh(Node::Stmt(&statement));
            shadow.children[body_index].children = shadows.drain(start..=end).collect();
            statements.insert(start, statement);
            shadows.insert(start, shadow);
        });
        Ok(())
    }

    fn unwrap(
        &mut self,
        base: &Site,
        branch: Branch,
        catalog: &dyn lash_kernel_doc::FunctionCatalog,
    ) -> Result<(), Fault> {
        let (block, index) = self.statement(base)?;
        let site = self.locate_as(base, NodeClass::Statement)?;
        let stmt = &self.block(&block)[index];
        let (body_index, binding, loop_body) = match (stmt, branch) {
            (
                Stmt::If {
                    condition,
                    then_block,
                    else_block,
                },
                Branch::Then | Branch::Else,
            ) => {
                header(condition, base)?;
                let discarded = if branch == Branch::Then {
                    else_block
                } else {
                    then_block
                };
                if !discarded.is_empty() {
                    return Err(at(base, Cause::NonEmptyDiscardedArm));
                }
                (if branch == Branch::Then { 1 } else { 2 }, None, false)
            }
            (
                Stmt::For {
                    iterable,
                    else_block,
                    ..
                },
                Branch::Body,
            ) => {
                header(iterable, base)?;
                if !else_block.is_empty() {
                    return Err(at(base, Cause::NonEmptyDiscardedArm));
                }
                (1, Some(()), true)
            }
            (Stmt::Try(scope), Branch::Body) => {
                if scope
                    .catch
                    .as_ref()
                    .is_some_and(|catch| !catch.body.is_empty())
                    || scope.finally.as_ref().is_some_and(|body| !body.is_empty())
                {
                    return Err(at(base, Cause::NonEmptyDiscardedArm));
                }
                (0, None, false)
            }
            _ => return Err(at(base, Cause::UnsupportedCanvasEdit)),
        };
        if binding.is_some() {
            let graph = lash_kernel_check::derive(&self.document, catalog)
                .map_err(|error| Box::new(crate::apply::FaultKind::Unlinked(error)))?;
            if let Some(binding) = graph.bindings().iter().find(|binding| {
                graph.node(binding.declared_at).site == site
                    && binding.kind == lash_kernel_check::BindingKind::For
            }) && graph.nodes().iter().any(|node| {
                references(&node.kind).iter().any(|reference| {
                    **reference == lash_kernel_check::Reference::Binding(binding.id)
                })
            }) {
                return Err(at(base, Cause::EscapingBinding));
            }
        }
        if loop_body {
            let Stmt::For { body, .. } = &self.block(&block)[index] else {
                unreachable!()
            };
            if loses_control(body) {
                return Err(at(base, Cause::ControlTargetLost));
            }
        }
        // One change, so the block is never empty part way through.
        self.splice(&block, |statements, shadows| {
            let body = match statements.remove(index) {
                Stmt::If {
                    then_block,
                    else_block,
                    ..
                } => {
                    if branch == Branch::Then {
                        then_block
                    } else {
                        else_block
                    }
                }
                Stmt::For { body, .. } => body,
                Stmt::Try(scope) => scope.body,
                _ => unreachable!(),
            };
            let mut shadow = shadows.remove(index);
            let children = std::mem::take(&mut shadow.children[body_index].children);
            statements.splice(index..index, body);
            shadows.splice(index..index, children);
        });
        Ok(())
    }
}

/// The statement `wrapper` makes of `body`.
fn wrapped(wrapper: &Wrapper, body: Vec<Stmt>) -> Stmt {
    match wrapper {
        Wrapper::If { condition } => Stmt::If {
            condition: condition.clone(),
            then_block: body,
            else_block: vec![],
        },
        Wrapper::For { binding, iterable } => Stmt::For {
            metered: 0,
            binding: binding.clone(),
            iterable: iterable.clone(),
            via: None,
            body,
            else_block: vec![],
        },
        Wrapper::Try { catch_binding } => Stmt::Try(TryStmt {
            body,
            catch: Some(Catch {
                binding: catch_binding.clone(),
                body: vec![],
            }),
            finally: None,
        }),
    }
}

/// Nested loops retain their own targets; closures are distinct callables.
fn loses_control(body: &[Stmt]) -> bool {
    body.iter().any(|stmt| match stmt {
        Stmt::Break | Stmt::Continue => true,
        Stmt::If {
            then_block,
            else_block,
            ..
        } => loses_control(then_block) || loses_control(else_block),
        Stmt::Try(scope) => {
            loses_control(&scope.body)
                || scope.catch.as_ref().is_some_and(|c| loses_control(&c.body))
                || scope.finally.as_ref().is_some_and(|b| loses_control(b))
        }
        _ => false,
    })
}

/// Structural edits preserve what every surviving, unwritten use names,
/// including an outer binding hidden by an identically named destination local.
pub(crate) fn preserve_bindings(
    before: &lash_kernel_check::Graph,
    after: &lash_kernel_check::Graph,
    correspondence: &lash_kernel_doc::Correspondence,
    operations: &[Operation],
) -> Result<(), crate::EditRefusal> {
    for old in before.nodes() {
        let Some(survivor) = correspondence.survivor(&old.site) else {
            continue;
        };
        let Some(new) = after.node_at(&survivor.to) else {
            continue;
        };
        if survivor.edited {
            if let NodeKind::Action(ActionNode::Perform { args, .. }) = &old.kind {
                for (index, atom) in args.iter().enumerate() {
                    if operations.iter().any(|operation| matches!(operation, Operation::SetValue { target: ValueTarget::ToolArgument { action, index: changed, path }, .. } if action == &old.site && *changed as usize == index && path.is_empty())) { continue; }
                    let AtomNode::Variable(old_ref) = atom else {
                        continue;
                    };
                    let new_ref = match &new.kind {
                        NodeKind::Action(ActionNode::Perform { args, .. }) => match args.get(index)
                        {
                            Some(AtomNode::Variable(reference)) => Some(reference),
                            _ => None,
                        },
                        NodeKind::Expr(ExprNode::Act { args, .. }) => {
                            args.get(index)
                                .and_then(|arg| match &after.node(*arg).kind {
                                    NodeKind::Expr(ExprNode::Variable(reference)) => {
                                        Some(reference)
                                    }
                                    _ => None,
                                })
                        }
                        _ => None,
                    };
                    if new_ref.is_none_or(|new_ref| {
                        !same_reference(old_ref, new_ref, before, after, correspondence)
                    }) {
                        return Err(escaping(&old.site));
                    }
                }
            }
            continue;
        }
        let old_refs = references(&old.kind);
        let new_refs = references(&new.kind);
        let same = old_refs.len() == new_refs.len()
            && old_refs
                .iter()
                .zip(new_refs)
                .all(|(old, new)| same_reference(old, new, before, after, correspondence));
        if !same {
            return Err(escaping(&old.site));
        }
    }
    Ok(())
}

pub(crate) fn references(kind: &NodeKind) -> Vec<&Reference> {
    let mut out = Vec::new();
    fn callee(value: &CalleeNode) -> Option<&Reference> {
        match value {
            CalleeNode::Value(reference) => Some(reference),
            _ => None,
        }
    }
    match kind {
        NodeKind::Expr(ExprNode::Variable(reference))
        | NodeKind::Stmt(StmtNode::Assign {
            place: PlaceNode::Variable(reference),
            ..
        }) => out.push(reference),
        NodeKind::Expr(ExprNode::Act {
            kind: ActNode::Call(value) | ActNode::Spawn(value),
            ..
        }) => out.extend(callee(value)),
        NodeKind::Action(action) => {
            let atoms: Vec<_> = match action {
                ActionNode::Perform { args, .. } => args.iter().collect(),
                ActionNode::Call {
                    callee: value,
                    args,
                }
                | ActionNode::Spawn {
                    callee: value,
                    args,
                } => {
                    out.extend(callee(value));
                    args.iter().collect()
                }
                ActionNode::Join { task } | ActionNode::Cancel { task } => vec![task],
                ActionNode::JoinMany { tasks, .. } => vec![tasks],
                ActionNode::Await { value, .. } => vec![value],
                ActionNode::Yield => vec![],
            };
            out.extend(atoms.into_iter().filter_map(|atom| {
                if let AtomNode::Variable(reference) = atom {
                    Some(reference)
                } else {
                    None
                }
            }));
        }
        _ => {}
    }
    out
}

fn same_reference(
    old: &Reference,
    new: &Reference,
    before: &lash_kernel_check::Graph,
    after: &lash_kernel_check::Graph,
    correspondence: &lash_kernel_doc::Correspondence,
) -> bool {
    match (old, new) {
        (Reference::Binding(old), Reference::Binding(new)) => {
            let old = before.binding(*old);
            let new = after.binding(*new);
            correspondence.successor(&before.node(old.declared_at).site)
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
