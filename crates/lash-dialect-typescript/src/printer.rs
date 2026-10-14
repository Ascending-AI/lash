//! Format v1 exact forms. Paired readable rules: generated_rules.md.
use crate::TypeScript;
use lash_kernel_dialect::{
    NamePlan, PrintError, PrintOptions, PrintedSource, Printer, SourceFormat, SourceWriter,
    check_document_depth,
};
use lash_kernel_doc as k;

/// Print without compilation, library lookup or admission.
pub fn print(document: &k::Document, options: PrintOptions) -> Result<PrintedSource, PrintError> {
    check_document_depth(document, options.limits)?;
    if k::KernelVersion::of(document.manifest.kernel).is_none() {
        return Err(PrintError::Invalid("unsupported kernel version".into()));
    }
    let plan = NamePlan::new(document, legal_identifier);
    let mut renderer = Renderer {
        out: SourceWriter::new(options),
        plan: &plan,
        indent: 0,
    };
    renderer
        .out
        .write("k.document(\"lash.generated.typescript\", \"1\", ");
    renderer.metadata(&document.manifest);
    renderer.out.write(", ");
    renderer.metadata(&document.entries);
    renderer.out.write(", ");
    renderer.out.json(&document.private_bindings);
    renderer.out.write(", ");
    let names: Vec<_> = plan
        .names
        .iter()
        .map(|(name, alias)| (alias, name))
        .collect();
    renderer.metadata(&names);
    renderer.out.write(", ");
    let helpers: Vec<_> = plan.helpers.iter().map(|(id, alias)| (alias, id)).collect();
    renderer.metadata(&helpers);
    renderer.out.write(");\n");
    for (name, function) in &document.functions {
        renderer.out.write("k.declare(");
        renderer.out.json(name);
        renderer.out.write(", ");
        let unit = options.source_map.then(|| k::Unit::Function(name.clone()));
        renderer.function(&function.params, &function.body, unit.as_ref());
        renderer.out.write(");\n");
    }
    renderer.out.write("k.main(");
    renderer.function(
        &[],
        &document.main,
        options.source_map.then_some(&k::Unit::Main),
    );
    renderer.out.write(");\n");
    renderer.out.finish(document, SourceFormat::TypeScriptV1)
}
impl Printer for TypeScript {
    fn print(
        &self,
        document: &k::Document,
        options: PrintOptions,
    ) -> Result<PrintedSource, PrintError> {
        print(document, options)
    }
}

pub(crate) fn legal_identifier(name: &str) -> bool {
    property_identifier(name)
        && !name.starts_with("helper_")
        && !name.starts_with("kernel_name_")
        && !matches!(
            name,
            "k" | "eval"
                | "arguments"
                | "await"
                | "break"
                | "case"
                | "catch"
                | "class"
                | "const"
                | "continue"
                | "debugger"
                | "default"
                | "delete"
                | "do"
                | "else"
                | "enum"
                | "export"
                | "extends"
                | "false"
                | "finally"
                | "for"
                | "function"
                | "if"
                | "implements"
                | "import"
                | "in"
                | "instanceof"
                | "interface"
                | "let"
                | "new"
                | "null"
                | "package"
                | "private"
                | "protected"
                | "public"
                | "return"
                | "static"
                | "super"
                | "switch"
                | "this"
                | "throw"
                | "true"
                | "try"
                | "typeof"
                | "var"
                | "void"
                | "while"
                | "with"
                | "yield"
        )
}

