//! Scope declaration dominance and canonical loop-local bindings.
use super::*;
use lash_kernel_doc::{Action, Atom, Node};

struct Uses {
    first_statement: usize,
    first_assignment: bool,
    first: usize,
    last: usize,
    captured: bool,
}

struct LoopLocal {
    name: Name,
    id: usize,
    body: std::ops::Range<usize>,
}

/// Positions in one traversal let loop bodies prove that no use escapes.
/// A first use also records whether a straight-line assignment dominates it.
#[derive(Default)]
struct Analysis {
    uses: BTreeMap<Name, Uses>,
    loops: Vec<LoopLocal>,
    position: usize,
    next_loop: usize,
    captures: BTreeMap<FunctionId, BTreeSet<Name>>,
}

fn names(node: Node<'_>, mut use_name: impl FnMut(&Name)) {
    match node {
        Node::Expr(Expr::Variable(name))
        | Node::Stmt(Stmt::Assign {
            place: Place::Variable(name),
            ..
        }) => use_name(name),
        Node::Expr(Expr::Act(act)) => {
            if let ActKind::Call(Callee::Value(name)) | ActKind::Spawn(Callee::Value(name)) =
                &act.kind
            {
                use_name(name);
            }
        }
        Node::Action(action) => {
            let (callee, atoms): (Option<&Callee>, &[Atom]) = match action {
                Action::Call { callee, args } | Action::Spawn { callee, args } => {
                    (Some(callee), args)
                }
                Action::Perform { args, .. } => (None, args),
                Action::Join { task: atom }
                | Action::JoinMany { tasks: atom, .. }
                | Action::Await { value: atom, .. }
                | Action::Cancel { task: atom }
                | Action::CancelEffect { task: atom } => (None, std::slice::from_ref(atom)),
                Action::Yield => (None, &[]),
            };
            if let Some(Callee::Value(name)) = callee {
                use_name(name);
            }
            for atom in atoms {
                if let Atom::Variable(name) = atom {
                    use_name(name);
                }
            }
        }
        _ => {}
    }
}

impl Analysis {
    fn record(&mut self, name: &Name, statement: usize, assignment: bool, captured: bool) {
        self.uses
            .entry(name.clone())
            .and_modify(|usage| {
                if usage.first_statement == statement {
                    usage.first_assignment = false;
                }
                usage.last = self.position;
                usage.captured |= captured;
            })
            .or_insert(Uses {
                first_statement: statement,
                first_assignment: assignment,
                first: self.position,
                last: self.position,
                captured,
            });
    }

    fn walk<'a>(
        &mut self,
        node: Node<'a>,
        statement: usize,
        top_level: bool,
        pending: &mut Vec<Node<'a>>,
        definitions: &'a BTreeMap<FunctionId, lash_kernel_doc::FunctionDefinition>,
    ) {
        self.position += 1;
        let assignment = top_level && matches!(node, Node::Stmt(Stmt::Assign { .. }));
        names(node, |name| self.record(name, statement, assignment, false));
        if let Node::Expr(Expr::Closure(closure)) = node {
            let captures = self.captures.entry(closure.function).or_insert_with(|| {
                let mut names_read = BTreeSet::new();
                let mut functions = BTreeSet::new();
                let mut pending = vec![node];
                while let Some(node) = pending.pop() {
                    names(node, |name| {
                        names_read.insert(name.clone());
                    });
                    if let Node::Expr(Expr::Closure(closure)) = node {
                        if functions.insert(closure.function)
                            && let Some(body) = definitions
                                .get(&closure.function)
                                .and_then(|definition| definition.body())
                        {
                            pending.push(Node::Block(&body.block));
                        }
                    } else {
                        node.children_into(&mut pending);
                    }
                }
                names_read
            });
            // Move the summary out while updating the per-block use index.
            let captures = std::mem::take(captures);
            for name in &captures {
                self.record(name, statement, false, true);
            }
            self.captures.insert(closure.function, captures);
        }
        let loop_id = if matches!(node, Node::Stmt(Stmt::For { .. })) {
            let id = self.next_loop;
            self.next_loop += 1;
            Some(id)
        } else {
            None
        };
        let candidate = match node {
            Node::Stmt(Stmt::For { binding, body, .. }) => match body.first() {
                Some(Stmt::Assign {
                    place: Place::Variable(write),
                    value: Rhs::Expr(Expr::Variable(read)),
                    ..
                }) if read == binding => Some(write.clone()),
                _ => None,
            },
            _ => None,
        };
        let start = pending.len();
        node.children_into(pending);
        let end = pending.len();
        let mut body_range = 0..0;
        for index in start..end {
            let before = self.position + 1;
            self.walk(pending[index], statement, false, pending, definitions);
            if index == start + 1 {
                body_range = before..self.position + 1;
            }
        }
        pending.truncate(start);
        if let (Some(name), Some(id)) = (candidate, loop_id) {
            self.loops.push(LoopLocal {
                name,
                id,
                body: body_range,
            });
        }
    }
}

