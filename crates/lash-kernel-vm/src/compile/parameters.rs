//! Which operation parameters need shared cells before a native is offered a
//! call. This lexical walk resolves only captures; it emits no body code.
use lash_kernel_doc::{self as doc};

use lash_vocabulary::names::Name;

pub(super) fn shared(
    definition: &doc::FunctionDefinition,
    catalog: &dyn doc::FunctionCatalog,
) -> Vec<usize> {
    definition
        .signature
        .params
        .iter()
        .enumerate()
        .filter_map(|(index, param)| {
            let mut capture = Capture {
                name: &param.name,
                catalog,
                shadowed: false,
                depth: 0,
                captured: false,
            };
            if let Some(body) = definition.body() {
                capture.node(doc::Node::Block(&body.block));
            }
            capture.captured.then_some(index)
        })
        .collect()
}

struct Capture<'a> {
    name: &'a Name,
    catalog: &'a dyn doc::FunctionCatalog,
    shadowed: bool,
    depth: usize,
    captured: bool,
}

impl Capture<'_> {
    fn read(&mut self, name: &Name) {
        self.captured |= self.depth != 0 && !self.shadowed && name == self.name;
    }

    fn atom(&mut self, atom: &doc::Atom) {
        if let doc::Atom::Variable(name) = atom {
            self.read(name);
        }
    }

    fn callee(&mut self, callee: &doc::Callee) {
        if let doc::Callee::Value(name) = callee {
            self.read(name);
        }
    }

    fn bound(&mut self, name: &Name, node: doc::Node<'_>) {
        let shadowed = self.shadowed;
        self.shadowed |= name == self.name;
        self.node(node);
        self.shadowed = shadowed;
    }

    fn node(&mut self, node: doc::Node<'_>) {
        use doc::{ActKind, Action, Expr, Node, Stmt};
        match node {
            Node::Block(block) => {
                let shadowed = self.shadowed;
                for stmt in block {
                    self.node(Node::Stmt(stmt));
                    // A let's initializer still sees the preceding binding.
                    if let Stmt::Let { name, .. } = stmt {
                        self.shadowed |= name == self.name;
                    }
                }
                self.shadowed = shadowed;
                return;
            }
            Node::Stmt(Stmt::For {
                binding,
                iterable,
                body,
                else_block,
                ..
            }) => {
                self.node(Node::Expr(iterable));
                self.bound(binding, Node::Block(body));
                self.node(Node::Block(else_block));
                return;
            }
            Node::Stmt(Stmt::Try(scope)) => {
                self.node(Node::Block(&scope.body));
                if let Some(catch) = &scope.catch {
                    self.bound(&catch.binding, Node::Block(&catch.body));
                }
                if let Some(finally) = &scope.finally {
                    self.node(Node::Block(finally));
                }
                return;
            }
            Node::Expr(Expr::Closure(closure)) => {
                let shadowed = self.shadowed;
                if let Some(definition) = self.catalog.definition(&closure.function) {
                    self.shadowed |= definition
                        .signature
                        .params
                        .iter()
                        .any(|p| &p.name == self.name);
                    self.depth += 1;
                    if let Some(body) = definition.body() {
                        self.node(Node::Block(&body.block));
                    }
                } else {
                    return;
                }

                self.depth -= 1;
                self.shadowed = shadowed;
                return;
            }
            Node::Expr(Expr::Let { name, value, body }) => {
                self.node(Node::Expr(value));
                self.bound(name, Node::Expr(body));
                return;
            }
            Node::Stmt(Stmt::Assign {
                place: doc::Place::Variable(name),
                ..
            })
            | Node::Expr(Expr::Variable(name)) => self.read(name),
            Node::Action(action) => match action {
                Action::Call { callee, args } | Action::Spawn { callee, args } => {
                    self.callee(callee);
                    for arg in args {
                        self.atom(arg);
                    }
                }
                Action::Perform { args, .. } => {
                    for arg in args {
                        self.atom(arg);
                    }
                }
                Action::Join { task: value }
                | Action::Cancel { task: value }
                | Action::JoinMany { tasks: value, .. }
                | Action::Await { value, .. } => self.atom(value),
                Action::Yield => {}
            },
            Node::Expr(Expr::Act(act)) => match &act.kind {
                ActKind::Call(callee) | ActKind::Spawn(callee) => self.callee(callee),
                _ => {}
            },
            _ => {}
        }
        for child in node.children() {
            self.node(child);
        }
    }
}