/// Property names have no binding-keyword or alias-namespace restrictions.
pub(crate) fn property_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    swc_ecma_ast::Ident::is_valid_start(first) && chars.all(swc_ecma_ast::Ident::is_valid_continue)
}
fn native_record(items: &[k::RecordEntry]) -> bool {
    let mut fields = std::collections::BTreeSet::new();
    items.iter().all(|item| {
        property_identifier(&item.field) && item.field != "__proto__" && fields.insert(&item.field)
    })
}
struct Renderer<'a> {
    out: SourceWriter,
    plan: &'a NamePlan,
    indent: usize,
}
impl Renderer<'_> {
    fn metadata(&mut self, value: &impl serde::Serialize) {
        self.out.metadata(value, SourceFormat::TypeScriptV1);
    }

    fn name(&mut self, name: &k::Name) {
        self.out.write(&self.plan.names[name]);
    }
    fn helper(&mut self, id: &k::FunctionId) {
        self.out.write(&self.plan.helpers[id]);
    }
    fn function(&mut self, params: &[k::Name], body: &k::Block, unit: Option<&k::Unit>) {
        self.out.write("function(");
        for (i, param) in params.iter().enumerate() {
            if i > 0 {
                self.out.write(", ");
            }
            self.name(param);
        }
        self.out.write(") ");
        self.node(k::Node::Block(body), unit);
    }
    fn node(&mut self, node: k::Node<'_>, unit: Option<&k::Unit>) {
        self.out.begin_node(unit);
        match node {
            k::Node::Block(body) => {
                self.out.write("{\n");
                self.indent += 1;
                for stmt in body {
                    self.indentation();
                    self.node(k::Node::Stmt(stmt), None);
                    self.out.write("\n");
                }
                self.indent -= 1;
                self.indentation();
                self.out.write("}");
            }
            k::Node::Stmt(stmt) => self.statement(stmt),
            k::Node::Expr(expr) => self.expression(expr),
            k::Node::Action(action) => self.action(action),
        }
        self.out.end_node();
    }
    fn indentation(&mut self) {
        for _ in 0..self.indent {
            self.out.write("  ");
        }
    }
    fn expr(&mut self, expr: &k::Expr) {
        self.node(k::Node::Expr(expr), None);
    }
    fn block(&mut self, block: &k::Block) {
        self.node(k::Node::Block(block), None);
    }
    fn rhs(&mut self, rhs: &k::Rhs) {
        match rhs {
            k::Rhs::Expr(e) => self.expr(e),
            k::Rhs::Action(a) => self.node(k::Node::Action(a), None),
        }
    }
    fn statement(&mut self, stmt: &k::Stmt) {
        match stmt {
            k::Stmt::Let { name, value } => {
                self.out.write("let ");
                self.name(name);
                self.out.write(" = ");
                self.rhs(value);
                self.out.write(";");
            }
            k::Stmt::Assign { place, value } => {
                match place {
                    k::Place::Variable(n) => self.name(n),
                    k::Place::Member(m) => {
                        self.member(m);
                        if matches!(m, k::Member::Field { field, .. } if !property_identifier(field))
                        {
                            self.out.write(".value");
                        }
                    }
                }
                self.out.write(" = ");
                self.rhs(value);
                self.out.write(";");
            }
            k::Stmt::Remove { member } => {
                self.out.write("k.remove(");
                self.member(member);
                self.out.write(");");
            }
            k::Stmt::Do { action } => {
                self.node(k::Node::Action(action), None);
                self.out.write(";");
            }
            k::Stmt::If {
                condition,
                then_block,
                else_block,
            } => {
                self.out.write("if (");
                self.expr(condition);
                self.out.write(") ");
                self.block(then_block);
                if else_block.is_empty() {
                    // The omitted empty else is the sole implicit TS node rule.
                    self.out.begin_node(None);
                    self.out.end_node();
                } else {
                    self.out.write(" else ");
                    self.block(else_block);
                }
            }
            k::Stmt::For {
                binding,
                iterable,
                body,
            } => {
                self.out.write("for (let ");
                self.name(binding);
                self.out.write(" of ");
                self.expr(iterable);
                self.out.write(") ");
                self.block(body);
            }
            k::Stmt::While { condition, body } => {
                self.out.write("while (");
                self.expr(condition);
                self.out.write(") ");
                self.block(body);
            }
            k::Stmt::Break => self.out.write("break;"),
            k::Stmt::Continue => self.out.write("continue;"),
            k::Stmt::Return { value } => {
                self.out.write("return ");
                self.expr(value);
                self.out.write(";");
            }
            k::Stmt::Throw { value } => {
                self.out.write("throw ");
                self.expr(value);
                self.out.write(";");
            }
            k::Stmt::Print { value } | k::Stmt::Finish { value } | k::Stmt::Fail { value } => {
                self.out.write(match stmt {
                    k::Stmt::Print { .. } => "k.print(",
                    k::Stmt::Finish { .. } => "k.finish(",
                    _ => "k.fail(",
                });
                self.expr(value);
                self.out.write(");");
            }
            k::Stmt::Try(scope) if scope.catch.is_none() && scope.finally.is_none() => {
                self.out.write("k.try(");
                self.function(&[], &scope.body, None);
                self.out.write(");");
            }
            k::Stmt::Try(scope) => {
                self.out.write("try ");
                self.block(&scope.body);
                if let Some(catch) = &scope.catch {
                    self.out.write(" catch (");
                    self.name(&catch.binding);
                    self.out.write(") ");
                    self.block(&catch.body);
                }
                if let Some(finally) = &scope.finally {
                    self.out.write(" finally ");
                    self.block(finally);
                }
            }
        }
    }
    fn member(&mut self, member: &k::Member) {
        match member {
            k::Member::Field { target, field } if property_identifier(field) => {
                self.out.write("(");
                self.expr(target);
                self.out.write(").");
                self.out.write(field);
            }
            k::Member::Field { target, field } => {
                self.out.write("k.field(");
                self.expr(target);
                self.out.write(", ");
                self.out.json(field);
                self.out.write(")");
            }
            k::Member::Index { target, index } => {
                self.out.write("(");
                self.expr(target);
                self.out.write(")[");
                self.expr(index);
                self.out.write("]");
            }
        }
    }
    fn literal(&mut self, literal: &k::Literal) {
        match literal {
            k::Literal::Null => self.out.write("null"),
            k::Literal::Absent => self.out.write("k.absent"),
            k::Literal::Bool(b) => self.out.write(if *b { "true" } else { "false" }),
            k::Literal::Text(s) => self.out.json(s),
            k::Literal::Int(n) if n.bits() <= 53 => self.out.write(&n.to_string()),
            k::Literal::Int(n) => {
                self.out.write("k.int(");
                self.out.json(&n.to_string());
                self.out.write(")");
            }
            k::Literal::Float(n) if n.get().is_finite() => self.out.write(&n.to_string()),
            k::Literal::Float(n) => {
                self.out.write("k.float(");
                self.out.json(&n.to_string());
                self.out.write(")");
            }
            k::Literal::Bytes(b) => {
                self.out.write("k.bytes(");
                self.out.json(&b.to_hex());
                self.out.write(")");
            }
            k::Literal::Function(n) => {
                self.out.write("k.function(");
                self.out.json(n);
                self.out.write(")");
            }
        }
    }
    fn atom(&mut self, atom: &k::Atom) {
        match atom {
            k::Atom::Literal(l) => self.literal(l),
            k::Atom::Variable(n) => self.name(n),
        }
    }
    fn atoms(&mut self, atoms: &[k::Atom]) {
        self.out.write("[");
        for (i, atom) in atoms.iter().enumerate() {
            if i > 0 {
                self.out.write(", ");
            }
            self.atom(atom);
        }
        self.out.write("]");
    }
    fn callee(&mut self, callee: &k::Callee) {
        match callee {
            k::Callee::Declared(n) | k::Callee::Value(n) => {
                self.out.write(if matches!(callee, k::Callee::Declared(_)) {
                    "k.declared("
                } else {
                    "k.value("
                });
                self.out.json(n);
            }
            k::Callee::Library(id) => {
                self.helper(id);
                return;
            }
        }
        self.out.write(")");
    }
    fn action(&mut self, action: &k::Action) {
        match action {
            k::Action::Call { callee, args } | k::Action::Spawn { callee, args } => {
                self.out.write(if matches!(action, k::Action::Call { .. }) {
                    "k.call("
                } else {
                    "k.spawn("
                });
                self.callee(callee);
                self.out.write(", ");
                self.atoms(args);
            }
            k::Action::Perform {
                effect,
                args,
                result,
            } => {
                self.out.write("k.perform(");
                self.out.json(effect);
                self.out.write(", ");
                self.atoms(args);
                self.out.write(", k.type(");
                self.metadata(result);
                self.out.write(")");
            }
            k::Action::Sleep { duration } => {
                self.out.write("k.sleep(");
                self.atom(duration);
            }
            k::Action::Join { task } | k::Action::Cancel { task } => {
                self.out.write(if matches!(action, k::Action::Join { .. }) {
                    "k.join("
                } else {
                    "k.cancel("
                });
                self.atom(task);
            }
            k::Action::JoinMany { mode, tasks } => {
                self.out.write("k.joinMany(");
                self.out.json(match mode {
                    k::JoinMode::All => "all",
                    k::JoinMode::AllSettled => "all_settled",
                    k::JoinMode::Race => "race",
                    k::JoinMode::Any => "any",
                });
                self.out.write(", ");
                self.atom(tasks);
            }
            k::Action::Yield => self.out.write("k.yield("),
        }
        self.out.write(")");
    }
    fn expression(&mut self, expr: &k::Expr) {
        match expr {
            k::Expr::Literal(l) => self.literal(l),
            k::Expr::Variable(n) => self.name(n),
            k::Expr::List(items) => {
                self.out.write("[");
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        self.out.write(", ");
                    }
                    self.expr(item);
                }
                self.out.write("]");
            }
            k::Expr::Tuple(items) | k::Expr::Set(items) => {
                self.out.write(match expr {
                    k::Expr::Tuple(_) => "k.tuple(",
                    _ => "k.set(",
                });
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        self.out.write(", ");
                    }
                    self.expr(item);
                }
                self.out.write(")");
            }
            k::Expr::Map(items) => {
                self.out.write("k.map(");
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        self.out.write(", ");
                    }
                    self.out.write("[");
                    self.expr(&item.key);
                    self.out.write(", ");
                    self.expr(&item.value);
                    self.out.write("]");
                }
                self.out.write(")");
            }
            k::Expr::Record(items) if native_record(items) => {
                self.out.write("{");
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        self.out.write(", ");
                    }
                    self.out.write(&item.field);
                    self.out.write(": ");
                    self.expr(&item.value);
                }
                self.out.write("}");
            }
            k::Expr::Record(items) => {
                self.out.write("k.record(");
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        self.out.write(", ");
                    }
                    self.out.write("[");
                    self.out.json(&item.field);
                    self.out.write(", ");
                    self.expr(&item.value);
                    self.out.write("]");
                }
                self.out.write(")");
            }
            k::Expr::Member(m) => self.member(m),
            k::Expr::Closure(c) => self.function(&c.params, &c.body, None),
            k::Expr::Call { function, args } => {
                self.helper(function);
                self.out.write("(");
                for (i, arg) in args.iter().enumerate() {
                    if i > 0 {
                        self.out.write(", ");
                    }
                    self.expr(arg);
                }
                self.out.write(")");
            }
            k::Expr::Clock => self.out.write("k.clock()"),
            k::Expr::Random => self.out.write("k.random()"),
            k::Expr::Read(read) => {
                self.out.write("k.read(");
                self.expr(&read.handle);
                self.out.write(", ");
                self.expr(&read.request);
                self.out.write(")");
            }
        }
    }
}
