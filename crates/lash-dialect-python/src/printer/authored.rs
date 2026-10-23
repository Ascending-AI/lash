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
            // An awaited terminal operation keeps its perform inside Finish;
            // the discarded reply and null result have no authored spelling.
            k::Stmt::Finish {
                value: k::Expr::Let { name, value, body },
            } if self.main
                && temporary_name(name)
                && matches!(&**body, k::Expr::Literal(k::Literal::Null))
                && self.inline.contains(value)
                && matches!(&**value, k::Expr::Act(action)
                    if matches!(action.kind, k::ActKind::Perform { .. })) =>
            {
                let k::Expr::Act(action) = &**value else {
                    unreachable!()
                };
                self.out.begin_node(None); // expression binding
                self.out.begin_node(None); // performed operation
                self.nested_action(action, false);
                self.out.end_node();
                self.implicit(); // null result
                self.out.end_node();
                self.out.write("\n");
                true
            }
            // Indexed assignment lowers to a discarded call of the selected
            // mutation helper. Python evaluates the value before the place;
            // lowering retains any required operand pins in the document.
            k::Stmt::Do {
                value: k::Expr::Act(action),
            } if matches!(&action.kind, k::ActKind::Call(callee)
                if super::task::target(callee)
                    .is_some_and(|target| self.operators.is(&target, "py.setitem"))) =>
            {
                let [target, index, value] = action.args.as_slice() else {
                    return false;
                };
                self.out.begin_node(None); // discarded action
                self.inline_expr(target);
                self.out.write("[");
                self.inline_expr(index);
                self.out.write("] = ");
                self.inline_expr(value);
                self.out.end_node();
                self.out.write("\n");
                true
            }
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
                        callee: k::Callee::Local(id),
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
                k::ActKind::Call(k::Callee::Local(id)) if self.operators.is(id, "py.iadd") => {
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
                let k::ActKind::Call(k::Callee::Local(id)) = &act.kind else {
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
pub(super) fn annotation(ty: &Type) -> Option<String> {
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
                    k::ActKind::Call(k::Callee::Local(_) | k::Callee::Import(_))
                    | k::ActKind::Spawn(_) => false,
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

/// A collection and its generator clauses, recovered from the exact append
/// loop emitted by lowering. This is a print proposal, never executable IR.
#[derive(Clone)]
pub(super) struct Comprehension {
    element: k::Expr,
    binding: Name,
    source_binding: Name,
    iterable: k::Expr,
    view: Option<&'static str>,
}

impl Renderer<'_> {
    /// Invert statement-only lowering before rendering an authored suite.
    /// Each proposal is admitted only by the complete readback oracle.
    pub(super) fn authored_statements(&mut self, block: &[k::Stmt]) {
        self.authored_statement_range(block, 0..block.len());
    }

    pub(super) fn authored_statement_range(
        &mut self,
        block: &[k::Stmt],
        range: std::ops::Range<usize>,
    ) {
        let substitutions = self.substitutions.clone();
        let comprehensions = self.comprehensions.clone();
        let mut pending_caption = None;
        let mut index = range.start;
        while index < range.end {
            let statement = &block[index];
            let packed = self.packed_definition(&block[index..range.end]);
            if packed > 0 {
                index += packed;
                continue;
            }
            if self.try_else(&block[index..range.end]) {
                index += 3;
                continue;
            }
            if let k::Stmt::Let {
                name,
                value: k::Rhs::Expr(k::Expr::Literal(k::Literal::Absent)),
            } = statement
                && assigned(&block[index + 1..], name)
            {
                // A Python local exists for its whole scope. Its first
                // authored assignment supplies the declaration's spelling.
                self.implicit();
                index += 1;
                continue;
            }
            if let Some((name, comprehension)) = self.lift_comprehension(&block[index..range.end]) {
                pending_caption = self.hoist_caption(pending_caption);
                self.implicit(); // collection allocation
                self.implicit(); // generator loop
                self.comprehensions.insert(name, comprehension);
                index += 2;
                continue;
            }
            if let k::Stmt::Let { name, value } = statement
                && temporary_name(name)
                && reads(&block[index + 1..], name) == 1
                && !assigned(&block[index + 1..], name)
                && (!matches!(value, k::Rhs::Expr(k::Expr::Closure(_)))
                    || self.scope_provider(value))
            {
                let expression = match value {
                    k::Rhs::Expr(value) => value.clone(),
                    k::Rhs::Action(action) => action.clone().into(),
                };
                pending_caption = self.hoist_caption(pending_caption);
                self.implicit();
                self.substitutions.insert(name.clone(), expression);
                index += 1;
                continue;
            }
            if let Some(caption) = pending_caption.take() {
                self.spell(&caption);
            }
            if let Some((name, k::Rhs::Expr(value))) = binding_value(statement)
                && matches!(value, k::Expr::Let { name, value, body }
                    if matches!(value.as_ref(), k::Expr::Closure(_))
                        && matches!(body.as_ref(), k::Expr::Variable(read) if read == name))
            {
                // A lambda is synchronous. Its call is already the apply
                // expression; adding `await` would introduce an await policy.
                self.sync.insert(name.clone());
            }
            self.node(k::Node::Stmt(statement), None);
            index += 1;
        }
        if pending_caption.is_some() {
            self.out.refuse(super::PrintError::Invalid(
                "a hoisted caption has no authored statement".into(),
            ));
        }
        self.substitutions = substitutions;
        self.comprehensions = comprehensions;
    }

    fn hoist_caption(&mut self, previous: Option<k::Caption>) -> Option<k::Caption> {
        let caption = self.next_caption();
        if previous.is_some() && caption.is_some() {
            self.out.refuse(super::PrintError::Invalid(
                "two lowered statements claim one authored caption".into(),
            ));
        }
        previous.or(caption)
    }

    /// The success flag is the lowering of `try … except … else`, not an
    /// authored boolean variable. Its complete def/use chain identifies it.
    fn try_else(&mut self, statements: &[k::Stmt]) -> bool {
        let [
            k::Stmt::Let {
                name: flag,
                value: k::Rhs::Expr(k::Expr::Literal(k::Literal::Bool(false))),
            },
            k::Stmt::Try(scope),
            k::Stmt::If {
                condition: k::Expr::Variable(test),
                then_block,
                else_block,
            },
            rest @ ..,
        ] = statements
        else {
            return false;
        };
        let Some(k::Stmt::Assign {
            place: k::Place::Variable(set),
            value: k::Rhs::Expr(k::Expr::Literal(k::Literal::Bool(true))),
            order: k::AssignOrder::ValueFirst,
        }) = scope.body.last()
        else {
            return false;
        };
        if flag != test
            || flag != set
            || !else_block.is_empty()
            || scope.catch.is_none()
            || scope.finally.is_some()
            || reads(rest, flag) != 0
        {
            return false;
        }
        let Some(catch) = &scope.catch else {
            return false;
        };
        let Some(clauses) = clauses(self, &catch.binding, &catch.body) else {
            return false;
        };
        self.implicit(); // initial success flag
        self.caption_next();
        self.out.begin_node(None); // try statement
        self.padding();
        self.out.write("try:\n");
        self.out.begin_node(None); // try body
        self.indent += 1;
        if scope.body.len() == 1 {
            self.empty();
        }
        self.authored_statements(&scope.body[..scope.body.len() - 1]);
        self.implicit(); // success assignment
        self.indent -= 1;
        self.out.end_node();
        self.caption_next();
        self.out.begin_node(None); // catch body
        self.except(&clauses);
        self.out.end_node();
        self.out.end_node();
        self.out.begin_node(None); // success test
        self.implicit();
        self.padding();
        self.out.write("else:\n");
        self.readable_block(then_block);
        self.out.end_node();
        true
    }

    fn lift_comprehension(&self, statements: &[k::Stmt]) -> Option<(Name, Comprehension)> {
        let [
            k::Stmt::Let {
                name,
                value: k::Rhs::Expr(k::Expr::List(empty)),
            },
            k::Stmt::For {
                binding,
                iterable,
                via,
                body,
                else_block,
                metered: 0,
            },
            rest @ ..,
        ] = statements
        else {
            return None;
        };
        if !temporary_name(name)
            || !empty.is_empty()
            || !else_block.is_empty()
            || reads(rest, name) != 1
            || assigned(rest, name)
        {
            return None;
        }
        let [
            k::Stmt::Assign {
                place:
                    k::Place::Member(k::Member::Index {
                        target: k::Expr::Variable(target),
                        index: k::Expr::Call { function, args },
                    }),
                value,
                order: k::AssignOrder::ValueFirst,
            },
        ] = body.as_slice()
        else {
            return None;
        };
        if target != name
            || self.operators.unique("list.len") != Some(*function)
            || args.as_slice() != [k::Expr::Variable(name.clone())]
        {
            return None;
        }
        // Lowering gives comprehension-local bindings a fresh suffix and
        // then folds the iteration assignment into the loop's own binding.
        // Recover that alpha-renaming only for this complete generator shape;
        // the readback must reproduce the exact original binding and body ID.
        let (source, suffix) = binding.as_str().rsplit_once('_')?;
        if suffix.is_empty() || !suffix.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        let element = match value {
            k::Rhs::Expr(value) => value.clone(),
            k::Rhs::Action(action) => action.clone().into(),
        };
        Some((
            name.clone(),
            Comprehension {
                element,
                binding: binding.clone(),
                source_binding: Name::new(source),
                iterable: iterable.clone(),
                view: self.operators.native_loop(via.as_ref())?,
            },
        ))
    }

    pub(super) fn comprehension(&mut self, comprehension: &Comprehension) {
        let previous = self.substitutions.insert(
            comprehension.binding.clone(),
            k::Expr::Variable(comprehension.source_binding.clone()),
        );
        self.out.write("[");
        self.inline_expr(&comprehension.element);
        self.out.write(" for ");
        self.name(&comprehension.source_binding);
        self.out.write(" in ");
        // The first iterable is outside the comprehension's local scope.
        if let Some(previous) = previous {
            self.substitutions
                .insert(comprehension.binding.clone(), previous);
        } else {
            self.substitutions.remove(&comprehension.binding);
        }
        self.inline_expr(&comprehension.iterable);
        if let Some(view) = comprehension.view {
            self.out.write(".");
            self.out.write(view);
            self.out.write("()");
        }
        self.out.write("]");
    }

    pub(super) fn name_check(&mut self, target: &k::FunctionTarget, args: &[k::Expr]) -> bool {
        if !self.operators.is(target, "py.name_check") {
            return false;
        }
        let [
            k::Expr::Variable(name),
            k::Expr::Literal(k::Literal::Text(spelling)),
            k::Expr::Literal(k::Literal::Bool(_)),
        ] = args
        else {
            return false;
        };
        if name.as_str() != spelling {
            return false;
        }
        self.name(name);
        true
    }

    pub(super) fn operator(&mut self, token: &str, args: &[k::Expr]) {
        self.out.write("(");
        if matches!(token, "neg" | "not") {
            self.out.write(if token == "neg" { "-" } else { "not " });
            self.out.write("(");
            self.inline_expr(&args[0]);
            self.out.write(")");
        } else {
            self.out.write("(");
            self.inline_expr(&args[0]);
            self.out.write(") ");
            self.out.write(token);
            self.out.write(" (");
            self.inline_expr(&args[1]);
            self.out.write(")");
        }
        self.out.write(")");
    }

    pub(super) fn lambda(&mut self, closure: &k::Closure) {
        let Some(definition) = self.document.functions.get(&closure.function) else {
            self.out
                .refuse(super::PrintError::Invalid("missing lambda body".into()));
            return;
        };
        let Some(body) = definition.body() else {
            self.out
                .refuse(super::PrintError::Invalid("native lambda body".into()));
            return;
        };
        let [k::Stmt::Return { value }] = body.block.as_slice() else {
            self.out.refuse(super::PrintError::Invalid(
                "a lambda needs an expression body".into(),
            ));
            return;
        };
        self.out.write("(lambda ");
        for (index, param) in definition.signature.params.iter().enumerate() {
            if index > 0 {
                self.out.write(", ");
            }
            self.name(&param.name);
        }
        self.out.write(": ");
        self.inline_expr(value);
        self.out.write(")");
    }
}

fn temporary_name(name: &Name) -> bool {
    name.as_str().strip_prefix('t').is_some_and(|suffix| {
        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
    })
}

fn binding_value(statement: &k::Stmt) -> Option<(&Name, &k::Rhs)> {
    match statement {
        k::Stmt::Let { name, value }
        | k::Stmt::Assign {
            place: k::Place::Variable(name),
            value,
            ..
        } => Some((name, value)),
        _ => None,
    }
}

fn assigned(block: &[k::Stmt], name: &Name) -> bool {
    fn visits(item: k::Node<'_>, name: &Name) -> bool {
        matches!(item, k::Node::Stmt(k::Stmt::Assign { place: k::Place::Variable(written), .. }) if written == name)
            || item.children().into_iter().any(|child| visits(child, name))
    }
    block
        .iter()
        .any(|statement| visits(k::Node::Stmt(statement), name))
}

fn reads(block: &[k::Stmt], name: &Name) -> usize {
    fn count(node: k::Node<'_>, name: &Name) -> usize {
        usize::from(matches!(node, k::Node::Expr(k::Expr::Variable(read)) if read == name))
            + node
                .children()
                .into_iter()
                .map(|child| count(child, name))
                .sum::<usize>()
    }
    block
        .iter()
        .map(|statement| count(k::Node::Stmt(statement), name))
        .sum()
}
