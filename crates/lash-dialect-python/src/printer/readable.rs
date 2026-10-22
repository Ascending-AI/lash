//! Native expression and statement rendering; exact generic suites live in the parent.
use super::authored::authored_awaits;
use super::{BindingScope, PrintError, Renderer, attribute_identifier, k};
use lash_vocabulary::Name;

/// The binding strength of atoms, calls and members.
const PRIMARY: u8 = 10;
impl Renderer<'_> {
    pub(super) fn inline_rhs(&self, rhs: &k::Rhs) -> bool {
        match rhs {
            k::Rhs::Expr(e) => self.inline.contains(e),
            k::Rhs::Action(_) => true,
        }
    }
    pub(super) fn write_inline_rhs(&mut self, rhs: &k::Rhs) {
        match rhs {
            k::Rhs::Expr(e) => self.statement_value(e),
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
                self.member_target(target, true);
                self.out.write(".");
                self.out.write(field);
            }
            k::Member::Index { target, index } => {
                self.member_target(target, false);
                self.out.write("[");
                self.inline_expr(index);
                self.out.write("]");
            }
        }
    }
    /// A member's target is a primary: an operator is grouped, and so is any
    /// target but a variable, member or call when `simple` asks for it.
    fn member_target(&mut self, target: &k::Expr, simple: bool) {
        let grouped = self.precedence(target) < PRIMARY
            || simple
                && !matches!(
                    target,
                    k::Expr::Variable(_) | k::Expr::Member(_) | k::Expr::Call { .. }
                );
        self.grouped(target, grouped);
    }
    fn grouped(&mut self, expr: &k::Expr, grouped: bool) {
        if grouped {
            self.out.write("(");
        }
        self.inline_expr(expr);
        if grouped {
            self.out.write(")");
        }
    }
    /// Python's binding strength of `expr` as printed: `not` 3, comparisons
    /// 4, `+`/`-` 5, `*`/`/`/`//`/`%` 6, unary minus and negative numbers 7,
    /// `**` 8, and every primary [`PRIMARY`].
    fn precedence(&self, expr: &k::Expr) -> u8 {
        match expr {
            k::Expr::Call { function, args } => {
                match self.operators.token(function, args, &self.scope.types) {
                    Some("not") => 3,
                    Some("==" | "is" | "<" | "<=" | ">" | ">=") => 4,
                    Some("+" | "-") => 5,
                    Some("*" | "/" | "//" | "%") => 6,
                    Some("neg") => 7,
                    Some("**") => 8,
                    _ => PRIMARY,
                }
            }
            k::Expr::Literal(k::Literal::Int(n)) if n.is_negative() => 7,
            k::Expr::Literal(k::Literal::Float(n))
                if n.get().is_finite() && n.get().is_sign_negative() =>
            {
                7
            }
            _ => PRIMARY,
        }
    }
    /// Print `expr` as an operand that binds at least `minimum`, grouping it
    /// only when Python would otherwise bind it differently.
    fn operand(&mut self, expr: &k::Expr, minimum: u8) {
        let grouped = self.precedence(expr) < minimum;
        self.grouped(expr, grouped);
    }
    /// A statement's value: authored source writes an action there without
    /// grouping it.
    fn statement_value(&mut self, expr: &k::Expr) {
        match expr {
            k::Expr::Act(act) if self.authored => {
                self.out.begin_node(None);
                self.nested_action(act, false);
                self.out.end_node();
            }
            expr => self.inline_expr(expr),
        }
    }
    /// A node the authored source spells by what holds it.
    pub(super) fn implicit(&mut self) {
        self.out.begin_node(None);
        self.out.end_node();
    }
    pub(super) fn inline_expr(&mut self, expr: &k::Expr) {
        self.out.begin_node(None);
        self.inline_body(expr);
        self.out.end_node();
    }
    pub(super) fn inline_body(&mut self, expr: &k::Expr) {
        if let Some(task) = self.inline.tasks.get(&(expr as *const _)).cloned() {
            let k::ActKind::Perform { slot, .. } = task.kind else {
                unreachable!("task operation")
            };
            let Some(import) = self.document.imports.get(&slot) else {
                self.out
                    .refuse(PrintError::Invalid("missing task operation".into()));
                return;
            };
            self.out.write("asyncio.create_task(");
            self.operation_path(&import.name.to_string());
            self.out.write("(");
            self.authored_args(&task.args, true);
            self.out.write("))");
            return;
        }
        match expr {
            k::Expr::Literal(l) => self.literal(l),
            k::Expr::Act(act) => self.nested_action(act, true),
            k::Expr::Variable(n) => self.name(n),
            k::Expr::Call { function, args } => {
                if let Some(token) = self.operators.token(function, args, &self.scope.types) {
                    match token {
                        "not" => {
                            self.out.write("not ");
                            self.operand(&args[0], 3);
                        }
                        "neg" => {
                            self.out.write("-");
                            // `-(7)` negates; `-7` is a negative literal.
                            let number = matches!(
                                &args[0],
                                k::Expr::Literal(k::Literal::Int(_) | k::Literal::Float(_))
                            );
                            self.grouped(&args[0], number || self.precedence(&args[0]) < 7);
                        }
                        _ => {
                            let (left, right) = match token {
                                "==" | "is" | "<" | "<=" | ">" | ">=" => (5, 5),
                                "**" => (PRIMARY, 7),
                                _ => {
                                    let own = self.precedence(expr);
                                    (own, own + 1)
                                }
                            };
                            self.operand(&args[0], left);
                            self.out.write(" ");
                            self.out.write(token);
                            self.out.write(" ");
                            self.operand(&args[1], right);
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
                self.member_target(target, true);
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
    pub(super) fn nested_action(&mut self, act: &k::ActExpr, grouped: bool) {
        if grouped {
            self.out.write("(");
        }
        match &act.kind {
            k::ActKind::Call(callee) => {
                let target = match callee {
                    k::Callee::Local(id) => Some(k::FunctionTarget::Local(*id)),
                    k::Callee::Import(slot) => Some(k::FunctionTarget::Import(*slot)),
                    _ => None,
                };
                if let Some(target) = target {
                    if self.operators.is(&target, "py.reply") {
                        if let [k::Expr::Act(performed)] = act.args.as_slice() {
                            self.nested_action(performed, false);
                        } else {
                            self.out.refuse(PrintError::Invalid(
                                "reply has no authored operation".into(),
                            ));
                        }
                    } else if self.operators.is(&target, "py.sleep") {
                        self.out.write("await asyncio.sleep(");
                        self.authored_args(&act.args, true);
                        self.out.write(")");
                    } else if self.operators.is(&target, "py.time") {
                        self.out.write("time.time()");
                    } else if !self.authored_call(&target, &act.args, true) {
                        self.helper(&target);
                        self.out.write("(");
                        self.authored_args(&act.args, true);
                        self.out.write(")");
                    }
                } else {
                    if !matches!(callee,k::Callee::Value(name) if self.sync.contains(name)) {
                        self.out.write("await ");
                    }
                    self.callee(callee);
                    self.out.write("(");
                    self.authored_args(&act.args, true);
                    self.out.write(")");
                }
            }
            k::ActKind::Perform {
                slot,
                then_yield: false,
                ..
            } => {
                let Some(import) = self.document.imports.get(slot) else {
                    self.out
                        .refuse(PrintError::Invalid("missing operation import".into()));
                    return;
                };
                self.out.write("await ");
                self.operation_path(&import.name.to_string());
                self.out.write("(");
                self.authored_args(&act.args, true);
                self.out.write(")");
            }
            k::ActKind::Await(policy) if self.operators.is(policy, "py.await") => {
                self.out.write("await ");
                self.authored_args(&act.args, true);
            }
            _ => self.out.refuse(PrintError::Invalid(
                "action has no authored Python spelling".into(),
            )),
        }
        if grouped {
            self.out.write(")");
        }
    }
    // The caller owns the block's indentation and any clause label. Empty
    // suites spell pass, so every real block has a nonempty final-text range.
    pub(super) fn readable_block(&mut self, block: &k::Block) {
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
        // A captioned else Block has its own clause, where its caption goes.
        let otherwise = self.next_caption();
        if let [
            k::Stmt::If {
                condition,
                then_block,
                else_block,
            },
        ] = else_block.as_slice()
            && self.inline.contains(condition)
            && otherwise.is_none()
        {
            self.out.begin_node(None); // else Block
            self.caption_next();
            self.out.begin_node(None); // nested If
            self.padding();
            self.readable_if(condition, then_block, else_block, "elif");
            self.out.end_node();
            self.out.end_node();
        } else if self.authored && else_block.is_empty() && otherwise.is_none() {
            self.implicit();
        } else {
            if let Some(caption) = otherwise {
                self.spell(&caption);
            }
            self.padding();
            self.out.write("else:\n");
            self.readable_block(else_block);
        }
    }
    /// A loop's `else` suite, which an empty `else` has none of.
    fn loop_else(&mut self, else_block: &k::Block) {
        if !else_block.is_empty() {
            self.caption_next();
            self.padding();
            self.out.write("else:\n");
            self.readable_block(else_block);
        }
    }
    pub(super) fn readable_statement(&mut self, stmt: &k::Stmt) -> bool {
        if self.authored && self.authored_statement(stmt) {
            return true;
        }
        match stmt {
            k::Stmt::Let {
                name,
                value: k::Rhs::Expr(k::Expr::Closure(c)),
            }
            | k::Stmt::Assign {
                place: k::Place::Variable(name),
                value: k::Rhs::Expr(k::Expr::Closure(c)),
                ..
            } => {
                let Some(definition) = self.document.functions.get(&c.function) else {
                    return false;
                };
                let Some(body) = definition.body() else {
                    return false;
                };
                let params = definition
                    .signature
                    .params
                    .iter()
                    .map(|p| p.name.clone())
                    .collect::<Vec<_>>();
                let asynchronous = authored_awaits(&body.block, &self.sync);
                if self.authored && !asynchronous {
                    self.sync.insert(name.clone());
                }
                self.out
                    .write(if asynchronous { "async def " } else { "def " });
                self.name(name);
                self.out.begin_node(None); // Closure expression
                match &c.presentation {
                    Some(start) if self.authored => self.annotated(&params, start),
                    _ => self.parameters(&params),
                }
                let (_, annotation) = self
                    .scope
                    .bind(name, Some(matches!(stmt, k::Stmt::Let { .. })));
                if let Some(annotation) = annotation {
                    self.out.write(" -> ");
                    self.out.write(annotation);
                }
                let parent = std::mem::replace(&mut self.scope, BindingScope::new(&params));
                let outer_main = self.main;
                self.main = false;
                self.out.write(":\n");
                self.readable_block(&body.block);
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
                order: k::AssignOrder::ValueFirst,
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
            k::Stmt::Assign {
                place,
                value,
                order: k::AssignOrder::ValueFirst,
            } if self.inline_rhs(value)
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
            // A text alone would read as a docstring, or as the kernel marker.
            k::Stmt::Do { value }
                if self.inline.contains(value)
                    && !matches!(value, k::Expr::Literal(k::Literal::Text(_))) =>
            {
                self.out.begin_node(None);
                match value {
                    k::Expr::Act(act) => self.nested_action(act, false),
                    value => self.inline_body(value),
                }
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
            // Python's `for` is a loop through `py.for_in`, or through a
            // dict view's protocol when it walks the view; in a base that
            // labels no such protocol, the kernel's own loop.
            k::Stmt::For {
                binding,
                iterable,
                via,
                body,
                else_block,
                ..
            } if self.inline.contains(iterable)
                && self.operators.native_loop(via.as_ref()).is_some() =>
            {
                self.out.write("for ");
                self.name(binding);
                self.scope.introduce(binding);
                self.out.write(" in ");
                self.inline_expr(iterable);
                if let Some(Some(view)) = self.operators.native_loop(via.as_ref()) {
                    self.out.write(".");
                    self.out.write(view);
                    self.out.write("()");
                }
                self.out.write(":\n");
                self.readable_block(body);
                self.loop_else(else_block);
            }
            k::Stmt::While {
                condition,
                body,
                else_block,
                ..
            } if self.inline.contains(condition) => {
                self.out.write("while ");
                self.inline_expr(condition);
                self.out.write(":\n");
                self.readable_block(body);
                self.loop_else(else_block);
            }
            k::Stmt::Try(scope) if scope.catch.is_some() || scope.finally.is_some() => {
                self.out.write("try:\n");
                self.readable_block(&scope.body);
                if let Some(catch) = &scope.catch {
                    self.caption_next();
                    self.padding();
                    self.out.write("except BaseException as ");
                    self.name(&catch.binding);
                    self.scope.introduce(&catch.binding);
                    self.out.write(":\n");
                    self.readable_block(&catch.body);
                }
                if let Some(finally) = &scope.finally {
                    self.caption_next();
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
                if call {
                    self.inline_expr(value);
                    self.out.write(")");
                } else {
                    self.statement_value(value);
                }
                self.out.write("\n");
            }
            _ => return false,
        }
        true
    }
    fn binding_annotation(&mut self, name: &Name, declaration: bool) {
        let (_, annotation) = self.scope.bind(name, Some(declaration));
        if let Some(annotation) = annotation {
            self.out.write(": ");
            self.out.write(annotation);
        }
    }

    /// An operation as its dotted path: Python spells canonical names
    /// verbatim, and a name it cannot write as an attribute (a keyword)
    /// goes through the qualified escape, `getattr(path, "name")`.
    pub(super) fn operation_path(&mut self, effect: &str) {
        let mut segments = effect.split('.');
        let mut path = segments.next().unwrap_or_default().to_owned();
        for segment in segments {
            if attribute_identifier(segment) {
                path.push('.');
                path.push_str(segment);
            } else {
                path = format!("getattr({path}, {})", serde_json::Value::from(segment));
            }
        }
        self.out.write(&path);
    }
}
