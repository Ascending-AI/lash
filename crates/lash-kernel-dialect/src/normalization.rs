//! Printer verification allows alpha-renaming of private lexical bindings only.
use lash_kernel_doc as k;
use std::collections::BTreeMap;
use {lash_kernel_value::FunctionId, lash_kernel_value::Name, lash_kernel_value::sites::Site};

/// Verify a module after private alpha-renaming, local function naming and
/// exact import-slot renumbering. Executable node shapes stay identical.
/// The result maps lowered library Sites back to the printed document.
pub fn module(
    old: &k::Document,
    new: &k::Document,
    library: &dyn crate::Library,
) -> Option<BTreeMap<FunctionId, FunctionId>> {
    if old.exports.is_empty() {
        return None;
    }
    let imports_equal = old
        .imports
        .values()
        .all(|a| new.imports.values().any(|b| a == b))
        && new
            .imports
            .values()
            .all(|a| old.imports.values().any(|b| a == b));
    if old.numbers != new.numbers
        || old.requires != new.requires
        || old.library != new.library
        || old.private_bindings != new.private_bindings
        || !imports_equal
        || old.exports.keys().ne(new.exports.keys())
    {
        return None;
    }
    let mut functions = BTreeMap::new();
    for (name, export) in &old.exports {
        let (k::Export::Function(a), Some(k::Export::Function(b))) =
            (export, new.exports.get(name))
        else {
            return None;
        };
        if a.signature != b.signature
            || a.convention != b.convention
            || a.asynchronous != b.asynchronous
            || a.rest != b.rest
        {
            return None;
        }
        one(
            old,
            a.function,
            new,
            b.function,
            &BTreeMap::new(),
            &mut functions,
        )?;
    }
    if functions.len() != new.functions.len()
        || functions
            .values()
            .copied()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            != old.functions.len()
    {
        return None;
    }
    let mut main = new.main.clone();
    let mut names = BTreeMap::new();
    pair(
        k::Node::Block(old.main.as_deref().unwrap_or_default()),
        k::Node::Block(main.as_deref().unwrap_or_default()),
        &mut names,
    )?;
    if let Some(main) = &mut main {
        rename(k::NodeMut::Block(main), &names);
        translate(k::NodeMut::Block(main), &functions, &BTreeMap::new());
    }
    if old.main != main {
        return None;
    }
    // Alpha-renaming must preserve resolved binding identity, including
    // shadowing and writes from a nested closure. Spelling alone is no proof.
    let before = lash_kernel_check::derive(old, library).ok()?;
    let after = lash_kernel_check::derive(new, library).ok()?;
    let site = |at: &Site| {
        let mut at = at.clone();
        if let lash_kernel_value::sites::Unit::Library(id) = &mut at.unit {
            *id = functions.get(id).copied().unwrap_or(*id);
        }
        at
    };
    let mut available = BTreeMap::new();
    for node in before.nodes() {
        if available
            .insert(lexical_path(&before, node.id), node)
            .is_some()
        {
            return None;
        }
    }
    for node in after.nodes() {
        let path = lexical_path(&after, node.id)
            .iter()
            .map(&site)
            .collect::<Vec<_>>();
        let old_node = available.remove(&path)?;
        let old_refs = lash_kernel_check::binding_references(&old_node.kind);
        let new_refs = lash_kernel_check::binding_references(&node.kind);
        if old_refs.len() != new_refs.len() {
            return None;
        }
        for (a, b) in old_refs.into_iter().zip(new_refs) {
            match (a, b) {
                (
                    lash_kernel_check::Reference::Binding(a),
                    lash_kernel_check::Reference::Binding(b),
                ) => {
                    let a = before.binding(*a);
                    let b = after.binding(*b);
                    if a.kind != b.kind
                        || lexical_path(&before, a.declared_at)
                            != lexical_path(&after, b.declared_at)
                                .iter()
                                .map(&site)
                                .collect::<Vec<_>>()
                    {
                        return None;
                    }
                }
                (a, b) if a == b => {}
                _ => return None,
            }
        }
    }
    if !available.is_empty() {
        return None;
    }
    Some(functions)
}
/// Code coordinates followed by their enclosing closure creation coordinates.
fn lexical_path(graph: &lash_kernel_check::Graph, id: lash_kernel_check::NodeId) -> Vec<Site> {
    let mut node = graph.node(id);
    let mut path = vec![node.site.clone()];
    while let Some(parent) = node.parent {
        let parent = graph.node(parent);
        if node.site.unit != parent.site.unit {
            path.push(parent.site.clone());
        }
        node = parent;
    }
    path
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
    // Pair local bodies by the reference at the same tree coordinate.
    // Installed helper identities remain exact, regardless of display names.
    let mut pending = vec![(k::Node::Block(&old_body.block), k::Node::Block(&body.block))];
    while let Some((a, b)) = pending.pop() {
        let a_ids = a.functions();
        let b_ids = b.functions();
        if a_ids.len() != b_ids.len() {
            return None;
        }
        for (a, b) in a_ids.into_iter().zip(b_ids) {
            match (old.functions.contains_key(a), new.functions.contains_key(b)) {
                (true, true) => one(old, *a, new, *b, &names, functions)?,
                (false, false) if a == b => {}
                _ => return None,
            }
        }
        pending.extend(a.children().into_iter().zip(b.children()));
    }
    let mut slots = BTreeMap::new();
    for (slot, import) in &body.imports {
        let (old_slot, _) = old_body
            .imports
            .iter()
            .find(|(_, candidate)| *candidate == import)?;
        slots.insert(*slot, *old_slot);
    }
    rename(k::NodeMut::Block(&mut body.block), &names);
    translate(k::NodeMut::Block(&mut body.block), functions, &slots);
    body.imports = std::mem::take(&mut body.imports)
        .into_iter()
        .map(|(slot, import)| (slots[&slot], import))
        .collect();
    body.functions = std::mem::take(&mut body.functions)
        .into_iter()
        .map(|(id, name)| {
            let id = functions.get(&id).copied().unwrap_or(id);
            let name = old.functions.get(&id).map_or(name, |f| f.name.clone());
            (id, name)
        })
        .collect();
    b.name.clone_from(&a.name);
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
            k::Action::Join { task }
            | k::Action::Cancel { task }
            | k::Action::CancelEffect { task } => atom(task, names),
            k::Action::JoinMany { tasks, .. } => atom(tasks, names),
            k::Action::Yield => {}
        },
        _ => {}
    }
    for child in node.children() {
        rename(child, names);
    }
}

