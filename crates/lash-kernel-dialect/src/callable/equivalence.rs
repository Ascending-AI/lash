//! Printer verification allows alpha-renaming of private lexical bindings only.
use lash_kernel_doc as k;
use lash_vocabulary::{FunctionId, Name, sites::Site};
use std::collections::BTreeMap;

/// Match two selected lexical trees. Every executable field is compared after
/// pairing binders at the same code coordinates; external names stay exact.
pub fn equivalent(
    old: &k::Document,
    old_id: FunctionId,
    new: &k::Document,
    new_id: FunctionId,
) -> Option<BTreeMap<FunctionId, FunctionId>> {
    let mut functions = BTreeMap::new();
    one(old, old_id, new, new_id, &BTreeMap::new(), &mut functions)?;
    Some(functions)
}
fn one(
    old: &k::Document,
    old_id: FunctionId,
    new: &k::Document,
    new_id: FunctionId,
    outer: &BTreeMap<Name, Name>,
    functions: &mut BTreeMap<FunctionId, FunctionId>,
) -> Option<()> {
    if let Some(id) = functions.get(&new_id) {
        return (*id == old_id).then_some(());
    }
    let a = old.functions.get(&old_id)?;
    let mut b = new.functions.get(&new_id)?.clone();
    if a.signature.params.len() != b.signature.params.len() {
        return None;
    }
    let mut names = outer.clone();
    for (a, b) in a.signature.params.iter().zip(&mut b.signature.params) {
        names.insert(b.name.clone(), a.name.clone());
        b.name.clone_from(&a.name);
    }
    let old_body = a.body()?;
    let body = b.body_mut()?;
    pair(
        k::Node::Block(&old_body.block),
        k::Node::Block(&body.block),
        &mut names,
    )?;
    // A closure reference is matched by its original Site, never by map order.
    let mut pending = vec![(k::Node::Block(&old_body.block), k::Node::Block(&body.block))];
    while let Some((a, b)) = pending.pop() {
        if let (k::Node::Expr(k::Expr::Closure(a)), k::Node::Expr(k::Expr::Closure(b))) = (a, b) {
            one(old, a.function, new, b.function, &names, functions)?;
        }
        pending.extend(a.children().into_iter().zip(b.children()));
    }
    rename(k::NodeMut::Block(&mut body.block), &names);
    let _: Result<(), ()> = k::NodeMut::Block(&mut body.block).try_each_function_mut(
        Site::new(lash_vocabulary::sites::Unit::Library(new_id), []),
        &mut |_, id| {
            *id = functions.get(id).copied().unwrap_or(*id);
            Ok(())
        },
    );
    body.functions = std::mem::take(&mut body.functions)
        .into_iter()
        .map(|(id, name)| (functions.get(&id).copied().unwrap_or(id), name))
        .collect();
    if *a != b {
        return None;
    }
    functions.insert(new_id, old_id);
    Some(())
}
fn pair(a: k::Node<'_>, b: k::Node<'_>, names: &mut BTreeMap<Name, Name>) -> Option<()> {
    let binding = match (a, b) {
        (
            k::Node::Stmt(k::Stmt::Let { name: a, .. }),
            k::Node::Stmt(k::Stmt::Let { name: b, .. }),
        )
        | (
            k::Node::Stmt(k::Stmt::For { binding: a, .. }),
            k::Node::Stmt(k::Stmt::For { binding: b, .. }),
        )
        | (
            k::Node::Expr(k::Expr::Let { name: a, .. }),
            k::Node::Expr(k::Expr::Let { name: b, .. }),
        ) => Some((a, b)),
        (k::Node::Stmt(k::Stmt::Try(a)), k::Node::Stmt(k::Stmt::Try(b))) => a
            .catch
            .as_ref()
            .zip(b.catch.as_ref())
            .map(|(a, b)| (&a.binding, &b.binding)),
        _ => None,
    };
    if let Some((a, b)) = binding
        && names
            .insert(b.clone(), a.clone())
            .is_some_and(|previous| previous != *a)
    {
        return None;
    }

    let a = a.children();
    let b = b.children();
    if a.len() != b.len() {
        return None;
    }
    for (a, b) in a.into_iter().zip(b) {
        pair(a, b, names)?;
    }
    Some(())
}
pub(super) fn rename(node: k::NodeMut<'_>, names: &BTreeMap<Name, Name>) {
    fn name(n: &mut Name, names: &BTreeMap<Name, Name>) {
        if let Some(new) = names.get(n) {
            n.clone_from(new);
        }
    }
    fn atom(a: &mut k::Atom, names: &BTreeMap<Name, Name>) {
        if let k::Atom::Variable(n) = a {
            name(n, names);
        }
    }
    fn callee(c: &mut k::Callee, names: &BTreeMap<Name, Name>) {
        if let k::Callee::Value(n) = c {
            name(n, names);
        }
    }
    let mut node = node;
    match &mut node {
        k::NodeMut::Stmt(
            k::Stmt::Let { name: n, .. }
            | k::Stmt::For { binding: n, .. }
            | k::Stmt::Assign {
                place: k::Place::Variable(n),
                ..
            },
        )
        | k::NodeMut::Expr(k::Expr::Variable(n) | k::Expr::Let { name: n, .. }) => name(n, names),
        k::NodeMut::Stmt(k::Stmt::Try(t)) => {
            if let Some(catch) = &mut t.catch {
                name(&mut catch.binding, names);
            }
        }
        k::NodeMut::Expr(k::Expr::Act(a)) => match &mut a.kind {
            k::ActKind::Call(c) | k::ActKind::Spawn(c) => callee(c, names),
            _ => {}
        },
        k::NodeMut::Action(action) => match action {
            k::Action::Call { callee: c, args } | k::Action::Spawn { callee: c, args } => {
                callee(c, names);
                for a in args {
                    atom(a, names);
                }
            }
            k::Action::Perform { args, .. } => {
                for a in args {
                    atom(a, names);
                }
            }
            k::Action::Await { value, .. } => atom(value, names),
            k::Action::Join { task } | k::Action::Cancel { task } => atom(task, names),
            k::Action::JoinMany { tasks, .. } => atom(tasks, names),
            k::Action::Yield => {}
        },
        _ => {}
    }
    for child in node.children() {
        rename(child, names);
    }
}
