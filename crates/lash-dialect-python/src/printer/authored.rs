//! The statements an authored region writes as their author did, where
//! that differs from the kernel surface (`crate::readable`). Each keeps
//! the node frames of what it spells, so captions and maps keep their
//! sites; a node the authored form spells by what holds it is implicit.

use std::collections::BTreeSet;

use super::{Renderer, k};
use lash_vocabulary::{Name, Type};

/// A clause of an `except` chain: the classes it names, or none for a
/// bare `except`, and its body.
enum Clause<'a> {
    Named(String, &'a k::Block),
    Raise,
    Everything(&'a k::Block),
}

impl Renderer<'_> {
    pub(super) fn authored_statement(&mut self, stmt: &k::Stmt) -> bool {
        match stmt {
            k::Stmt::Assign {
                place:
                    k::Place::Member(k::Member::Index {
                        target: k::Expr::Variable(list),
                        index: k::Expr::Call { function, args },
                    }),
                value,
                ..
            } if self.operators.unique("list.len") == Some(*function)
                && matches!(args.as_slice(), [k::Expr::Variable(of)] if of == list)
                && self.inline_rhs(value) =>
            {
                self.out.begin_node(None);
                self.name(list);
                self.out.end_node();
                self.implicit();
                self.out.write(".append(");
                self.write_inline_rhs(value);
                self.out.write(")\n");
                true
            }
            k::Stmt::Assign {
                place: k::Place::Variable(name),
                value: k::Rhs::Expr(value),
                order: k::AssignOrder::ValueFirst,
            } if self.inline.contains(value) => {
                let Some((operator, operand)) = self.in_place(name, value) else {
                    return false;
                };
                self.name(name);
                self.out.write(" ");
                self.out.write(operator);
                self.out.write("= ");
                self.out.begin_node(None);
                self.implicit();
                self.inline_expr(operand);
                self.out.end_node();
                self.out.write("\n");
                true
            }
            k::Stmt::Assign {
                place: k::Place::Variable(name),
                value:
                    k::Rhs::Action(k::Action::Call {
                        callee: k::Callee::Import(id),
                        args,
                    }),
                order: k::AssignOrder::ValueFirst,
            } if self.operators.is(id, "py.iadd") => {
                let [k::Atom::Variable(left), operand] = args.as_slice() else {
                    return false;
                };
                if left != name {
                    return false;
                }
                self.name(name);
                self.out.write(" += ");
                self.out.begin_node(None);
                self.atom(operand);
                self.out.end_node();
                self.out.write("\n");
                true
            }
            k::Stmt::Try(scope) if scope.catch.is_some() || scope.finally.is_some() => {
                let clauses = match &scope.catch {
                    Some(catch) => match clauses(self, &catch.binding, &catch.body) {
                        Some(clauses) => Some(clauses),
                        None => return false,
                    },
                    None => None,
                };
                self.out.write("try:\n");
                self.readable_block(&scope.body);
                if let Some(clauses) = clauses {
                    self.caption_next();
                    self.out.begin_node(None);
                    self.except(&clauses);
                    self.out.end_node();
                }
                if let Some(finally) = &scope.finally {
                    self.caption_next();
                    self.padding();
                    self.out.write("finally:\n");
                    self.readable_block(finally);
                }
                true
            }
            _ => false,
        }
    }

    /// `value` as `name op= operand`: the operator and its operand.
    fn in_place<'e>(&self, name: &Name, value: &'e k::Expr) -> Option<(&'static str, &'e k::Expr)> {
        let (operator, args) = match value {
            k::Expr::Act(act) => match &act.kind {
                k::ActKind::Call(k::Callee::Import(id)) if self.operators.is(id, "py.iadd") => {
                    ("+", act.args.as_slice())
                }
                _ => return None,
            },
            k::Expr::Call { function, args } => {
                let operator = self.operators.token(function, args, &self.scope.types)?;
                if !matches!(operator, "+" | "-" | "*" | "/" | "//" | "%" | "**") {
                    return None;
                }
                (operator, args.as_slice())
            }
            _ => return None,
        };
        match args {
            [k::Expr::Variable(left), operand] if left == name => Some((operator, operand)),
            _ => None,
        }
    }

    /// The `except` clauses of a chain, each in the frames of the test and
    /// arms that lowering it gives.
    fn except(&mut self, clauses: &[Clause<'_>]) {
        let Some((clause, rest)) = clauses.split_first() else {
            return;
        };
        match clause {
            Clause::Named(classes, then_block) => {
                self.padding();
                self.out.write("except ");
                self.out.write(classes);
                self.out.write(":\n");
                self.out.begin_node(None); // the test
                self.implicit();
                self.readable_block(then_block);
                self.caption_next();
                self.out.begin_node(None); // the other clauses
                self.except(rest);
                self.out.end_node();
                self.out.end_node();
            }
            Clause::Raise => self.implicit(),
            Clause::Everything(body) => {
                self.padding();
                self.out.write("except:\n");
                self.indent += 1;
                if body.is_empty() {
                    self.empty();
                }
                for stmt in *body {
                    self.node(k::Node::Stmt(stmt), None);
                }
                self.indent -= 1;
            }
        }
    }

    /// A local function's parameters, each annotated with the type its
    /// start signature gives it.
    pub(super) fn annotated(&mut self, params: &[Name], start: &k::CallablePresentation) {
        self.out.write("(");
        for (i, name) in params.iter().enumerate() {
            if i > 0 {
                self.out.write(", ");
            }
            self.name(name);
            let ty = start
                .signature
                .params
                .iter()
                .find(|param| param.name == *name)
                .map(|param| &param.ty);
            if let Some(annotation) = ty.and_then(annotation) {
                self.out.write(": ");
                self.out.write(&annotation);
            }
        }
        self.out.write(")");
    }
}

