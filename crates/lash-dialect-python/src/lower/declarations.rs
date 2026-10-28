//! Scope declaration dominance and canonical loop-local bindings.
use super::*;

impl Lowerer<'_> {
    /// Declaration dominance (FIG-6402 rev2 §11): a straight-line first
    /// assignment may declare the slot, unless its RHS or an earlier node
    /// reads or captures it. Nested assignments cannot declare an outer slot.
    /// A cell's carried names are also used after its root block finishes.
    pub(super) fn finish_declarations(&self, body: Buf, carried: Option<&BTreeSet<String>>) -> Buf {
        let (mut statements, mut notes) = body.into_parts();
        /// Whether `action` reads `name`, as the expression it stands for
        /// would: as its callee or as one of its arguments.
        fn action_mentions(action: &lash_kernel_doc::Action, name: &Name) -> bool {
            use lash_kernel_doc::{Action, Atom};

            let (callee, atoms): (Option<&Callee>, &[Atom]) = match action {
                Action::Call { callee, args } | Action::Spawn { callee, args } => {
                    (Some(callee), args)
                }
                Action::Perform { args, .. } => (None, args),
                Action::Join { task: atom }
                | Action::JoinMany { tasks: atom, .. }
                | Action::Await { value: atom, .. }
                | Action::Cancel { task: atom } => (None, std::slice::from_ref(atom)),
                Action::Yield => (None, &[]),
            };
            matches!(callee, Some(Callee::Value(read)) if read == name)
                || atoms
                    .iter()
                    .any(|atom| matches!(atom, Atom::Variable(read) if read == name))
        }
        fn mentions(
            node: lash_kernel_doc::Node<'_>,
            name: &Name,
            definitions: &BTreeMap<FunctionId, lash_kernel_doc::FunctionDefinition>,
        ) -> bool {
            let mut pending = vec![node];
            let mut closures = BTreeSet::new();
            while let Some(node) = pending.pop() {
                match node {
                    lash_kernel_doc::Node::Expr(Expr::Variable(read)) if read == name => {
                        return true;
                    }
                    lash_kernel_doc::Node::Expr(Expr::Act(act))
                        if matches!(&act.kind,
                        ActKind::Call(Callee::Value(read)) | ActKind::Spawn(Callee::Value(read)) if read == name) =>
                    {
                        return true;
                    }
                    lash_kernel_doc::Node::Stmt(Stmt::Assign {
                        place: Place::Variable(write),
                        ..
                    }) if write == name => return true,
                    lash_kernel_doc::Node::Action(action) if action_mentions(action, name) => {
                        return true;
                    }
                    lash_kernel_doc::Node::Expr(Expr::Closure(closure)) => {
                        if closures.insert(closure.function)
                            && let Some(body) = definitions
                                .get(&closure.function)
                                .and_then(|definition| definition.body())
                        {
                            pending.push(lash_kernel_doc::Node::Block(&body.block));
                        }
                    }
                    _ => pending.extend(node.children()),
                }
            }
            false
        }
        let mut remove = BTreeSet::new();
        for index in 0..statements.len() {
            let Stmt::Let {
                name,
                value: Rhs::Expr(Expr::Literal(Literal::Absent)),
            } = &statements[index]
            else {
                continue;
            };
            let name = name.clone();
            // A loop-only local has the loop's lifetime even when a branch
            // encloses it. Keep the same canonical form as a root-level loop;
            // a read, write or capture anywhere outside that loop forbids it.
            if !carried.is_some_and(|names| names.contains(name.as_str())) {
                fn loops(
                    node: lash_kernel_doc::Node<'_>,
                    path: &mut Vec<u32>,
                    out: &mut Vec<Vec<u32>>,
                ) {
                    if matches!(node, lash_kernel_doc::Node::Stmt(Stmt::For { .. })) {
                        out.push(path.clone());
                    }
                    for (i, child) in node.children().into_iter().enumerate() {
                        path.push(i as u32);
                        loops(child, path, out);
                        path.pop();
                    }
                }
                fn captured(
                    node: lash_kernel_doc::Node<'_>,
                    name: &Name,
                    definitions: &BTreeMap<FunctionId, lash_kernel_doc::FunctionDefinition>,
                ) -> bool {
                    match node {
                        lash_kernel_doc::Node::Expr(Expr::Closure(_)) => {
                            mentions(node, name, definitions)
                        }
                        _ => node
                            .children()
                            .into_iter()
                            .any(|child| captured(child, name, definitions)),
                    }
                }
                // Map an AST path to its parallel statement notes. Expressions
                // can own blocks too; count those rather than assuming indices.
                fn note_at<'a>(
                    stmts: &[Stmt],
                    notes: &'a mut [Note],
                    path: &[u32],
                ) -> Option<&'a mut Note> {
                    let (&index, rest) = path.split_first()?;
                    let stmt = stmts.get(index as usize)?;
                    let note = notes.get_mut(index as usize)?;
                    if rest.is_empty() {
                        return Some(note);
                    }
                    fn blocks<'b>(
                        node: lash_kernel_doc::Node<'b>,
                        path: &mut Vec<u32>,
                        out: &mut Vec<(Vec<u32>, &'b [Stmt])>,
                    ) {
                        if let lash_kernel_doc::Node::Block(body) = node {
                            out.push((path.clone(), body));
                            return;
                        }
                        for (i, child) in node.children().into_iter().enumerate() {
                            path.push(i as u32);
                            blocks(child, path, out);
                            path.pop();
                        }
                    }
                    let mut nested = Vec::new();
                    blocks(
                        lash_kernel_doc::Node::Stmt(stmt),
                        &mut Vec::new(),
                        &mut nested,
                    );
                    let (block_index, (prefix, block)) = nested
                        .into_iter()
                        .enumerate()
                        .find(|(_, (prefix, _))| rest.starts_with(prefix))?;
                    note_at(
                        block,
                        note.blocks.get_mut(block_index)?,
                        &rest[prefix.len()..],
                    )
                }
                let mut candidates = Vec::new();
                loops(
                    lash_kernel_doc::Node::Block(&statements),
                    &mut Vec::new(),
                    &mut candidates,
                );
                for path in candidates {
                    let Some(lash_kernel_doc::Node::Stmt(Stmt::For {
                        binding,
                        iterable,
                        body: pass,
                        else_block,
                        ..
                    })) = path
                        .iter()
                        .try_fold(lash_kernel_doc::Node::Block(&statements), |node, index| {
                            node.children().get(*index as usize).copied()
                        })
                    else {
                        continue;
                    };
                    if !matches!(pass.first(), Some(Stmt::Assign { place: Place::Variable(write), value: Rhs::Expr(Expr::Variable(read)), .. }) if write == &name && read == binding)
                        || mentions(
                            lash_kernel_doc::Node::Expr(iterable),
                            &name,
                            &self.local_functions,
                        )
                        || mentions(
                            lash_kernel_doc::Node::Block(else_block),
                            &name,
                            &self.local_functions,
                        )
                        || captured(
                            lash_kernel_doc::Node::Block(pass),
                            &name,
                            &self.local_functions,
                        )
                    {
                        continue;
                    }
                    fn outside(
                        node: lash_kernel_doc::Node<'_>,
                        path: &[u32],
                        name: &Name,
                        definitions: &BTreeMap<FunctionId, lash_kernel_doc::FunctionDefinition>,
                    ) -> bool {
                        let Some((&selected, rest)) = path.split_first() else {
                            return false;
                        };
                        node.children()
                            .into_iter()
                            .enumerate()
                            .any(|(index, child)| {
                                if index == selected as usize {
                                    outside(child, rest, name, definitions)
                                } else {
                                    mentions(child, name, definitions)
                                }
                            })
                    }
                    if outside(
                        lash_kernel_doc::Node::Block(&statements),
                        &path,
                        &name,
                        &self.local_functions,
                    ) {
                        continue;
                    }
                    let expression_blocks = {
                        fn count(node: lash_kernel_doc::Node<'_>) -> usize {
                            if matches!(node, lash_kernel_doc::Node::Block(_)) {
                                1
                            } else {
                                node.children().into_iter().map(count).sum()
                            }
                        }
                        count(lash_kernel_doc::Node::Expr(iterable))
                    };
                    if let Some(note) = note_at(&statements, &mut notes, &path)
                        && let Some(notes) = note.blocks.get_mut(expression_blocks)
                    {
                        notes.remove(0);
                    }
                    if let Some(lash_kernel_doc::NodeMut::Stmt(Stmt::For {
                        binding,
                        body: pass,
                        ..
                    })) = lash_kernel_doc::NodeMut::Block(&mut statements).descend(&path)
                    {
                        *binding = name.clone();
                        pass.remove(0);
                    }
                    remove.insert(index);
                    break;
                }
                if remove.contains(&index) {
                    continue;
                }
            }
            for next in statements.iter_mut().skip(index + 1) {
                if let Stmt::Assign {
                    place: Place::Variable(write),
                    value,
                    ..
                } = &*next
                    && write == &name
                {
                    let mentioned = match value {
                        Rhs::Expr(value) => mentions(
                            lash_kernel_doc::Node::Expr(value),
                            &name,
                            &self.local_functions,
                        ),
                        Rhs::Action(action) => action_mentions(action, &name),
                    };
                    if !mentioned {
                        *next = Stmt::Let {
                            name: name.clone(),
                            value: value.clone(),
                        };
                        remove.insert(index);
                    }
                    break;
                }
                if mentions(
                    lash_kernel_doc::Node::Stmt(next),
                    &name,
                    &self.local_functions,
                ) {
                    break;
                }
            }
        }
        let mut index = 0;
        statements.retain(|_| {
            let keep = !remove.contains(&index);
            index += 1;
            keep
        });
        let mut index = 0;
        notes.retain(|_| {
            let keep = !remove.contains(&index);
            index += 1;
            keep
        });
        Buf::from_parts(statements, notes)
    }
}