fn translate(
    mut node: k::NodeMut<'_>,
    functions: &BTreeMap<FunctionId, FunctionId>,
    slots: &BTreeMap<lash_kernel_value::ImportSlot, lash_kernel_value::ImportSlot>,
) {
    for id in node.functions_mut() {
        *id = functions.get(id).copied().unwrap_or(*id);
    }
    let slot = |s: &mut lash_kernel_value::ImportSlot| {
        *s = slots.get(s).copied().unwrap_or(*s);
    };
    let target = |t: &mut k::FunctionTarget| {
        if let k::FunctionTarget::Import(s) = t {
            slot(s);
        }
    };
    match &mut node {
        k::NodeMut::Expr(k::Expr::Act(a)) => match &mut a.kind {
            k::ActKind::Call(k::Callee::Import(s)) | k::ActKind::Perform { slot: s, .. } => slot(s),
            k::ActKind::Await(t) => target(t),
            _ => {}
        },
        k::NodeMut::Action(a) => match a {
            k::Action::Call {
                callee: k::Callee::Import(s),
                ..
            }
            | k::Action::Perform { slot: s, .. } => slot(s),
            k::Action::Await { policy, .. } => target(policy),
            _ => {}
        },
        k::NodeMut::Stmt(k::Stmt::ReturnVia { policy, .. }) => target(policy),
        _ => {}
    }
    for child in node.children() {
        translate(child, functions, slots);
    }
}