/// The clauses `block`, the body of a `catch` of `binding`, tests in turn.
fn clauses<'a>(
    renderer: &Renderer<'_>,
    binding: &Name,
    block: &'a k::Block,
) -> Option<Vec<Clause<'a>>> {
    let mut clauses = Vec::new();
    let mut block = block;
    loop {
        match block.as_slice() {
            [
                k::Stmt::Throw {
                    value: k::Expr::Variable(raised),
                },
            ] if raised == binding => {
                clauses.push(Clause::Raise);
                return Some(clauses);
            }
            [
                k::Stmt::If {
                    condition: k::Expr::Act(act),
                    then_block,
                    else_block,
                },
            ] => {
                let k::ActKind::Call(k::Callee::Import(id)) = &act.kind else {
                    return None;
                };
                let [k::Expr::Variable(raised), k::Expr::Tuple(names)] = act.args.as_slice() else {
                    return None;
                };
                if raised != binding {
                    return None;
                }
                let names = names
                    .iter()
                    .map(|name| match name {
                        k::Expr::Literal(k::Literal::Text(name))
                            if crate::printer::identifier(name) =>
                        {
                            Some(name.as_str())
                        }
                        _ => None,
                    })
                    .collect::<Option<Vec<_>>>()?;
                let classes =
                    if renderer.operators.is(id, "py.exc.is_exception") && names.is_empty() {
                        "Exception".to_owned()
                    } else if renderer.operators.is(id, "py.exc.matches") {
                        match names.as_slice() {
                            [] => return None,
                            [name] => (*name).to_owned(),
                            names => format!("({})", names.join(", ")),
                        }
                    } else {
                        return None;
                    };
                clauses.push(Clause::Named(classes, then_block));
                block = else_block;
            }
            _ => {
                clauses.push(Clause::Everything(block));
                return Some(clauses);
            }
        }
    }
}

/// A type as a plain annotation, when it has one.
fn annotation(ty: &Type) -> Option<String> {
    Some(match ty {
        Type::Bool => "bool".into(),
        Type::Int => "int".into(),
        Type::Float | Type::Number => "float".into(),
        Type::Text => "str".into(),
        Type::Bytes => "bytes".into(),
        Type::Null => "None".into(),
        Type::List(item) => format!("list[{}]", annotation(item)?),
        Type::Set(item) => format!("set[{}]", annotation(item)?),
        _ => return None,
    })
}

/// Whether an authored local function with `body` is an `async def`: it
/// performs, pauses, waits or calls a function that is one.
pub(super) fn authored_awaits(body: &k::Block, sync: &BTreeSet<Name>) -> bool {
    fn node(node: k::Node<'_>, sync: &BTreeSet<Name>) -> bool {
        match node {
            k::Node::Expr(k::Expr::Closure(_)) => false,
            k::Node::Expr(k::Expr::Act(act)) => {
                let waits = match &act.kind {
                    k::ActKind::Call(k::Callee::Import(_)) | k::ActKind::Spawn(_) => false,
                    k::ActKind::Call(k::Callee::Value(name)) => !sync.contains(name),
                    _ => true,
                };
                waits || node_children(node, sync)
            }
            k::Node::Action(_) => true,
            node => node_children(node, sync),
        }
    }
    fn node_children(of: k::Node<'_>, sync: &BTreeSet<Name>) -> bool {
        of.children().into_iter().any(|child| node(child, sync))
    }
    body.iter().any(|stmt| node(k::Node::Stmt(stmt), sync))
}
