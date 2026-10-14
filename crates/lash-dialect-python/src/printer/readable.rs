//! Native expression and statement rendering; exact generic suites live in the parent.
use super::{BindingScope, PrintError, Renderer, attribute_identifier, awaits, k};
impl Renderer<'_> {
    fn inline_rhs(&self, rhs: &k::Rhs) -> bool {
        match rhs {
            k::Rhs::Expr(e) => self.inline.contains(e),
            k::Rhs::Action(_) => true,
        }
    }
    fn write_inline_rhs(&mut self, rhs: &k::Rhs) {
        match rhs {
            k::Rhs::Expr(e) => self.inline_expr(e),
            k::Rhs::Action(a) => {
                self.out.begin_node(None);
                self.action(a);
                self.out.end_node();
            }
        }
    }
    fn inline_member(&self, member: &k::Member) -> bool {
        match member {
            k::Member::Field { target, .. } => self.inline.contains(target),
            k::Member::Index { target, index } => {
                self.inline.contains(target) && self.inline.contains(index)
            }
        }
    }
    fn write_member(&mut self, member: &k::Member) {
        match member {
            k::Member::Field { target, field } => {
                let grouped = !matches!(
                    target,
                    k::Expr::Variable(_) | k::Expr::Member(_) | k::Expr::Call { .. }
                );
                if grouped {
                    self.out.write("(");
                }
                self.inline_expr(target);
                if grouped {
                    self.out.write(")");
                }
                self.out.write(".");
                self.out.write(field);
            }
            k::Member::Index { target, index } => {
                self.inline_expr(target);
                self.out.write("[");
                self.inline_expr(index);
                self.out.write("]");
            }
        }
    }
    pub(super) fn inline_expr(&mut self, expr: &k::Expr) {
        self.out.begin_node(None);
        self.inline_body(expr);
        self.out.end_node();
    }
    pub(super) fn inline_body(&mut self, expr: &k::Expr) {
        match expr {
            k::Expr::Literal(l) => self.literal(l),
            k::Expr::Variable(n) => self.name(n),
            k::Expr::Clock => self.out.write("k.clock()"),
            k::Expr::Random => self.out.write("k.random()"),
            k::Expr::Call { function, args } => {
                if let Some(token) = self.operators.token(function, args, &self.scope.types) {
                    match token {
                        "not" | "neg" => {
                            self.out.write(if token == "not" { "not (" } else { "-(" });
                            self.inline_expr(&args[0]);
                            self.out.write(")");
                        }
                        _ => {
                            self.out.write("(");
                            self.inline_expr(&args[0]);
                            self.out.write(" ");
                            self.out.write(token);
                            self.out.write(" ");
                            self.inline_expr(&args[1]);
                            self.out.write(")");
                        }
                    }
                } else {
                    self.helper(function);
                    self.out.write("(");
                    for (i, arg) in args.iter().enumerate() {
                        if i > 0 {
                            self.out.write(", ");
                        }
                        self.inline_expr(arg);
                    }
                    self.out.write(")");
                }
            }
            k::Expr::List(items) | k::Expr::Tuple(items) | k::Expr::Set(items) => {
                let (open, close) = match expr {
                    k::Expr::List(_) => ("[", "]"),
                    k::Expr::Tuple(_) => ("(", ")"),
                    _ if items.is_empty() => ("set(", ")"),
                    _ => ("{", "}"),
                };
                self.out.write(open);
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        self.out.write(", ");
                    }
                    self.inline_expr(item);
                }
                if matches!(expr, k::Expr::Tuple(_)) && items.len() == 1 {
                    self.out.write(",");
                }
                self.out.write(close);
            }
            k::Expr::Map(items) => {
                self.out.write("{");
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        self.out.write(", ");
                    }
                    self.inline_expr(&item.key);
                    self.out.write(": ");
                    self.inline_expr(&item.value);
                }
                self.out.write("}");
            }
            k::Expr::Record(items) => {
                self.out.write("record({");
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        self.out.write(", ");
                    }
                    self.out.json(&item.field);
                    self.out.write(": ");
                    self.inline_expr(&item.value);
                }
                self.out.write("})");
            }
            k::Expr::Member(member) if matches!(member.as_ref(), k::Member::Field { field, .. } if !attribute_identifier(field)) =>
            {
                let k::Member::Field { target, field } = member.as_ref() else {
                    unreachable!()
                };
                self.out.write("getattr(");
                self.inline_expr(target);
                self.out.write(", ");
                self.out.json(field);
                self.out.write(")");
            }
            k::Expr::Member(member) => {
                let target = match member.as_ref() {
                    k::Member::Field { target, .. } | k::Member::Index { target, .. } => target,
                };
                let grouped = !matches!(
                    target,
                    k::Expr::Variable(_) | k::Expr::Member(_) | k::Expr::Call { .. }
                );
                if grouped {
                    self.out.write("(");
                }
                self.inline_expr(target);
                if grouped {
                    self.out.write(")");
                }
                match member.as_ref() {
                    k::Member::Field { field, .. } => {
                        self.out.write(".");
                        self.out.write(field);
                    }
                    k::Member::Index { index, .. } => {
                        self.out.write("[");
                        self.inline_expr(index);
                        self.out.write("]");
                    }
                }
            }
            _ => self
                .out
                .refuse(PrintError::Invalid("ineligible inline expression".into())),
        }
    }
    // The caller owns the block's indentation and any clause label. Empty
    // suites spell pass, so every real block has a nonempty final-text range.
    fn readable_block(&mut self, block: &k::Block) {
        self.out.begin_node(None);
        self.indent += 1;
        if block.is_empty() {
            self.empty();
        }
        for stmt in block {
            self.node(k::Node::Stmt(stmt), None);
        }
        self.indent -= 1;
        self.out.end_node();
    }
    fn readable_if(
        &mut self,
        condition: &k::Expr,
        then_block: &k::Block,
        else_block: &k::Block,
        keyword: &str,
    ) {
        self.out.write(keyword);
        self.out.write(" ");
        self.inline_expr(condition);
        self.out.write(":\n");
        self.readable_block(then_block);
        // An elif is exactly one nested If in the else Block. Keep both map
        // frames; this changes layout, never the stored statement boundaries.
        if let [
            k::Stmt::If {
                condition,
                then_block,
                else_block,
            },
        ] = else_block.as_slice()
            && self.inline.contains(condition)
        {
            self.out.begin_node(None); // else Block
            self.out.begin_node(None); // nested If
            self.padding();
            self.readable_if(condition, then_block, else_block, "elif");
            self.out.end_node();
            self.out.end_node();
        } else {
            self.padding();
            self.out.write("else:\n");
            self.readable_block(else_block);
        }
    }
    pub(super) fn readable_statement(&mut self, stmt: &k::Stmt) -> bool {
        match stmt {
            k::Stmt::Let {
                name,
                value: k::Rhs::Expr(k::Expr::Closure(c)),
            }
            | k::Stmt::Assign {
                place: k::Place::Variable(name),
                value: k::Rhs::Expr(k::Expr::Closure(c)),
            } => {
                self.out.write(if awaits(&c.body) {
                    "async def "
                } else {
                    "def "
                });
                self.name(name);
                self.out.begin_node(None); // Closure expression
                self.parameters(&c.params);
                let (_, annotation) = self
                    .scope
                    .bind(name, Some(matches!(stmt, k::Stmt::Let { .. })));
                if let Some(annotation) = annotation {
                    self.out.write(" -> ");
                    self.out.write(annotation);
                }
                let parent = std::mem::replace(&mut self.scope, BindingScope::new(&c.params));
                let outer_main = self.main;
                self.main = false;
                self.out.write(":\n");
                self.readable_block(&c.body);
                self.main = outer_main;
                self.scope = parent;
                self.out.end_node();
            }
            k::Stmt::Let { name, value } if self.inline_rhs(value) => {
                self.name(name);
                self.binding_annotation(name, true);
                self.out.write(" = ");
                self.write_inline_rhs(value);
                self.out.write("\n");
            }
            k::Stmt::Assign {
                place: k::Place::Member(k::Member::Field { target, field }),
                value,
            } if !attribute_identifier(field)
                && self.inline.contains(target)
                && self.inline_rhs(value) =>
            {
                self.out.write("setattr(");
                self.inline_expr(target);
                self.out.write(", ");
                self.out.json(field);
                self.out.write(", ");
                self.write_inline_rhs(value);
                self.out.write(")\n");
            }
            k::Stmt::Assign { place, value }
                if self.inline_rhs(value)
                    && match place {
                        k::Place::Variable(_) => true,
                        k::Place::Member(m) => self.inline_member(m),
                    } =>
            {
                match place {
                    k::Place::Variable(n) => {
                        self.name(n);
                        self.binding_annotation(n, false);
                    }
                    k::Place::Member(m) => self.write_member(m),
                }
                self.out.write(" = ");
                self.write_inline_rhs(value);
                self.out.write("\n");
            }
            k::Stmt::Remove {
                member: k::Member::Field { target, field },
            } if !attribute_identifier(field) && self.inline.contains(target) => {
                self.out.write("delattr(");
                self.inline_expr(target);
                self.out.write(", ");
                self.out.json(field);
                self.out.write(")\n");
            }
            k::Stmt::Remove { member } if self.inline_member(member) => {
                self.out.write("del ");
                self.write_member(member);
                self.out.write("\n");
            }
            k::Stmt::Do { action } => {
                self.out.begin_node(None);
                self.action(action);
                self.out.end_node();
                self.out.write("\n");
            }
            k::Stmt::Break => self.out.write("break\n"),
            k::Stmt::Continue => self.out.write("continue\n"),
            k::Stmt::If {
                condition,
                then_block,
                else_block,
            } if self.inline.contains(condition) => {
                self.readable_if(condition, then_block, else_block, "if");
            }
            k::Stmt::For {
                binding,
                iterable,
                body,
            } if self.inline.contains(iterable) => {
                self.out.write("for ");
                self.name(binding);
                self.scope.introduce(binding);
                self.out.write(" in ");
                self.inline_expr(iterable);
                self.out.write(":\n");
                self.readable_block(body);
            }
            k::Stmt::While { condition, body } if self.inline.contains(condition) => {
                self.out.write("while ");
                self.inline_expr(condition);
                self.out.write(":\n");
                self.readable_block(body);
            }
            k::Stmt::Try(scope) if scope.catch.is_some() || scope.finally.is_some() => {
                self.out.write("try:\n");
                self.readable_block(&scope.body);
                if let Some(catch) = &scope.catch {
                    self.padding();
                    self.out.write("except BaseException as ");
                    self.name(&catch.binding);
                    self.scope.introduce(&catch.binding);
                    self.out.write(":\n");
                    self.readable_block(&catch.body);
                }
                if let Some(finally) = &scope.finally {
                    self.padding();
                    self.out.write("finally:\n");
                    self.readable_block(finally);
                }
            }
            k::Stmt::Return { value }
            | k::Stmt::Throw { value }
            | k::Stmt::Print { value }
            | k::Stmt::Finish { value }
            | k::Stmt::Fail { value }
                if self.inline.contains(value) =>
            {
                let call = match stmt {
                    k::Stmt::Return { .. } => self.main,
                    k::Stmt::Finish { .. } => !self.main,
                    k::Stmt::Throw { .. } => false,
                    _ => true,
                };
                self.out.write(match stmt {
                    k::Stmt::Return { .. } if self.main => "return returned(",
                    k::Stmt::Return { .. } => "return ",
                    k::Stmt::Throw { .. } => "raise ",
                    k::Stmt::Print { .. } => "print(",
                    k::Stmt::Finish { .. } if self.main => "return ",
                    k::Stmt::Finish { .. } => "return finish(",
                    _ => "raise failure(",
                });
                self.inline_expr(value);
                if call {
                    self.out.write(")");
                }
                self.out.write("\n");
            }
            _ => return false,
        }
        true
    }
    fn binding_annotation(&mut self, name: &k::Name, declaration: bool) {
        let (_, annotation) = self.scope.bind(name, Some(declaration));
        if let Some(annotation) = annotation {
            self.out.write(": ");
            self.out.write(annotation);
        }
    }
}