/// Apply selected loop bindings in one pass, keeping their parallel notes.
/// Expressions have no statement blocks; closure bodies are in the function table.
fn rewrite_loops(
    block: &mut [Stmt],
    notes: &mut [Note],
    selected: &mut BTreeMap<usize, Name>,
    next_loop: &mut usize,
) {
    for (statement, note) in block.iter_mut().zip(notes) {
        let local = if matches!(statement, Stmt::For { .. }) {
            let id = *next_loop;
            *next_loop += 1;
            selected.remove(&id)
        } else {
            None
        };
        let mut block_index = 0;
        for child in lash_kernel_doc::NodeMut::Stmt(&mut *statement).children() {
            if let lash_kernel_doc::NodeMut::Block(body) = child {
                rewrite_loops(body, &mut note.blocks[block_index], selected, next_loop);
                block_index += 1;
            }
        }
        if let Some(name) = local
            && let Stmt::For { binding, body, .. } = statement
        {
            *binding = name;
            body.remove(0);
            note.blocks[0].remove(0);
        }
    }
}

impl Lowerer<'_> {
    /// A straight-line first assignment declares unless its RHS or an earlier
    /// node reads or captures the name. Carried names outlive the root block.
    pub(super) fn finish_declarations(&self, body: Buf, carried: Option<&BTreeSet<String>>) -> Buf {
        let (mut statements, mut notes) = body.into_parts();
        let declarations: BTreeMap<_, _> = statements
            .iter()
            .enumerate()
            .filter_map(|(index, statement)| {
                if let Stmt::Let {
                    name,
                    value: Rhs::Expr(Expr::Literal(Literal::Absent)),
                } = statement
                {
                    Some((name.clone(), index))
                } else {
                    None
                }
            })
            .collect();
        if declarations.is_empty() {
            return Buf::from_parts(statements, notes);
        }
        let mut analysis = Analysis::default();
        let mut pending = Vec::new();
        for (index, statement) in statements.iter().enumerate() {
            analysis.walk(
                Node::Stmt(statement),
                index,
                true,
                &mut pending,
                &self.local_functions,
            );
        }
        let mut remove = BTreeSet::new();
        let mut selected = BTreeMap::new();
        for local in analysis.loops {
            let Some(&index) = declarations.get(&local.name) else {
                continue;
            };
            let usage = &analysis.uses[&local.name];
            if carried.is_some_and(|names| names.contains(local.name.as_str()))
                || usage.captured
                || !local.body.contains(&usage.first)
                || !local.body.contains(&usage.last)
            {
                continue;
            }
            selected.insert(local.id, local.name);
            remove.insert(index);
        }
        if !selected.is_empty() {
            rewrite_loops(&mut statements, &mut notes, &mut selected, &mut 0);
        }
        for (name, index) in declarations {
            if remove.contains(&index) {
                continue;
            }
            let Some(usage) = analysis.uses.get(&name) else {
                continue;
            };
            if usage.first_statement <= index || !usage.first_assignment {
                continue;
            }
            let next = &mut statements[usage.first_statement];
            if let Stmt::Assign { value, .. } = next {
                *next = Stmt::Let {
                    name,
                    value: value.clone(),
                };
                remove.insert(index);
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
